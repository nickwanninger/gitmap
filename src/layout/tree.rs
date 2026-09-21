//! Path list → nested tree.
//!
//! Nodes live in a flat arena addressed by index, which keeps the layout pass a
//! simple loop and makes the hit buffer's `FileId` a plain integer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub type NodeId = usize;

#[derive(Debug, Clone)]
pub struct Node {
    /// Path relative to the repo root. Empty for the root node.
    pub path: PathBuf,
    /// Final component, for labels.
    pub name: String,
    pub is_dir: bool,
    pub children: Vec<NodeId>,
    pub parent: Option<NodeId>,
    /// Raw metric before any scaling: bytes, or lines of code.
    pub size: u64,
    /// Scaled area weight, summed up the tree.
    pub weight: f64,
    pub depth: usize,
    /// Set when the node stands in for a subtree too small to lay out.
    pub collapsed_children: usize,
    /// Slice of the hue circle this node owns, in degrees, as `(start, width)`.
    ///
    /// The root owns the whole circle and each directory subdivides its own arc
    /// among its children in proportion to their weight, so a big directory
    /// gets a wide arc with room for internal variety while a small one stays
    /// tight and reads as a single colour. A file's hue is the centre of its
    /// arc, which means files in the same directory land near each other.
    pub hue: (f32, f32),
}

pub struct Tree {
    pub nodes: Vec<Node>,
    pub root: NodeId,
}

/// How a raw size becomes an area weight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scale {
    Linear,
    Sqrt,
    Log,
}

impl Scale {
    pub fn apply(&self, size: u64) -> f64 {
        let s = size.max(1) as f64;
        match self {
            // Square root compresses the dynamic range enough that a
            // 5,000-line file is ~30x a 5-line file rather than 1,000x.
            Scale::Sqrt => s.sqrt(),
            Scale::Log => (s + 1.0).ln(),
            Scale::Linear => s,
        }
    }
}

