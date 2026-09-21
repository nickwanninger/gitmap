//! Braille commit sparkline.
//!
//! Braille packs 2x4 subpixels into one cell at a single foreground colour, so
//! the strip carries density two ways at once: dot height within a cell, and
//! the cell's own colour on GitHub's contribution greens. Font coverage for
//! U+28xx is patchy on some systems, so `ascii` is kept as a fallback and
//! nothing load-bearing is drawn this way — the strip is orientation, not data
//! you act on.

/// Dot bit for a braille cell, indexed `[column][row]` with column 0 on the
/// left and row 0 at the top.
///
/// The layout is historical rather than sequential: dots 1-6 came first, and
/// 7-8 were appended underneath when the 8-dot form was standardised, so the
/// bottom row is bits 6 and 7 rather than continuing the column order.
const DOTS: [[u8; 4]; 2] = [
    [0x01, 0x02, 0x04, 0x40], // left column, top to bottom
    [0x08, 0x10, 0x20, 0x80], // right column, top to bottom
];

/// One rendered cell of the sparkline: its glyph and how busy it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    /// Activity level in `0..=4`, indexing the contribution-green ramp.
    pub level: u8,
}

/// Render counts as a braille sparkline `width` cells wide.
///
/// Each cell holds two columns of four rows, so the strip resolves
/// `2 * width` buckets vertically quantised to four levels. The returned
/// `level` is the busier of the cell's two columns, which is what its colour
/// should track — a cell whose taller half is busy reads as busy.
pub fn cells(counts: &[usize], width: u16) -> Vec<Cell> {
    if width == 0 || counts.is_empty() {
        return Vec::new();
    }
    let cols = width as usize * 2;
    let buckets = bucket(counts, cols);
    let peak = buckets.iter().copied().max().unwrap_or(0).max(1);

    let mut out = Vec::with_capacity(width as usize);
    for cell in 0..width as usize {
        let mut bits = 0u8;
        let mut level = 0u8;
        for (c, dots) in DOTS.iter().enumerate() {
            let Some(&v) = buckets.get(cell * 2 + c) else {
                continue;
            };
            // Height in dots, 0..=4. Any non-zero count shows at least one dot,
            // so a quiet day is still visibly different from no day at all.
            let h = if v == 0 {
                0
            } else {
                ((v * 4).div_ceil(peak)).clamp(1, 4)
            };
            level = level.max(h as u8);
            // Fill from the bottom up.
            for r in 0..h {
                bits |= dots[3 - r];
            }
        }
        out.push(Cell {
            ch: char::from_u32(0x2800 + bits as u32).unwrap_or(' '),
            level,
        });
    }
    out
}

/// The sparkline as a plain string, without per-cell colour.
pub fn braille(counts: &[usize], width: u16) -> String {
    cells(counts, width).into_iter().map(|c| c.ch).collect()
}

/// ASCII fallback for terminals whose font lacks braille.
pub fn ascii(counts: &[usize], width: u16) -> String {
    if width == 0 || counts.is_empty() {
        return String::new();
    }
    let buckets = bucket(counts, width as usize);
    let peak = buckets.iter().copied().max().unwrap_or(0).max(1);
    buckets
        .iter()
        .map(|&v| {
            if v == 0 {
                ' '
            } else {
                let h = (v * 4).div_ceil(peak).clamp(1, 4);
                [' ', '.', ':', '|', '#'][h as usize]
            }
        })
        .collect()
}

/// Resample `counts` into exactly `n` buckets, summing when compressing and
/// repeating when stretching.
///
/// Summing rather than sampling matters: a busy day must not disappear because
/// it happened to fall between two sample points.
fn bucket(counts: &[usize], n: usize) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    let mut out = vec![0usize; n];
    if counts.len() >= n {
        for (i, &v) in counts.iter().enumerate() {
            let slot = i * n / counts.len();
            out[slot.min(n - 1)] += v;
        }
    } else {
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = counts[i * counts.len() / n];
        }
    }
    out
}

