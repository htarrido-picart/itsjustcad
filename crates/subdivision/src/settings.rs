// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! `SubdivisionSettings` — the parameter object (plan §6). serde-defaulted so old
//! documents load; stored on the Document by the commands crate. Mirrors
//! CityEngine attribute names where they exist.
//!
//! **Semantic trap (plan §6):** `lot_area_min`, `lot_width_min`, `irregularity`
//! mean different things per method. Phase 3 uses only the Recursive
//! interpretation (recursion stop / min side length / split-pivot deviation).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SubdivisionMethod {
    #[default]
    Recursive,
    Offset,
    Skeleton,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LoadingType {
    #[default]
    FrontLoaded,
    AlleyLoaded,
    Mixed,
}

/// Which straight-skeleton implementation the skeleton subdivider uses
/// (plan §12.2). `Offset` = the Phase-7 [`crate::straight_skeleton::
/// OffsetApproxSkeleton`] (robust on any polygon) — the DEFAULT for safety.
/// `Felkel` = the Phase-12 [`crate::straight_skeleton::FelkelSkeleton`] (a true
/// straight skeleton, exact on convex polygons via the priority-queue event loop,
/// falling back to the approximate skeleton on non-convex input). Opt-in via
/// `lotsettings skeleton=felkel|offset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SkeletonImpl {
    /// Offset-approximate skeleton (Phase 7). Robust; the safe default.
    #[default]
    Offset,
    /// True Felkel skeleton (Phase 12), convex-exact + non-convex fallback.
    Felkel,
}

impl SkeletonImpl {
    /// Parse a skeleton-impl keyword. `None` if unknown.
    pub fn parse(s: &str) -> Option<SkeletonImpl> {
        match s.to_lowercase().as_str() {
            "offset" | "approx" | "offsetapprox" => Some(SkeletonImpl::Offset),
            "felkel" | "true" | "exact" => Some(SkeletonImpl::Felkel),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CornerAlignment {
    #[default]
    StreetWidth,
    StreetLength,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum StreetPattern {
    #[default]
    Orthogonal,
    Skewed,
    Organic,
    CulDeSac,
    /// Owner scope (Phase 5b).
    Radial,
    /// Owner scope (Phase 5b).
    Hexagonal,
    /// Owner scope (Phase 5b).
    Voronoi,
}

/// A width-mix product list for Phase 6 (packing). Unused in Phase 3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LotWidthMix {
    /// `(width, proportion)` pairs, e.g. `[(40,0.3),(50,0.5),(60,0.2)]`.
    pub products: Vec<(f64, f64)>,
    pub strict_proportions: bool,
}

impl LotWidthMix {
    /// The euro_latam default width mix (plan §6b): 6 / 8 / 10 m at 25 / 50 /
    /// 25 %, SOFT (not a hard ratio). **Placeholder** until Manuel confirms his
    /// real product list — flag it in command output.
    pub fn euro_latam_default() -> LotWidthMix {
        LotWidthMix {
            products: vec![(6.0, 0.25), (8.0, 0.50), (10.0, 0.25)],
            strict_proportions: false,
        }
    }
}

/// Building typology (plan §7.6, Phase 10) — drives the footprint shape inside
/// the buildable envelope. All owner-scoped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Typology {
    /// Free-standing house: a compact mass centred in the envelope. DEFAULT.
    #[default]
    Detached,
    /// Row / terrace: fills the full lot width (party-wall, side = 0, matching
    /// the euro_latam medianería) with a front/rear margin.
    Row,
    /// Courtyard: a ring footprint with an interior void.
    Courtyard,
    /// Apartment slab: a single large mass ~= the envelope.
    Slab,
}

impl Typology {
    /// Parse a typology keyword (with aliases). `None` if unknown.
    pub fn parse(s: &str) -> Option<Typology> {
        match s.to_lowercase().as_str() {
            "detached" | "house" | "single" => Some(Typology::Detached),
            "row" | "terrace" | "townhouse" | "rowhouse" => Some(Typology::Row),
            "courtyard" | "court" | "perimeter" => Some(Typology::Courtyard),
            "slab" | "apartment" | "apt" | "block" => Some(Typology::Slab),
            _ => None,
        }
    }
}

/// Footprint fill mode (plan §7.6, Phase 10) — how much of the buildable
/// envelope the footprint claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FootprintMode {
    /// The footprint IS the buildable envelope.
    FullEnvelope,
    /// A centred footprint whose area is `coverage_frac × lot_area` (lot-coverage
    /// %), clamped to the envelope.
    CoverageRatio,
    /// The envelope inset by `footprint_inset` on all sides.
    Inset,
    /// The typology dictates the shape. DEFAULT.
    #[default]
    TypologyDriven,
}

impl FootprintMode {
    /// Parse a footprint-mode keyword. `None` if unknown.
    pub fn parse(s: &str) -> Option<FootprintMode> {
        match s.to_lowercase().as_str() {
            "full" | "fullenvelope" | "envelope" => Some(FootprintMode::FullEnvelope),
            "coverage" | "coverageratio" | "ratio" => Some(FootprintMode::CoverageRatio),
            "inset" => Some(FootprintMode::Inset),
            "typology" | "typologydriven" | "auto" => Some(FootprintMode::TypologyDriven),
            _ => None,
        }
    }
}

/// Roof form (plan §7.6, Phase 10). `PerTypology` picks a sensible default from
/// the typology (detached/row → gable, courtyard/slab → flat).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RoofType {
    /// Horizontal slab cap.
    Flat,
    /// Symmetric two-slope roof with a ridge along the long axis.
    Gable,
    /// Four sloped faces to a central ridge.
    Hip,
    /// Single mono-pitch plane.
    Shed,
    /// Pick per-typology (DEFAULT).
    #[default]
    PerTypology,
}

