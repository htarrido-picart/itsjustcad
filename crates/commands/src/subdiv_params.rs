// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Live-parameter schema for associative subdivision (W1).
//!
//! `SubdivisionSettings` has ~50 fields; most are advanced/rarely-touched. This
//! module exposes a CURATED subset as a schema-driven parameter set, reusing the
//! same `ParamField`/`ParamValue`/`ParamMap` + `sanitize_fields` machinery that
//! drives the expressive-structures inspector (so the subdivision inspector is
//! generated for free). The two conversions keep the `ParamMap` (what the UI and
//! the associative link store) and the full `SubdivisionSettings` (what the
//! subdivider consumes) in sync:
//!
//! - [`settings_to_params`] projects a settings struct down to the exposed map.
//! - [`params_to_settings`] folds an exposed map back onto a base settings,
//!   leaving every non-exposed field untouched.
//!
//! The method enum maps to [`SubdivisionMethod`] with the same tokens the
//! `lotsubdivide method=…` verb accepts (grid/perimeter/streetfollowing).

use itsjustcad_doc::{sanitize_fields, ParamField, ParamMap, ParamValue, Unit, Widget};
use subdivision::{
    CornerAlignment, FootprintMode, LoadingType, RegionProfile, RoofType, SkeletonImpl,
    StreetPattern, SubdivisionMethod, SubdivisionSettings, Typology,
};

/// Method enum tokens exposed in the schema (match the `lotsubdivide method=…`
/// verb's accepted spellings; the primary token is listed).
const METHOD_CHOICES: &[&str] = &["grid", "perimeter", "streetfollowing"];

/// Stable token for a [`SubdivisionMethod`] (the primary `method=…` spelling).
pub fn method_token(m: SubdivisionMethod) -> &'static str {
    match m {
        SubdivisionMethod::Recursive => "grid",
        SubdivisionMethod::Offset => "perimeter",
        SubdivisionMethod::Skeleton => "streetfollowing",
    }
}

/// Parse a method token back to the enum (accepts the verb's aliases too).
pub fn method_from_token(s: &str) -> Option<SubdivisionMethod> {
    match s {
        "grid" | "recursive" => Some(SubdivisionMethod::Recursive),
        "perimeter" | "offset" => Some(SubdivisionMethod::Offset),
        "streetfollowing" | "skeleton" => Some(SubdivisionMethod::Skeleton),
        _ => None,
    }
}

// ── Enum token round-trips for the phase params ─────────────────────────────

const STREET_PATTERN_CHOICES: &[&str] =
    &["orthogonal", "skewed", "organic", "culdesac", "radial", "hexagonal", "voronoi"];

pub fn street_pattern_token(p: StreetPattern) -> &'static str {
    match p {
        StreetPattern::Orthogonal => "orthogonal",
        StreetPattern::Skewed => "skewed",
        StreetPattern::Organic => "organic",
        StreetPattern::CulDeSac => "culdesac",
        StreetPattern::Radial => "radial",
        StreetPattern::Hexagonal => "hexagonal",
        StreetPattern::Voronoi => "voronoi",
    }
}

pub fn street_pattern_from_token(s: &str) -> Option<StreetPattern> {
    match s.to_lowercase().as_str() {
        "orthogonal" | "grid" => Some(StreetPattern::Orthogonal),
        "skewed" => Some(StreetPattern::Skewed),
        "organic" => Some(StreetPattern::Organic),
        "culdesac" | "cul-de-sac" => Some(StreetPattern::CulDeSac),
        "radial" => Some(StreetPattern::Radial),
        "hexagonal" | "hex" => Some(StreetPattern::Hexagonal),
        "voronoi" => Some(StreetPattern::Voronoi),
        _ => None,
    }
}

const REGION_CHOICES: &[&str] = &["euro_latam", "us_suburban"];

pub fn region_token(r: RegionProfile) -> &'static str {
    match r {
        RegionProfile::EuroLatam => "euro_latam",
        RegionProfile::UsSuburban => "us_suburban",
    }
}

pub fn region_from_token(s: &str) -> Option<RegionProfile> {
    // Same aliases the `lotsettings region=…` verb accepts.
    match s.to_lowercase().as_str() {
        "euro_latam" | "eurolatam" | "metric" => Some(RegionProfile::EuroLatam),
        "us_suburban" | "us" | "imperial" => Some(RegionProfile::UsSuburban),
        _ => None,
    }
}

const LOADING_CHOICES: &[&str] = &["front", "alley", "mixed"];

pub fn loading_token(l: LoadingType) -> &'static str {
    match l {
        LoadingType::FrontLoaded => "front",
        LoadingType::AlleyLoaded => "alley",
        LoadingType::Mixed => "mixed",
    }
}

pub fn loading_from_token(s: &str) -> Option<LoadingType> {
    // Same aliases the `lotsettings loading=…` verb accepts.
    match s.to_lowercase().as_str() {
        "front" | "frontloaded" => Some(LoadingType::FrontLoaded),
        "alley" | "alleyloaded" => Some(LoadingType::AlleyLoaded),
        "mixed" => Some(LoadingType::Mixed),
        _ => None,
    }
}

const CORNER_ALIGN_CHOICES: &[&str] = &["streetwidth", "streetlength"];

pub fn corner_align_token(c: CornerAlignment) -> &'static str {
    match c {
        CornerAlignment::StreetWidth => "streetwidth",
        CornerAlignment::StreetLength => "streetlength",
    }
}

pub fn corner_align_from_token(s: &str) -> Option<CornerAlignment> {
    match s.to_lowercase().as_str() {
        "streetwidth" | "width" => Some(CornerAlignment::StreetWidth),
        "streetlength" | "length" => Some(CornerAlignment::StreetLength),
        _ => None,
    }
}

