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

/// The layer baked buildable envelopes land on (`lotsetbacks`) — kept distinct
/// from `lots` so an envelope and its lot are separately selectable / toggleable.
pub const SETBACKS_LAYER: &str = "setbacks";

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

// ── Phase 8: setbacks + buildable envelopes + frontage-at-setback ───────────

use subdivision::{FrontageAt, RegionProfile};

/// The result of computing buildable envelopes for a set of lots: the envelope
/// polygons (only the ones that did not collapse) + counts, ready to bake onto
/// the `setbacks` layer.
#[derive(Debug)]
pub struct SetbackBake {
    pub envelopes: Vec<Polygon2d>,
    pub z: f64,
    /// Lots whose envelope collapsed (setbacks exceeded the lot) — reported.
    pub collapsed: usize,
    /// Whether any envelope pinned its front to the build-to line.
    pub build_to_used: bool,
    /// euro_latam placeholder banner when the run used profile defaults.
    pub placeholder_note: Option<String>,
}

/// Compute the buildable envelope for each lot polygon in `lots` under
/// `settings`. Deterministic. A lot whose envelope collapses is counted, not
/// baked (reported to the user). The `setbacks` are read from `settings`
/// (front/side/rear/build-to), already overridden per-run by the verb.
pub fn compute_setbacks(
    lots: &[(Polygon2d, f64)],
    settings: &SubdivisionSettings,
) -> Result<SetbackBake, String> {
    let mut envelopes = Vec::new();
    let mut collapsed = 0usize;
    let mut build_to_used = false;
    let mut z_acc = 0.0;
    let mut z_n = 0usize;
    for (poly, z) in lots {
        // Preserve any street tags the source curve carries by treating the lot
        // as untagged (a baked lot curve has no tags) — the longest edge is the
        // front, matching the subdivision "usable without a street graph" spirit.
        let block = Block::untagged(poly.clone());
        let env = subdivision::buildable_envelope(&block, settings);
        build_to_used |= env.build_to_used;
        match env.polygon {
            Some(p) => envelopes.push(p),
            None => collapsed += 1,
        }
        z_acc += *z;
        z_n += 1;
    }
    if envelopes.is_empty() {
        return Err(format!(
            "every buildable envelope collapsed ({collapsed} lot(s)) — setbacks exceed the lot \
             size; reduce front/side/rear"
        ));
    }
    // euro_latam placeholder banner: when the profile is EuroLatam the setback
    // numbers are §6b placeholders (front 3 / side 0 / rear 3) pending Manuel.
    let placeholder_note = if settings.region == RegionProfile::EuroLatam {
        Some(
            "using euro_latam setback defaults (placeholder — confirm with Manuel)".to_string(),
        )
    } else {
        None
    };
    let z = if z_n > 0 { z_acc / z_n as f64 } else { 0.0 };
    Ok(SetbackBake {
        envelopes,
        z,
        collapsed,
        build_to_used,
        placeholder_note,
    })
}

/// Ensure the `setbacks` layer exists (a muted teal for buildable envelopes).
pub fn ensure_setbacks_layer(doc: &mut Document) -> Option<String> {
    ensure_layer(doc, SETBACKS_LAYER, [0.25, 0.60, 0.58, 1.0])
}

