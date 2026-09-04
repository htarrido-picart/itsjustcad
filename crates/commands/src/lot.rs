// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! `lot*` verbs (M-intemfit) — the bridge from document curves to the pure
//! `subdivision` crate and back. Dependency direction: `subdivision` (leaf) ←
//! `commands`. All conversion (doc `Curve` ↔ `Polygon2d`) lives here; the pure
//! crate never sees a document.
//!
//! Phases 2–4 ship two verbs:
//! - `lotsubdivide` — subdivide selected closed block curve(s) into lots on the
//!   `lots` layer. `method=grid` (recursive OBB, Phase 3) and `method=perimeter`
//!   (offset/perimeter, Phase 4) are implemented; `streetfollowing` returns a
//!   clear "not yet implemented" error (Phase 7).
//! - `lotsettings` — show or set the sticky `SubdivisionSettings` on the doc.
//!
//! Results bake as a logged op with written-back ids (contours/landscape
//! precedent) so undo is one `CreatedOnLayer` inverse and op-log replay recreates
//! byte-identical lots (the subdivider is deterministic for a fixed seed).

use glam::{DVec2, DVec3};
use itsjustcad_doc::{Document, Geometry, LayerStyle, ObjectId, SceneObject};
use kernel_curve::Curve;
use subdivision::{Polygon2d, SubdivisionMethod, SubdivisionSettings};

/// Chord tolerance for tessellating a block boundary curve to a polygon.
const BLOCK_TOL: f64 = 0.01;

/// The layer baked lots land on.
pub const LOTS_LAYER: &str = "lots";

/// Convert a closed document `Curve` to a `Polygon2d` (XY projection; z dropped).
pub fn curve_to_polygon(curve: &Curve) -> Option<Polygon2d> {
    if !curve.is_closed() {
        return None;
    }
    let pts3 = curve.tessellate(BLOCK_TOL);
    Polygon2d::new(pts3.iter().map(|p| DVec2::new(p.x, p.y)).collect())
}

/// Convert a `Polygon2d` back to a closed document `Curve` at elevation `z`.
pub fn polygon_to_curve(poly: &Polygon2d, z: f64) -> Curve {
    Curve::Polyline {
        points: poly.verts().iter().map(|v| DVec3::new(v.x, v.y, z)).collect(),
        closed: true,
    }
}

/// A subdivision method string (`grid`/`perimeter`/`streetfollowing`) → enum.
pub fn parse_method(s: &str) -> Option<SubdivisionMethod> {
    match s.to_lowercase().as_str() {
        "grid" | "recursive" => Some(SubdivisionMethod::Recursive),
        "perimeter" | "offset" => Some(SubdivisionMethod::Offset),
        "streetfollowing" | "skeleton" => Some(SubdivisionMethod::Skeleton),
        _ => None,
    }
}

/// The result of subdividing one or more blocks: the flat list of lot polygons
/// with their source z, ready to bake.
#[derive(Debug)]
pub struct LotBake {
    pub polygons: Vec<Polygon2d>,
    pub z: f64,
    pub with_street: usize,
}

/// Core Phase-3 bridge: run recursive-OBB subdivision on `blocks` (already
/// tessellated + validated closed) with `settings`. Errors for unimplemented
/// methods. Deterministic — output depends only on the blocks + settings.
pub fn subdivide_blocks(
    blocks: &[(Polygon2d, f64)],
    settings: &SubdivisionSettings,
) -> Result<LotBake, String> {
    match settings.method {
        SubdivisionMethod::Recursive | SubdivisionMethod::Offset => {}
        SubdivisionMethod::Skeleton => {
            return Err(
                "lotsubdivide method=streetfollowing is not yet implemented (Phase 7 — \
                 skeleton subdivision). Use method=grid."
                    .into(),
            );
        }
    }

    let mut polygons = Vec::new();
    let mut with_street = 0usize;
    let mut z_acc = 0.0;
    let mut z_n = 0usize;
    for (block, z) in blocks {
        let lots = match settings.method {
            SubdivisionMethod::Offset => subdivision::subdivide_offset(block, settings),
            _ => subdivision::subdivide(block, settings),
        };
        for lot in lots {
            if lot.has_street {
                with_street += 1;
            }
            polygons.push(lot.polygon);
        }
        z_acc += *z;
        z_n += 1;
    }
    if polygons.is_empty() {
        return Err("subdivision produced no lots (block too small for the given lot_area_min / \
                    lot_width_min?)"
            .into());
    }
    let z = if z_n > 0 { z_acc / z_n as f64 } else { 0.0 };
    Ok(LotBake { polygons, z, with_street })
}

