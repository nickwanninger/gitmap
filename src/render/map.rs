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

impl Default for MapData {
    fn default() -> Self {
        Self::new()
    }
}

impl MapData {
    pub fn new() -> MapData {
        MapData {
            status: HashMap::new(),
            age: HashMap::new(),
        }
    }
}

/// Amplitude of the per-file lightness jitter, as a fraction of the block's
/// own lightness. Large enough to separate neighbours at a glance, small enough
/// that it does not read as a status difference.
///
/// Kept small now that hue separates directories: lightness is what the status
/// intensity uses, so a large jitter here would blur staged against unstaged.
const JITTER: f32 = 0.10;

/// How much of a node's own hue arc the per-file hue jitter may use.
///
/// Spreading files across the arc their directory already owns is what keeps
/// neighbours distinguishable without any of them leaving the directory's
/// colour family. A fraction rather than a fixed angle, so a small directory
/// with a narrow arc stays tight and a large one spreads out.
const HUE_SPREAD: f32 = 0.7;

/// Never spread a file's hue further than this from its directory's centre,
/// however wide the arc. Beyond roughly this, two files in one directory stop
/// reading as related.
const HUE_SPREAD_MAX_DEG: f32 = 14.0;

/// A path's raw bytes, so the jitter is stable for the non-UTF-8 paths that
/// exist on Linux rather than collapsing them all onto the same hash.
#[cfg(unix)]
fn path_bytes(p: &std::path::Path) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes()
}

#[cfg(not(unix))]
fn path_bytes(p: &std::path::Path) -> &[u8] {
    // Windows paths are UTF-16; the lossy view is stable enough here because
    // the value only drives a cosmetic tint.
    p.to_str().map(|s| s.as_bytes()).unwrap_or(b"")
}

