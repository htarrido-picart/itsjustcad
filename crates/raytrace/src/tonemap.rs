// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! HDR → LDR tone mapping. The path tracer accumulates unbounded linear
//! radiance; this compresses it to displayable `[0,1]`, then gamma-encodes to
//! 8-bit sRGB.

use glam::DVec3;

/// Tone-mapping operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ToneMap {
    /// Reinhard `x / (1 + x)` — gentle, never clips, slightly desaturating.
    Reinhard,
    /// ACES filmic approximation — punchier contrast, film-like shoulder.
    #[default]
    Aces,
}

impl ToneMap {
    /// Map one linear radiance value to `[0,1]` display-linear.
    pub fn apply(self, c: DVec3) -> DVec3 {
        match self {
            ToneMap::Reinhard => c / (c + DVec3::ONE),
            ToneMap::Aces => aces(c),
        }
    }
}

/// Narkowicz's ACES filmic curve, applied per channel.
fn aces(c: DVec3) -> DVec3 {
    const A: f64 = 2.51;
    const B: f64 = 0.03;
    const C: f64 = 2.43;
    const D: f64 = 0.59;
    const E: f64 = 0.14;
    let f = |x: f64| ((x * (A * x + B)) / (x * (C * x + D) + E)).clamp(0.0, 1.0);
    DVec3::new(f(c.x), f(c.y), f(c.z))
}

/// Encode display-linear `[0,1]` to an 8-bit sRGB byte.
pub fn linear_to_srgb8(u: f64) -> u8 {
    let u = u.clamp(0.0, 1.0);
    let s = if u <= 0.003_130_8 {
        u * 12.92
    } else {
        1.055 * u.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0 + 0.5).clamp(0.0, 255.0) as u8
}

/// Full pipeline: tone-map an HDR radiance to an sRGB `[R,G,B]` byte triple.
pub fn to_srgb8(c: DVec3, tm: ToneMap) -> [u8; 3] {
    let m = tm.apply(c);
    [
        linear_to_srgb8(m.x),
        linear_to_srgb8(m.y),
        linear_to_srgb8(m.z),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tonemap_maps_hdr_into_byte_range() {
        for tm in [ToneMap::Reinhard, ToneMap::Aces] {
            for &v in &[0.0, 0.1, 1.0, 5.0, 100.0, 1e6] {
                let [r, g, b] = to_srgb8(DVec3::splat(v), tm);
                // u8 is inherently in range; assert monotonic sensible values.
                assert_eq!(r, g);
                assert_eq!(g, b);
            }
        }
    }

    #[test]
    fn tonemap_is_monotonic() {
        // Brighter input never produces a darker byte.
        for tm in [ToneMap::Reinhard, ToneMap::Aces] {
            let mut last = 0u8;
            for i in 0..=100 {
                let v = i as f64 * 0.1;
                let [r, _, _] = to_srgb8(DVec3::splat(v), tm);
                assert!(r >= last, "non-monotonic at v={v}: {r} < {last}");
                last = r;
            }
        }
    }

    #[test]
    fn black_is_zero_bright_is_high() {
        let [r, _, _] = to_srgb8(DVec3::ZERO, ToneMap::Aces);
        assert_eq!(r, 0);
        let [r, _, _] = to_srgb8(DVec3::splat(1e6), ToneMap::Aces);
        assert!(r > 240, "very bright saturates near white: {r}");
    }

    #[test]
    fn srgb_encode_gamma_boosts_midtones() {
        // Linear 0.5 encodes to sRGB ~0.735 → ~188.
        let b = linear_to_srgb8(0.5);
        assert!((185..=191).contains(&b), "b={b}");
    }
}