/// Insert baked envelope curves onto the `setbacks` layer with the given ids.
pub fn insert_setbacks(doc: &mut Document, bake: &SetbackBake, ids: &[ObjectId]) {
    for (poly, id) in bake.envelopes.iter().zip(ids) {
        doc.insert(SceneObject {
            visible: true,
            id: *id,
            name: Some("envelope".to_string()),
            layer: SETBACKS_LAYER.to_string(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Curve(polygon_to_curve(poly, bake.z)),
        });
    }
}

/// Parse a frontage-measurement location string → [`FrontageAt`]. Default (and
/// Manuel's explicit ask) is the setback line.
pub fn parse_frontage_at(s: &str) -> Option<FrontageAt> {
    match s.to_lowercase().as_str() {
        "setback" | "setbackline" | "" => Some(FrontageAt::Setback),
        "curb" | "curbline" | "street" => Some(FrontageAt::Curb),
        _ => None,
    }
}

/// One lot's frontage measurement, ready to fold into an AnalysisReport sample.
pub struct FrontageMeasure {
    pub length: f64,
    /// Front-edge midpoint of the lot (a location the deck can point at).
    pub at: DVec2,
}

/// Measure the frontage of each lot polygon under `at` (default: setback line).
/// Returns one [`FrontageMeasure`] per lot. Deterministic.
pub fn measure_frontage(
    lots: &[Polygon2d],
    at: FrontageAt,
    settings: &SubdivisionSettings,
) -> Vec<FrontageMeasure> {
    lots.iter()
        .map(|poly| {
            let block = Block::untagged(poly.clone());
            let length = subdivision::frontage(&block, at, settings);
            FrontageMeasure { length, at: poly.centroid() }
        })
        .collect()
}

// ── Phase 9: open space — feature placement + blind %-reserve ───────────────

use subdivision::OpenSpaceFeature;

/// The layer baked open-space geometry (parks / greenways / ponds / tree-save /
/// reserved blocks) lands on. Tagged distinctly so a later `lotreport`
/// (Phase 11) can net open space out of gross site area.
pub const OPENSPACE_LAYER: &str = "openspace";

/// The result of an open-space run: the feature/reserved polygons + per-feature
/// labels, ready to bake onto the `openspace` layer, plus a summary the exec
/// surfaces (mode, achieved reserve fraction, advisory note).
#[derive(Debug)]
pub struct OpenSpaceBake {
    /// The open-space polygons to bake.
    pub polygons: Vec<Polygon2d>,
    /// One object-name label per polygon (e.g. `openspace:park`,
    /// `openspace:reserve`).
    pub labels: Vec<String>,
    pub z: f64,
    /// Human summary of what was placed / reserved.
    pub summary: String,
    /// The no-false-precision advisory (design intent, not engineering).
    pub advisory: Option<String>,
}

/// Ensure the `openspace` layer exists (a muted forest green).
pub fn ensure_openspace_layer(doc: &mut Document) -> Option<String> {
    ensure_layer(doc, OPENSPACE_LAYER, [0.30, 0.55, 0.32, 1.0])
}

/// A feature-type keyword → [`OpenSpaceFeature`].
pub fn parse_open_space_feature(s: &str) -> Option<OpenSpaceFeature> {
    OpenSpaceFeature::parse(s)
}

/// Feature-placement mode: place one open-space amenity of `feature` at the
/// given `region` (a selected closed curve or the largest empty block),
/// optionally sized to `area`. `path` is the region's tessellated boundary; for
/// a greenway an explicit open `path` routes the corridor, otherwise the
/// corridor follows the region's long-axis centerline.
pub fn place_feature(
    region: &Polygon2d,
    path: Option<&[DVec2]>,
    feature: OpenSpaceFeature,
    area: Option<f64>,
    z: f64,
) -> Result<OpenSpaceBake, String> {
    let poly = match feature {
        OpenSpaceFeature::PocketPark => subdivision::pocket_park(region, area),
        OpenSpaceFeature::RetentionPond => subdivision::retention_pond(region, area),
        OpenSpaceFeature::TreeSave => subdivision::tree_save(region, area),
        OpenSpaceFeature::Greenway => {
            let route: Vec<DVec2> = match path {
                Some(p) if p.len() >= 2 => p.to_vec(),
                // Route along the region's long-axis centerline (aabb diagonal
                // spine through the centroid).
                _ => long_axis_spine(region),
            };
            subdivision::greenway(&route, None, area)
        }
    };
    let Some(poly) = poly else {
        return Err(format!(
            "could not place a {} in the selected region (region too small for the requested area?)",
            feature.label().trim_start_matches("openspace:")
        ));
    };

    let advisory = Some(open_space_advisory(feature));
    let summary = format!(
        "placed a {} of {:.0} m² on '{}'",
        feature.label().trim_start_matches("openspace:"),
        poly.area(),
        OPENSPACE_LAYER,
    );
    Ok(OpenSpaceBake {
        polygons: vec![poly],
        labels: vec![feature.label().to_string()],
        z,
        summary,
        advisory,
    })
}

/// Blind %-reserve mode: pull whole blocks out of `site` until ~`pct` (0..100)
/// of the site is open, biggest-and-most-central first. Reserved blocks bake as
/// open space (tagged `openspace:reserve`) so a later `lotreport` nets them out
/// and a subsequent `lotsubdivide` on the site's blocks skips them.
pub fn reserve_open_space(
    site: &Polygon2d,
    pct: f64,
    settings: &SubdivisionSettings,
    z: f64,
) -> Result<OpenSpaceBake, String> {
    let frac = (pct / 100.0).clamp(0.0, 0.9);
    // The reserve block set comes from the street generator, which needs a
    // positive block depth + road width. If the sticky settings never set them
    // (defaults leave block_depth = 0), derive a sensible depth from the site so
    // a bare `lotopenspace reserve=20` still produces blocks.
    let mut settings = settings.clone();
    if settings.block_depth <= 0.0 {
        let (lo, hi) = site.aabb();
        let short = (hi.x - lo.x).min(hi.y - lo.y).max(1.0);
        // ~4 blocks across the short dimension, floored so tiny sites still work.
        settings.block_depth = (short / 4.0).max(10.0);
    }
    if settings.road_width <= 0.0 {
        settings.road_width = 12.0;
    }
    let res = subdivision::reserve_blocks(site, frac, &settings);
    if res.reserved.is_empty() {
        return Err(format!(
            "blind reserve={pct}% reserved no blocks (site too small for the given blockdepth, \
             or the generator produced no blocks)"
        ));
    }
    let polygons: Vec<Polygon2d> = res.reserved.iter().map(|b| b.polygon.clone()).collect();
    let labels: Vec<String> = polygons.iter().map(|_| "openspace:reserve".to_string()).collect();
    let summary = format!(
        "reserved {} block(s) as open space on '{}' — {:.1}% of the site (requested {:.0}%)",
        polygons.len(),
        OPENSPACE_LAYER,
        res.achieved_frac() * 100.0,
        pct,
    );
    Ok(OpenSpaceBake {
        polygons,
        labels,
        z,
        summary,
        advisory: Some(
            "reserved blocks are excluded from subdivision and tagged so yield nets them out; \
             this is a planning reservation, not a programmed park"
                .to_string(),
        ),
    })
}

/// The no-false-precision advisory for a placed feature (plan §9 advisory).
fn open_space_advisory(feature: OpenSpaceFeature) -> String {
    match feature {
        OpenSpaceFeature::RetentionPond =>
            "advisory: a design-intent basin footprint, NOT a sized detention volume \
             (no hydrology — storage/outflow/storm event)".to_string(),
        OpenSpaceFeature::TreeSave =>
            "advisory: a design-intent preservation boundary, NOT a surveyed canopy or \
             arborist assessment".to_string(),
        _ =>
            "advisory: a design-intent amenity placement, not landscape or civil engineering"
                .to_string(),
    }
}

/// The long-axis spine of a region: from one aabb corner through the centroid to
/// the opposite corner, clipped to a 3-point centerline. A cheap deterministic
/// route for a greenway when the user gives no explicit path.
fn long_axis_spine(region: &Polygon2d) -> Vec<DVec2> {
    let (lo, hi) = region.aabb();
    let c = region.centroid();
    if (hi.x - lo.x) >= (hi.y - lo.y) {
        vec![DVec2::new(lo.x, c.y), c, DVec2::new(hi.x, c.y)]
    } else {
        vec![DVec2::new(c.x, lo.y), c, DVec2::new(c.x, hi.y)]
    }
}

/// Insert baked open-space curves onto the `openspace` layer with the given ids.
pub fn insert_open_space(doc: &mut Document, bake: &OpenSpaceBake, ids: &[ObjectId]) {
    for ((poly, label), id) in bake.polygons.iter().zip(&bake.labels).zip(ids) {
        doc.insert(SceneObject {
            visible: true,
            id: *id,
            name: Some(label.clone()),
            layer: OPENSPACE_LAYER.to_string(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Curve(polygon_to_curve(poly, bake.z)),
        });
    }
}

// ── Phase 10: buildings — footprint + stepped massing + roof ────────────────

use kernel_mesh::Mesh;
use subdivision::{FootprintMode, RoofType, Typology};

/// The layer baked building geometry (footprints + 3D mass + roof) lands on.
/// Tagged distinctly so a later `lotreport` (Phase 11) can total built GFA/FAR.
pub const BUILDINGS_LAYER: &str = "buildings";

/// One baked building: its footprint polygon (2D), the stepped mass mesh (3D,
/// positioned at the lot z), the roof mesh (3D), and per-floor GFA data
/// (floor count + per-floor areas) so Phase 11 yield can compute GFA / FAR.
#[derive(Debug)]
pub struct BuiltBuilding {
    pub footprint: Polygon2d,
    pub void: Option<Polygon2d>,
    pub mass: Mesh,
    pub roof: Mesh,
    /// Per-floor net areas (net of step-backs / courtyard void).
    pub floor_areas: Vec<f64>,
    pub height: f64,
    pub gfa: f64,
    pub z: f64,
}

/// The result of a `lotbuilding` run over one or more lots.
#[derive(Debug)]
pub struct BuildingBake {
    pub buildings: Vec<BuiltBuilding>,
    /// Lots whose buildable envelope collapsed (no building) — reported.
    pub collapsed: usize,
    /// euro_latam placeholder banner when profile defaults (floor height, roof
    /// pitch, coverage) were in play.
    pub placeholder_note: Option<String>,
    /// Total built GFA across all lots (sum of per-building GFA).
    pub total_gfa: f64,
    /// Total achieved floor count across all lots.
    pub total_floors: usize,
    /// The resolved roof type used (after PerTypology resolution).
    pub roof_type: RoofType,
    /// The typology used.
    pub typology: Typology,
}

/// Parse a typology keyword → [`Typology`].
pub fn parse_typology(s: &str) -> Option<Typology> {
    Typology::parse(s)
}

/// Parse a footprint-mode keyword → [`FootprintMode`].
pub fn parse_footprint_mode(s: &str) -> Option<FootprintMode> {
    FootprintMode::parse(s)
}

/// Parse a roof-type keyword → [`RoofType`].
pub fn parse_roof_type(s: &str) -> Option<RoofType> {
    RoofType::parse(s)
}

/// Compute the footprint + stepped mass + roof for each lot polygon under
/// `settings`. Deterministic. A lot whose buildable envelope collapses is
/// counted (not built) and reported. `settings` already carries the per-run
/// building overrides (typology / mode / floors / roof / …).
pub fn compute_buildings(
    lots: &[(Polygon2d, f64)],
    settings: &SubdivisionSettings,
) -> Result<BuildingBake, String> {
    let mut buildings = Vec::new();
    let mut collapsed = 0usize;
    let mut total_gfa = 0.0;
    let mut total_floors = 0usize;
    for (poly, z) in lots {
        // Buildings generate INSIDE the Phase-8 buildable envelope. A baked lot
        // curve carries no street tags → longest edge is the front (same spirit
        // as lotsetbacks).
        let block = Block::untagged(poly.clone());
        let env = subdivision::buildable_envelope(&block, settings);
        let Some(envelope) = env.polygon else {
            collapsed += 1;
            continue;
        };
        match subdivision::build_on_envelope(&envelope, poly, *z, settings) {
            Some(b) => {
                total_gfa += b.gfa;
                total_floors += b.floors.len();
                buildings.push(BuiltBuilding {
                    footprint: b.footprint,
                    void: b.void,
                    mass: b.mass,
                    roof: b.roof,
                    floor_areas: b.floors.iter().map(|f| f.area).collect(),
                    height: b.height,
                    gfa: b.gfa,
                    z: *z,
                });
            }
            None => collapsed += 1,
        }
    }
    if buildings.is_empty() {
        return Err(format!(
            "no buildings generated ({collapsed} lot(s) had a collapsed buildable envelope) — \
             reduce setbacks or the footprint size"
        ));
    }
    // Resolve PerTypology for reporting.
    let roof_type = match settings.roof_type {
        RoofType::PerTypology => subdivision::default_roof_for(settings.typology),
        rt => rt,
    };
    let placeholder_note = if settings.region == RegionProfile::EuroLatam {
        Some(
            "using euro_latam building defaults (floor height 3 m / roof pitch 30° / coverage — \
             placeholder, confirm with Manuel)"
                .to_string(),
        )
    } else {
        None
    };
    Ok(BuildingBake {
        buildings,
        collapsed,
        placeholder_note,
        total_gfa,
        total_floors,
        roof_type,
        typology: settings.typology,
    })
}

/// Ensure the `buildings` layer exists (a warm terracotta for built mass).
pub fn ensure_buildings_layer(doc: &mut Document) -> Option<String> {
    ensure_layer(doc, BUILDINGS_LAYER, [0.72, 0.45, 0.32, 1.0])
}

/// The number of baked objects one building produces (footprint curve + mass
/// mesh + roof mesh = 3). Used to allocate + slice the written-back id list so
/// replay is byte-identical.
pub const OBJECTS_PER_BUILDING: usize = 3;

/// Insert baked buildings onto the `buildings` layer. Each building emits, in
/// order: the footprint curve (2D), the mass mesh (3D), the roof mesh (3D). The
/// `ids` slice must hold `OBJECTS_PER_BUILDING × buildings.len()` ids in that
/// order so undo/replay round-trips exactly.
pub fn insert_buildings(doc: &mut Document, bake: &BuildingBake, ids: &[ObjectId]) {
    for (i, b) in bake.buildings.iter().enumerate() {
        let base = i * OBJECTS_PER_BUILDING;
        if base + 2 >= ids.len() {
            break;
        }
        // Footprint (2D curve).
        doc.insert(SceneObject {
            visible: true,
            id: ids[base],
            name: Some("building:footprint".to_string()),
            layer: BUILDINGS_LAYER.to_string(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Curve(polygon_to_curve(&b.footprint, b.z)),
        });
        // Mass (3D mesh).
        doc.insert(SceneObject {
            visible: true,
            id: ids[base + 1],
            name: Some("building:mass".to_string()),
            layer: BUILDINGS_LAYER.to_string(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Mesh(b.mass.clone()),
        });
        // Roof (3D mesh).
        doc.insert(SceneObject {
            visible: true,
            id: ids[base + 2],
            name: Some("building:roof".to_string()),
            layer: BUILDINGS_LAYER.to_string(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Mesh(b.roof.clone()),
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
    fn setbacks_envelope_area_and_collapse() {
        // Untagged square 40×40: the longest-edge tie picks the first (bottom)
        // edge as front, so front/rear inset vertically (40−5−7=28) and side
        // insets horizontally (40−2·3=34) → 28×34 = 952.
        let poly = curve_to_polygon(&rect_curve(40.0, 40.0)).unwrap();
        let s = SubdivisionSettings {
            setback_front: 5.0,
            setback_side: 3.0,
            setback_rear: 7.0,
            region: RegionProfile::UsSuburban,
            ..SubdivisionSettings::default()
        };
        let bake = compute_setbacks(&[(poly, 0.0)], &s).unwrap();
        assert_eq!(bake.envelopes.len(), 1);
        assert!((bake.envelopes[0].area() - 952.0).abs() < 1.0, "area {}", bake.envelopes[0].area());
        assert_eq!(bake.collapsed, 0);
        assert!(bake.placeholder_note.is_none());

        // Tiny lot + huge setbacks → all collapse → error, not a panic.
        let tiny = curve_to_polygon(&rect_curve(6.0, 6.0)).unwrap();
        assert!(compute_setbacks(&[(tiny, 0.0)], &s).is_err());
    }

    #[test]
    fn frontage_setback_is_default_and_differs_from_curb() {
        let poly = curve_to_polygon(&rect_curve(40.0, 20.0)).unwrap();
        let s = SubdivisionSettings {
            setback_front: 4.0,
            setback_side: 4.0,
            setback_rear: 2.0,
            region: RegionProfile::UsSuburban,
            ..SubdivisionSettings::default()
        };
        let m_setback = measure_frontage(&[poly.clone()], FrontageAt::Setback, &s);
        let m_curb = measure_frontage(&[poly], FrontageAt::Curb, &s);
        // Curb (40) vs setback (40 − 2*side = 32): they differ.
        assert!((m_curb[0].length - 40.0).abs() < 0.5, "curb {}", m_curb[0].length);
        assert!(
            (m_curb[0].length - m_setback[0].length).abs() > 1.0,
            "setback {} should differ from curb {}",
            m_setback[0].length,
            m_curb[0].length
        );
        assert_eq!(parse_frontage_at("nonsense"), None);
        assert_eq!(parse_frontage_at(""), Some(FrontageAt::Setback));
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
