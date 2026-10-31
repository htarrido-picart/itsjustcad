// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Display-time line contrast: darken a color (OKLCH lightness only, hue
//! kept) until it reaches a WCAG contrast ratio against the background.
//! Stored colors are never touched; this runs only when building a snapshot.

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.003_130_8 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
}

/// WCAG relative luminance of an sRGB color (components 0..1).
pub(crate) fn relative_luminance(rgb: [f32; 3]) -> f32 {
    let [r, g, b] = rgb.map(srgb_to_linear);
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// WCAG contrast ratio between two sRGB colors (1..21).
pub(crate) fn contrast_ratio(a: [f32; 3], b: [f32; 3]) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// sRGB (0..1) → OKLab `[L, a, b]`.
pub(crate) fn srgb_to_oklab(rgb: [f32; 3]) -> [f32; 3] {
    let [r, g, b] = rgb.map(srgb_to_linear);
    let l = (0.412_221_47 * r + 0.536_332_55 * g + 0.051_445_99 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
    [
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    ]
}

/// OKLab → linear sRGB (may fall outside 0..1 when out of gamut).
fn oklab_to_linear(lab: [f32; 3]) -> [f32; 3] {
    let [ll, a, b] = lab;
    let l = (ll + 0.396_337_78 * a + 0.215_803_76 * b).powi(3);
    let m = (ll - 0.105_561_346 * a - 0.063_854_17 * b).powi(3);
    let s = (ll - 0.089_484_18 * a - 1.291_485_5 * b).powi(3);
    [
        4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s,
        -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s,
        -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s,
    ]
}

fn in_gamut(lin: [f32; 3]) -> bool {
    lin.iter().all(|c| (-1e-4..=1.0 + 1e-4).contains(c))
}

/// OKLCH `(l, c, hue)` → in-gamut sRGB, shrinking chroma only as far as the
/// gamut requires.
fn oklch_to_srgb(l: f32, c: f32, h: f32) -> [f32; 3] {
    let lab = |c: f32| [l, c * h.cos(), c * h.sin()];
    let mut lin = oklab_to_linear(lab(c));
    if !in_gamut(lin) {
        let (mut lo, mut hi) = (0.0, c);
        for _ in 0..24 {
            let mid = 0.5 * (lo + hi);
            if in_gamut(oklab_to_linear(lab(mid))) { lo = mid } else { hi = mid }
        }
        lin = oklab_to_linear(lab(lo));
    }
    lin.map(|v| linear_to_srgb(v.clamp(0.0, 1.0)))
}

/// Lightest color with the hue of `rgb` (OKLCH lightness lowered, chroma kept
/// as far as the gamut allows) whose contrast against `bg` is at least `min`.
/// Colors that already pass are returned unchanged. Only darkening is tried,
/// so `bg` must be light (black is the fallback when nothing lighter passes).
pub(crate) fn ensure_contrast(rgb: [f32; 3], bg: [f32; 3], min: f32) -> [f32; 3] {
    if contrast_ratio(rgb, bg) >= min {
        return rgb;
    }
    let [l0, a, b] = srgb_to_oklab(rgb);
    let (c, h) = (a.hypot(b), b.atan2(a));
    let (mut lo, mut hi) = (0.0, l0); // lo passes (black), hi fails
    for _ in 0..24 {
        let mid = 0.5 * (lo + hi);
        if contrast_ratio(oklch_to_srgb(mid, c, h), bg) >= min { lo = mid } else { hi = mid }
    }
    oklch_to_srgb(lo, c, h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Theme;

    fn rgb(c: [f32; 4]) -> [f32; 3] {
        [c[0], c[1], c[2]]
    }

    fn hue_deg(c: [f32; 3]) -> f32 {
        let [_, a, b] = srgb_to_oklab(c);
        b.atan2(a).to_degrees()
    }

    #[test]
    fn every_palette_color_reaches_3_to_1_on_light_and_keeps_hue() {
        let bg = rgb(Theme::Light.background());
        for i in 0..11 {
            let c = rgb(itsjustcad_doc::default_layer_color(i));
            let out = ensure_contrast(c, bg, 3.0);
            let ratio = contrast_ratio(out, bg);
            assert!(ratio >= 3.0, "palette {i}: {ratio:.2}");
            if contrast_ratio(c, bg) >= 3.0 {
                assert_eq!(out, c, "palette {i} already passes");
            } else {
                assert!(ratio < 3.1, "palette {i} over-darkened: {ratio:.2}");
            }
            let dh = (hue_deg(out) - hue_deg(c) + 540.0).rem_euclid(360.0) - 180.0;
            assert!(dh.abs() < 5.0, "palette {i}: hue moved {dh:.1}°");
        }
    }

    #[test]
    fn passing_colors_are_unchanged() {
        let bg = rgb(Theme::Light.background());
        for c in [[0.10, 0.11, 0.14], [0.0, 0.0, 0.0], [0.2, 0.1, 0.4]] {
            assert_eq!(ensure_contrast(c, bg, 3.0), c);
        }
    }

    #[test]
    fn oklab_round_trips() {
        let c = [0.85098, 0.643137, 0.254902];
        let [l, a, b] = srgb_to_oklab(c);
        let back = oklch_to_srgb(l, a.hypot(b), b.atan2(a));
        for k in 0..3 {
            assert!((back[k] - c[k]).abs() < 1e-3, "{back:?} vs {c:?}");
        }
    }
}
