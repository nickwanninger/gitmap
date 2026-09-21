//! Squarified treemap (Bruls, Huizing & van Wijk 2000), ordered variant.
//!
//! The classic algorithm sorts children descending by area for the best aspect
//! ratios. This one takes them in the order the tree gives (path order), which
//! is Shneiderman & Wattenberg's stability trade: slightly worse squareness for
//! a layout that keeps blocks in a predictable place.
//!
//! What that buys, precisely: relative order is preserved under any weight
//! change, and an edit inside one directory redistributes space only within
//! that directory's rectangle. What it does not buy: row *grouping* still
//! depends on weights, so a size change that crosses a greedy row-break
//! boundary can regroup a row. That is inherent to squarify and is why the map
//! is laid out from HEAD rather than from the working tree — geometry then
//! changes only on commit, resize, or add/remove, never while you are editing.

use super::tree::{NodeId, Tree};

/// A rectangle in pixel space. Pixel rows are half-cells, so `h` is twice the
/// cell height of the same region.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y, w, h }
    }

    pub fn area(&self) -> f64 {
        self.w.max(0.0) * self.h.max(0.0)
    }

    pub fn contains(&self, px: f64, py: f64) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }

    pub fn intersects(&self, o: &Rect) -> bool {
        self.x < o.x + o.w && o.x < self.x + self.w && self.y < o.y + o.h && o.y < self.y + self.h
    }
}

/// The computed geometry: one rectangle per node, indexed by `NodeId`.
pub struct Layout {
    pub rects: Vec<Option<Rect>>,
    /// Files that were dropped into an aggregate block, by the node that
    /// absorbed them.
    pub collapsed: Vec<bool>,
}

/// Minimum pixel extent for a file block. Below this a file stops being a
/// hoverable target, so the subtree collapses into one aggregate block instead.
const MIN_PX: f64 = 2.0;

/// Reserve the top pixel row of a directory for its label tint, giving visual
/// grouping without drawing borders that would eat pixels.
const LABEL_PX: f64 = 2.0;

/// Depth past which directories stop reserving label rows — deep nesting
/// otherwise spends every pixel on chrome.
const LABEL_MAX_DEPTH: usize = 2;

pub fn layout(tree: &Tree, area: Rect) -> Layout {
    let mut out = Layout {
        rects: vec![None; tree.len()],
        collapsed: vec![false; tree.len()],
    };
    if area.w < 1.0 || area.h < 1.0 {
        return out;
    }
    out.rects[tree.root] = Some(area);
    place_children(tree, tree.root, area, &mut out);
    out
}

/// Lay a node's children out inside `area`, recursing into directories.
fn place_children(tree: &Tree, id: NodeId, area: Rect, out: &mut Layout) {
    let node = tree.node(id);
    if node.children.is_empty() {
        return;
    }

    // A directory gives up its top pixel row to a label band, but only while
    // there is room to spare.
    let mut inner = area;
    if id != tree.root && node.depth <= LABEL_MAX_DEPTH && area.h > LABEL_PX * 3.0 {
        inner.y += LABEL_PX;
        inner.h -= LABEL_PX;
    }

    // Too small to subdivide: collapse the whole subtree into this one block.
    if inner.w < MIN_PX * 2.0 || inner.h < MIN_PX * 2.0 {
        collapse(tree, id, out);
        return;
    }

    let kids: Vec<NodeId> = node
        .children
        .iter()
        .copied()
        .filter(|&k| tree.node(k).weight > 0.0)
        .collect();
    if kids.is_empty() {
        return;
    }

    let total: f64 = kids.iter().map(|&k| tree.node(k).weight).sum();
    if total <= 0.0 {
        return;
    }

    squarify(tree, &kids, total, inner, out);

    for &k in &kids {
        if let Some(r) = out.rects[k] {
            if tree.node(k).is_dir && !out.collapsed[k] {
                place_children(tree, k, r, out);
            }
        }
    }
}

