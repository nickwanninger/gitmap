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
            });
            nodes[parent].children.push(id);
        }

        let mut tree = Tree { nodes, root };
        tree.sort_children();
        tree.accumulate(root);
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

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
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
fn ensure_dir(
    nodes: &mut Vec<Node>,
    dirs: &mut HashMap<PathBuf, NodeId>,
    dir: &Path,
) -> NodeId {
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
        let names: Vec<_> = root.children.iter().map(|&c| t.node(c).name.clone()).collect();
        assert_eq!(names, vec!["src", "README.md"]);
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
