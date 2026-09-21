//! Colour ramps, OKLab mixing, and terminal capability detection.
//!
//! Ramps live here as data so themes stay out of the rendering code. The
//! heatmap encodes a continuous variable, so its ramp is perceptually uniform
//! (magma) rather than an HSV sweep, which bands badly at yellow and cyan.

use super::canvas::Rgb;
use crate::git::Change;

/// What the terminal can actually display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorDepth {
    True,
    Ansi256,
    Ansi16,
}

impl ColorDepth {
    /// Detect from `COLORTERM` and `TERM`.
    pub fn detect() -> ColorDepth {
        if let Ok(ct) = std::env::var("COLORTERM")
            && (ct.contains("truecolor") || ct.contains("24bit"))
        {
            return ColorDepth::True;
        }
        let term = std::env::var("TERM").unwrap_or_default();
        if term.contains("256color") {
            ColorDepth::Ansi256
        } else if term.is_empty() || term == "dumb" {
            ColorDepth::Ansi16
        } else {
            // Most modern terminals support truecolor without advertising it.
            ColorDepth::Ansi256
        }
    }

    /// Quantise a colour to what the terminal can show.
    pub fn quantize(&self, c: Rgb) -> Rgb {
        match self {
            ColorDepth::True => c,
            // Snap to the 6x6x6 cube the 256-colour palette uses, so ramps
            // step predictably instead of dithering between neighbours.
            ColorDepth::Ansi256 => {
                let q = |v: u8| -> u8 {
                    let i = (v as f32 / 255.0 * 5.0).round() as u8;
                    [0u8, 95, 135, 175, 215, 255][i.min(5) as usize]
                };
                Rgb(q(c.0), q(c.1), q(c.2))
            }
            // Last resort: the map degrades to a handful of states, so collapse
            // to the nearest of the eight primaries at two brightnesses.
            ColorDepth::Ansi16 => {
                let t = |v: u8| if v > 128 { 255u8 } else { 0u8 };
                let bright = c.0 as u16 + c.1 as u16 + c.2 as u16 > 200;
                let (r, g, b) = (t(c.0), t(c.1), t(c.2));
                if r == 0 && g == 0 && b == 0 && bright {
                    Rgb(128, 128, 128)
                } else {
                    Rgb(r, g, b)
                }
            }
        }
    }
}

// --- OKLab ---------------------------------------------------------------
//
// Mixing and lightness adjustment happen in OKLab rather than sRGB, because
// interpolating sRGB directly darkens through the middle of a ramp and shifts
// hue in ways that read as banding.

fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(v: f32) -> f32 {
    if v <= 0.0031308 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Oklab {
    pub l: f32,
    pub a: f32,
    pub b: f32,
}

pub fn to_oklab(c: Rgb) -> Oklab {
    let r = srgb_to_linear(c.0 as f32 / 255.0);
    let g = srgb_to_linear(c.1 as f32 / 255.0);
    let b = srgb_to_linear(c.2 as f32 / 255.0);

    let l = (0.412_221_46 * r + 0.536_332_55 * g + 0.051_445_995 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();

    Oklab {
        l: 0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        a: 1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        b: 0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    }
}

pub fn from_oklab(c: Oklab) -> Rgb {
    let l_ = c.l + 0.396_337_78 * c.a + 0.215_803_76 * c.b;
    let m_ = c.l - 0.105_561_346 * c.a - 0.063_854_17 * c.b;
    let s_ = c.l - 0.089_484_18 * c.a - 1.291_485_5 * c.b;

    let (l, m, s) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);

    let r = 4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s;
    let g = -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s;
    let b = -0.0041960863 * l - 0.703_418_6 * m + 1.707_614_7 * s;

    let f = |v: f32| (linear_to_srgb(v).clamp(0.0, 1.0) * 255.0).round() as u8;
    Rgb(f(r), f(g), f(b))
}

/// Blend two colours in OKLab. `t` runs 0 → `a`, 1 → `b`.
pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let (x, y) = (to_oklab(a), to_oklab(b));
    from_oklab(Oklab {
        l: x.l + (y.l - x.l) * t,
        a: x.a + (y.a - x.a) * t,
        b: x.b + (y.b - x.b) * t,
    })
}