impl RoofType {
    /// Parse a roof-type keyword. `None` if unknown.
    pub fn parse(s: &str) -> Option<RoofType> {
        match s.to_lowercase().as_str() {
            "flat" => Some(RoofType::Flat),
            "gable" | "pitched" => Some(RoofType::Gable),
            "hip" | "hipped" => Some(RoofType::Hip),
            "shed" | "mono" | "monopitch" | "skillion" => Some(RoofType::Shed),
            "auto" | "pertypology" | "typology" | "default" => Some(RoofType::PerTypology),
            _ => None,
        }
    }
}

/// A named default profile (plan §6b). `EuroLatam` carries the metric European /
/// Latin-American placeholder numbers; `UsSuburban` is a stub for later. **Every
/// EuroLatam value is a placeholder** pending Manuel's §1 answers — surfaced in
/// command output whenever a lot-rules run falls back to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RegionProfile {
    /// European / Latin-American metric urban form (plan §6b) — the default.
    #[default]
    EuroLatam,
    /// US suburban (feet-derived). Reserved; not yet populated.
    UsSuburban,
}

/// Upper clamp on `irregularity`: Manuel's soft default is 0.4; `loose` unlocks
/// to 1.0 (Phase 3: loose ONLY raises the cap — no organic subdivider yet).
pub const IRREGULARITY_CAP_TIGHT: f64 = 0.4;
pub const IRREGULARITY_CAP_LOOSE: f64 = 1.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubdivisionSettings {
    pub method: SubdivisionMethod,
    /// Deterministic output seed (combined with a block hash at run time).
    pub seed: u64,

    /// Named default profile (plan §6b). Drives the lot-rules (Phase 6)
    /// placeholder defaults; `EuroLatam` numbers are placeholders until Manuel
    /// confirms — flagged in output when used.
    pub region: RegionProfile,

    // ── Recursive ──
    /// 1.0 = every child lot must retain a street edge.
    pub force_street_access: f64,
    /// Recursion stop condition (Recursive) / merge threshold (Skeleton).
    pub lot_area_min: f64,
    pub lot_area_max: f64,
    /// Min length of any lot side (Recursive) / ideal frontage (Skeleton).
    pub lot_width_min: f64,
    /// Split-pivot deviation from the OBB midpoint (Recursive). Clamped to
    /// `[0, 0.4]` unless `loose`, which raises the cap to 1.0.
    pub irregularity: f64,
    /// Owner scope: unlock `irregularity` > 0.4. Phase 3 only widens the clamp.
    pub loose: bool,
    pub corner_angle_max: f64,
    pub corner_width: f64,

    // ── Offset (Phase 4) ──
    pub offset_width: f64,
    pub subdivide_core: bool,

    // ── Road network (Phase 5, `lotgeneratesite`) ──
    /// Which of the road generators to run (four rectilinear in Phase 5;
    /// radial/hex/Voronoi are Phase 5b owner scope).
    pub street_pattern: StreetPattern,
    /// Road right-of-way width (full, curb to curb). 0 = generator default.
    pub road_width: f64,
    /// Target block depth used to space roads. 0 = derive from lot depth.
    pub block_depth: f64,

    // ── Skeleton (Phase 7 / 12) ──
    pub shallow_lot_frac: f64,
    pub corner_align: CornerAlignment,
    pub simplify: f64,
    /// Which straight-skeleton backend `method=streetfollowing` uses (plan §12.2).
    /// Default `Offset` (the robust Phase-7 approximate skeleton); `Felkel` opts
    /// into the true Phase-12 skeleton (convex-exact, non-convex fallback).
    pub skeleton_impl: SkeletonImpl,

    // ── Lot rules (Phase 6) ──
    pub width_mix: Option<LotWidthMix>,
    pub lot_depth_target: f64,
    pub lot_depth_tolerance: f64,
    pub loading: LoadingType,
    pub alley_width: f64,
    pub corner_lot_width_bonus: f64,
    pub allow_flag_lots: bool,
    pub flag_pole_width_min: f64,
    pub merge_slivers: bool,
    pub sliver_area_frac: f64,
    pub frontage_at_setback: bool,

    // ── Setbacks (Phase 8) ──
    pub setback_front: f64,
    pub setback_side: f64,
    pub setback_rear: f64,
    pub build_to_line: f64,
    pub draw_buildable_envelope: bool,

    // ── Open space (Phase 9) ──
    /// 0.0 = off (feature-placement default); > 0 = blind %-reserve (owner).
    pub open_space_reserve_frac: f64,

    // ── Buildings (Phase 10) ──
    /// Building typology (drives the footprint shape). Default `Detached`.
    pub typology: Typology,
    /// Footprint fill mode. Default `TypologyDriven`.
    pub footprint_mode: FootprintMode,
    /// Lot-coverage fraction for `FootprintMode::CoverageRatio` (0..1).
    /// **euro_latam placeholder** (0.5) — confirm with Manuel.
    pub coverage_frac: f64,
    /// Fixed inset (metres) for `FootprintMode::Inset`.
    pub footprint_inset: f64,
    /// Storey height (metres). **euro_latam placeholder** (~3 m) — confirm with
    /// Manuel. Height = `floor_count × floor_height`.
    pub floor_height: f64,
    /// Number of storeys. Default 2.
    pub floor_count: usize,
    /// First floor index (0-based) that steps back. Floors at/above this inset
    /// cumulatively by `stepback_depth`.
    pub stepback_start_floor: usize,
    /// Per-floor step-back inset (metres) applied above `stepback_start_floor`.
    /// 0 = no step-back (a straight prism).
    pub stepback_depth: f64,
    /// Roof form. Default `PerTypology`.
    pub roof_type: RoofType,
    /// Roof pitch (degrees) for gable / hip / shed. **euro_latam placeholder**
    /// (30°) — confirm with Manuel.
    pub roof_pitch: f64,
}