const SKELETON_IMPL_CHOICES: &[&str] = &["offset", "felkel"];

pub fn skeleton_impl_token(i: SkeletonImpl) -> &'static str {
    match i {
        SkeletonImpl::Offset => "offset",
        SkeletonImpl::Felkel => "felkel",
    }
}

pub fn skeleton_impl_from_token(s: &str) -> Option<SkeletonImpl> {
    SkeletonImpl::parse(s)
}

const TYPOLOGY_CHOICES: &[&str] = &["detached", "row", "courtyard", "slab"];

pub fn typology_token(t: Typology) -> &'static str {
    match t {
        Typology::Detached => "detached",
        Typology::Row => "row",
        Typology::Courtyard => "courtyard",
        Typology::Slab => "slab",
    }
}

pub fn typology_from_token(s: &str) -> Option<Typology> {
    Typology::parse(s)
}

const FOOTPRINT_MODE_CHOICES: &[&str] = &["full", "coverage", "inset", "typology"];

pub fn footprint_mode_token(m: FootprintMode) -> &'static str {
    match m {
        FootprintMode::FullEnvelope => "full",
        FootprintMode::CoverageRatio => "coverage",
        FootprintMode::Inset => "inset",
        FootprintMode::TypologyDriven => "typology",
    }
}

pub fn footprint_mode_from_token(s: &str) -> Option<FootprintMode> {
    FootprintMode::parse(s)
}

const ROOF_TYPE_CHOICES: &[&str] = &["flat", "gable", "hip", "shed", "auto"];

pub fn roof_type_token(r: RoofType) -> &'static str {
    match r {
        RoofType::Flat => "flat",
        RoofType::Gable => "gable",
        RoofType::Hip => "hip",
        RoofType::Shed => "shed",
        RoofType::PerTypology => "auto",
    }
}

pub fn roof_type_from_token(s: &str) -> Option<RoofType> {
    RoofType::parse(s)
}

