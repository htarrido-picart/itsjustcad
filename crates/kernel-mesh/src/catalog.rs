// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Built-in named structural-section catalog.
//!
//! A curated set of TYPICAL cross-sections spanning both European (Euro-norm)
//! and US (AISC) standards, covering every [`Section`] family. Dimensions are
//! NOMINAL and expressed in METERS (US imperial shapes are converted). These
//! let `beam`/`column` reference a well-known profile by name (e.g. `IPE300`)
//! without first defining it, resolving through [`builtin_section`] at exec.
//!
//! Values are published nominal dimensions rounded to the millimeter; they are
//! good enough for modeling and takeoff, not a substitute for a certified
//! section-property database.

use crate::structsection::Section;

/// Feet → meters, for converting US imperial nominal dimensions.
const FT: f64 = 0.3048;
/// Inches → meters.
const IN: f64 = FT / 12.0;

/// The catalog: `(name, section)` pairs in a stable, curated order. Kept as a
/// single source of truth for both [`builtin_section`] and
/// [`builtin_section_names`]; a test asserts the two stay in sync.
///
/// Dimensions in meters. US shapes converted from imperial nominal values.
static CATALOG: &[(&str, Section)] = &[
    // --- I / wide-flange, Euro (IPE/HE series) ---
    // IPE: d, bf, tf, tw
    ("IPE200", Section::IWideFlange { d: 0.200, bf: 0.100, tf: 0.0085, tw: 0.0056 }),
    ("IPE300", Section::IWideFlange { d: 0.300, bf: 0.150, tf: 0.0107, tw: 0.0071 }),
    ("IPE400", Section::IWideFlange { d: 0.400, bf: 0.180, tf: 0.0135, tw: 0.0086 }),
    // HEA (lighter) / HEB (heavier) wide-flange columns.
    ("HEA200", Section::IWideFlange { d: 0.190, bf: 0.200, tf: 0.010, tw: 0.0065 }),
    ("HEA300", Section::IWideFlange { d: 0.290, bf: 0.300, tf: 0.014, tw: 0.0085 }),
    ("HEB200", Section::IWideFlange { d: 0.200, bf: 0.200, tf: 0.015, tw: 0.009 }),
    // --- I / wide-flange, US (W series), imperial nominal → m ---
    // W12x26: d≈12.22in, bf≈6.49in, tf≈0.38in, tw≈0.23in
    ("W12x26", Section::IWideFlange { d: 12.22 * IN, bf: 6.49 * IN, tf: 0.38 * IN, tw: 0.23 * IN }),
    // W16x40: d≈16.01in, bf≈6.995in, tf≈0.505in, tw≈0.305in
    ("W16x40", Section::IWideFlange { d: 16.01 * IN, bf: 6.995 * IN, tf: 0.505 * IN, tw: 0.305 * IN }),
    // W21x50: d≈20.83in, bf≈6.53in, tf≈0.535in, tw≈0.38in
    ("W21x50", Section::IWideFlange { d: 20.83 * IN, bf: 6.53 * IN, tf: 0.535 * IN, tw: 0.38 * IN }),
    // --- Tee ---
    // Euro tees (nominal square tee: d, bf, tf, tw).
    ("T150x150", Section::Tee { d: 0.150, bf: 0.150, tf: 0.014, tw: 0.009 }),
    ("T200x200", Section::Tee { d: 0.200, bf: 0.200, tf: 0.016, tw: 0.010 }),
    // WT8x25 (cut from W16x50): d≈8.13in, bf≈7.07in, tf≈0.565in, tw≈0.345in
    ("WT8x25", Section::Tee { d: 8.13 * IN, bf: 7.07 * IN, tf: 0.565 * IN, tw: 0.345 * IN }),
    // --- Channel (C / U) ---
    // UPN Euro channels: d, bf, tf(avg), tw.
    ("UPN200", Section::Channel { d: 0.200, bf: 0.075, tf: 0.0115, tw: 0.0085 }),
    ("UPN300", Section::Channel { d: 0.300, bf: 0.100, tf: 0.016, tw: 0.010 }),
    // C10x15.3 (US): d≈10in, bf≈2.6in, tf≈0.436in, tw≈0.24in
    ("C10x15", Section::Channel { d: 10.0 * IN, bf: 2.6 * IN, tf: 0.436 * IN, tw: 0.24 * IN }),
    // --- Angle (L) ---
    // Euro equal angles: a, b, t.
    ("L100x100", Section::Angle { a: 0.100, b: 0.100, t: 0.010 }),
    ("L75x75", Section::Angle { a: 0.075, b: 0.075, t: 0.008 }),
    // L4x4x1/4 (US): 4in legs, 1/4in thick.
    ("L4x4", Section::Angle { a: 4.0 * IN, b: 4.0 * IN, t: 0.25 * IN }),
    // --- HSS (square/rectangular tube) ---
    // SHS Euro square hollow: w, h, t.
    ("SHS150", Section::Hss { w: 0.150, h: 0.150, t: 0.008 }),
    ("RHS200x100", Section::Hss { w: 0.200, h: 0.100, t: 0.008 }),
    // HSS6x6x1/4 (US): 6in square, 1/4in wall.
    ("HSS6x6", Section::Hss { w: 6.0 * IN, h: 6.0 * IN, t: 0.25 * IN }),
    // --- Pipe / CHS (circular hollow) ---
    // CHS Euro: d (mm) outer, t wall.
    ("CHS168", Section::Pipe { d: 0.1683, t: 0.006 }),
    ("CHS219", Section::Pipe { d: 0.2191, t: 0.008 }),
    // PIPE 6 std (US, NPS6): OD≈6.625in, wall≈0.28in.
    ("PIPE6", Section::Pipe { d: 6.625 * IN, t: 0.28 * IN }),
    // --- Reinforced-concrete / plain ---
    ("RC300x600", Section::Rectangular { w: 0.300, h: 0.600 }),
    ("RC400x400", Section::Rectangular { w: 0.400, h: 0.400 }),
    ("CIRC400", Section::Circular { d: 0.400 }),
    // --- Timber (glulam) ---
    ("GLT200x400", Section::Timber { w: 0.200, h: 0.400 }),
    ("GLT160x320", Section::Timber { w: 0.160, h: 0.320 }),
    // --- Bamboo (Guadua culm) ---
    ("GUADUA100", Section::Guadua { d: 0.100, t: 0.010 }),
];

