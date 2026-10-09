// SPDX-License-Identifier: AGPL-3.0-or-later
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
use subdivision::{SubdivisionMethod, SubdivisionSettings};

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
            "alley_width",
            "param.subdivision.alley_width",
            20.0,
            Some(0.0),
            Some(200.0),
            0.5,
            Widget::Numeric,
            Unit::Meter,
        ),
        ParamField::bool("merge_slivers", "param.subdivision.merge_slivers", true),
        ParamField::bool(
            "draw_buildable_envelope",
            "param.subdivision.draw_buildable_envelope",
            true,
        ),
    ]
}

/// The subset of [`subdivision_fields`] relevant to a given link kind, so the
/// inspector never shows irrelevant controls. A `Lots` link (lotsubdivide) never
/// uses the road-network params (`road_width`/`block_depth`); a `Site` link
/// (lotgeneratesite) uses only those road-network params + `seed` — the lot
/// sizing / setback params don't affect `generate_site`.
pub fn fields_for_kind(kind: itsjustcad_doc::SubdivKind) -> Vec<ParamField> {
    subdivision_fields()
        .into_iter()
        .filter(|f| field_applies(f.name, kind))
        .collect()
}

/// Road-network params that only apply to `lotgeneratesite` (Site links).
const SITE_ONLY_FIELDS: &[&str] = &["road_width", "block_depth"];

fn field_applies(name: &str, kind: itsjustcad_doc::SubdivKind) -> bool {
    use itsjustcad_doc::SubdivKind;
    match kind {
        SubdivKind::Lots => !SITE_ONLY_FIELDS.contains(&name),
        SubdivKind::Site => name == "seed" || SITE_ONLY_FIELDS.contains(&name),
    }
}

/// Project a full [`SubdivisionSettings`] down to the exposed parameter map.
pub fn settings_to_params(s: &SubdivisionSettings) -> ParamMap {
    let mut m = ParamMap::new();
    m.insert("method".into(), ParamValue::Enum(method_token(s.method).into()));
    m.insert("seed".into(), ParamValue::Int(s.seed as i64));
    m.insert("lot_area_min".into(), ParamValue::Float(s.lot_area_min));
    m.insert("lot_area_max".into(), ParamValue::Float(s.lot_area_max));
    m.insert("lot_width_min".into(), ParamValue::Float(s.lot_width_min));
    m.insert("irregularity".into(), ParamValue::Float(s.irregularity));
    m.insert("setback_front".into(), ParamValue::Float(s.setback_front));
    m.insert("setback_side".into(), ParamValue::Float(s.setback_side));
    m.insert("setback_rear".into(), ParamValue::Float(s.setback_rear));
    m.insert("road_width".into(), ParamValue::Float(s.road_width));
    m.insert("block_depth".into(), ParamValue::Float(s.block_depth));
    m.insert("offset_width".into(), ParamValue::Float(s.offset_width));
    m.insert("corner_width".into(), ParamValue::Float(s.corner_width));
    m.insert("lot_depth_target".into(), ParamValue::Float(s.lot_depth_target));
    m.insert("alley_width".into(), ParamValue::Float(s.alley_width));
    m.insert("merge_slivers".into(), ParamValue::Bool(s.merge_slivers));
    m.insert(
        "draw_buildable_envelope".into(),
        ParamValue::Bool(s.draw_buildable_envelope),
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
    if let Some(v) = p.get("alley_width").and_then(|v| v.as_f64()) {
        s.alley_width = v;
    }
    if let Some(v) = p.get("merge_slivers").and_then(|v| v.as_bool()) {
        s.merge_slivers = v;
    }
    if let Some(v) = p.get("draw_buildable_envelope").and_then(|v| v.as_bool()) {
        s.draw_buildable_envelope = v;
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
        let mut base = SubdivisionSettings::default();
        base.coverage_frac = 0.33;
        base.floor_count = 5;
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