/// The curated field set shown in the subdivision inspector / stored as a
/// `ParamMap`. Bounds mirror the subdivider's sane ranges; labels are i18n keys
/// (`param.subdivision.*`). Order here is the order rendered.
pub fn subdivision_fields() -> Vec<ParamField> {
    vec![
        ParamField::enum_(
            "method",
            "param.subdivision.method",
            "grid",
            METHOD_CHOICES,
        ),
        ParamField::int(
            "seed",
            "param.subdivision.seed",
            0,
            0,
            1_000_000,
            1,
            Widget::Numeric,
        ),
        ParamField::enum_(
            "region",
            "param.subdivision.region",
            "euro_latam",
            REGION_CHOICES,
        ),
        // Areas are in m² (no dedicated unit suffix — kept unitless in the UI).
        ParamField::float(
            "lot_area_min",
            "param.subdivision.lot_area_min",
            5000.0,
            Some(10.0),
            Some(1_000_000.0),
            50.0,
            Widget::Numeric,
            Unit::None,
        ),
        ParamField::float(
            "lot_area_max",
            "param.subdivision.lot_area_max",
            9000.0,
            Some(10.0),
            Some(1_000_000.0),
            50.0,
            Widget::Numeric,
            Unit::None,
        ),
        ParamField::float(
            "lot_width_min",
            "param.subdivision.lot_width_min",
            50.0,
            Some(1.0),
            Some(1000.0),
            1.0,
            Widget::Numeric,
            Unit::Meter,
        ),
        ParamField::float(
            "irregularity",
            "param.subdivision.irregularity",
            0.0,
            Some(0.0),
            Some(1.0),
            0.05,
            Widget::Slider,
            Unit::None,
        ),
        ParamField::float(
            "force_street_access",
            "param.subdivision.force_street_access",
            1.0,
            Some(0.0),
            Some(1.0),
            0.05,
            Widget::Slider,
            Unit::None,
        ),
        ParamField::float(
            "setback_front",
            "param.subdivision.setback_front",
            25.0,
            Some(0.0),
            Some(500.0),
            1.0,
            Widget::Numeric,
            Unit::Meter,
        ),
        ParamField::float(
            "setback_side",
            "param.subdivision.setback_side",
            5.0,
            Some(0.0),
            Some(500.0),
            1.0,
            Widget::Numeric,
            Unit::Meter,
        ),
        ParamField::float(
            "setback_rear",
            "param.subdivision.setback_rear",
            20.0,
            Some(0.0),
            Some(500.0),
            1.0,
            Widget::Numeric,
            Unit::Meter,
        ),
        ParamField::float(
            "road_width",
            "param.subdivision.road_width",
            12.0,
            Some(0.0),
            Some(200.0),
            0.5,
            Widget::Numeric,
            Unit::Meter,
        ),
        ParamField::float(
            "block_depth",
            "param.subdivision.block_depth",
            0.0,
            Some(0.0),
            Some(2000.0),
            5.0,
            Widget::Numeric,
            Unit::Meter,
        ),
        ParamField::float(
            "offset_width",
            "param.subdivision.offset_width",
            120.0,
            Some(1.0),
            Some(1000.0),
            1.0,
            Widget::Numeric,
            Unit::Meter,
        ),
        ParamField::float(
            "corner_width",
            "param.subdivision.corner_width",
            0.0,
            Some(0.0),
            Some(500.0),
            1.0,
            Widget::Numeric,
            Unit::Meter,
        ),
        ParamField::float(
            "lot_depth_target",
            "param.subdivision.lot_depth_target",
            0.0,
            Some(0.0),
            Some(1000.0),
            1.0,
            Widget::Numeric,
            Unit::Meter,
        ),
        ParamField::float(
            "lot_depth_tolerance",
            "param.subdivision.lot_depth_tolerance",
            0.0,
            Some(0.0),
            Some(100.0),
            0.5,
            Widget::Numeric,
            Unit::Meter,
        ),
        ParamField::float(
            "corner_lot_width_bonus",
            "param.subdivision.corner_lot_width_bonus",
            0.0,
            Some(0.0),
            Some(1.0),
            0.05,
            Widget::Slider,
            Unit::None,
        ),
        ParamField::enum_(
            "corner_align",
            "param.subdivision.corner_align",
            "streetwidth",
            CORNER_ALIGN_CHOICES,
        ),
        ParamField::enum_(
            "loading",
            "param.subdivision.loading",
            "front",
            LOADING_CHOICES,
        ),
        ParamField::float(
            "alley_width",
            "param.subdivision.alley_width",
            20.0,
            Some(0.0),
            Some(200.0),
            0.5,
            Widget::Numeric,
            Unit::Meter,
        ),
        ParamField::bool("allow_flag_lots", "param.subdivision.allow_flag_lots", false),
        ParamField::float(
            "flag_pole_width_min",
            "param.subdivision.flag_pole_width_min",
            20.0,
            Some(1.0),
            Some(100.0),
            0.5,
            Widget::Numeric,
            Unit::Meter,
        ),
        ParamField::bool("merge_slivers", "param.subdivision.merge_slivers", true),
        ParamField::float(
            "sliver_area_frac",
            "param.subdivision.sliver_area_frac",
            0.5,
            Some(0.0),
            Some(1.0),
            0.05,
            Widget::Slider,
            Unit::None,
        ),
        ParamField::float(
            "shallow_lot_frac",
            "param.subdivision.shallow_lot_frac",
            0.0,
            Some(0.0),
            Some(1.0),
            0.05,
            Widget::Slider,
            Unit::None,
        ),
        ParamField::bool("subdivide_core", "param.subdivision.subdivide_core", true),
        ParamField::enum_(
            "skeleton_impl",
            "param.subdivision.skeleton_impl",
            "offset",
            SKELETON_IMPL_CHOICES,
        ),
        ParamField::float(
            "simplify",
            "param.subdivision.simplify",
            0.0,
            Some(0.0),
            Some(10.0),
            0.1,
            Widget::Numeric,
            Unit::Meter,
        ),
        ParamField::bool(
            "frontage_at_setback",
            "param.subdivision.frontage_at_setback",
            true,
        ),
        ParamField::bool(
            "draw_buildable_envelope",
            "param.subdivision.draw_buildable_envelope",
            true,
        ),
        // ── Streets ──
        ParamField::enum_(
            "street_pattern",
            "param.subdivision.street_pattern",
            "orthogonal",
            STREET_PATTERN_CHOICES,
        ),
        // ── Setbacks ──
        ParamField::float(
            "build_to_line",
            "param.subdivision.build_to_line",
            0.0,
            Some(0.0),
            Some(500.0),
            1.0,
            Widget::Numeric,
            Unit::Meter,
        ),
        // ── Buildings ──
        ParamField::enum_(
            "typology",
            "param.subdivision.typology",
            "detached",
            TYPOLOGY_CHOICES,
        ),
        ParamField::enum_(
            "footprint_mode",
            "param.subdivision.footprint_mode",
            "full",
            FOOTPRINT_MODE_CHOICES,
        ),
        ParamField::float(
            "coverage_frac",
            "param.subdivision.coverage_frac",
            0.5,
            Some(0.0),
            Some(1.0),
            0.05,
            Widget::Slider,
            Unit::None,
        ),
        ParamField::float(
            "footprint_inset",
            "param.subdivision.footprint_inset",
            2.0,
            Some(0.0),
            Some(50.0),
            0.5,
            Widget::Numeric,
            Unit::Meter,
        ),
        ParamField::int(
            "floor_count",
            "param.subdivision.floor_count",
            2,
            1,
            200,
            1,
            Widget::Numeric,
        ),
        ParamField::float(
            "floor_height",
            "param.subdivision.floor_height",
            3.0,
            Some(1.0),
            Some(20.0),
            0.5,
            Widget::Numeric,
            Unit::Meter,
        ),
        ParamField::int(
            "stepback_start_floor",
            "param.subdivision.stepback_start_floor",
            3,
            0,
            200,
            1,
            Widget::Numeric,
        ),
        ParamField::float(
            "stepback_depth",
            "param.subdivision.stepback_depth",
            0.0,
            Some(0.0),
            Some(50.0),
            0.5,
            Widget::Numeric,
            Unit::Meter,
        ),
        ParamField::enum_(
            "roof_type",
            "param.subdivision.roof_type",
            "auto",
            ROOF_TYPE_CHOICES,
        ),
        ParamField::float(
            "roof_pitch",
            "param.subdivision.roof_pitch",
            30.0,
            Some(0.0),
            Some(89.0),
            1.0,
            Widget::Numeric,
            Unit::Degree,
        ),
        // ── Open space ──
        ParamField::float(
            "open_space_reserve_frac",
            "param.subdivision.open_space_reserve_frac",
            0.0,
            Some(0.0),
            Some(1.0),
            0.05,
            Widget::Slider,
            Unit::None,
        ),
    ]
}