impl Default for SubdivisionSettings {
    fn default() -> Self {
        Self {
            method: SubdivisionMethod::Recursive,
            seed: 0,
            region: RegionProfile::EuroLatam,
            force_street_access: 1.0,
            lot_area_min: 5000.0,
            lot_area_max: 9000.0,
            lot_width_min: 50.0,
            irregularity: 0.0,
            loose: false,
            corner_angle_max: 45.0,
            corner_width: 0.0,
            offset_width: 120.0,
            subdivide_core: true,
            street_pattern: StreetPattern::Orthogonal,
            road_width: 12.0,
            block_depth: 0.0,
            shallow_lot_frac: 0.0,
            corner_align: CornerAlignment::StreetWidth,
            simplify: 0.0,
            skeleton_impl: SkeletonImpl::Offset,
            width_mix: None,
            lot_depth_target: 0.0,
            lot_depth_tolerance: 0.0,
            loading: LoadingType::FrontLoaded,
            alley_width: 20.0,
            corner_lot_width_bonus: 0.0,
            allow_flag_lots: false,
            flag_pole_width_min: 20.0,
            merge_slivers: true,
            sliver_area_frac: 0.5,
            frontage_at_setback: true,
            setback_front: 25.0,
            setback_side: 5.0,
            setback_rear: 20.0,
            build_to_line: 0.0,
            draw_buildable_envelope: true,
            open_space_reserve_frac: 0.0,
            // Buildings (Phase 10). euro_latam placeholders flagged in-verb.
            typology: Typology::Detached,
            footprint_mode: FootprintMode::TypologyDriven,
            coverage_frac: 0.5,
            footprint_inset: 2.0,
            floor_height: 3.0,
            floor_count: 2,
            stepback_start_floor: 3,
            stepback_depth: 0.0,
            roof_type: RoofType::PerTypology,
            roof_pitch: 30.0,
        }
    }
}

impl SubdivisionSettings {
    /// The effective upper clamp for `irregularity` given `loose`.
    pub fn irregularity_cap(&self) -> f64 {
        if self.loose {
            IRREGULARITY_CAP_LOOSE
        } else {
            IRREGULARITY_CAP_TIGHT
        }
    }

    /// `irregularity` clamped to `[0, cap]` (cap widened by `loose`).
    pub fn clamped_irregularity(&self) -> f64 {
        self.irregularity.clamp(0.0, self.irregularity_cap())
    }