/// Bucket commit timestamps into commits-per-day, oldest first.
///
/// Returns the per-day counts and the day index of each commit, so the caller
/// can mark where the selection sits without re-deriving the bucketing.
///
/// The range comes from the actual minimum and maximum rather than from the
/// ends of the slice. `git log` walks the commit graph in topological order,
/// not date order, so on any repository with merges or rebases the timestamps
/// are not monotonic — llama.cpp's most recent 200 commits swing back and forth
/// across four months. Trusting the ends there understated the span by a factor
/// of five and clamped a tenth of the commits onto the first day, which both
/// distorted the strip and made the selection marker jump around.
pub fn commits_per_day(times: &[i64]) -> (Vec<usize>, Vec<usize>) {
    if times.is_empty() {
        return (Vec::new(), Vec::new());
    }
    const DAY: i64 = 86_400;
    let oldest = *times.iter().min().unwrap();
    let newest = *times.iter().max().unwrap();
    let days = (((newest - oldest) / DAY) + 1).clamp(1, 4096) as usize;

    let mut counts = vec![0usize; days];
    let mut index = Vec::with_capacity(times.len());
    for &t in times {
        let d = (((t - oldest) / DAY).max(0) as usize).min(days - 1);
        counts[d] += 1;
        index.push(d);
    }
    (counts, index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_is_empty() {
        assert_eq!(braille(&[], 10), "");
        assert_eq!(braille(&[1, 2, 3], 0), "");
        assert_eq!(ascii(&[], 10), "");
    }

    #[test]
    fn width_is_respected() {
        for w in [1u16, 5, 20, 80] {
            assert_eq!(braille(&[1, 5, 2, 8, 3], w).chars().count(), w as usize);
            assert_eq!(ascii(&[1, 5, 2, 8, 3], w).chars().count(), w as usize);
        }
    }

    #[test]
    fn output_is_braille_range() {
        for ch in braille(&[0, 1, 4, 9, 2], 8).chars() {
            let c = ch as u32;
            assert!((0x2800..=0x28ff).contains(&c), "{ch:?} is not braille");
        }
    }

    #[test]
    fn zero_is_blank_and_nonzero_is_not() {
        // A day with no commits must look different from a quiet day, or the
        // strip lies about the shape of the history.
        let s = braille(&[0, 0], 1);
        assert_eq!(s, "\u{2800}", "all-zero should be the blank braille cell");
        let s = braille(&[0, 1], 1);
        assert_ne!(s, "\u{2800}", "a single commit should still show a dot");
    }

    #[test]
    fn taller_buckets_get_more_dots_within_one_strip() {
        // Heights normalise to each strip's own peak, so the comparison that
        // means anything is between buckets of the same strip, not across two
        // separate calls.
        let s = braille(&[1, 8], 1);
        let bits = s.chars().next().unwrap() as u32 - 0x2800;
        let left: u32 = DOTS[0]
            .iter()
            .map(|&d| (bits & d as u32).count_ones())
            .sum();
        let right: u32 = DOTS[1]
            .iter()
            .map(|&d| (bits & d as u32).count_ones())
            .sum();
        assert!(
            right > left,
            "the busier right-hand bucket should be taller: {left} vs {right}"
        );
    }

    #[test]
    fn heights_normalise_to_the_strips_own_peak() {
        // A flat run fills the strip rather than rendering as one dot each;
        // the sparkline shows shape, not absolute magnitude.
        let flat = braille(&[5, 5], 1).chars().next().unwrap() as u32 - 0x2800;
        assert_eq!(flat.count_ones(), 8, "a flat strip should be full height");
    }

    #[test]
    fn cell_level_tracks_activity() {
        // Colour is driven by `level`, so it has to rise with the count or the
        // strip would be green in the wrong places.
        let c = cells(&[0, 0, 1, 1, 9, 9], 3);
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].level, 0, "an empty stretch is level 0");
        assert!(c[2].level > c[1].level, "busier cells rank higher");
        assert!(c[2].level <= 4, "level must index the 5-stop ramp");
    }

    #[test]
    fn cell_level_is_the_busier_of_the_two_columns() {
        // A cell whose taller half is busy should read as busy, not be
        // averaged down by its quiet half.
        let c = cells(&[0, 8], 1);
        assert_eq!(c[0].level, 4);
    }

    #[test]
    fn braille_matches_the_cell_glyphs() {
        let counts = [0, 3, 1, 7, 2];
        let joined: String = cells(&counts, 4).into_iter().map(|c| c.ch).collect();
        assert_eq!(braille(&counts, 4), joined);
    }

    #[test]
    fn bucket_sums_when_compressing() {
        // Compressing must not drop data: ten days into two buckets is two
        // sums, not two samples.
        let b = bucket(&[1, 1, 1, 1, 1, 2, 2, 2, 2, 2], 2);
        assert_eq!(b, vec![5, 10]);
    }

    #[test]
    fn bucket_stretches_when_expanding() {
        let b = bucket(&[3, 7], 4);
        assert_eq!(b.len(), 4);
        assert_eq!(b[0], 3);
        assert_eq!(b[3], 7);
    }

    #[test]
    fn commits_per_day_counts_and_indexes() {
        const DAY: i64 = 86_400;
        let base = 1_700_000_000i64;
        // Newest first: two commits today, one two days ago.
        let times = vec![base + 2 * DAY, base + 2 * DAY, base];
        let (counts, index) = commits_per_day(&times);
        assert_eq!(counts.len(), 3, "three days spanned");
        assert_eq!(counts[2], 2, "two commits on the newest day");
        assert_eq!(counts[0], 1, "one on the oldest");
        assert_eq!(counts[1], 0, "the quiet middle day is still a bucket");
        assert_eq!(index, vec![2, 2, 0]);
    }

    #[test]
    fn commits_per_day_handles_a_single_commit() {
        let (counts, index) = commits_per_day(&[1_700_000_000]);
        assert_eq!(counts, vec![1]);
        assert_eq!(index, vec![0]);
    }

    #[test]
    fn commits_per_day_handles_non_monotonic_times() {
        // Regression: `git log` walks topologically, not by date, so a repo
        // with merges or rebases yields timestamps that swing back and forth.
        // Taking the range from the ends of the slice understated the span and
        // clamped the out-of-range commits onto day 0.
        const DAY: i64 = 86_400;
        let base = 1_700_000_000i64;
        // Ends span 2 days, but one commit in the middle is 100 days older.
        let times = vec![base + 2 * DAY, base - 100 * DAY, base];
        let (counts, index) = commits_per_day(&times);

        assert_eq!(counts.len(), 103, "span must cover the true minimum");
        assert_eq!(counts.iter().sum::<usize>(), 3, "no commit may be lost");
        // Each commit lands in its own day, none piled onto day 0.
        assert_eq!(index[1], 0, "the true oldest anchors day 0");
        assert_eq!(index[2], 100);
        assert_eq!(index[0], 102);
    }

    #[test]
    fn commits_per_day_places_every_commit_in_its_own_bucket() {
        // Property: whatever the ordering, a commit's day index must reflect
        // its own timestamp relative to the real minimum.
        const DAY: i64 = 86_400;
        let base = 1_700_000_000i64;
        let times: Vec<i64> = [5i64, 0, 90, 12, 3, 60, 1]
            .iter()
            .map(|d| base + d * DAY)
            .collect();
        let (counts, index) = commits_per_day(&times);
        let min = *times.iter().min().unwrap();
        for (i, &t) in times.iter().enumerate() {
            assert_eq!(
                index[i],
                ((t - min) / DAY) as usize,
                "commit {i} landed in the wrong bucket"
            );
        }
        assert_eq!(counts.iter().sum::<usize>(), times.len());
    }

    #[test]
    fn commits_per_day_clamps_an_absurd_span() {
        // A repo with a commit dated 1970 and one dated now must not allocate
        // twenty thousand buckets.
        let (counts, _) = commits_per_day(&[1_900_000_000, 0]);
        assert!(counts.len() <= 4096);
    }

    #[test]
    fn ascii_fallback_has_no_braille() {
        for ch in ascii(&[0, 3, 9], 6).chars() {
            assert!(!(0x2800..=0x28ff).contains(&(ch as u32)));
        }
    }
}