/// Ordered phase groups for the inspector: each `(group i18n key, [field names])`.
/// The field names are resolved against [`subdivision_fields`] in order.
const FIELD_GROUPS: &[(&str, &[&str])] = &[
    ("param.subdivision.group.general", &["seed", "region"]),
    (
        "param.subdivision.group.streets",
        &["street_pattern", "road_width", "block_depth"],
    ),
    (
        "param.subdivision.group.lots",
        &[
            "method",
            "lot_area_min",
            "lot_area_max",
            "lot_width_min",
            "irregularity",
            "force_street_access",
            "offset_width",
            "subdivide_core",
            "corner_width",
            "corner_lot_width_bonus",
            "corner_align",
            "lot_depth_target",
            "lot_depth_tolerance",
            "loading",
            "alley_width",
            "allow_flag_lots",
            "flag_pole_width_min",
            "merge_slivers",
            "sliver_area_frac",
            "shallow_lot_frac",
            "skeleton_impl",
            "simplify",
        ],
    ),
    (
        "param.subdivision.group.setbacks",
        &[
            "setback_front",
            "setback_side",
            "setback_rear",
            "build_to_line",
            "frontage_at_setback",
            "draw_buildable_envelope",
        ],
    ),
    (
        "param.subdivision.group.buildings",
        &[
            "typology",
            "footprint_mode",
            "coverage_frac",
            "footprint_inset",
            "floor_count",
            "floor_height",
            "stepback_start_floor",
            "stepback_depth",
            "roof_type",
            "roof_pitch",
        ],
    ),
    (
        "param.subdivision.group.openspace",
        &["open_space_reserve_frac"],
    ),
];

/// Which phase groups apply to a given link kind (CityEngine-style visibility):
/// - `SitePlan` (the meta plan) drives the whole chain → every group.
/// - `Lots` (lotsubdivide) → general + lots only (it consumes an existing block).
/// - `Site` (lotgeneratesite) → general + streets only (road network).
fn groups_for_kind(kind: itsjustcad_doc::SubdivKind) -> &'static [&'static str] {
    use itsjustcad_doc::SubdivKind;
    match kind {
        SubdivKind::Lots => &[
            "param.subdivision.group.general",
            "param.subdivision.group.lots",
        ],
        SubdivKind::Site => &[
            "param.subdivision.group.general",
            "param.subdivision.group.streets",
        ],
        SubdivKind::SitePlan => &[
            "param.subdivision.group.general",
            "param.subdivision.group.streets",
            "param.subdivision.group.lots",
            "param.subdivision.group.setbacks",
            "param.subdivision.group.buildings",
            "param.subdivision.group.openspace",
        ],
    }
}

/// The fields for a link kind, partitioned into ordered collapsible groups
/// (CityEngine-style inspector). Empty groups are omitted.
pub fn grouped_fields_for_kind(
    kind: itsjustcad_doc::SubdivKind,
) -> Vec<(&'static str, Vec<ParamField>)> {
    let all = subdivision_fields();
    let by_name = |name: &str| all.iter().find(|f| f.name == name).cloned();
    let visible = groups_for_kind(kind);
    FIELD_GROUPS
        .iter()
        .filter(|(key, _)| visible.contains(key))
        .filter_map(|(key, names)| {
            let fields: Vec<ParamField> = names.iter().filter_map(|n| by_name(n)).collect();
            if fields.is_empty() {
                None
            } else {
                Some((*key, fields))
            }
        })
        .collect()
}

/// The flat list of fields relevant to a given link kind, so the inspector (and
/// callers that don't want groups) never shows irrelevant controls. Derived from
/// [`grouped_fields_for_kind`] so the flat and grouped views always agree.
///
/// A `Lots` link (lotsubdivide) uses general + lot-sizing params; a `Site` link
/// (lotgeneratesite) uses general + road-network params; a `SitePlan` link (the
/// meta plan) drives the whole chain and exposes every phase group.
pub fn fields_for_kind(kind: itsjustcad_doc::SubdivKind) -> Vec<ParamField> {
    grouped_fields_for_kind(kind)
        .into_iter()
        .flat_map(|(_, fields)| fields)
        .collect()
}