/// Shift a colour's lightness by a fixed OKLab delta. Used for the hover
/// highlight, which must read as "brighter" without changing hue.
pub fn lighten(c: Rgb, delta: f32) -> Rgb {
    let mut o = to_oklab(c);
    o.l = (o.l + delta).clamp(0.0, 1.0);
    from_oklab(o)
}

/// Build a colour from a hue angle (degrees), a chroma and a lightness.
///
/// This is OKLCh — the polar form of OKLab — which is the natural space for
/// "same hue, more saturated": chroma and lightness move independently of hue,
/// so a staged file and an unchanged file in the same directory are visibly the
/// same colour family at different intensities.
///
/// Chroma is clamped down until the result is inside the sRGB gamut. Without
/// that, a request for high chroma at a hue sRGB cannot reach gets silently
/// clipped per channel, which shifts the hue — exactly the drift that would
/// break the directory-is-hue promise.
pub fn from_lch(hue_deg: f32, chroma: f32, lightness: f32) -> Rgb {
    let h = hue_deg.to_radians();
    let (sin, cos) = h.sin_cos();
    let mut c = chroma.max(0.0);
    // Binary search the largest in-gamut chroma; 12 steps is well under the
    // precision of an 8-bit channel.
    if !in_gamut(lightness, c * cos, c * sin) {
        let (mut lo, mut hi) = (0.0f32, c);
        for _ in 0..12 {
            let mid = (lo + hi) / 2.0;
            if in_gamut(lightness, mid * cos, mid * sin) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        c = lo;
    }
    from_oklab(Oklab {
        l: lightness,
        a: c * cos,
        b: c * sin,
    })
}

/// Whether an OKLab triple lands inside sRGB without clipping.
fn in_gamut(l: f32, a: f32, b: f32) -> bool {
    let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;
    let (x, y, z) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    let r = 4.076_741_7 * x - 3.307_711_6 * y + 0.230_969_94 * z;
    let g = -1.268_438 * x + 2.609_757_4 * y - 0.341_319_38 * z;
    let bl = -0.004_196_086_3 * x - 0.703_418_6 * y + 1.707_614_7 * z;
    let ok = |v: f32| (-0.0001..=1.0001).contains(&v);
    ok(r) && ok(g) && ok(bl)
}

/// Rotate a colour's hue by an angle in degrees, preserving chroma and
/// lightness. Used to spread files across the arc their directory owns.
pub fn shift_hue(c: Rgb, degrees: f32) -> Rgb {
    let o = to_oklab(c);
    let chroma = (o.a * o.a + o.b * o.b).sqrt();
    // A near-grey has no meaningful hue to rotate; leave it alone rather than
    // manufacturing a colour out of rounding noise.
    if chroma < 1e-4 {
        return c;
    }
    let hue = o.b.atan2(o.a).to_degrees() + degrees;
    from_lch(hue, chroma, o.l)
}

/// A stable pseudo-random value in -1..1 derived from a byte string.
///
/// FNV-1a: tiny, no dependency, and well enough distributed for this. The
/// point is determinism — the same path must yield the same value on every
/// redraw, or the map shimmers as blocks re-roll their tint each frame.
pub fn jitter_for(bytes: &[u8]) -> f32 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // Take the high bits, which mix better than the low ones.
    let v = (h >> 32) as u32 as f32 / u32::MAX as f32;
    v * 2.0 - 1.0
}

