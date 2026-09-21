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
        if let Ok(ct) = std::env::var("COLORTERM") {
            if ct.contains("truecolor") || ct.contains("24bit") {
                return ColorDepth::True;
            }
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

    let l = (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b).cbrt();
    let m = (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b).cbrt();
    let s = (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b).cbrt();

    Oklab {
        l: 0.2104542553 * l + 0.7936177850 * m - 0.0040720468 * s,
        a: 1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s,
        b: 0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s,
    }
}

pub fn from_oklab(c: Oklab) -> Rgb {
    let l_ = c.l + 0.3963377774 * c.a + 0.2158037573 * c.b;
    let m_ = c.l - 0.1055613458 * c.a - 0.0638541728 * c.b;
    let s_ = c.l - 0.0894841775 * c.a - 1.2914855480 * c.b;

    let (l, m, s) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);

    let r = 4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s;
    let g = -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s;
    let b = -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s;

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

impl StatusPalette {
    /// Colour for a file, given its change and whether it is staged.
    ///
    /// Staged files render at full saturation and unstaged at ~40%, so staging
    /// shows as a saturation change on one hue — which reads well even when a
    /// file is only a couple of pixels across.
    pub fn color(&self, change: Change, staged: bool) -> Rgb {
        let base = match change {
            Change::None => return self.unchanged,
            Change::Added => self.added,
            Change::Modified => self.modified,
            Change::Deleted => self.deleted,
            Change::Untracked => self.untracked,
            Change::Conflicted => self.conflicted,
        };
        if staged {
            base
        } else {
            desaturate(base, 0.6)
        }
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