/// Project a full [`SubdivisionSettings`] down to the exposed parameter map.
pub fn settings_to_params(s: &SubdivisionSettings) -> ParamMap {
    let mut m = ParamMap::new();
    m.insert("method".into(), ParamValue::Enum(method_token(s.method).into()));
    m.insert("seed".into(), ParamValue::Int(s.seed as i64));
    m.insert("region".into(), ParamValue::Enum(region_token(s.region).into()));
    m.insert("lot_area_min".into(), ParamValue::Float(s.lot_area_min));
    m.insert("lot_area_max".into(), ParamValue::Float(s.lot_area_max));
    m.insert("lot_width_min".into(), ParamValue::Float(s.lot_width_min));
    m.insert("irregularity".into(), ParamValue::Float(s.irregularity));
    m.insert(
        "force_street_access".into(),
        ParamValue::Float(s.force_street_access),
    );
    m.insert("setback_front".into(), ParamValue::Float(s.setback_front));
    m.insert("setback_side".into(), ParamValue::Float(s.setback_side));
    m.insert("setback_rear".into(), ParamValue::Float(s.setback_rear));
    m.insert("road_width".into(), ParamValue::Float(s.road_width));
    m.insert("block_depth".into(), ParamValue::Float(s.block_depth));
    m.insert("offset_width".into(), ParamValue::Float(s.offset_width));
    m.insert("corner_width".into(), ParamValue::Float(s.corner_width));
    m.insert("lot_depth_target".into(), ParamValue::Float(s.lot_depth_target));
    m.insert(
        "lot_depth_tolerance".into(),
        ParamValue::Float(s.lot_depth_tolerance),
    );
    m.insert(
        "corner_lot_width_bonus".into(),
        ParamValue::Float(s.corner_lot_width_bonus),
    );
    m.insert(
        "corner_align".into(),
        ParamValue::Enum(corner_align_token(s.corner_align).into()),
    );
    m.insert("loading".into(), ParamValue::Enum(loading_token(s.loading).into()));
    m.insert("alley_width".into(), ParamValue::Float(s.alley_width));
    m.insert("allow_flag_lots".into(), ParamValue::Bool(s.allow_flag_lots));
    m.insert(
        "flag_pole_width_min".into(),
        ParamValue::Float(s.flag_pole_width_min),
    );
    m.insert("merge_slivers".into(), ParamValue::Bool(s.merge_slivers));
    m.insert("sliver_area_frac".into(), ParamValue::Float(s.sliver_area_frac));
    m.insert("shallow_lot_frac".into(), ParamValue::Float(s.shallow_lot_frac));
    m.insert("subdivide_core".into(), ParamValue::Bool(s.subdivide_core));
    m.insert(
        "skeleton_impl".into(),
        ParamValue::Enum(skeleton_impl_token(s.skeleton_impl).into()),
    );
    m.insert("simplify".into(), ParamValue::Float(s.simplify));
    m.insert(
        "draw_buildable_envelope".into(),
        ParamValue::Bool(s.draw_buildable_envelope),
    );
    // Streets
    m.insert(
        "street_pattern".into(),
        ParamValue::Enum(street_pattern_token(s.street_pattern).into()),
    );
    // Setbacks
    m.insert("build_to_line".into(), ParamValue::Float(s.build_to_line));
    m.insert(
        "frontage_at_setback".into(),
        ParamValue::Bool(s.frontage_at_setback),
    );
    // Buildings
    m.insert("typology".into(), ParamValue::Enum(typology_token(s.typology).into()));
    m.insert(
        "footprint_mode".into(),
        ParamValue::Enum(footprint_mode_token(s.footprint_mode).into()),
    );
    m.insert("coverage_frac".into(), ParamValue::Float(s.coverage_frac));
    m.insert("footprint_inset".into(), ParamValue::Float(s.footprint_inset));
    m.insert("floor_count".into(), ParamValue::Int(s.floor_count as i64));
    m.insert("floor_height".into(), ParamValue::Float(s.floor_height));
    m.insert(
        "stepback_start_floor".into(),
        ParamValue::Int(s.stepback_start_floor as i64),
    );
    m.insert("stepback_depth".into(), ParamValue::Float(s.stepback_depth));
    m.insert("roof_type".into(), ParamValue::Enum(roof_type_token(s.roof_type).into()));
    m.insert("roof_pitch".into(), ParamValue::Float(s.roof_pitch));
    // Open space
    m.insert(
        "open_space_reserve_frac".into(),
        ParamValue::Float(s.open_space_reserve_frac),
    );
    m
}