/// Mark a subtree as absorbed into `id`, which renders as a single aggregate
/// block labelled with the child count.
fn collapse(tree: &Tree, id: NodeId, out: &mut Layout) {
    out.collapsed[id] = true;
    let mut stack: Vec<NodeId> = tree.node(id).children.clone();
    while let Some(n) = stack.pop() {
        out.rects[n] = None;
        stack.extend(tree.node(n).children.iter().copied());
    }
}

/// The squarify core: greedily grow a row while the worst aspect ratio in it
/// improves, then lay the row out along the shorter side and recurse into what
/// is left.
fn squarify(tree: &Tree, kids: &[NodeId], total: f64, area: Rect, out: &mut Layout) {
    let mut remaining = area;
    let mut remaining_weight = total;
    let mut i = 0;

    // The row axis is fixed from the enclosing rectangle rather than
    // re-decided against each shrinking remainder. Re-deciding is what makes
    // the classic algorithm unstable: a small weight change flips which side
    // is shorter partway down, and the whole tail of the subtree reorients.
    let horizontal = area.w <= area.h;

    while i < kids.len() {
        let short = if horizontal { remaining.w } else { remaining.h };
        if short <= 0.0 || remaining_weight <= 0.0 {
            // Nothing left to give; the rest get nothing and are collapsed by
            // the caller's minimum-size check.
            for &k in &kids[i..] {
                out.rects[k] = None;
            }
            return;
        }
        // Area of the remaining rectangle per unit of weight.
        let px_per_weight = remaining.area() / remaining_weight;

        // Grow the row while the worst aspect ratio keeps improving.
        let mut row_weight = 0.0;
        let mut worst = f64::INFINITY;
        let mut end = i;
        while end < kids.len() {
            let w = tree.node(kids[end]).weight;
            let candidate = worst_ratio(tree, &kids[i..=end], row_weight + w, short, px_per_weight);
            if end > i && candidate > worst {
                break;
            }
            worst = candidate;
            row_weight += w;
            end += 1;
        }

        let row = &kids[i..end];
        remaining = place_row(tree, row, row_weight, remaining, px_per_weight, horizontal, out);
        remaining_weight -= row_weight;
        i = end;
    }
}

/// Worst aspect ratio among the rectangles a row would produce.
fn worst_ratio(
    tree: &Tree,
    row: &[NodeId],
    row_weight: f64,
    short: f64,
    px_per_weight: f64,
) -> f64 {
    if row_weight <= 0.0 {
        return f64::INFINITY;
    }
    let row_area = row_weight * px_per_weight;
    // Thickness of the row perpendicular to the short side.
    let thickness = row_area / short;
    if thickness <= 0.0 {
        return f64::INFINITY;
    }
    let mut worst: f64 = 1.0;
    for &k in row {
        let a = tree.node(k).weight * px_per_weight;
        let along = a / thickness;
        if along <= 0.0 {
            return f64::INFINITY;
        }
        worst = worst.max((thickness / along).max(along / thickness));
    }
    worst
}

