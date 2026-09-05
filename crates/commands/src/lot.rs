// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! `lot*` verbs (M-intemfit) — the bridge from document curves to the pure
//! `subdivision` crate and back. Dependency direction: `subdivision` (leaf) ←
//! `commands`. All conversion (doc `Curve` ↔ `Polygon2d`) lives here; the pure
//! crate never sees a document.
//!
//! Phases 2–4 ship two verbs:
//! - `lotsubdivide` — subdivide selected closed block curve(s) into lots on the
//!   `lots` layer. `method=grid` (recursive OBB, Phase 3), `method=perimeter`
//!   (offset/perimeter, Phase 4), and `method=streetfollowing` (skeleton /
//!   street-following, Phase 7 — perpendicular-to-curve lot lines) are all
//!   implemented.
//! - `lotsettings` — show or set the sticky `SubdivisionSettings` on the doc.
//!
//! Results bake as a logged op with written-back ids (contours/landscape
//! precedent) so undo is one `CreatedOnLayer` inverse and op-log replay recreates
//! byte-identical lots (the subdivider is deterministic for a fixed seed).

use glam::{DVec2, DVec3};
use itsjustcad_doc::{Document, Geometry, LayerStyle, ObjectId, SceneObject};
use kernel_curve::Curve;
use subdivision::{
    Block, Polygon2d, StreetPattern, SubdivisionMethod, SubdivisionSettings,
};

/// Chord tolerance for tessellating a block boundary curve to a polygon.
const BLOCK_TOL: f64 = 0.01;

/// The layer baked lots land on.
pub const LOTS_LAYER: &str = "lots";

/// The layer baked road centerlines land on (`lotgeneratesite`).
pub const ROADS_LAYER: &str = "roads";

/// The layer baked blocks land on (`lotgeneratesite`).
pub const BLOCKS_LAYER: &str = "blocks";

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
    /// Lot-rules (Phase 6) summary: total slivers merged + corners widened.
    pub slivers_merged: usize,
    pub corners_widened: usize,
    /// The euro_latam placeholder banner, if any rule fell back to defaults.
    pub placeholder_note: Option<String>,
    /// Width-mix proportion error achieved (`None` if width-mix not run).
    pub width_mix_error: Option<f64>,
}

/// Dispatch one block to the subdivider selected by `settings.method`.
/// `tagged` carries the block's street tags (Phase 5) for the skeleton method;
/// for a plain `lotsubdivide` on an arbitrary curve it is `Block::untagged`, so
/// the skeleton treats every contour edge as frontage (same as grid/perimeter).
fn subdivide_by_method(
    block: &Polygon2d,
    tagged: &Block,
    settings: &SubdivisionSettings,
) -> Vec<subdivision::Lot> {
    match settings.method {
        SubdivisionMethod::Offset => subdivision::subdivide_offset(block, settings),
        SubdivisionMethod::Skeleton => subdivision::subdivide_skeleton_block(tagged, settings),
        SubdivisionMethod::Recursive => subdivision::subdivide(block, settings),
    }
}