/// Nudge a colour's lightness by a per-file amount so that adjacent blocks
/// sharing a status do not merge into one flat mass.
///
/// Applied in OKLab so the shift is perceptually even across the palette, and
/// kept small: it has to separate neighbours without reading as a difference in
/// meaning, since lightness is what the hover highlight and the heatmap ramp
/// already use.
pub fn jitter(c: Rgb, amount: f32) -> Rgb {
    let mut o = to_oklab(c);
    // Scale the nudge by the headroom on the side we are moving toward. Going
    // up, a bright saturated colour has little room left before it leaves the
    // sRGB gamut, and `from_oklab` clamping the channels there would shift the
    // hue — which would make a modified block drift toward looking added.
    // Going down, a near-black unchanged block needs a floor or every one of
    // them clamps to the same value and stays a flat mass.
    let headroom = if amount >= 0.0 {
        (1.0 - o.l).max(0.08)
    } else {
        o.l.max(0.12)
    };
    o.l = (o.l + amount * headroom).clamp(0.0, 1.0);
    from_oklab(o)
}

/// Pull a colour toward neutral grey at its own lightness. Unstaged changes
/// render desaturated so staging reads as a saturation jump on the same hue.
pub fn desaturate(c: Rgb, amount: f32) -> Rgb {
    let mut o = to_oklab(c);
    let k = 1.0 - amount.clamp(0.0, 1.0);
    o.a *= k;
    o.b *= k;
    from_oklab(o)
}

/// Sample a ramp given as evenly-spaced stops.
pub fn sample(stops: &[Rgb], t: f32) -> Rgb {
    if stops.is_empty() {
        return Rgb::BLACK;
    }
    if stops.len() == 1 {
        return stops[0];
    }
    let t = t.clamp(0.0, 1.0);
    let scaled = t * (stops.len() - 1) as f32;
    let i = (scaled.floor() as usize).min(stops.len() - 2);
    mix(stops[i], stops[i + 1], scaled - i as f32)
}

/// Magma, sampled at eight stops. Perceptually uniform and dark at the low end,
/// so an ancient file fades into the background rather than competing with it.
pub const MAGMA: [Rgb; 8] = [
    Rgb(0, 0, 4),
    Rgb(28, 16, 68),
    Rgb(79, 18, 123),
    Rgb(129, 37, 129),
    Rgb(181, 54, 122),
    Rgb(229, 80, 100),
    Rgb(251, 135, 97),
    Rgb(254, 194, 135),
];

/// Status colours.
///
/// Deliberately not red/green as the only distinction between modified and
/// added: added is blue-cyan and modified amber, which stay distinguishable
/// under the common colour-vision deficiencies.
pub struct StatusPalette {
    pub added: Rgb,
    pub modified: Rgb,
    pub deleted: Rgb,
    pub untracked: Rgb,
    pub conflicted: Rgb,
    /// Unchanged files stay very dim so the codebase's shape shows through —
    /// changes are only meaningful relative to the mass they sit in.
    pub unchanged: Rgb,
    /// Background of a directory's label band.
    pub dir_tint: Rgb,
    pub background: Rgb,
}

impl Default for StatusPalette {
    fn default() -> Self {
        StatusPalette {
            added: Rgb(64, 190, 220),
            modified: Rgb(224, 164, 60),
            deleted: Rgb(214, 74, 130),
            untracked: Rgb(110, 130, 110),
            conflicted: Rgb(240, 90, 70),
            unchanged: Rgb(46, 48, 56),
            dir_tint: Rgb(70, 74, 86),
            background: Rgb(16, 17, 21),
        }
    }
}

/// How a file's change state renders once hue is spoken for by the directory
/// tree: as chroma and lightness on the directory's own hue.
///
/// An unchanged file sits at very low chroma — a near-grey tinted toward its
/// directory — so the codebase's shape is legible without competing with the
/// changes sitting in it. A staged change goes to full chroma, an unstaged one
/// to roughly half, which makes staging read as the colour filling in.
#[derive(Debug, Clone, Copy)]
pub struct Intensity {
    pub chroma: f32,
    pub lightness: f32,
}