impl Tree {
    /// Build the directory tree from a flat list of `(path, size)` pairs.
    ///
    /// Children are ordered by path, not by size. That costs some squareness
    /// and buys an ordering that only changes when files are added or removed,
    /// so a hover target does not slide out from under the cursor.
    pub fn build(entries: &[(PathBuf, u64)], scale: Scale) -> Tree {
        let mut nodes = vec![Node {
            path: PathBuf::new(),
            name: String::new(),
            is_dir: true,
            children: Vec::new(),
            parent: None,
            size: 0,
            weight: 0.0,
            depth: 0,
            collapsed_children: 0,
            hue: (0.0, 360.0),
        }];
        let root = 0;
        let mut dirs: HashMap<PathBuf, NodeId> = HashMap::new();
        dirs.insert(PathBuf::new(), root);

        let mut sorted: Vec<&(PathBuf, u64)> = entries.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));

        for (path, size) in sorted {
            let parent = match path.parent() {
                Some(p) => ensure_dir(&mut nodes, &mut dirs, p),
                None => root,
            };
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let depth = nodes[parent].depth + 1;
            let id = nodes.len();
            nodes.push(Node {
                path: path.clone(),
                name,
                is_dir: false,
                children: Vec::new(),
                parent: Some(parent),
                size: *size,
                weight: scale.apply(*size),
                depth,
                collapsed_children: 0,
                hue: (0.0, 360.0),
            });
            nodes[parent].children.push(id);
        }

        let mut tree = Tree { nodes, root };
        tree.sort_children();
        tree.accumulate(root);
        // Weights must be summed before arcs can be sized by them.
        tree.assign_hues(root, 0.0, 360.0);
        tree
    }

    /// Order every directory's children by path, directories first so their
    /// labels cluster at the top-left of the enclosing rectangle.
    fn sort_children(&mut self) {
        for i in 0..self.nodes.len() {
            let mut kids = std::mem::take(&mut self.nodes[i].children);
            kids.sort_by(|&a, &b| {
                let (x, y) = (&self.nodes[a], &self.nodes[b]);
                y.is_dir.cmp(&x.is_dir).then_with(|| x.path.cmp(&y.path))
            });
            self.nodes[i].children = kids;
        }
    }

    /// Sum sizes and weights from the leaves up.
    fn accumulate(&mut self, id: NodeId) -> (u64, f64) {
        let kids = self.nodes[id].children.clone();
        if kids.is_empty() {
            let n = &self.nodes[id];
            return (n.size, n.weight);
        }
        let (mut size, mut weight) = (0u64, 0.0f64);
        for k in kids {
            let (s, w) = self.accumulate(k);
            size += s;
            weight += w;
        }
        self.nodes[id].size = size;
        self.nodes[id].weight = weight;
        (size, weight)
    }

    /// Hand each node a slice of the hue circle, proportional to weight.
    ///
    /// Only *directories* subdivide the arc. Every file in a directory inherits
    /// that directory's whole arc, so files in one folder share a hue and read
    /// as a family; the renderer spreads them within the arc with a small
    /// per-file jitter. Letting files take their own slices — the obvious first
    /// implementation — smears a large directory's contents right across its
    /// arc and destroys exactly the grouping this is for.
    ///
    /// Arc width follows weight, so a directory holding most of the repo owns
    /// most of the circle and has room for its subdirectories to differ, while
    /// a small one stays tight and reads as a single colour.
    fn assign_hues(&mut self, id: NodeId, start: f32, width: f32) {
        self.nodes[id].hue = (start, width);
        let kids = self.nodes[id].children.clone();
        if kids.is_empty() {
            return;
        }
        let dirs: Vec<NodeId> = kids
            .iter()
            .copied()
            .filter(|&k| self.nodes[k].is_dir)
            .collect();
        let loose: Vec<NodeId> = kids
            .iter()
            .copied()
            .filter(|&k| !self.nodes[k].is_dir)
            .collect();

        if dirs.is_empty() {
            // Nothing to subdivide: every file here shares the whole arc.
            for &k in &loose {
                self.nodes[k].hue = (start, width);
            }
            return;
        }

        // Subdirectories divide the arc between them, weighted by size. The
        // directories' combined share of the parent is what they get to split,
        // so a folder that is mostly loose files keeps most of its arc for
        // them and its few subdirectories stay nearby in hue.
        let dir_weight: f64 = dirs.iter().map(|&k| self.nodes[k].weight).sum();
        let total: f64 = kids.iter().map(|&k| self.nodes[k].weight).sum();
        if dir_weight <= 0.0 || total <= 0.0 {
            let each = width / dirs.len() as f32;
            for (i, &k) in dirs.iter().enumerate() {
                self.assign_hues(k, start + each * i as f32, each);
            }
            return;
        }
        // Loose files get a reserved slice of their own at the head of the arc
        // rather than inheriting all of it. Sharing the whole arc would put
        // them on the parent's centre hue, which is exactly where a middle
        // subdirectory lands — so `src/app.rs` would come out the same colour
        // as `src/layout/`.
        let loose_weight: f64 = loose.iter().map(|&k| self.nodes[k].weight).sum();
        let loose_share = if loose.is_empty() {
            0.0
        } else {
            // Floor the share so a handful of loose files among large
            // subdirectories still get a visible band of their own.
            ((loose_weight / total) as f32).clamp(0.18, 0.5)
        };
        let loose_span = width * loose_share;
        for &k in &loose {
            self.nodes[k].hue = (start, loose_span);
        }

        let mut at = start + loose_span;
        let dir_span = width - loose_span;
        for &k in &dirs {
            let share = (self.nodes[k].weight / dir_weight) as f32;
            let w = dir_span * share;
            self.assign_hues(k, at, w);
            at += w;
        }
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    /// The hue a node renders at: the centre of its arc, in degrees.
    pub fn hue_of(&self, id: NodeId) -> f32 {
        let (start, width) = self.nodes[id].hue;
        (start + width / 2.0).rem_euclid(360.0)
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Walk from a node to the root, nearest ancestor first.
    pub fn ancestors(&self, mut id: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        while let Some(p) = self.nodes[id].parent {
            out.push(p);
            id = p;
        }
        out
    }

    /// Every file at or beneath `id`.
    pub fn files_under(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack = vec![id];
        while let Some(n) = stack.pop() {
            if self.nodes[n].is_dir {
                stack.extend(self.nodes[n].children.iter().copied());
            } else {
                out.push(n);
            }
        }
        out
    }

    /// Find the node for a path, if it is in the tree.
    pub fn find(&self, path: &Path) -> Option<NodeId> {
        self.nodes.iter().position(|n| n.path == path)
    }
}

/// Get or create the chain of directory nodes for `dir`.
fn ensure_dir(nodes: &mut Vec<Node>, dirs: &mut HashMap<PathBuf, NodeId>, dir: &Path) -> NodeId {
    if let Some(&id) = dirs.get(dir) {
        return id;
    }
    let parent = match dir.parent() {
        Some(p) => ensure_dir(nodes, dirs, p),
        None => 0,
    };
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let depth = nodes[parent].depth + 1;
    let id = nodes.len();
    nodes.push(Node {
        path: dir.to_path_buf(),
        name,
        is_dir: true,
        children: Vec::new(),
        parent: Some(parent),
        size: 0,
        weight: 0.0,
        depth,
        collapsed_children: 0,
        hue: (0.0, 360.0),
    });
    nodes[parent].children.push(id);
    dirs.insert(dir.to_path_buf(), id);
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<(PathBuf, u64)> {
        vec![
            (PathBuf::from("src/main.rs"), 100),
            (PathBuf::from("src/git/mod.rs"), 400),
            (PathBuf::from("README.md"), 25),
        ]
    }

    #[test]
    fn builds_nested_structure() {
        let t = Tree::build(&entries(), Scale::Linear);
        let root = t.node(t.root);
        assert_eq!(root.size, 525);
        // src/ and README.md
        assert_eq!(root.children.len(), 2);
        let files = t.files_under(t.root);
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn directories_sum_their_children() {
        let t = Tree::build(&entries(), Scale::Linear);
        let src = t.find(Path::new("src")).unwrap();
        assert_eq!(t.node(src).size, 500);
        assert!(t.node(src).is_dir);
    }

    #[test]
    fn sqrt_scale_compresses_range() {
        let t = Tree::build(&entries(), Scale::Sqrt);
        let big = t.find(Path::new("src/git/mod.rs")).unwrap();
        let small = t.find(Path::new("README.md")).unwrap();
        let ratio = t.node(big).weight / t.node(small).weight;
        // 400/25 = 16x raw, 4x under sqrt.
        assert!((ratio - 4.0).abs() < 1e-9);
    }

    #[test]
    fn ordering_is_by_path_not_size() {
        let t = Tree::build(&entries(), Scale::Linear);
        let root = t.node(t.root);
        // Directories first, then files, each group in path order.
        let names: Vec<_> = root
            .children
            .iter()
            .map(|&c| t.node(c).name.clone())
            .collect();
        assert_eq!(names, vec!["src", "README.md"]);
    }

    #[test]
    fn sibling_directories_get_disjoint_arcs_inside_their_parent() {
        // Two directories must never share a hue, and both must stay inside
        // the arc their parent owns.
        let t = Tree::build(
            &[
                (PathBuf::from("src/git/a.rs"), 100),
                (PathBuf::from("src/render/b.rs"), 100),
                (PathBuf::from("src/layout/c.rs"), 100),
                (PathBuf::from("docs/x.md"), 50),
            ],
            Scale::Linear,
        );
        for id in 0..t.len() {
            let dirs: Vec<NodeId> = t
                .node(id)
                .children
                .iter()
                .copied()
                .filter(|&k| t.node(k).is_dir)
                .collect();
            let (ps, pw) = t.node(id).hue;
            let mut spans: Vec<(f32, f32)> = dirs.iter().map(|&k| t.node(k).hue).collect();
            spans.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
            for w in spans.windows(2) {
                assert!(
                    w[0].0 + w[0].1 <= w[1].0 + 1e-3,
                    "sibling directory arcs overlap: {:?} {:?}",
                    w[0],
                    w[1]
                );
            }
            for (s, width) in spans {
                assert!(
                    s >= ps - 1e-3 && s + width <= ps + pw + 1e-3,
                    "child arc ({s}, {width}) escapes parent ({ps}, {pw})"
                );
            }
        }
    }

    #[test]
    fn files_inherit_their_directory_arc() {
        // This is what makes "same folder, same colour" true: files do not
        // carve up the arc, they all sit on their directory's hue.
        let t = Tree::build(
            &[
                (PathBuf::from("src/a.rs"), 100),
                (PathBuf::from("src/b.rs"), 100),
                (PathBuf::from("src/c.rs"), 900),
            ],
            Scale::Linear,
        );
        let src = t.find(Path::new("src")).unwrap();
        for name in ["src/a.rs", "src/b.rs", "src/c.rs"] {
            let f = t.find(Path::new(name)).unwrap();
            assert_eq!(
                t.node(f).hue,
                t.node(src).hue,
                "{name} did not inherit src's arc"
            );
            assert!((t.hue_of(f) - t.hue_of(src)).abs() < 1e-3);
        }
    }

    #[test]
    fn arc_width_tracks_weight() {
        // A big directory owns more of the circle than a small one, so it has
        // room for its own subdirectories to differ while a small one stays
        // tight enough to read as a single colour.
        let t = Tree::build(
            &[
                (PathBuf::from("big/a.rs"), 900),
                (PathBuf::from("big/b.rs"), 900),
                (PathBuf::from("small/c.rs"), 100),
            ],
            Scale::Linear,
        );
        let big = t.node(t.find(Path::new("big")).unwrap()).hue.1;
        let small = t.node(t.find(Path::new("small")).unwrap()).hue.1;
        assert!(big > small * 10.0, "big {big} vs small {small}");
        // Together the two directories still fit inside the circle.
        assert!(big + small <= 360.0 + 1e-3);
    }

    #[test]
    fn files_in_one_directory_share_a_hue_neighbourhood() {
        // The whole point: same folder means same colour family.
        let t = Tree::build(
            &[
                (PathBuf::from("src/a.rs"), 100),
                (PathBuf::from("src/b.rs"), 100),
                (PathBuf::from("src/c.rs"), 100),
                (PathBuf::from("docs/x.md"), 100),
            ],
            Scale::Linear,
        );
        let hue = |p: &str| t.hue_of(t.find(Path::new(p)).unwrap());
        let (a, b, c) = (hue("src/a.rs"), hue("src/b.rs"), hue("src/c.rs"));
        let far = hue("docs/x.md");
        // All three sit on exactly src's hue before the renderer's jitter.
        assert_eq!(a, b);
        assert_eq!(b, c);
        // And docs is a different colour entirely.
        assert!(
            (far - a).abs() > 30.0,
            "docs at {far} is too close to src at {a}"
        );
    }

    #[test]
    fn loose_files_do_not_collide_with_a_subdirectory() {
        // A directory holding both loose files and subdirectories must give
        // the loose files hue of their own. Letting them inherit the parent's
        // whole arc puts them on its centre — which is exactly where a middle
        // subdirectory lands, so `src/app.rs` came out the same colour as
        // `src/layout/`.
        let t = Tree::build(
            &[
                (PathBuf::from("src/app.rs"), 100),
                (PathBuf::from("src/main.rs"), 100),
                (PathBuf::from("src/git/a.rs"), 100),
                (PathBuf::from("src/layout/b.rs"), 100),
                (PathBuf::from("src/render/c.rs"), 100),
            ],
            Scale::Linear,
        );
        let loose = t.hue_of(t.find(Path::new("src/app.rs")).unwrap());
        for d in ["src/git", "src/layout", "src/render"] {
            let h = t.hue_of(t.find(Path::new(d)).unwrap());
            assert!(
                (loose - h).abs() > 5.0,
                "loose files at {loose} collide with {d} at {h}"
            );
        }
    }

    #[test]
    fn hue_is_stable_for_unrelated_edits() {
        // A file changing size must not repaint the whole tree, or the map's
        // colours would churn on every save.
        let build = |n: u64| {
            Tree::build(
                &[
                    (PathBuf::from("src/a.rs"), 100),
                    (PathBuf::from("src/b.rs"), n),
                    (PathBuf::from("docs/x.md"), 100),
                ],
                Scale::Linear,
            )
        };
        let (t1, t2) = (build(100), build(104));
        let h = |t: &Tree, p: &str| t.hue_of(t.find(Path::new(p)).unwrap());
        // Sizes feed arc widths, so a small edit shifts hues a little; it must
        // stay small rather than reshuffling the palette.
        assert!((h(&t1, "src/a.rs") - h(&t2, "src/a.rs")).abs() < 10.0);
        assert!((h(&t1, "docs/x.md") - h(&t2, "docs/x.md")).abs() < 10.0);
    }

    #[test]
    fn empty_tree_does_not_panic_assigning_hues() {
        let t = Tree::build(&[], Scale::Linear);
        assert_eq!(t.node(t.root).hue, (0.0, 360.0));
    }

    #[test]
    fn ancestors_chain_to_root() {
        let t = Tree::build(&entries(), Scale::Linear);
        let f = t.find(Path::new("src/git/mod.rs")).unwrap();
        let a = t.ancestors(f);
        assert_eq!(t.node(a[0]).path, PathBuf::from("src/git"));
        assert_eq!(t.node(a[1]).path, PathBuf::from("src"));
        assert_eq!(a[2], t.root);
    }
}
