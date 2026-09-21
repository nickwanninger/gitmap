//! The map widget and the colour functions that give it meaning.
//!
//! All views share one geometry and one canvas. They differ only in the
//! function mapping a file to a colour, so switching views is swapping a
//! `Colorizer` and redrawing — no relayout, so the map appears to hold still
//! while its meaning changes.

use super::canvas::{Canvas, Rgb};
use super::palette::{self, ColorDepth, StatusPalette};
use crate::git::{Change, FileStatus};
use crate::layout::tree::{NodeId, Tree};
use crate::layout::treemap::Layout;
use std::collections::HashMap;
use std::path::PathBuf;

/// Per-file facts a colorizer may consult. Assembled once per refresh.
pub struct MapData {
    pub status: HashMap<PathBuf, FileStatus>,
    /// Seconds since the file's last modifying commit, when known.
    pub age: HashMap<PathBuf, i64>,
}

impl MapData {
    pub fn new() -> MapData {
        MapData {
            status: HashMap::new(),
            age: HashMap::new(),
        }
    }
}

pub trait Colorizer {
    fn color(&self, tree: &Tree, id: NodeId, data: &MapData) -> Rgb;
    fn legend(&self) -> Vec<(&'static str, Rgb)>;
    fn name(&self) -> &'static str;
}

/// Colour by working-tree state.
pub struct StatusColorizer {
    pub palette: StatusPalette,
}

impl Colorizer for StatusColorizer {
    fn color(&self, tree: &Tree, id: NodeId, data: &MapData) -> Rgb {
        let node = tree.node(id);
        match data.status.get(&node.path) {
            Some(s) => self.palette.color(s.dominant(), s.is_staged()),
            None => self.palette.unchanged,
        }
    }

    fn legend(&self) -> Vec<(&'static str, Rgb)> {
        let p = &self.palette;
        vec![
            ("added", p.added),
            ("modified", p.modified),
            ("deleted", p.deleted),
            ("untracked", p.untracked),
        ]
    }

    fn name(&self) -> &'static str {
        "status"
    }
}

/// Colour by time since last modifying commit, on a magma ramp.
pub struct HeatColorizer {
    pub palette: StatusPalette,
}

impl HeatColorizer {
    /// Map an age in seconds onto 0..1, log-scaled.
    ///
    /// Linear time makes everything older than a month indistinguishable, and
    /// the interesting structure is all at the recent end.
    fn t(age_secs: i64) -> f32 {
        const HOUR: f32 = 3600.0;
        // Two years saturates the ramp.
        const MAX: f32 = 730.0 * 24.0 * HOUR;
        let a = (age_secs.max(0) as f32).max(HOUR);
        let t = (a / HOUR).ln() / (MAX / HOUR).ln();
        // Recent is bright, so invert.
        1.0 - t.clamp(0.0, 1.0)
    }
}

impl Colorizer for HeatColorizer {
    fn color(&self, tree: &Tree, id: NodeId, data: &MapData) -> Rgb {
        match data.age.get(&tree.node(id).path) {
            Some(&age) => palette::sample(&palette::MAGMA, Self::t(age)),
            None => self.palette.unchanged,
        }
    }

    fn legend(&self) -> Vec<(&'static str, Rgb)> {
        vec![
            ("old", palette::MAGMA[1]),
            ("", palette::MAGMA[3]),
            ("", palette::MAGMA[5]),
            ("recent", palette::MAGMA[7]),
        ]
    }

    fn name(&self) -> &'static str {
        "heatmap"
    }
}

/// Draw the whole map into `canvas`.
///
/// Directory label bands are painted first, then files over them, then the
/// hover overlay — which never mutates the base colours, so un-hovering is a
/// plain redraw of the previous state.
pub fn draw(
    canvas: &mut Canvas,
    tree: &Tree,
    layout: &Layout,
    data: &MapData,
    colorizer: &dyn Colorizer,
    palette: &StatusPalette,
    depth: ColorDepth,
    hovered: Option<NodeId>,
) {
    canvas.clear(depth.quantize(palette.background));

    // Directory tints, shallowest first so deeper ones paint over.
    let mut dirs: Vec<NodeId> = (0..tree.len()).filter(|&i| tree.node(i).is_dir).collect();
    dirs.sort_by_key(|&i| tree.node(i).depth);
    for id in dirs {
        let Some(r) = layout.rects[id] else { continue };
        if id == tree.root {
            continue;
        }
        // A dim tint proportional to depth gives visual grouping without
        // borders, which would eat pixels the files need.
        let t = 0.10 + 0.05 * tree.node(id).depth.min(4) as f32;
        let tint = palette::mix(palette.background, palette.dir_tint, t);
        canvas.fill_rect(r, depth.quantize(tint));
    }

    // Files.
    for id in 0..tree.len() {
        let Some(r) = layout.rects[id] else { continue };
        let node = tree.node(id);
        let leaf = !node.is_dir || layout.collapsed[id];
        if !leaf {
            continue;
        }
        let c = if layout.collapsed[id] {
            // An aggregate block reads as one dim mass; its child count goes
            // in the status line on hover.
            palette::mix(palette.background, palette.dir_tint, 0.35)
        } else {
            colorizer.color(tree, id, data)
        };
        canvas.fill_rect(r, depth.quantize(c));
    }

    // Hover overlay: brighten by a fixed OKLab lightness delta.
    if let Some(h) = hovered {
        if let Some(r) = layout.rects[h] {
            let x0 = r.x.round().max(0.0) as u16;
            let y0 = r.y.round().max(0.0) as u16;
            let x1 = ((r.x + r.w).round() as u16).min(canvas.w);
            let y1 = ((r.y + r.h).round() as u16).min(canvas.h);
            for y in y0..y1 {
                for x in x0..x1 {
                    let base = canvas.get(x, y);
                    canvas.set(x, y, depth.quantize(palette::lighten(base, 0.18)));
                }
            }
        }
    }
}