impl StatusPalette {
    /// Chroma and lightness for a change state.
    ///
    /// Deliberately not a hue: with per-directory hue, the kind of change
    /// (added vs modified vs deleted) is carried by the diff pane and the
    /// status line, and the map answers "how much is happening, and where".
    pub fn intensity(&self, change: Change, staged: bool) -> Intensity {
        match change {
            // Unchanged files are most of the map, so this is the tint that
            // has to carry the directory structure. Enough chroma for the hue
            // to be legible, but dark enough that the mass recedes behind the
            // changes sitting in it.
            Change::None => Intensity {
                chroma: 0.040,
                lightness: 0.34,
            },
            // Untracked is deliberately weaker than a tracked change: it is
            // not yet part of the repository and should not shout.
            Change::Untracked => Intensity {
                chroma: 0.085,
                lightness: 0.50,
            },
            // A conflict is the one state that must override everything else,
            // so it gets the most chroma and the most light.
            Change::Conflicted => Intensity {
                chroma: 0.200,
                lightness: 0.80,
            },
            // Added, modified and deleted read the same on the map; which one
            // it is shows in the diff.
            _ => {
                if staged {
                    Intensity {
                        chroma: 0.170,
                        lightness: 0.74,
                    }
                } else {
                    Intensity {
                        chroma: 0.110,
                        lightness: 0.60,
                    }
                }
            }
        }
    }

    /// Colour for a file: the directory's hue at the change's intensity.
    pub fn color_at_hue(&self, hue: f32, change: Change, staged: bool) -> Rgb {
        let i = self.intensity(change, staged);
        from_lch(hue, i.chroma, i.lightness)
    }