/// Core bridge: run the subdivider selected by `settings.method`
/// (grid/perimeter/streetfollowing) on `blocks` (already tessellated + validated
/// closed) with `settings`. Deterministic — output depends only on the blocks +
/// settings.
pub fn subdivide_blocks(
    blocks: &[(Polygon2d, f64)],
    settings: &SubdivisionSettings,
) -> Result<LotBake, String> {
    // All three methods (grid / perimeter / streetfollowing) are implemented.

    // A width mix turns subdivision into frontage packing (Phase 6). It is
    // opt-in: set via `lotsubdivide widthmix=...` / `lotsettings`, never forced
    // on a plain `grid` run — so the base recursive/offset behaviour is
    // unchanged unless the user asks for a mix. (The euro_latam default mix is a
    // placeholder surfaced only once a mix is actually requested.) Skeleton
    // subdivision drives its own perpendicular slicing, so width-mix packing does
    // not override it either.
    let use_width_mix = settings.width_mix.is_some()
        && settings.method == SubdivisionMethod::Recursive;

    let mut polygons = Vec::new();
    let mut with_street = 0usize;
    let mut slivers_merged = 0usize;
    let mut corners_widened = 0usize;
    let mut placeholder_note: Option<String> = None;
    let mut width_mix_error: Option<f64> = None;
    let mut z_acc = 0.0;
    let mut z_n = 0usize;
    for (block, z) in blocks {
        let tagged = Block::untagged(block.clone());
        // Base lots: width-mix frontage packing when requested + it applies,
        // else the recursive/offset subdivider.
        let base_lots = if use_width_mix {
            match subdivision::subdivision::lot_rules::subdivide_width_mix(&tagged, settings) {
                Some((lots, err, _ph)) if !lots.is_empty() => {
                    width_mix_error = Some(width_mix_error.map_or(err, |e: f64| e.max(err)));
                    lots
                }
                // Frontage too short / no usable street → fall back to the method.
                _ => subdivide_by_method(block, &tagged, settings),
            }
        } else {
            subdivide_by_method(block, &tagged, settings)
        };

        // Lot-rules post-pass: corner widening + sliver merge (+ placeholder note).
        let (lots, report) = subdivision::apply_lot_rules(&tagged, base_lots, settings);
        slivers_merged += report.slivers_merged;
        corners_widened += report.corners_widened;
        if placeholder_note.is_none() {
            placeholder_note = report.placeholder_banner();
        }
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
    Ok(LotBake {
        polygons,
        z,
        with_street,
        slivers_merged,
        corners_widened,
        placeholder_note,
        width_mix_error,
    })
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

/// A street-pattern string → enum (`lotgeneratesite pattern=`).
pub fn parse_pattern(s: &str) -> Option<StreetPattern> {
    match s.to_lowercase().as_str() {
        "orthogonal" | "ortho" | "grid" => Some(StreetPattern::Orthogonal),
        "skewed" | "skew" | "diagonal" => Some(StreetPattern::Skewed),
        "organic" | "free" | "freeform" => Some(StreetPattern::Organic),
        "culdesac" | "cul-de-sac" | "cul" => Some(StreetPattern::CulDeSac),
        // Phase 5b — owner scope, non-rectilinear.
        "radial" | "circular" | "ring" => Some(StreetPattern::Radial),
        "hexagonal" | "hex" => Some(StreetPattern::Hexagonal),
        "voronoi" | "voro" => Some(StreetPattern::Voronoi),
        _ => None,
    }
}

/// The result of generating a site: road centerline polylines + block boundary
/// polygons, all at elevation `z`, ready to bake.
#[derive(Debug)]
pub struct SiteBake {
    /// Road centerlines as open (or closed, for bulbs) polylines.
    pub roads: Vec<Vec<DVec2>>,
    /// Whether each road centerline is a closed loop (cul-de-sac bulb).
    pub road_closed: Vec<bool>,
    /// Block boundary polygons.
    pub blocks: Vec<Polygon2d>,
    /// How many block edges are street-tagged (frontage) / alley-tagged.
    pub street_edges: usize,
    pub alley_edges: usize,
    pub z: f64,
}

/// Core site-generation bridge: generate roads + blocks for `site` with
/// `settings`. Deterministic — output depends only on the site + settings (seed).
/// Handles all seven patterns: the four rectilinear (Phase 5) plus the three
/// non-rectilinear radial/hexagonal/Voronoi generators (Phase 5b — owner scope).
pub fn generate_site(site: &Polygon2d, z: f64, settings: &SubdivisionSettings) -> Result<SiteBake, String> {
    let graph = subdivision::generate_streets(site, settings);
    if graph.is_empty() {
        return Err(
            "lotgeneratesite produced no roads (site too small for the given blockdepth?)".into(),
        );
    }
    let blocks = subdivision::extract_blocks(site, &graph, settings);
    if blocks.is_empty() {
        return Err("lotgeneratesite produced no blocks".into());
    }

    let mut roads = Vec::new();
    let mut road_closed = Vec::new();
    for s in &graph.streets {
        let closed = s.centerline.len() >= 3
            && s.centerline.first().unwrap().distance(*s.centerline.last().unwrap()) < 1e-6;
        roads.push(s.centerline.clone());
        road_closed.push(closed);
    }
    let street_edges = blocks.iter().map(|b| b.street_edge_count()).sum();
    let alley_edges = blocks.iter().map(|b| b.alley_edge_count()).sum();

    Ok(SiteBake {
        roads,
        road_closed,
        blocks: blocks.into_iter().map(|b| b.polygon).collect(),
        street_edges,
        alley_edges,
        z,
    })
}

/// Ensure a layer exists; returns `Some(name)` if newly created.
fn ensure_layer(doc: &mut Document, name: &str, color: [f32; 4]) -> Option<String> {
    if doc.layers.contains_key(name) {
        return None;
    }
    doc.layers.insert(
        name.to_string(),
        LayerStyle {
            color: Some(color),
            ..LayerStyle::default()
        },
    );
    Some(name.to_string())
}

/// Ensure the `roads` layer exists (a slate grey for centerlines).
pub fn ensure_roads_layer(doc: &mut Document) -> Option<String> {
    ensure_layer(doc, ROADS_LAYER, [0.35, 0.38, 0.42, 1.0])
}

/// Ensure the `blocks` layer exists (a muted olive for block outlines).
pub fn ensure_blocks_layer(doc: &mut Document) -> Option<String> {
    ensure_layer(doc, BLOCKS_LAYER, [0.55, 0.58, 0.30, 1.0])
}

/// Insert baked roads onto the `roads` layer and blocks onto the `blocks` layer
/// with the given ids (roads first, then blocks — the id order the exec records).
pub fn insert_site(doc: &mut Document, bake: &SiteBake, road_ids: &[ObjectId], block_ids: &[ObjectId]) {
    for ((pts, closed), id) in bake.roads.iter().zip(&bake.road_closed).zip(road_ids) {
        doc.insert(SceneObject {
            visible: true,
            id: *id,
            name: Some("road".to_string()),
            layer: ROADS_LAYER.to_string(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Curve(Curve::Polyline {
                points: pts.iter().map(|v| DVec3::new(v.x, v.y, bake.z)).collect(),
                closed: *closed,
            }),
        });
    }
    for (poly, id) in bake.blocks.iter().zip(block_ids) {
        doc.insert(SceneObject {
            visible: true,
            id: *id,
            name: Some("block".to_string()),
            layer: BLOCKS_LAYER.to_string(),
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
    fn streetfollowing_method_runs_and_conserves_area() {
        // Phase 7: method=streetfollowing now runs (skeleton subdivision) rather
        // than returning the deferral error.
        let poly = curve_to_polygon(&rect_curve(400.0, 120.0)).unwrap();
        let s = SubdivisionSettings {
            method: SubdivisionMethod::Skeleton,
            lot_area_min: 3000.0,
            lot_width_min: 30.0,
            merge_slivers: false,
            corner_lot_width_bonus: 0.0,
            width_mix: None,
            region: subdivision::RegionProfile::UsSuburban,
            ..SubdivisionSettings::default()
        };
        let bake = subdivide_blocks(&[(poly.clone(), 0.0)], &s).unwrap();
        assert!(!bake.polygons.is_empty());
        let sum: f64 = bake.polygons.iter().map(|p| p.area()).sum();
        assert!(
            (sum - poly.area()).abs() / poly.area() < 0.02,
            "skeleton conserves area: {sum} vs {}",
            poly.area()
        );
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

    #[test]
    fn parse_pattern_aliases() {
        assert_eq!(parse_pattern("orthogonal"), Some(StreetPattern::Orthogonal));
        assert_eq!(parse_pattern("skew"), Some(StreetPattern::Skewed));
        assert_eq!(parse_pattern("organic"), Some(StreetPattern::Organic));
        assert_eq!(parse_pattern("culdesac"), Some(StreetPattern::CulDeSac));
        // Phase 5b non-rectilinear patterns are now accepted.
        assert_eq!(parse_pattern("radial"), Some(StreetPattern::Radial));
        assert_eq!(parse_pattern("hex"), Some(StreetPattern::Hexagonal));
        assert_eq!(parse_pattern("voronoi"), Some(StreetPattern::Voronoi));
        assert_eq!(parse_pattern("bogus"), None);
    }

    #[test]
    fn generate_site_produces_roads_and_blocks() {
        let poly = curve_to_polygon(&rect_curve(400.0, 300.0)).unwrap();
        let s = SubdivisionSettings {
            street_pattern: StreetPattern::Orthogonal,
            road_width: 12.0,
            block_depth: 60.0,
            seed: 7,
            ..SubdivisionSettings::default()
        };
        let bake = generate_site(&poly, 0.0, &s).unwrap();
        assert!(!bake.roads.is_empty(), "expected roads");
        assert!(!bake.blocks.is_empty(), "expected blocks");
        // Blocks stay inside the site.
        let sum: f64 = bake.blocks.iter().map(|p| p.area()).sum();
        assert!(sum <= poly.area() + 1.0);
        // Street tags survived into the bake (every block fronts a road).
        assert!(bake.street_edges > 0, "blocks should carry street tags");
    }

    #[test]
    fn generate_site_nonrectilinear_patterns_produce_output() {
        // Phase 5b: radial / hexagonal / Voronoi now generate roads + blocks
        // through the shared extractor rather than erroring.
        let poly = curve_to_polygon(&rect_curve(400.0, 300.0)).unwrap();
        for p in [
            StreetPattern::Radial,
            StreetPattern::Hexagonal,
            StreetPattern::Voronoi,
        ] {
            let s = SubdivisionSettings {
                street_pattern: p,
                road_width: 10.0,
                block_depth: 60.0,
                seed: 7,
                ..SubdivisionSettings::default()
            };
            let bake = generate_site(&poly, 0.0, &s)
                .unwrap_or_else(|e| panic!("{p:?} errored: {e}"));
            assert!(!bake.roads.is_empty(), "{p:?}: expected roads");
            assert!(!bake.blocks.is_empty(), "{p:?}: expected blocks");
            let sum: f64 = bake.blocks.iter().map(|p| p.area()).sum();
            assert!(sum <= poly.area() + 1.0, "{p:?}: blocks exceed site");
        }
    }

    #[test]
    fn generated_block_subdivides_with_street_access() {
        // A generated block, fed back to recursive-OBB subdivision, must produce
        // lots that keep street frontage — confirming the tagged block geometry
        // survives into a downstream lotsubdivide (plan Phase 5 requirement).
        let poly = curve_to_polygon(&rect_curve(400.0, 300.0)).unwrap();
        let gs = SubdivisionSettings {
            street_pattern: StreetPattern::Orthogonal,
            road_width: 12.0,
            block_depth: 80.0,
            seed: 7,
            ..SubdivisionSettings::default()
        };
        let bake = generate_site(&poly, 0.0, &gs).unwrap();
        // Take the largest generated block and subdivide it.
        let block = bake
            .blocks
            .iter()
            .max_by(|a, b| a.area().partial_cmp(&b.area()).unwrap())
            .unwrap()
            .clone();
        let ss = SubdivisionSettings {
            method: SubdivisionMethod::Recursive,
            lot_area_min: 1500.0,
            lot_width_min: 15.0,
            force_street_access: 1.0,
            ..SubdivisionSettings::default()
        };
        let out = subdivide_blocks(&[(block, 0.0)], &ss).unwrap();
        assert!(out.polygons.len() >= 1);
        // With force_street_access, every lot keeps a boundary (street) edge.
        assert_eq!(out.with_street, out.polygons.len(), "all lots must front a street");
    }
}
