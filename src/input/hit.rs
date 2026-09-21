//! Cell-resolution hit testing.
//!
//! The treemap tree is never walked per mouse event. Instead an ID buffer is
//! rasterised alongside the layout, so a lookup is one index into a `Vec`.
//! Resolution is one entry per *cell*, not per pixel, because the terminal
//! reports mouse position in cells: a cell straddling two files resolves to
//! whichever owns its top half.

use crate::layout::tree::NodeId;
use crate::layout::treemap::{Layout, Rect};

pub struct HitBuffer {
    pub w: u16,
    pub h: u16,
    ids: Vec<Option<NodeId>>,
}

impl HitBuffer {
    /// Rasterise the leaf rectangles of `layout` at cell resolution.
    ///
    /// `is_leaf` decides what is hoverable — files, plus directories that were
    /// collapsed into an aggregate block.
    pub fn build(
        layout: &Layout,
        cells_w: u16,
        cells_h: u16,
        is_leaf: &dyn Fn(NodeId) -> bool,
    ) -> HitBuffer {
        let mut buf = HitBuffer {
            w: cells_w,
            h: cells_h,
            ids: vec![None; cells_w as usize * cells_h as usize],
        };
        for (id, rect) in layout.rects.iter().enumerate() {
            let Some(r) = rect else { continue };
            if !is_leaf(id) {
                continue;
            }
            buf.stamp(id, *r);
        }
        buf
    }

    /// Paint one rectangle's ID into the cells it covers.
    ///
    /// Pixel rows are halved to cell rows. A rectangle thinner than a full cell
    /// still claims the cell its top half falls in, so a 2-pixel file stays
    /// hoverable rather than vanishing from the hit buffer.
    fn stamp(&mut self, id: NodeId, r: Rect) {
        let x0 = r.x.round().max(0.0) as i64;
        let x1 = ((r.x + r.w).round() as i64).min(self.w as i64);
        let cy0 = (r.y / 2.0).floor().max(0.0) as i64;
        let cy1 = (((r.y + r.h) / 2.0).ceil() as i64).min(self.h as i64);

        for cy in cy0..cy1 {
            for cx in x0..x1 {
                if cx < 0 || cy < 0 || cx >= self.w as i64 || cy >= self.h as i64 {
                    continue;
                }
                let idx = cy as usize * self.w as usize + cx as usize;
                // Later rectangles are deeper in the tree; a child stamping
                // over its parent's cells is what we want.
                self.ids[idx] = Some(id);
            }
        }
    }

    /// O(1) lookup of the node under a cell.
    pub fn at(&self, x: u16, y: u16) -> Option<NodeId> {
        if x >= self.w || y >= self.h {
            return None;
        }
        self.ids[y as usize * self.w as usize + x as usize]
    }

    pub fn empty() -> HitBuffer {
        HitBuffer {
            w: 0,
            h: 0,
            ids: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::tree::{Scale, Tree};
    use crate::layout::{treemap};
    use std::path::PathBuf;

    #[test]
    fn lookup_finds_the_file_under_a_cell() {
        let entries: Vec<(PathBuf, u64)> = vec![
            (PathBuf::from("a.rs"), 100),
            (PathBuf::from("b.rs"), 100),
            (PathBuf::from("c.rs"), 100),
            (PathBuf::from("d.rs"), 100),
        ];
        let t = Tree::build(&entries, Scale::Linear);
        let l = treemap::layout(&t, Rect::new(0.0, 0.0, 40.0, 40.0));
        let hb = HitBuffer::build(&l, 40, 20, &|id| !t.node(id).is_dir);

        // Every file's own rectangle centre must resolve back to that file.
        for f in t.files_under(t.root) {
            let r = l.rects[f].unwrap();
            let cx = (r.x + r.w / 2.0) as u16;
            let cy = ((r.y + r.h / 2.0) / 2.0) as u16;
            assert_eq!(hb.at(cx, cy), Some(f), "{:?} at ({cx},{cy})", t.node(f).path);
        }
    }

    #[test]
    fn out_of_bounds_is_none() {
        let hb = HitBuffer {
            w: 4,
            h: 4,
            ids: vec![Some(1); 16],
        };
        assert_eq!(hb.at(0, 0), Some(1));
        assert_eq!(hb.at(4, 0), None);
        assert_eq!(hb.at(0, 4), None);
    }

    #[test]
    fn thin_rect_still_claims_a_cell() {
        // A file only two pixels tall spans less than one cell row, but must
        // stay hoverable or it becomes an unhittable target.
        let mut l = Layout {
            rects: vec![None, Some(Rect::new(0.0, 2.0, 4.0, 2.0))],
            collapsed: vec![false, false],
        };
        l.rects[0] = Some(Rect::new(0.0, 0.0, 10.0, 10.0));
        let hb = HitBuffer::build(&l, 10, 5, &|id| id == 1);
        assert_eq!(hb.at(1, 1), Some(1));
    }

    #[test]
    fn empty_buffer_never_panics() {
        let hb = HitBuffer::empty();
        assert_eq!(hb.at(0, 0), None);
        assert_eq!(hb.at(100, 100), None);
    }
}