pub trait Colorizer {
    fn color(&self, tree: &Tree, id: NodeId, data: &MapData) -> Rgb;
    fn legend(&self) -> Vec<(&'static str, Rgb)>;
    fn name(&self) -> &'static str;

    /// Whether this view's colours carry the directory tree in their hue.
    ///
    /// When they do, per-file jitter may rotate hue within the directory's arc.
    /// When they do not — the heatmap, where hue *is* the data — jitter has to
    /// stay out of hue entirely or it would corrupt the ramp.
    fn uses_tree_hue(&self) -> bool {
        false
    }
}

/// Colour by working-tree state.
pub struct StatusColorizer {
    pub palette: StatusPalette,
}

impl Colorizer for StatusColorizer {
    fn color(&self, tree: &Tree, id: NodeId, data: &MapData) -> Rgb {
        let node = tree.node(id);
        let (change, staged) = match data.status.get(&node.path) {
            Some(s) => (s.dominant(), s.is_staged()),
            None => (Change::None, false),
        };
        // Hue is the directory's, so files cluster by where they live;
        // chroma and lightness carry the change state.
        self.palette.color_at_hue(tree.hue_of(id), change, staged)
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

    fn uses_tree_hue(&self) -> bool {
        true
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
        let tint = if colorizer.uses_tree_hue() {
            // Tint toward the directory's own hue, so the gaps between blocks
            // reinforce the same regions the files do.
            let base = palette::mix(palette.background, palette.dir_tint, t);
            let o = palette::to_oklab(base);
            palette::from_lch(tree.hue_of(id), 0.022, o.l)
        } else {
            palette::mix(palette.background, palette.dir_tint, t)
        };
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
        // Per-file jitter, so neighbouring blocks do not merge into one flat
        // region. Keyed on the path, so it is stable across redraws and across
        // relayouts. Mostly hue — a nudge within the arc the file's own
        // directory owns, which separates siblings while keeping them in the
        // same colour family — plus a little lightness for the views where hue
        // is not free to move.
        let n = palette::jitter_for(path_bytes(&node.path));
        let c = if colorizer.uses_tree_hue() {
            let (_, arc) = node.hue;
            let spread = (arc * HUE_SPREAD / 2.0).min(HUE_SPREAD_MAX_DEG);
            palette::shift_hue(c, n * spread)
        } else {
            c
        };
        canvas.fill_rect(r, depth.quantize(palette::jitter(c, n * JITTER)));
    }

    if let Some(h) = hovered {
        draw_hover(canvas, tree, layout, depth, h);
    }
}

/// Brightness lift applied to the hovered block itself. Each enclosing
/// directory gets `HOVER_LIFT * HOVER_FALLOFF^depth`, so the hovered file is
/// brightest, its siblings next, then its parent's siblings, and so on.
const HOVER_LIFT: f32 = 0.18;

/// How much of the lift each step up the tree keeps. Low enough that the
/// hovered block stays clearly the focus, high enough that two or three levels
/// of context are still visible.
const HOVER_FALLOFF: f32 = 0.45;

/// Below this the lift is not worth a pass over the pixels — it rounds away in
/// 8-bit sRGB and costs a full-rectangle redraw for nothing.
const HOVER_MIN: f32 = 0.012;

/// Draw the hover highlight as a gradient radiating out through the hovered
/// node's ancestors.
///
/// This is what turns the highlight into an answer to "where am I?": the
/// hovered file is brightest, everything sharing its directory is lifted a
/// little, its parent directory a little less, out to the root. Painted
/// outermost-first so that nested rectangles overwrite their enclosing ones and
/// every pixel ends up at the level of its *closest* highlighted ancestor.
///
/// The base canvas is never mutated by anything else, so un-hovering is a plain
/// redraw rather than an undo.
fn draw_hover(
    canvas: &mut Canvas,
    tree: &Tree,
    layout: &Layout,
    depth: ColorDepth,
    hovered: NodeId,
) {
    // `ancestors` is nearest-first; reversed it runs root → parent, which is
    // the order that lets the inner rectangles win.
    let mut steps: Vec<(NodeId, f32)> = Vec::new();
    let chain = tree.ancestors(hovered);
    for (i, &a) in chain.iter().enumerate() {
        // The root covers the whole canvas, so lifting it just brightens
        // everything uniformly — no contrast, no information.
        if a == tree.root {
            continue;
        }
        // The immediate parent is one step out, its parent two, and so on.
        let lift = HOVER_LIFT * HOVER_FALLOFF.powi(i as i32 + 1);
        if lift >= HOVER_MIN {
            steps.push((a, lift));
        }
    }
    steps.reverse();
    steps.push((hovered, HOVER_LIFT));

    for (id, lift) in steps {
        let Some(r) = layout.rects[id] else { continue };
        let x0 = r.x.round().max(0.0) as u16;
        let y0 = r.y.round().max(0.0) as u16;
        let x1 = ((r.x + r.w).round() as u16).min(canvas.w);
        let y1 = ((r.y + r.h).round() as u16).min(canvas.h);
        for y in y0..y1 {
            for x in x0..x1 {
                let base = canvas.get(x, y);
                canvas.set(x, y, depth.quantize(palette::lighten(base, lift)));
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
    fn unknown_file_renders_as_a_dim_tint_of_its_directory() {
        // A file with no status entry is unchanged: near-grey, but carrying
        // its directory's hue so the codebase's shape stays readable.
        let t = tree();
        let d = MapData::new();
        let c = StatusColorizer {
            palette: StatusPalette::default(),
        };
        let id = t.find(std::path::Path::new("src/a.rs")).unwrap();
        let got = palette::to_oklab(c.color(&t, id, &d));
        let chroma = (got.a * got.a + got.b * got.b).sqrt();
        // Enough colour to show which directory it belongs to, far less than a
        // file with an actual change.
        assert!(
            chroma > 0.02,
            "unchanged file has no directory hue: {chroma}"
        );
        assert!(chroma < 0.06, "unchanged file is too colourful: {chroma}");
        assert!(
            got.l > 0.15 && got.l < 0.45,
            "unchanged file should be dim, got lightness {}",
            got.l
        );
    }

    /// Lightness at the centre of a node's rectangle.
    fn lum_at(cv: &Canvas, l: &treemap::Layout, id: NodeId) -> f32 {
        let r = l.rects[id].unwrap();
        palette::to_oklab(cv.get((r.x + r.w / 2.0) as u16, (r.y + r.h / 2.0) as u16)).l
    }

    fn nested_tree() -> Tree {
        Tree::build(
            &[
                (PathBuf::from("src/git/a.rs"), 100),
                (PathBuf::from("src/git/b.rs"), 100),
                (PathBuf::from("src/ui/c.rs"), 100),
                (PathBuf::from("docs/d.md"), 100),
                (PathBuf::from("docs/e.md"), 100),
            ],
            Scale::Linear,
        )
    }

    fn render_hover(t: &Tree, l: &treemap::Layout, hovered: Option<NodeId>) -> Canvas {
        let d = MapData::new();
        let p = StatusPalette::default();
        let c = StatusColorizer {
            palette: StatusPalette::default(),
        };
        let mut cv = Canvas::new(80, 80);
        draw(&mut cv, t, l, &d, &c, &p, ColorDepth::True, hovered);
        cv
    }

    #[test]
    fn hover_brightens_the_hovered_block_most() {
        let t = nested_tree();
        let l = treemap::layout(&t, Rect::new(0.0, 0.0, 80.0, 80.0));
        let a = t.find(std::path::Path::new("src/git/a.rs")).unwrap();

        let plain = render_hover(&t, &l, None);
        let hov = render_hover(&t, &l, Some(a));
        assert!(
            lum_at(&hov, &l, a) > lum_at(&plain, &l, a),
            "hovered block was not brightened"
        );
    }

    #[test]
    fn hover_lift_falls_off_with_tree_distance() {
        // The point of the graded highlight: a sibling in the same directory
        // is lifted more than a cousin one level out, which is lifted more
        // than an unrelated subtree.
        let t = nested_tree();
        let l = treemap::layout(&t, Rect::new(0.0, 0.0, 80.0, 80.0));
        let p = std::path::Path::new("src/git/a.rs");
        let a = t.find(p).unwrap();

        let plain = render_hover(&t, &l, None);
        let hov = render_hover(&t, &l, Some(a));
        let delta = |id: NodeId| lum_at(&hov, &l, id) - lum_at(&plain, &l, id);

        let sibling = t.find(std::path::Path::new("src/git/b.rs")).unwrap();
        let cousin = t.find(std::path::Path::new("src/ui/c.rs")).unwrap();
        let stranger = t.find(std::path::Path::new("docs/d.md")).unwrap();

        let (d_self, d_sib, d_cou, d_str) =
            (delta(a), delta(sibling), delta(cousin), delta(stranger));

        assert!(d_self > d_sib, "self {d_self} should beat sibling {d_sib}");
        assert!(d_sib > d_cou, "sibling {d_sib} should beat cousin {d_cou}");
        assert!(d_cou > d_str, "cousin {d_cou} should beat stranger {d_str}");
        // An unrelated subtree is left alone entirely.
        assert!(d_str.abs() < 0.005, "unrelated subtree moved by {d_str}");
    }

    #[test]
    fn hover_does_not_lift_the_whole_canvas() {
        // The root encloses everything, so lifting it would wash the map out
        // uniformly and convey nothing.
        let t = nested_tree();
        let l = treemap::layout(&t, Rect::new(0.0, 0.0, 80.0, 80.0));
        let a = t.find(std::path::Path::new("src/git/a.rs")).unwrap();

        let plain = render_hover(&t, &l, None);
        let hov = render_hover(&t, &l, Some(a));
        let far = t.find(std::path::Path::new("docs/e.md")).unwrap();
        assert!(
            (lum_at(&hov, &l, far) - lum_at(&plain, &l, far)).abs() < 0.005,
            "a file in an unrelated top-level directory was brightened"
        );
    }

    #[test]
    fn hover_at_top_level_still_highlights() {
        // A file directly under the root has no non-root ancestor, so the
        // gradient is just the block itself. It must not panic or vanish.
        let t = Tree::build(
            &[(PathBuf::from("a.rs"), 100), (PathBuf::from("b.rs"), 100)],
            Scale::Linear,
        );
        let l = treemap::layout(&t, Rect::new(0.0, 0.0, 40.0, 40.0));
        let a = t.find(std::path::Path::new("a.rs")).unwrap();

        let plain = render_hover(&t, &l, None);
        let hov = render_hover(&t, &l, Some(a));
        assert!(lum_at(&hov, &l, a) > lum_at(&plain, &l, a));
    }

    #[test]
    fn hovering_a_directory_lifts_its_whole_subtree() {
        // Hovering the label band of a directory should light up everything
        // inside it, since that is the unit `a` stages.
        let t = nested_tree();
        let l = treemap::layout(&t, Rect::new(0.0, 0.0, 80.0, 80.0));
        let dir = t.find(std::path::Path::new("src/git")).unwrap();

        let plain = render_hover(&t, &l, None);
        let hov = render_hover(&t, &l, Some(dir));
        for name in ["src/git/a.rs", "src/git/b.rs"] {
            let f = t.find(std::path::Path::new(name)).unwrap();
            assert!(
                lum_at(&hov, &l, f) > lum_at(&plain, &l, f),
                "{name} not lifted when its directory was hovered"
            );
        }
    }

    #[test]
    fn un_hovering_restores_the_original_colours() {
        // The overlay must never mutate the base canvas, or the map would
        // drift brighter every time the cursor passed over it.
        let t = nested_tree();
        let l = treemap::layout(&t, Rect::new(0.0, 0.0, 80.0, 80.0));
        let a = t.find(std::path::Path::new("src/git/a.rs")).unwrap();

        let first = render_hover(&t, &l, None);
        let _ = render_hover(&t, &l, Some(a));
        let after = render_hover(&t, &l, None);
        assert_eq!(first.px, after.px, "colours drifted after a hover");
    }

    #[test]
    fn files_in_one_directory_render_in_one_colour_family() {
        // The headline promise: two files in the same folder look related, and
        // a file in a different folder does not.
        let t = Tree::build(
            &[
                (PathBuf::from("src/git/a.rs"), 100),
                (PathBuf::from("src/git/b.rs"), 100),
                (PathBuf::from("docs/x.md"), 100),
                (PathBuf::from("docs/y.md"), 100),
            ],
            Scale::Linear,
        );
        let l = treemap::layout(&t, Rect::new(0.0, 0.0, 60.0, 60.0));
        let cv = render_hover(&t, &l, None);
        let hue_at = |name: &str| {
            let id = t.find(std::path::Path::new(name)).unwrap();
            let r = l.rects[id].unwrap();
            let o = palette::to_oklab(cv.get((r.x + r.w / 2.0) as u16, (r.y + r.h / 2.0) as u16));
            o.b.atan2(o.a).to_degrees().rem_euclid(360.0)
        };
        let sep = |a: f32, b: f32| ((a - b + 180.0).rem_euclid(360.0) - 180.0).abs();

        let within = sep(hue_at("src/git/a.rs"), hue_at("src/git/b.rs"));
        let across = sep(hue_at("src/git/a.rs"), hue_at("docs/x.md"));
        assert!(
            within < 30.0,
            "files in one directory are {within} degrees apart"
        );
        assert!(
            across > 60.0,
            "files in different directories are only {across} degrees apart"
        );
        assert!(across > within * 2.0, "grouping is not legible");
    }

    #[test]
    fn a_change_raises_chroma_without_leaving_the_directory_hue() {
        // Staging must read as the directory's colour filling in, not as a
        // different colour.
        let t = Tree::build(
            &[
                (PathBuf::from("src/a.rs"), 100),
                (PathBuf::from("src/b.rs"), 100),
            ],
            Scale::Linear,
        );
        let mut d = MapData::new();
        d.status.insert(
            PathBuf::from("src/a.rs"),
            FileStatus {
                path: PathBuf::from("src/a.rs"),
                staged: Change::Modified,
                unstaged: Change::None,
            },
        );
        let c = StatusColorizer {
            palette: StatusPalette::default(),
        };
        let changed = t.find(std::path::Path::new("src/a.rs")).unwrap();
        let quiet = t.find(std::path::Path::new("src/b.rs")).unwrap();
        let o = |id| palette::to_oklab(c.color(&t, id, &d));
        let (oc, oq) = (o(changed), o(quiet));
        let ch = |x: &palette::Oklab| (x.a * x.a + x.b * x.b).sqrt();
        let hue = |x: &palette::Oklab| x.b.atan2(x.a).to_degrees().rem_euclid(360.0);

        assert!(ch(&oc) > ch(&oq) * 2.0, "the change barely shows");
        let d_hue = ((hue(&oc) - hue(&oq) + 180.0).rem_euclid(360.0) - 180.0).abs();
        assert!(d_hue < 10.0, "the change moved hue by {d_hue} degrees");
    }

    #[test]
    fn same_directory_neighbours_still_render_differently() {
        // Restored after the hue change: within one directory's colour family,
        // per-file jitter must still separate adjacent blocks or they merge
        // into a single flat region again.
        let t = Tree::build(
            &[
                (PathBuf::from("src/a.rs"), 100),
                (PathBuf::from("src/b.rs"), 100),
                (PathBuf::from("src/c.rs"), 100),
                (PathBuf::from("src/d.rs"), 100),
            ],
            Scale::Linear,
        );
        let l = treemap::layout(&t, Rect::new(0.0, 0.0, 40.0, 40.0));
        let cv = render_hover(&t, &l, None);
        let mut seen = std::collections::HashSet::new();
        for f in t.files_under(t.root) {
            let r = l.rects[f].unwrap();
            seen.insert(cv.get((r.x + r.w / 2.0) as u16, (r.y + r.h / 2.0) as u16));
        }
        assert_eq!(
            seen.len(),
            4,
            "four files in one directory collapsed to {} colours",
            seen.len()
        );
    }

    #[test]
    fn jitter_is_stable_across_redraws() {
        // A value re-rolled per frame would make the map shimmer.
        let t = nested_tree();
        let l = treemap::layout(&t, Rect::new(0.0, 0.0, 40.0, 40.0));
        let a = render_hover(&t, &l, None);
        let b = render_hover(&t, &l, None);
        assert_eq!(a.px, b.px);
    }

    #[test]
    fn hue_jitter_stays_inside_the_directory_family() {
        // The jitter spreads files within their directory's arc; it must never
        // push one far enough to look like it belongs elsewhere.
        let t = Tree::build(
            &[
                (PathBuf::from("src/a.rs"), 100),
                (PathBuf::from("src/b.rs"), 100),
                (PathBuf::from("src/c.rs"), 100),
                (PathBuf::from("other/z.rs"), 100),
            ],
            Scale::Linear,
        );
        let l = treemap::layout(&t, Rect::new(0.0, 0.0, 60.0, 60.0));
        let cv = render_hover(&t, &l, None);
        let hue_at = |name: &str| {
            let id = t.find(std::path::Path::new(name)).unwrap();
            let r = l.rects[id].unwrap();
            let o = palette::to_oklab(cv.get((r.x + r.w / 2.0) as u16, (r.y + r.h / 2.0) as u16));
            o.b.atan2(o.a).to_degrees().rem_euclid(360.0)
        };
        let sep = |a: f32, b: f32| ((a - b + 180.0).rem_euclid(360.0) - 180.0).abs();
        let base = t.hue_of(t.find(std::path::Path::new("src")).unwrap());
        for f in ["src/a.rs", "src/b.rs", "src/c.rs"] {
            assert!(
                sep(hue_at(f), base) <= HUE_SPREAD_MAX_DEG + 1.0,
                "{f} strayed {} degrees from its directory",
                sep(hue_at(f), base)
            );
        }
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