/// Ensure the `lots` layer exists; returns `Some(name)` if it was newly created.
pub fn ensure_lots_layer(doc: &mut Document) -> Option<String> {
    if doc.layers.contains_key(LOTS_LAYER) {
        return None;
    }
    doc.layers.insert(
        LOTS_LAYER.to_string(),
        LayerStyle {
            // A muted surveyor's magenta for lot lines.
            color: Some([0.72, 0.30, 0.55, 1.0]),
            ..LayerStyle::default()
        },
    );
    Some(LOTS_LAYER.to_string())
}

/// Insert baked lot curves onto the `lots` layer with the given ids.
pub fn insert_lots(doc: &mut Document, bake: &LotBake, ids: &[ObjectId]) {
    for (poly, id) in bake.polygons.iter().zip(ids) {
        doc.insert(SceneObject {
            visible: true,
            id: *id,
            name: Some("lot".to_string()),
            layer: LOTS_LAYER.to_string(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Curve(polygon_to_curve(poly, bake.z)),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect_curve(w: f64, h: f64) -> Curve {
        Curve::Polyline {
            points: vec![
                DVec3::new(0.0, 0.0, 0.0),
                DVec3::new(w, 0.0, 0.0),
                DVec3::new(w, h, 0.0),
                DVec3::new(0.0, h, 0.0),
            ],
            closed: true,
        }
    }

    #[test]
    fn curve_roundtrip_area() {
        let c = rect_curve(400.0, 120.0);
        let poly = curve_to_polygon(&c).unwrap();
        assert!((poly.area() - 48000.0).abs() < 1e-6);
        let back = polygon_to_curve(&poly, 0.0);
        assert!(back.is_closed());
    }

    #[test]
    fn open_curve_rejected() {
        let c = Curve::Polyline {
            points: vec![DVec3::ZERO, DVec3::new(10.0, 0.0, 0.0)],
            closed: false,
        };
        assert!(curve_to_polygon(&c).is_none());
    }

    #[test]
    fn grid_method_subdivides() {
        let poly = curve_to_polygon(&rect_curve(400.0, 120.0)).unwrap();
        let s = SubdivisionSettings {
            method: SubdivisionMethod::Recursive,
            lot_area_min: 4000.0,
            lot_width_min: 30.0,
            ..SubdivisionSettings::default()
        };
        let bake = subdivide_blocks(&[(poly.clone(), 0.0)], &s).unwrap();
        assert!(bake.polygons.len() > 1);
        let sum: f64 = bake.polygons.iter().map(|p| p.area()).sum();
        assert!((sum - poly.area()).abs() / poly.area() < 1e-6);
    }

    #[test]
    fn perimeter_method_runs_and_conserves_area() {
        // Phase 4: method=perimeter now runs (no longer the deferral error).
        let poly = curve_to_polygon(&rect_curve(400.0, 300.0)).unwrap();
        let s = SubdivisionSettings {
            method: SubdivisionMethod::Offset,
            offset_width: 30.0,
            subdivide_core: true,
            lot_area_min: 4000.0,
            lot_width_min: 20.0,
            force_street_access: 0.0,
            seed: 7,
            ..SubdivisionSettings::default()
        };
        let bake = subdivide_blocks(&[(poly.clone(), 0.0)], &s).unwrap();
        assert!(bake.polygons.len() > 1, "expected multiple perimeter lots");
        let sum: f64 = bake.polygons.iter().map(|p| p.area()).sum();
        assert!((sum - poly.area()).abs() / poly.area() < 1e-3);
    }

    #[test]
    fn perimeter_zero_offset_falls_back() {
        // offset_width ≈ 0 → falls back to recursive OBB, not an error.
        let poly = curve_to_polygon(&rect_curve(400.0, 300.0)).unwrap();
        let s = SubdivisionSettings {
            method: SubdivisionMethod::Offset,
            offset_width: 0.0,
            lot_area_min: 4000.0,
            lot_width_min: 20.0,
            ..SubdivisionSettings::default()
        };
        let bake = subdivide_blocks(&[(poly.clone(), 0.0)], &s).unwrap();
        let sum: f64 = bake.polygons.iter().map(|p| p.area()).sum();
        assert!((sum - poly.area()).abs() / poly.area() < 1e-3);
    }

    #[test]
    fn streetfollowing_method_errors_cleanly() {
        let poly = curve_to_polygon(&rect_curve(400.0, 120.0)).unwrap();
        let s = SubdivisionSettings {
            method: SubdivisionMethod::Skeleton,
            ..SubdivisionSettings::default()
        };
        let err = subdivide_blocks(&[(poly, 0.0)], &s).unwrap_err();
        assert!(err.contains("Phase 7"));
    }

    #[test]
    fn parse_method_aliases() {
        assert_eq!(parse_method("grid"), Some(SubdivisionMethod::Recursive));
        assert_eq!(parse_method("perimeter"), Some(SubdivisionMethod::Offset));
        assert_eq!(
            parse_method("streetfollowing"),
            Some(SubdivisionMethod::Skeleton)
        );
        assert_eq!(parse_method("bogus"), None);
    }
}