/// Fold an exposed parameter map back onto `base`, leaving every non-exposed
/// field untouched. The map is sanitized first (missing fields filled from
/// defaults, numerics clamped to bounds, bad enums snapped), so the result is
/// always a valid settings struct — deterministic, replay-stable.
pub fn params_to_settings(base: &SubdivisionSettings, params: &ParamMap) -> SubdivisionSettings {
    let p = sanitize_fields(&subdivision_fields(), params);
    let mut s = base.clone();
    if let Some(m) = p.get("method").and_then(|v| v.as_enum()).and_then(method_from_token) {
        s.method = m;
    }
    if let Some(v) = p.get("seed").and_then(|v| v.as_i64()) {
        s.seed = v.max(0) as u64;
    }
    if let Some(v) = p.get("region").and_then(|v| v.as_enum()).and_then(region_from_token) {
        s.region = v;
    }
    if let Some(v) = p.get("lot_area_min").and_then(|v| v.as_f64()) {
        s.lot_area_min = v;
    }
    if let Some(v) = p.get("lot_area_max").and_then(|v| v.as_f64()) {
        s.lot_area_max = v;
    }
    if let Some(v) = p.get("lot_width_min").and_then(|v| v.as_f64()) {
        s.lot_width_min = v;
    }
    if let Some(v) = p.get("irregularity").and_then(|v| v.as_f64()) {
        s.irregularity = v;
    }
    if let Some(v) = p.get("force_street_access").and_then(|v| v.as_f64()) {
        s.force_street_access = v;
    }
    if let Some(v) = p.get("setback_front").and_then(|v| v.as_f64()) {
        s.setback_front = v;
    }
    if let Some(v) = p.get("setback_side").and_then(|v| v.as_f64()) {
        s.setback_side = v;
    }
    if let Some(v) = p.get("setback_rear").and_then(|v| v.as_f64()) {
        s.setback_rear = v;
    }
    if let Some(v) = p.get("road_width").and_then(|v| v.as_f64()) {
        s.road_width = v;
    }
    if let Some(v) = p.get("block_depth").and_then(|v| v.as_f64()) {
        s.block_depth = v;
    }
    if let Some(v) = p.get("offset_width").and_then(|v| v.as_f64()) {
        s.offset_width = v;
    }
    if let Some(v) = p.get("corner_width").and_then(|v| v.as_f64()) {
        s.corner_width = v;
    }
    if let Some(v) = p.get("lot_depth_target").and_then(|v| v.as_f64()) {
        s.lot_depth_target = v;
    }
    if let Some(v) = p.get("lot_depth_tolerance").and_then(|v| v.as_f64()) {
        s.lot_depth_tolerance = v;
    }
    if let Some(v) = p.get("corner_lot_width_bonus").and_then(|v| v.as_f64()) {
        s.corner_lot_width_bonus = v;
    }
    if let Some(v) = p
        .get("corner_align")
        .and_then(|v| v.as_enum())
        .and_then(corner_align_from_token)
    {
        s.corner_align = v;
    }
    if let Some(v) = p.get("loading").and_then(|v| v.as_enum()).and_then(loading_from_token) {
        s.loading = v;
    }
    if let Some(v) = p.get("alley_width").and_then(|v| v.as_f64()) {
        s.alley_width = v;
    }
    if let Some(v) = p.get("allow_flag_lots").and_then(|v| v.as_bool()) {
        s.allow_flag_lots = v;
    }
    if let Some(v) = p.get("flag_pole_width_min").and_then(|v| v.as_f64()) {
        s.flag_pole_width_min = v;
    }
    if let Some(v) = p.get("merge_slivers").and_then(|v| v.as_bool()) {
        s.merge_slivers = v;
    }
    if let Some(v) = p.get("sliver_area_frac").and_then(|v| v.as_f64()) {
        s.sliver_area_frac = v;
    }
    if let Some(v) = p.get("shallow_lot_frac").and_then(|v| v.as_f64()) {
        s.shallow_lot_frac = v;
    }
    if let Some(v) = p.get("subdivide_core").and_then(|v| v.as_bool()) {
        s.subdivide_core = v;
    }
    if let Some(v) = p
        .get("skeleton_impl")
        .and_then(|v| v.as_enum())
        .and_then(skeleton_impl_from_token)
    {
        s.skeleton_impl = v;
    }
    if let Some(v) = p.get("simplify").and_then(|v| v.as_f64()) {
        s.simplify = v;
    }
    if let Some(v) = p.get("draw_buildable_envelope").and_then(|v| v.as_bool()) {
        s.draw_buildable_envelope = v;
    }
    // Streets
    if let Some(v) = p
        .get("street_pattern")
        .and_then(|v| v.as_enum())
        .and_then(street_pattern_from_token)
    {
        s.street_pattern = v;
    }
    // Setbacks
    if let Some(v) = p.get("build_to_line").and_then(|v| v.as_f64()) {
        s.build_to_line = v;
    }
    if let Some(v) = p.get("frontage_at_setback").and_then(|v| v.as_bool()) {
        s.frontage_at_setback = v;
    }
    // Buildings
    if let Some(v) = p.get("typology").and_then(|v| v.as_enum()).and_then(typology_from_token) {
        s.typology = v;
    }
    if let Some(v) = p
        .get("footprint_mode")
        .and_then(|v| v.as_enum())
        .and_then(footprint_mode_from_token)
    {
        s.footprint_mode = v;
    }
    if let Some(v) = p.get("coverage_frac").and_then(|v| v.as_f64()) {
        s.coverage_frac = v;
    }
    if let Some(v) = p.get("footprint_inset").and_then(|v| v.as_f64()) {
        s.footprint_inset = v;
    }
    if let Some(v) = p.get("floor_count").and_then(|v| v.as_i64()) {
        s.floor_count = v.max(0) as usize;
    }
    if let Some(v) = p.get("floor_height").and_then(|v| v.as_f64()) {
        s.floor_height = v;
    }
    if let Some(v) = p.get("stepback_start_floor").and_then(|v| v.as_i64()) {
        s.stepback_start_floor = v.max(0) as usize;
    }
    if let Some(v) = p.get("stepback_depth").and_then(|v| v.as_f64()) {
        s.stepback_depth = v;
    }
    if let Some(v) = p.get("roof_type").and_then(|v| v.as_enum()).and_then(roof_type_from_token) {
        s.roof_type = v;
    }
    if let Some(v) = p.get("roof_pitch").and_then(|v| v.as_f64()) {
        s.roof_pitch = v;
    }
    // Open space
    if let Some(v) = p.get("open_space_reserve_frac").and_then(|v| v.as_f64()) {
        s.open_space_reserve_frac = v;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_for_kind_filters_by_link_kind() {
        use itsjustcad_doc::SubdivKind;
        let names = |k| {
            fields_for_kind(k)
                .into_iter()
                .map(|f| f.name)
                .collect::<Vec<_>>()
        };
        let lots = names(SubdivKind::Lots);
        let site = names(SubdivKind::Site);
        // Lots: has lot sizing, no road-network params.
        assert!(lots.contains(&"lot_area_min"));
        assert!(!lots.contains(&"road_width"));
        assert!(!lots.contains(&"block_depth"));
        // Site: road-network params + seed only, no lot sizing/setbacks.
        assert!(site.contains(&"road_width"));
        assert!(site.contains(&"block_depth"));
        assert!(site.contains(&"seed"));
        assert!(!site.contains(&"lot_area_min"));
        assert!(!site.contains(&"setback_front"));
    }

    #[test]
    fn grouped_fields_cover_expected_groups() {
        use itsjustcad_doc::SubdivKind;
        let keys = |k| {
            grouped_fields_for_kind(k)
                .into_iter()
                .map(|(key, _)| key)
                .collect::<Vec<_>>()
        };
        // SitePlan exposes all six phase groups.
        let plan = keys(SubdivKind::SitePlan);
        for g in [
            "param.subdivision.group.general",
            "param.subdivision.group.streets",
            "param.subdivision.group.lots",
            "param.subdivision.group.setbacks",
            "param.subdivision.group.buildings",
            "param.subdivision.group.openspace",
        ] {
            assert!(plan.contains(&g), "SitePlan missing group {g}");
        }
        // Site → general + streets only.
        assert_eq!(
            keys(SubdivKind::Site),
            vec![
                "param.subdivision.group.general",
                "param.subdivision.group.streets",
            ]
        );
        // Lots → general + lots only.
        assert_eq!(
            keys(SubdivKind::Lots),
            vec![
                "param.subdivision.group.general",
                "param.subdivision.group.lots",
            ]
        );
        // Flat view agrees with the flattened grouped view.
        let flat: Vec<_> = fields_for_kind(SubdivKind::SitePlan)
            .into_iter()
            .map(|f| f.name)
            .collect();
        let grouped_flat: Vec<_> = grouped_fields_for_kind(SubdivKind::SitePlan)
            .into_iter()
            .flat_map(|(_, fs)| fs)
            .map(|f| f.name)
            .collect();
        assert_eq!(flat, grouped_flat);
    }

    #[test]
    fn phase_params_round_trip() {
        // An enum field (typology) and a usize numeric (floor_count) survive the
        // settings → params → settings round trip, plus a few neighbours.
        let s = SubdivisionSettings {
            street_pattern: StreetPattern::Organic,
            typology: Typology::Courtyard,
            footprint_mode: FootprintMode::CoverageRatio,
            roof_type: RoofType::Hip,
            coverage_frac: 0.42,
            floor_count: 7,
            floor_height: 3.5,
            stepback_start_floor: 4,
            stepback_depth: 1.5,
            roof_pitch: 22.0,
            build_to_line: 6.0,
            open_space_reserve_frac: 0.2,
            ..SubdivisionSettings::default()
        };
        let params = settings_to_params(&s);
        let back = params_to_settings(&SubdivisionSettings::default(), &params);
        assert_eq!(back.street_pattern, StreetPattern::Organic);
        assert_eq!(back.typology, Typology::Courtyard);
        assert_eq!(back.footprint_mode, FootprintMode::CoverageRatio);
        assert_eq!(back.roof_type, RoofType::Hip);
        assert_eq!(back.coverage_frac, 0.42);
        assert_eq!(back.floor_count, 7);
        assert_eq!(back.floor_height, 3.5);
        assert_eq!(back.stepback_start_floor, 4);
        assert_eq!(back.stepback_depth, 1.5);
        assert_eq!(back.roof_pitch, 22.0);
        assert_eq!(back.build_to_line, 6.0);
        assert_eq!(back.open_space_reserve_frac, 0.2);
    }

    #[test]
    fn issue25_enum_tokens_round_trip() {
        for r in [RegionProfile::EuroLatam, RegionProfile::UsSuburban] {
            assert_eq!(region_from_token(region_token(r)), Some(r));
        }
        // Verb aliases also parse.
        assert_eq!(region_from_token("metric"), Some(RegionProfile::EuroLatam));
        assert_eq!(region_from_token("us"), Some(RegionProfile::UsSuburban));
        assert_eq!(region_from_token("bogus"), None);

        for l in [
            LoadingType::FrontLoaded,
            LoadingType::AlleyLoaded,
            LoadingType::Mixed,
        ] {
            assert_eq!(loading_from_token(loading_token(l)), Some(l));
        }
        assert_eq!(loading_from_token("frontloaded"), Some(LoadingType::FrontLoaded));
        assert_eq!(loading_from_token("alleyloaded"), Some(LoadingType::AlleyLoaded));
        assert_eq!(loading_from_token("bogus"), None);

        for c in [CornerAlignment::StreetWidth, CornerAlignment::StreetLength] {
            assert_eq!(corner_align_from_token(corner_align_token(c)), Some(c));
        }
        assert_eq!(corner_align_from_token("width"), Some(CornerAlignment::StreetWidth));
        assert_eq!(corner_align_from_token("length"), Some(CornerAlignment::StreetLength));
        assert_eq!(corner_align_from_token("bogus"), None);

        for i in [SkeletonImpl::Offset, SkeletonImpl::Felkel] {
            assert_eq!(skeleton_impl_from_token(skeleton_impl_token(i)), Some(i));
        }
        assert_eq!(skeleton_impl_from_token("exact"), Some(SkeletonImpl::Felkel));
        assert_eq!(skeleton_impl_from_token("bogus"), None);
    }

    #[test]
    fn issue25_fields_round_trip() {
        // Every field newly exposed for issue 25 survives settings → params →
        // settings, including the non-default enum variants.
        let s = SubdivisionSettings {
            region: RegionProfile::UsSuburban,
            force_street_access: 0.5,
            lot_depth_tolerance: 4.0,
            corner_lot_width_bonus: 0.15,
            corner_align: CornerAlignment::StreetLength,
            loading: LoadingType::AlleyLoaded,
            allow_flag_lots: true,
            flag_pole_width_min: 6.0,
            sliver_area_frac: 0.25,
            shallow_lot_frac: 0.4,
            subdivide_core: false,
            skeleton_impl: SkeletonImpl::Felkel,
            simplify: 1.5,
            frontage_at_setback: false,
            footprint_inset: 3.5,
            ..SubdivisionSettings::default()
        };
        let params = settings_to_params(&s);
        let back = params_to_settings(&SubdivisionSettings::default(), &params);
        assert_eq!(back.region, RegionProfile::UsSuburban);
        assert_eq!(back.force_street_access, 0.5);
        assert_eq!(back.lot_depth_tolerance, 4.0);
        assert_eq!(back.corner_lot_width_bonus, 0.15);
        assert_eq!(back.corner_align, CornerAlignment::StreetLength);
        assert_eq!(back.loading, LoadingType::AlleyLoaded);
        assert!(back.allow_flag_lots);
        assert_eq!(back.flag_pole_width_min, 6.0);
        assert_eq!(back.sliver_area_frac, 0.25);
        assert_eq!(back.shallow_lot_frac, 0.4);
        assert!(!back.subdivide_core);
        assert_eq!(back.skeleton_impl, SkeletonImpl::Felkel);
        assert_eq!(back.simplify, 1.5);
        assert!(!back.frontage_at_setback);
        assert_eq!(back.footprint_inset, 3.5);
    }

    #[test]
    fn issue25_fields_land_in_expected_groups() {
        use itsjustcad_doc::SubdivKind;
        let group_of = |field: &str| {
            grouped_fields_for_kind(SubdivKind::SitePlan)
                .into_iter()
                .find(|(_, fs)| fs.iter().any(|f| f.name == field))
                .map(|(key, _)| key)
        };
        assert_eq!(group_of("region"), Some("param.subdivision.group.general"));
        for f in [
            "force_street_access",
            "lot_depth_tolerance",
            "corner_lot_width_bonus",
            "corner_align",
            "loading",
            "allow_flag_lots",
            "flag_pole_width_min",
            "sliver_area_frac",
            "shallow_lot_frac",
            "subdivide_core",
            "skeleton_impl",
            "simplify",
        ] {
            assert_eq!(group_of(f), Some("param.subdivision.group.lots"), "{f}");
        }
        assert_eq!(
            group_of("frontage_at_setback"),
            Some("param.subdivision.group.setbacks")
        );
        assert_eq!(
            group_of("footprint_inset"),
            Some("param.subdivision.group.buildings")
        );
        // width_mix stays unexposed (no widget for an Option<LotWidthMix>).
        assert_eq!(group_of("width_mix"), None);
    }

    #[test]
    fn method_token_round_trips() {
        for m in [
            SubdivisionMethod::Recursive,
            SubdivisionMethod::Offset,
            SubdivisionMethod::Skeleton,
        ] {
            assert_eq!(method_from_token(method_token(m)), Some(m));
        }
        // Verb aliases also parse.
        assert_eq!(method_from_token("recursive"), Some(SubdivisionMethod::Recursive));
        assert_eq!(method_from_token("offset"), Some(SubdivisionMethod::Offset));
        assert_eq!(method_from_token("skeleton"), Some(SubdivisionMethod::Skeleton));
        assert_eq!(method_from_token("bogus"), None);
    }

    #[test]
    fn settings_params_round_trip_exposed_fields() {
        let mut s = SubdivisionSettings {
            method: SubdivisionMethod::Offset,
            seed: 7,
            lot_area_min: 1234.0,
            lot_width_min: 42.0,
            irregularity: 0.3,
            setback_front: 9.0,
            road_width: 15.0,
            ..SubdivisionSettings::default()
        };
        // An un-exposed field: must survive a round trip untouched.
        s.coverage_frac = 0.77;

        let params = settings_to_params(&s);
        let back = params_to_settings(&SubdivisionSettings::default(), &params);
        assert_eq!(back.method, SubdivisionMethod::Offset);
        assert_eq!(back.seed, 7);
        assert_eq!(back.lot_area_min, 1234.0);
        assert_eq!(back.lot_width_min, 42.0);
        assert_eq!(back.irregularity, 0.3);
        assert_eq!(back.setback_front, 9.0);
        assert_eq!(back.road_width, 15.0);
    }

    #[test]
    fn params_to_settings_preserves_unexposed_fields() {
        let base = SubdivisionSettings {
            coverage_frac: 0.33,
            floor_count: 5,
            ..SubdivisionSettings::default()
        };
        // Only change one exposed param.
        let mut params = settings_to_params(&base);
        params.insert("lot_area_min".into(), ParamValue::Float(2000.0));
        let out = params_to_settings(&base, &params);
        assert_eq!(out.lot_area_min, 2000.0);
        // Un-exposed fields are carried through from base.
        assert_eq!(out.coverage_frac, 0.33);
        assert_eq!(out.floor_count, 5);
    }

    #[test]
    fn new_fields_round_trip() {
        let s = SubdivisionSettings {
            offset_width: 250.0,
            corner_width: 12.0,
            lot_depth_target: 30.0,
            alley_width: 8.5,
            merge_slivers: false,
            draw_buildable_envelope: false,
            ..SubdivisionSettings::default()
        };
        let params = settings_to_params(&s);
        let back = params_to_settings(&SubdivisionSettings::default(), &params);
        assert_eq!(back.offset_width, 250.0);
        assert_eq!(back.corner_width, 12.0);
        assert_eq!(back.lot_depth_target, 30.0);
        assert_eq!(back.alley_width, 8.5);
        assert!(!back.merge_slivers);
        assert!(!back.draw_buildable_envelope);
    }

    #[test]
    fn sanitize_clamps_out_of_range_and_fills_missing() {
        // irregularity is clamped to [0, 1]; a missing field takes its default.
        let mut params = ParamMap::new();
        params.insert("irregularity".into(), ParamValue::Float(5.0)); // over max
        params.insert("lot_width_min".into(), ParamValue::Float(-3.0)); // under min
        let out = params_to_settings(&SubdivisionSettings::default(), &params);
        assert_eq!(out.irregularity, 1.0, "clamped to max");
        assert_eq!(out.lot_width_min, 1.0, "clamped to min");
        // A field absent from the map falls back to the schema default (5000).
        assert_eq!(out.lot_area_min, 5000.0);
    }

    #[test]
    fn bad_method_token_snaps_to_default() {
        let mut params = settings_to_params(&SubdivisionSettings::default());
        params.insert("method".into(), ParamValue::Enum("nonsense".into()));
        let out = params_to_settings(&SubdivisionSettings::default(), &params);
        // Snapped to the schema default ("grid" → Recursive).
        assert_eq!(out.method, SubdivisionMethod::Recursive);
    }
}
