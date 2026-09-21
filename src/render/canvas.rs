//! Pixel framebuffer and half-block blit.
//!
//! The canvas is a plain RGB framebuffer that knows nothing about terminals;
//! only `blit` does. That keeps layout and colour logic testable without a PTY
//! and leaves room for a second backend (Kitty graphics) later.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as TermRect;
use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub const BLACK: Rgb = Rgb(0, 0, 0);

    pub fn to_color(self) -> Color {
        Color::Rgb(self.0, self.1, self.2)
    }
}

/// A framebuffer whose height is in *pixel* rows: two pixel rows per cell row.
pub struct Canvas {
    pub w: u16,
    pub h: u16,
    pub px: Vec<Rgb>,
}

impl Canvas {
    pub fn new(w: u16, h: u16) -> Canvas {
        Canvas {
            w,
            h,
            px: vec![Rgb::BLACK; w as usize * h as usize],
        }
    }

    pub fn clear(&mut self, c: Rgb) {
        self.px.fill(c);
    }

    pub fn set(&mut self, x: u16, y: u16, c: Rgb) {
        if x < self.w && y < self.h {
            self.px[y as usize * self.w as usize + x as usize] = c;
        }
    }

    pub fn get(&self, x: u16, y: u16) -> Rgb {
        if x < self.w && y < self.h {
            self.px[y as usize * self.w as usize + x as usize]
        } else {
            Rgb::BLACK
        }
    }

    /// Fill a rectangle given in fractional pixel coordinates.
    ///
    /// Rounding is done on the rectangle's edges rather than its size so that
    /// abutting rectangles share a boundary exactly and leave no seam.
    pub fn fill_rect(&mut self, r: crate::layout::treemap::Rect, c: Rgb) {
        let x0 = r.x.round().max(0.0) as i64;
        let y0 = r.y.round().max(0.0) as i64;
        let x1 = (r.x + r.w).round().min(self.w as f64) as i64;
        let y1 = (r.y + r.h).round().min(self.h as f64) as i64;
        for y in y0..y1 {
            for x in x0..x1 {
                self.set(x as u16, y as u16, c);
            }
        }
    }

    /// Pair pixel rows into `▀` cells: the top pixel becomes the foreground,
    /// the bottom the background. Two stacked half-cells are close to square,
    /// which corrects the terminal's roughly 1:2 cell aspect ratio.
    pub fn blit(&self, buf: &mut Buffer, area: TermRect) {
        let cols = area.width.min(self.w);
        let rows = area.height.min(self.h / 2);
        for cy in 0..rows {
            for cx in 0..cols {
                let top = self.get(cx, cy * 2);
                let bottom = self.get(cx, cy * 2 + 1);
                let (Some(x), Some(y)) = (area.x.checked_add(cx), area.y.checked_add(cy)) else {
                    continue;
                };
                if x >= area.right() || y >= area.bottom() {
                    continue;
                }
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_symbol("▀")
                        .set_fg(top.to_color())
                        .set_bg(bottom.to_color());
                }
            }
        }
    }

    /// Render as a text grid for snapshot tests, one character per pixel,
    /// mapping each distinct colour to a stable symbol.
    #[cfg(test)]
    pub fn to_ascii(&self, map: &dyn Fn(Rgb) -> char) -> String {
        let mut s = String::new();
        for y in 0..self.h {
            for x in 0..self.w {
                s.push(map(self.get(x, y)));
            }
            s.push('\n');
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::treemap::Rect;

    const R: Rgb = Rgb(255, 0, 0);
    const G: Rgb = Rgb(0, 255, 0);

    fn sym(c: Rgb) -> char {
        match c {
            R => 'r',
            G => 'g',
            _ => '.',
        }
    }

    #[test]
    fn fill_rect_covers_expected_pixels() {
        let mut c = Canvas::new(6, 4);
        c.fill_rect(Rect::new(1.0, 1.0, 3.0, 2.0), R);
        assert_eq!(
            c.to_ascii(&sym),
            "......\n\
             .rrr..\n\
             .rrr..\n\
             ......\n"
        );
    }

    #[test]
    fn abutting_rects_leave_no_seam() {
        // Fractional edges that meet must round to the same boundary, or the
        // map grows one-pixel gaps between neighbouring files.
        let mut c = Canvas::new(10, 2);
        c.fill_rect(Rect::new(0.0, 0.0, 3.4, 2.0), R);
        c.fill_rect(Rect::new(3.4, 0.0, 3.3, 2.0), G);
        for x in 0..6u16 {
            assert_ne!(c.get(x, 0), Rgb::BLACK, "seam at x={x}");
        }
    }

    #[test]
    fn blit_pairs_rows_into_half_blocks() {
        let mut c = Canvas::new(2, 2);
        c.set(0, 0, R);
        c.set(0, 1, G);
        let mut buf = Buffer::empty(TermRect::new(0, 0, 2, 1));
        c.blit(&mut buf, TermRect::new(0, 0, 2, 1));
        let cell = buf.cell((0u16, 0u16)).unwrap();
        assert_eq!(cell.symbol(), "▀");
        assert_eq!(cell.fg, R.to_color());
        assert_eq!(cell.bg, G.to_color());
    }

    #[test]
    fn blit_clips_to_area() {
        // A canvas larger than its area must not write outside it.
        let c = Canvas::new(40, 40);
        let mut buf = Buffer::empty(TermRect::new(0, 0, 10, 10));
        c.blit(&mut buf, TermRect::new(2, 2, 4, 3));
        // Cells outside the target area keep the empty buffer's default symbol.
        assert_eq!(buf.cell((0u16, 0u16)).unwrap().symbol(), " ");
        assert_eq!(buf.cell((2u16, 2u16)).unwrap().symbol(), "▀");
        assert_eq!(buf.cell((6u16, 2u16)).unwrap().symbol(), " ");
        assert_eq!(buf.cell((2u16, 5u16)).unwrap().symbol(), " ");
    }
}