    /// A fully-metric **euro_latam** settings object (plan §6b). Every value is a
    /// placeholder pending Manuel's confirmation. Use this when the user selects
    /// the metric profile explicitly; the struct `Default` keeps the legacy
    /// imperial example numbers so pre-Phase-6 tests/files are unchanged.
    pub fn euro_latam() -> SubdivisionSettings {
        SubdivisionSettings {
            region: RegionProfile::EuroLatam,
            lot_width_min: 6.0,
            lot_area_min: 120.0,
            lot_area_max: 200.0,
            width_mix: Some(LotWidthMix::euro_latam_default()),
            lot_depth_target: 25.0,
            lot_depth_tolerance: 5.0,
            loading: LoadingType::FrontLoaded,
            alley_width: 5.0,
            corner_lot_width_bonus: 0.15,
            corner_angle_max: 45.0,
            allow_flag_lots: false,
            flag_pole_width_min: 3.0,
            merge_slivers: true,
            sliver_area_frac: 0.5,
            setback_front: 3.0,
            setback_side: 0.0,
            setback_rear: 3.0,
            road_width: 12.0,
            ..SubdivisionSettings::default()
        }
    }

    // ── Lot-rules (Phase 6) effective values + placeholder tracking ──────────
    //
    // Each accessor returns `(value, used_placeholder)`. `used_placeholder` is
    // true when the value came from the euro_latam profile because the user gave
    // no explicit override — the signal the command surfaces as
    // "using euro_latam defaults (placeholder — confirm with Manuel)".

    /// Effective width mix for the lot-rules pass. When `width_mix` is unset and
    /// the region is EuroLatam, fall back to the placeholder 6/8/10 m mix.
    pub fn effective_width_mix(&self) -> (Option<LotWidthMix>, bool) {
        match (&self.width_mix, self.region) {
            (Some(m), _) => (Some(m.clone()), false),
            (None, RegionProfile::EuroLatam) => {
                (Some(LotWidthMix::euro_latam_default()), true)
            }
            (None, _) => (None, false),
        }
    }

    /// Effective independent depth target (metres). Falls back to the euro_latam
    /// 25 m placeholder when unset (0) under the EuroLatam profile.
    pub fn effective_depth_target(&self) -> (f64, bool) {
        if self.lot_depth_target > 0.0 {
            (self.lot_depth_target, false)
        } else if self.region == RegionProfile::EuroLatam {
            (25.0, true)
        } else {
            (0.0, false)
        }
    }

    /// Effective depth tolerance (metres). Placeholder ±5 m under EuroLatam.
    pub fn effective_depth_tolerance(&self) -> f64 {
        if self.lot_depth_tolerance > 0.0 {
            self.lot_depth_tolerance
        } else if self.region == RegionProfile::EuroLatam {
            5.0
        } else {
            0.0
        }
    }

    /// Effective corner-lot width bonus (fraction). Placeholder +15 % under
    /// EuroLatam when unset (0).
    pub fn effective_corner_bonus(&self) -> (f64, bool) {
        if self.corner_lot_width_bonus > 0.0 {
            (self.corner_lot_width_bonus, false)
        } else if self.region == RegionProfile::EuroLatam {
            (0.15, true)
        } else {
            (0.0, false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let s = SubdivisionSettings::default();
        assert_eq!(s.lot_area_min, 5000.0);
        assert_eq!(s.lot_area_max, 9000.0);
        assert_eq!(s.lot_width_min, 50.0);
        assert_eq!(s.force_street_access, 1.0);
        assert!(s.merge_slivers);
        assert!(s.frontage_at_setback);
    }

    #[test]
    fn irregularity_clamped_tight_by_default() {
        let s = SubdivisionSettings {
            irregularity: 0.9,
            ..SubdivisionSettings::default()
        };
        assert!((s.clamped_irregularity() - 0.4).abs() < 1e-12);
    }

    #[test]
    fn loose_raises_the_cap() {
        let s = SubdivisionSettings {
            irregularity: 0.9,
            loose: true,
            ..SubdivisionSettings::default()
        };
        assert!((s.clamped_irregularity() - 0.9).abs() < 1e-12);
    }

    #[test]
    fn serde_roundtrip_and_default_backfill() {
        let s = SubdivisionSettings::default();
        let j = serde_json::to_string(&s).unwrap();
        let back: SubdivisionSettings = serde_json::from_str(&j).unwrap();
        assert_eq!(s, back);
        // Old file with only a couple of fields still loads (serde default).
        let partial: SubdivisionSettings =
            serde_json::from_str(r#"{"lot_width_min": 40.0}"#).unwrap();
        assert_eq!(partial.lot_width_min, 40.0);
        assert_eq!(partial.lot_area_min, 5000.0);
    }
}