    /// Legacy flat colour, kept for the legend and for the 16-colour fallback
    /// where per-directory hue cannot survive quantisation anyway.
    pub fn color(&self, change: Change, staged: bool) -> Rgb {
        let base = match change {
            Change::None => return self.unchanged,
            Change::Added => self.added,
            Change::Modified => self.modified,
            Change::Deleted => self.deleted,
            Change::Untracked => self.untracked,
            Change::Conflicted => self.conflicted,
        };
        if staged { base } else { desaturate(base, 0.6) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oklab_roundtrips() {
        for c in [
            Rgb(0, 0, 0),
            Rgb(255, 255, 255),
            Rgb(128, 64, 32),
            Rgb(64, 190, 220),
        ] {
            let back = from_oklab(to_oklab(c));
            for (a, b) in [(c.0, back.0), (c.1, back.1), (c.2, back.2)] {
                assert!(
                    (a as i16 - b as i16).abs() <= 1,
                    "{c:?} roundtripped to {back:?}"
                );
            }
        }
    }

    #[test]
    fn mix_hits_both_ends() {
        let (a, b) = (Rgb(10, 20, 30), Rgb(200, 100, 50));
        assert_eq!(mix(a, b, 0.0), a);
        assert_eq!(mix(a, b, 1.0), b);
    }

    #[test]
    fn mix_steps_are_perceptually_even() {
        // The reason for mixing in OKLab: equal steps in `t` are equal steps in
        // perceived lightness. Checked in OKLab space rather than sRGB, where
        // the same ramp is deliberately non-linear (L=0.5 lands near sRGB 99,
        // not 128, because sRGB mid-grey is already L≈0.60).
        let ramp: Vec<f32> = (0..=10)
            .map(|i| to_oklab(mix(Rgb(0, 0, 0), Rgb(255, 255, 255), i as f32 / 10.0)).l)
            .collect();
        for w in ramp.windows(2) {
            let step = w[1] - w[0];
            assert!(
                (step - 0.1).abs() < 0.01,
                "uneven step {step} in ramp {ramp:?}"
            );
        }
    }

    #[test]
    fn lighten_raises_lightness() {
        let c = Rgb(60, 60, 90);
        let l = lighten(c, 0.15);
        assert!(to_oklab(l).l > to_oklab(c).l);
    }

    #[test]
    fn desaturate_moves_toward_grey() {
        let c = Rgb(224, 164, 60);
        let d = desaturate(c, 0.6);
        let (oc, od) = (to_oklab(c), to_oklab(d));
        assert!(od.a.abs() < oc.a.abs() && od.b.abs() < oc.b.abs());
        // Lightness is preserved, so it reads as the same colour drained.
        assert!((oc.l - od.l).abs() < 0.02);
    }

    #[test]
    fn staged_is_more_saturated_than_unstaged() {
        let p = StatusPalette::default();
        let (s, u) = (
            p.color(Change::Modified, true),
            p.color(Change::Modified, false),
        );
        let chroma = |c: Rgb| {
            let o = to_oklab(c);
            (o.a * o.a + o.b * o.b).sqrt()
        };
        assert!(chroma(s) > chroma(u));
    }

    #[test]
    fn ramp_samples_in_range() {
        for i in 0..=10 {
            let c = sample(&MAGMA, i as f32 / 10.0);
            // Just needs to be a real colour; the ends are the anchors.
            let _ = c;
        }
        assert_eq!(sample(&MAGMA, 0.0), MAGMA[0]);
        assert_eq!(sample(&MAGMA, 1.0), MAGMA[7]);
        // Out-of-range input clamps rather than wrapping or panicking.
        assert_eq!(sample(&MAGMA, -5.0), MAGMA[0]);
        assert_eq!(sample(&MAGMA, 5.0), MAGMA[7]);
    }

    #[test]
    fn jitter_is_deterministic_and_bounded() {
        // Stability is the whole point: a value that changed per redraw would
        // make the map shimmer.
        for p in [&b"src/main.rs"[..], b"a", b"", b"very/deep/nested/path.rs"] {
            let a = jitter_for(p);
            assert_eq!(a, jitter_for(p), "jitter must be stable for {p:?}");
            assert!((-1.0..=1.0).contains(&a), "{a} out of range");
        }
    }

    #[test]
    fn jitter_separates_different_paths() {
        let vals: Vec<f32> = ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"]
            .iter()
            .map(|p| jitter_for(p.as_bytes()))
            .collect();
        for (i, a) in vals.iter().enumerate() {
            for b in &vals[i + 1..] {
                assert!((a - b).abs() > 0.01, "paths collided: {vals:?}");
            }
        }
    }

    #[test]
    fn jitter_shifts_lightness_both_ways() {
        let c = Rgb(224, 164, 60);
        let up = to_oklab(jitter(c, 0.2)).l;
        let down = to_oklab(jitter(c, -0.2)).l;
        let base = to_oklab(c).l;
        assert!(up > base && down < base, "{down} {base} {up}");
    }

    #[test]
    fn jitter_varies_even_a_near_black_block() {
        // Unchanged files sit at a very low lightness. Without the headroom
        // floor they would all clamp to the same value and stay a flat mass.
        let dim = StatusPalette::default().unchanged;
        let a = jitter(dim, 0.8);
        let b = jitter(dim, -0.8);
        assert_ne!(a, b, "dim blocks must still separate");
    }

    #[test]
    fn jitter_preserves_hue() {
        // It is a lightness nudge, not a recolour: a modified block must not
        // drift toward looking added.
        let c = Rgb(224, 164, 60);
        let j = jitter(c, 0.2);
        let (oc, oj) = (to_oklab(c), to_oklab(j));
        let hue = |o: Oklab| o.b.atan2(o.a);
        assert!((hue(oc) - hue(oj)).abs() < 0.1, "hue drifted");
    }

    #[test]
    fn intensity_rises_with_how_much_is_happening() {
        // Chroma is the status channel now, so the ordering has to be strict:
        // unchanged < untracked < unstaged < staged < conflicted.
        let p = StatusPalette::default();
        let c = |ch, st| p.intensity(ch, st).chroma;
        let unchanged = c(Change::None, false);
        let untracked = c(Change::Untracked, false);
        let unstaged = c(Change::Modified, false);
        let staged = c(Change::Modified, true);
        let conflict = c(Change::Conflicted, false);
        assert!(
            unchanged < untracked && untracked < unstaged && unstaged < staged && staged < conflict,
            "{unchanged} {untracked} {unstaged} {staged} {conflict}"
        );
        // Each step has to be big enough to see, not just ordered.
        for (a, b) in [
            (unchanged, untracked),
            (untracked, unstaged),
            (unstaged, staged),
        ] {
            assert!(b - a > 0.015, "step from {a} to {b} is too small to read");
        }
    }

    #[test]
    fn unchanged_still_carries_a_legible_hue() {
        // Unchanged files are most of the map, so they are what actually
        // conveys the directory structure. Too little chroma and the whole
        // point is lost.
        let p = StatusPalette::default();
        assert!(p.intensity(Change::None, false).chroma > 0.025);
    }

    #[test]
    fn from_lch_preserves_the_requested_hue() {
        // The gamut clamp must reduce chroma, never rotate hue — a drifting
        // hue would put a file in the wrong directory's colour family.
        for hue in (0..360).step_by(15) {
            for l in [0.3f32, 0.5, 0.7] {
                let c = from_lch(hue as f32, 0.30, l);
                let o = to_oklab(c);
                let got = o.b.atan2(o.a).to_degrees().rem_euclid(360.0);
                let diff = ((got - hue as f32 + 180.0).rem_euclid(360.0) - 180.0).abs();
                assert!(diff < 3.0, "hue {hue} at L={l} came back as {got}");
            }
        }
    }

    #[test]
    fn from_lch_clamps_into_gamut() {
        // An impossible chroma must come back as the most saturated colour
        // that actually exists, not as clipped garbage.
        let c = from_lch(120.0, 0.9, 0.5);
        let o = to_oklab(c);
        let chroma = (o.a * o.a + o.b * o.b).sqrt();
        assert!(chroma < 0.5, "chroma {chroma} was not clamped");
        assert!(chroma > 0.05, "chroma {chroma} was clamped to nothing");
    }

    #[test]
    fn shift_hue_rotates_without_touching_chroma_or_lightness() {
        let c = from_lch(100.0, 0.10, 0.55);
        let s = shift_hue(c, 20.0);
        let (oc, os) = (to_oklab(c), to_oklab(s));
        let ch = |o: &Oklab| (o.a * o.a + o.b * o.b).sqrt();
        assert!((ch(&oc) - ch(&os)).abs() < 0.01);
        assert!((oc.l - os.l).abs() < 0.01);
        let hue = |o: &Oklab| o.b.atan2(o.a).to_degrees().rem_euclid(360.0);
        let d = ((hue(&os) - hue(&oc) + 180.0).rem_euclid(360.0) - 180.0).abs();
        assert!((d - 20.0).abs() < 3.0, "rotated by {d}, wanted 20");
    }

    #[test]
    fn shift_hue_leaves_a_grey_alone() {
        // A neutral has no hue to rotate; inventing one from rounding noise
        // would put stray colour on the map.
        let grey = Rgb(50, 50, 50);
        assert_eq!(shift_hue(grey, 40.0), grey);
    }

    #[test]
    fn quantize_256_snaps_to_cube() {
        let q = ColorDepth::Ansi256.quantize(Rgb(200, 10, 130));
        for v in [q.0, q.1, q.2] {
            assert!([0u8, 95, 135, 175, 215, 255].contains(&v), "got {v}");
        }
    }

    #[test]
    fn truecolor_passes_through() {
        let c = Rgb(1, 2, 3);
        assert_eq!(ColorDepth::True.quantize(c), c);
    }
}