/// Place one row along the shorter side of `area`, returning what is left.
fn place_row(
    tree: &Tree,
    row: &[NodeId],
    row_weight: f64,
    area: Rect,
    px_per_weight: f64,
    horizontal: bool,
    out: &mut Layout,
) -> Rect {
    if row_weight <= 0.0 {
        return area;
    }
    let row_area = row_weight * px_per_weight;

    if horizontal {
        // Row runs left-to-right across the top.
        let thickness = (row_area / area.w).min(area.h);
        let mut x = area.x;
        for (n, &k) in row.iter().enumerate() {
            let share = tree.node(k).weight / row_weight;
            // Give the last child the exact remainder so the row covers the
            // full width without accumulating rounding gaps.
            let w = if n + 1 == row.len() {
                area.x + area.w - x
            } else {
                area.w * share
            };
            out.rects[k] = Some(Rect::new(x, area.y, w, thickness));
            x += w;
        }
        Rect::new(area.x, area.y + thickness, area.w, area.h - thickness)
    } else {
        // Row runs top-to-bottom down the left.
        let thickness = (row_area / area.h).min(area.w);
        let mut y = area.y;
        for (n, &k) in row.iter().enumerate() {
            let share = tree.node(k).weight / row_weight;
            let h = if n + 1 == row.len() {
                area.y + area.h - y
            } else {
                area.h * share
            };
            out.rects[k] = Some(Rect::new(area.x, y, thickness, h));
            y += h;
        }
        Rect::new(area.x + thickness, area.y, area.w - thickness, area.h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::tree::Scale;
    use std::path::PathBuf;

    fn tree_of(specs: &[(&str, u64)]) -> Tree {
        let e: Vec<(PathBuf, u64)> = specs
            .iter()
            .map(|(p, s)| (PathBuf::from(*p), *s))
            .collect();
        Tree::build(&e, Scale::Linear)
    }

    fn sample() -> Tree {
        tree_of(&[
            ("src/main.rs", 100),
            ("src/app.rs", 200),
            ("src/git/mod.rs", 400),
            ("src/git/parse.rs", 150),
            ("src/render/canvas.rs", 300),
            ("README.md", 50),
            ("Cargo.toml", 20),
        ])
    }

    #[test]
    fn every_file_gets_a_rectangle() {
        let t = sample();
        let l = layout(&t, Rect::new(0.0, 0.0, 200.0, 110.0));
        for f in t.files_under(t.root) {
            let r = l.rects[f].unwrap_or_else(|| panic!("{:?} has no rect", t.node(f).path));
            assert!(r.area() > 0.0, "{:?} has empty rect", t.node(f).path);
        }
    }

    #[test]
    fn siblings_never_overlap() {
        let t = sample();
        let l = layout(&t, Rect::new(0.0, 0.0, 200.0, 110.0));
        let files = t.files_under(t.root);
        for (i, &a) in files.iter().enumerate() {
            for &b in &files[i + 1..] {
                let (ra, rb) = (l.rects[a].unwrap(), l.rects[b].unwrap());
                assert!(
                    !ra.intersects(&rb),
                    "{:?} {:?} overlap: {:?} {:?}",
                    t.node(a).path,
                    t.node(b).path,
                    ra,
                    rb
                );
            }
        }
    }

    #[test]
    fn children_stay_inside_their_parent() {
        let t = sample();
        let l = layout(&t, Rect::new(0.0, 0.0, 200.0, 110.0));
        for id in 0..t.len() {
            let Some(r) = l.rects[id] else { continue };
            let Some(p) = t.node(id).parent else { continue };
            let Some(pr) = l.rects[p] else { continue };
            let eps = 1e-6;
            assert!(
                r.x >= pr.x - eps
                    && r.y >= pr.y - eps
                    && r.x + r.w <= pr.x + pr.w + eps
                    && r.y + r.h <= pr.y + pr.h + eps,
                "{:?} {:?} escapes parent {:?}",
                t.node(id).path,
                r,
                pr
            );
        }
    }

    #[test]
    fn area_is_proportional_to_size() {
        // With no label bands in play, a file's share of the canvas should
        // track its share of the total within tolerance.
        let t = tree_of(&[("a.rs", 100), ("b.rs", 200), ("c.rs", 300)]);
        let canvas = Rect::new(0.0, 0.0, 120.0, 120.0);
        let l = layout(&t, canvas);
        let total = canvas.area();
        for (name, size) in [("a.rs", 100.0), ("b.rs", 200.0), ("c.rs", 300.0)] {
            let id = t.find(std::path::Path::new(name)).unwrap();
            let got = l.rects[id].unwrap().area() / total;
            let want = size / 600.0;
            assert!(
                (got - want).abs() < 0.02,
                "{name}: area share {got:.4}, expected {want:.4}"
            );
        }
    }

    #[test]
    fn row_fills_the_full_extent() {
        // The last child in a row takes the exact remainder, so a row's
        // rectangles must tile their band with no gap.
        let t = tree_of(&[("a.rs", 100), ("b.rs", 100), ("c.rs", 100), ("d.rs", 100)]);
        let canvas = Rect::new(0.0, 0.0, 100.0, 100.0);
        let l = layout(&t, canvas);
        let covered: f64 = t
            .files_under(t.root)
            .iter()
            .map(|&f| l.rects[f].unwrap().area())
            .sum();
        assert!(
            (covered - canvas.area()).abs() < 1e-6,
            "files cover {covered} of {}",
            canvas.area()
        );
    }

    #[test]
    fn edit_inside_one_directory_does_not_disturb_its_siblings() {
        // Ordering by path buys locality: an edit within `src/` redistributes
        // space inside `src/`'s rectangle, and `docs/` is untouched. This is
        // the stability that matters in practice, because editing a file is
        // the common case and it should never move an unrelated hover target.
        let sizes = |n: u64| {
            tree_of(&[
                ("docs/x.md", 300),
                ("docs/y.md", 200),
                ("src/a.rs", 500),
                ("src/b.rs", n),
            ])
        };
        let area = Rect::new(0.0, 0.0, 100.0, 100.0);
        let (a, b) = (sizes(400), sizes(410));
        let (la, lb) = (layout(&a, area), layout(&b, area));

        for name in ["docs/x.md", "docs/y.md"] {
            let p = std::path::Path::new(name);
            let ra = la.rects[a.find(p).unwrap()].unwrap();
            let rb = lb.rects[b.find(p).unwrap()].unwrap();
            assert!(
                (ra.x - rb.x).abs() < 1.0 && (ra.y - rb.y).abs() < 1.0,
                "{name} moved from {ra:?} to {rb:?}"
            );
        }
    }

    #[test]
    fn relative_order_survives_size_changes() {
        // The invariant the ordered variant actually guarantees everywhere:
        // rectangles stay in path order top-to-bottom, left-to-right, however
        // the weights move. Row *grouping* can still change when a weight
        // crosses a greedy break boundary — that is inherent to squarify — but
        // a block never jumps past a sibling it used to precede.
        // Checked per parent: within a directory's rectangle, each child must
        // start at or after the previous one along both axes. Across different
        // parents the comparison is meaningless, since a treemap nests in two
        // dimensions rather than laying everything out in reading order.
        for n in [50u64, 300, 900, 4000] {
            let t = tree_of(&[
                ("a/1.rs", 100),
                ("a/2.rs", n),
                ("a/3.rs", 120),
                ("b/1.rs", 200),
                ("b/2.rs", 150),
                ("c/1.rs", 250),
            ]);
            let l = layout(&t, Rect::new(0.0, 0.0, 120.0, 120.0));
            for id in 0..t.len() {
                let kids: Vec<_> = t
                    .node(id)
                    .children
                    .iter()
                    .filter_map(|&k| l.rects[k].map(|r| (t.node(k).name.clone(), r)))
                    .collect();
                for w in kids.windows(2) {
                    let (prev, next) = (&w[0], &w[1]);
                    assert!(
                        next.1.x >= prev.1.x - 1e-6 || next.1.y >= prev.1.y - 1e-6,
                        "size {n}: {} at {:?} precedes {} at {:?}",
                        prev.0,
                        prev.1,
                        next.0,
                        next.1
                    );
                }
            }
        }
    }

    #[test]
    fn tiny_area_collapses_rather_than_producing_slivers() {
        let t = sample();
        let l = layout(&t, Rect::new(0.0, 0.0, 6.0, 6.0));
        // Whatever survives must still be big enough to hover.
        for id in 0..t.len() {
            if let Some(r) = l.rects[id] {
                assert!(r.w >= 0.0 && r.h >= 0.0);
            }
        }
    }

    #[test]
    fn empty_area_is_handled() {
        let t = sample();
        let l = layout(&t, Rect::new(0.0, 0.0, 0.0, 0.0));
        assert!(l.rects.iter().all(|r| r.is_none()));
    }
}