/// Count the staged / unstaged / untracked files for the status line.
pub fn counts(status: &HashMap<PathBuf, FileStatus>) -> (usize, usize, usize) {
    let mut staged = 0;
    let mut unstaged = 0;
    let mut untracked = 0;
    for s in status.values() {
        if s.unstaged == Change::Untracked {
            untracked += 1;
            continue;
        }
        if s.is_staged() {
            staged += 1;
        }
        if s.unstaged != Change::None {
            unstaged += 1;
        }
    }
    (staged, unstaged, untracked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::tree::Scale;
    use crate::layout::treemap::{self, Rect};

    fn tree() -> Tree {
        Tree::build(
            &[
                (PathBuf::from("src/a.rs"), 100),
                (PathBuf::from("src/b.rs"), 100),
            ],
            Scale::Linear,
        )
    }

    #[test]
    fn heat_t_is_monotonic_and_bounded() {
        let recent = HeatColorizer::t(3600);
        let old = HeatColorizer::t(365 * 24 * 3600);
        let ancient = HeatColorizer::t(10 * 365 * 24 * 3600);
        assert!(recent > old && old > ancient);
        for v in [recent, old, ancient] {
            assert!((0.0..=1.0).contains(&v), "{v} out of range");
        }
        // A file touched an hour ago sits at the bright end.
        assert!(recent > 0.95);
        // Saturates rather than going negative for very old files.
        assert_eq!(ancient, 0.0);
    }

    #[test]
    fn status_colorizer_distinguishes_staged() {
        let t = tree();
        let mut d = MapData::new();
        d.status.insert(
            PathBuf::from("src/a.rs"),
            FileStatus {
                path: PathBuf::from("src/a.rs"),
                staged: Change::Modified,
                unstaged: Change::None,
            },
        );
        d.status.insert(
            PathBuf::from("src/b.rs"),
            FileStatus {
                path: PathBuf::from("src/b.rs"),
                staged: Change::None,
                unstaged: Change::Modified,
            },
        );
        let c = StatusColorizer {
            palette: StatusPalette::default(),
        };
        let a = c.color(&t, t.find(std::path::Path::new("src/a.rs")).unwrap(), &d);
        let b = c.color(&t, t.find(std::path::Path::new("src/b.rs")).unwrap(), &d);
        assert_ne!(a, b, "staged and unstaged must not render identically");
    }

    #[test]
    fn unknown_file_falls_back_to_unchanged() {
        let t = tree();
        let d = MapData::new();
        let p = StatusPalette::default();
        let c = StatusColorizer {
            palette: StatusPalette::default(),
        };
        let id = t.find(std::path::Path::new("src/a.rs")).unwrap();
        assert_eq!(c.color(&t, id, &d), p.unchanged);
    }

    #[test]
    fn hover_brightens_only_the_hovered_block() {
        let t = tree();
        let l = treemap::layout(&t, Rect::new(0.0, 0.0, 20.0, 20.0));
        let d = MapData::new();
        let p = StatusPalette::default();
        let c = StatusColorizer {
            palette: StatusPalette::default(),
        };
        let a = t.find(std::path::Path::new("src/a.rs")).unwrap();
        let b = t.find(std::path::Path::new("src/b.rs")).unwrap();

        let mut plain = Canvas::new(20, 20);
        draw(&mut plain, &t, &l, &d, &c, &p, ColorDepth::True, None);
        let mut hov = Canvas::new(20, 20);
        draw(&mut hov, &t, &l, &d, &c, &p, ColorDepth::True, Some(a));

        let sample = |cv: &Canvas, id: NodeId| {
            let r = l.rects[id].unwrap();
            cv.get((r.x + r.w / 2.0) as u16, (r.y + r.h / 2.0) as u16)
        };
        assert_ne!(sample(&plain, a), sample(&hov, a), "hovered block unchanged");
        assert_eq!(sample(&plain, b), sample(&hov, b), "sibling was disturbed");
    }

    #[test]
    fn counts_classify_each_side() {
        let mut m = HashMap::new();
        let add = |m: &mut HashMap<PathBuf, FileStatus>, p: &str, s, u| {
            m.insert(
                PathBuf::from(p),
                FileStatus {
                    path: PathBuf::from(p),
                    staged: s,
                    unstaged: u,
                },
            );
        };
        add(&mut m, "a", Change::Modified, Change::None);
        add(&mut m, "b", Change::None, Change::Modified);
        add(&mut m, "c", Change::Modified, Change::Modified);
        add(&mut m, "d", Change::None, Change::Untracked);
        let (s, u, un) = counts(&m);
        assert_eq!((s, u, un), (2, 2, 1));
    }
}