/// Resolve a built-in section by name. Matches the catalog key exactly first,
/// then falls back to a case-insensitive match (so `ipe300` resolves too).
/// Returns `None` for an unknown name.
pub fn builtin_section(name: &str) -> Option<Section> {
    if let Some((_, s)) = CATALOG.iter().find(|(k, _)| *k == name) {
        return Some(*s);
    }
    CATALOG
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, s)| *s)
}

/// The catalog names, in stable curated order. Used by the guided beam/column
/// section keyword and by tests to keep the guided options in sync.
pub fn builtin_section_names() -> &'static [&'static str] {
    // Backed by a lazily-materialized static slice of the catalog keys.
    use std::sync::OnceLock;
    static NAMES: OnceLock<Vec<&'static str>> = OnceLock::new();
    NAMES.get_or_init(|| CATALOG.iter().map(|(k, _)| *k).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_lookups_resolve() {
        // IPE300 → wide-flange, overall depth ≈ 0.30 m.
        match builtin_section("IPE300") {
            Some(Section::IWideFlange { d, .. }) => assert!((d - 0.3).abs() < 1e-9),
            other => panic!("IPE300 should be an I wide-flange, got {other:?}"),
        }
        // HSS6x6 → Hss.
        assert!(matches!(builtin_section("HSS6x6"), Some(Section::Hss { .. })));
        // Unknown → None.
        assert!(builtin_section("NOPE999").is_none());
    }

    #[test]
    fn lookup_is_case_insensitive_fallback() {
        assert_eq!(builtin_section("ipe300"), builtin_section("IPE300"));
    }

    #[test]
    fn names_and_lookup_stay_in_sync() {
        let names = builtin_section_names();
        assert_eq!(names.len(), CATALOG.len());
        for name in names {
            assert!(
                builtin_section(name).is_some(),
                "catalog name {name} must resolve"
            );
        }
    }

    #[test]
    fn spans_every_family() {
        // Every Section family should appear at least once in the catalog.
        let has = |pred: fn(&Section) -> bool| CATALOG.iter().any(|(_, s)| pred(s));
        assert!(has(|s| matches!(s, Section::Rectangular { .. })));
        assert!(has(|s| matches!(s, Section::Circular { .. })));
        assert!(has(|s| matches!(s, Section::IWideFlange { .. })));
        assert!(has(|s| matches!(s, Section::Pipe { .. })));
        assert!(has(|s| matches!(s, Section::Timber { .. })));
        assert!(has(|s| matches!(s, Section::Guadua { .. })));
        assert!(has(|s| matches!(s, Section::Tee { .. })));
        assert!(has(|s| matches!(s, Section::Channel { .. })));
        assert!(has(|s| matches!(s, Section::Angle { .. })));
        assert!(has(|s| matches!(s, Section::Hss { .. })));
    }
}
