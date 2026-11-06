// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! `osmfetch [radius_m]` (#343): OpenStreetMap context around the site, from
//! the Overpass API, as scene geometry for architects.
//!
//! The core has no network code: a host installs an [`OsmFetcher`] on the
//! [`crate::Session`] ([`crate::Session::set_osm_fetcher`]). The desktop app
//! and the headless CLI POST the Overpass QL this module builds
//! ([`overpass_query`]); the iOS app runs it with `URLSession` through the FFI
//! (`ijc_set_osm_fetcher` / `ijc_osm_answer`). With none installed,
//! `osmfetch` fails and changes nothing.
//!
//! The command logs one [`crate::Command::OsmContext`] op holding the
//! geometry it generated ([`ContextItem`]), never the request or the raw
//! Overpass JSON: a saved file replays offline and identically, and `undo`
//! removes the whole context in one step.
//!
//! Layers: buildings (extruded like `osmfile`) on `context`, road centrelines
//! on `context-roads`, water areas and waterway lines on `context-water`,
//! parks and green areas on `context-green`, trees (one point each) on
//! `context-trees`. OSM data is © OpenStreetMap contributors, ODbL 1.0
//! ([`ATTRIBUTION`]); the command's message carries it.

use glam::{DVec2, DVec3};
use itsjustcad_doc::{Document, Geometry, LayerStyle, ObjectId, SceneObject};
use kernel_curve::Curve;
use serde::{Deserialize, Serialize};

use crate::geo::{building_height, close_ring, to_local, GeoOrigin};

/// Default radius: about four city blocks (owner decision, #343).
pub const DEFAULT_RADIUS_M: f64 = 400.0;
/// Largest radius one request may ask for (π km² ≈ 3 km²).
pub const MAX_RADIUS_M: f64 = 1000.0;
/// Smallest radius worth a request.
pub const MIN_RADIUS_M: f64 = 10.0;
/// Overpass server-side run time limit, seconds (`[timeout:…]`).
pub const QUERY_TIMEOUT_S: u32 = 60;
/// Overpass server-side memory limit, bytes (`[maxsize:…]`, 64 MiB).
pub const QUERY_MAXSIZE: u64 = 64 * 1024 * 1024;
/// More generated objects than this is refused as a too-large area.
pub const MAX_ITEMS: usize = 20_000;
/// The licence notice OSM data needs wherever it is shown.
pub const ATTRIBUTION: &str = "© OpenStreetMap contributors, ODbL 1.0";

pub const LAYER_BUILDINGS: &str = "context";
pub const LAYER_ROADS: &str = "context-roads";
pub const LAYER_WATER: &str = "context-water";
pub const LAYER_GREEN: &str = "context-green";
pub const LAYER_TREES: &str = "context-trees";

/// The context layers, in report order, with their display colours.
const LAYERS: [(&str, [f32; 4]); 5] = [
    (LAYER_BUILDINGS, [0.62, 0.62, 0.60, 1.0]),
    (LAYER_ROADS, [0.36, 0.36, 0.40, 1.0]),
    (LAYER_WATER, [0.25, 0.50, 0.85, 1.0]),
    (LAYER_GREEN, [0.36, 0.64, 0.30, 1.0]),
    (LAYER_TREES, [0.15, 0.45, 0.18, 1.0]),
];

/// One Overpass request: the site centre, the radius and the QL to send.
#[derive(Clone, Debug, PartialEq)]
pub struct OsmRequest {
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub radius_m: f64,
    /// Overpass QL (`[out:json]…out geom;`), sent as the `data` form field.
    pub query: String,
}

impl OsmRequest {
    pub fn new(lat_deg: f64, lon_deg: f64, radius_m: f64) -> Self {
        Self { lat_deg, lon_deg, radius_m, query: overpass_query(lat_deg, lon_deg, radius_m) }
    }

    /// The key hosts cache answers by: the QL itself (it embeds the centre
    /// and the radius).
    pub fn key(&self) -> &str {
        &self.query
    }
}

/// Why a fetch gave no data. Nothing is added.
#[derive(Clone, Debug, PartialEq)]
pub enum OsmError {
    /// No network, DNS failure, refused connection, bad answer.
    Unreachable(String),
    /// The request or the server-side query ran out of time.
    Timeout(String),
    /// The area holds more data than the server or the core accepts.
    TooLarge(String),
    /// The server's rate limit or load shedding refused the request (HTTP
    /// 429, or 502 "at capacity").
    Busy(String),
    /// The host started an asynchronous fetch (iOS): run the command again
    /// once it has answered. The message says so.
    Pending(String),
}

impl std::fmt::Display for OsmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(e) => {
                write!(f, "osmfetch: cannot reach the Overpass API ({e}); nothing added")
            }
            Self::Timeout(e) => write!(
                f,
                "osmfetch: the Overpass API timed out ({e}); try a smaller radius or try again later; nothing added"
            ),
            Self::TooLarge(e) => write!(
                f,
                "osmfetch: the area is too large ({e}); try a smaller radius; nothing added"
            ),
            Self::Busy(e) => write!(
                f,
                "osmfetch: the Overpass API is busy ({e}); wait a minute and run the command again; nothing added"
            ),
            Self::Pending(m) => f.write_str(m),
        }
    }
}

impl OsmError {
    /// A host's transport error text, classified: an HTTP 429, or a 502 (the
    /// public server answers "at capacity" so), is [`Self::Busy`], a 504 or a timeout [`Self::Timeout`], a 413 or
    /// "too large" [`Self::TooLarge`], anything else [`Self::Unreachable`].
    pub fn classify(msg: &str) -> Self {
        let m = msg.to_ascii_lowercase();
        if m.contains("429") || m.contains("502") || m.contains("too many requests") || m.contains("rate limit") || m.contains("at capacity") {
            Self::Busy(msg.to_string())
        } else if m.contains("504") || m.contains("timed out") || m.contains("timeout") || m.contains("no answer within") {
            Self::Timeout(msg.to_string())
        } else if m.contains("413") || m.contains("too large") || m.contains("out of memory") {
            Self::TooLarge(msg.to_string())
        } else {
            Self::Unreachable(msg.to_string())
        }
    }
}

/// A host's Overpass access. One call is one request: the core calls
/// [`OsmFetcher::fetch`] once per `osmfetch` command.
pub trait OsmFetcher: Send + Sync {
    /// The Overpass JSON answer to `req.query`.
    fn fetch(&self, req: &OsmRequest) -> Result<Vec<u8>, OsmError>;

    /// Data attribution to show with the result.
    fn attribution(&self) -> String {
        ATTRIBUTION.to_string()
    }
}

/// The Overpass QL for the context within `radius_m` of the site: building,
/// road, water, green and tree features, with inline geometry (`out geom`).
/// The server limits are set explicitly ([`QUERY_TIMEOUT_S`],
/// [`QUERY_MAXSIZE`]), as the Overpass usage policy asks.
pub fn overpass_query(lat_deg: f64, lon_deg: f64, radius_m: f64) -> String {
    let a = format!("(around:{:.0},{lat_deg:.7},{lon_deg:.7})", radius_m.round());
    let natural = "^(water|wood|scrub|grassland|heath)$";
    let landuse = "^(reservoir|basin|grass|forest|meadow|recreation_ground|village_green|allotments|cemetery)$";
    let leisure = "^(park|garden|playground|pitch|common|nature_reserve)$";
    format!(
        "[out:json][timeout:{QUERY_TIMEOUT_S}][maxsize:{QUERY_MAXSIZE}];(\
         way[\"building\"]{a};relation[\"building\"]{a};\
         way[\"highway\"]{a};\
         way[\"natural\"~\"{natural}\"]{a};relation[\"natural\"~\"{natural}\"]{a};\
         way[\"waterway\"]{a};relation[\"waterway\"=\"riverbank\"]{a};\
         way[\"landuse\"~\"{landuse}\"]{a};relation[\"landuse\"~\"{landuse}\"]{a};\
         way[\"leisure\"~\"{leisure}\"]{a};relation[\"leisure\"~\"{leisure}\"]{a};\
         node[\"natural\"=\"tree\"]{a};\
         );out geom;"
    )
}

/// One generated context object, in local metres, as the logged op stores it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ContextItem {
    pub layer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub geometry: ContextGeometry,
}

/// A context object's geometry.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContextGeometry {
    /// An extruded building.
    Mesh { positions: Vec<DVec3>, faces: Vec<[u32; 3]> },
    /// A road or waterway centreline (open) or an area outline (closed).
    Polyline { points: Vec<DVec3>, closed: bool },
    /// A tree.
    Point { at: DVec3 },
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Building,
    Road,
    WaterArea,
    WaterLine,
    Green,
}

fn tag<'a>(tags: Option<&'a serde_json::Value>, key: &str) -> Option<&'a str> {
    tags?.get(key)?.as_str()
}

fn kind(tags: Option<&serde_json::Value>, is_way: bool) -> Option<Kind> {
    let in_set = |key: &str, set: &[&str]| tag(tags, key).is_some_and(|v| set.contains(&v));
    if tag(tags, "building").is_some_and(|v| v != "no") {
        return Some(Kind::Building);
    }
    if is_way && tag(tags, "highway").is_some() {
        return Some(Kind::Road);
    }
    if in_set("natural", &["water"])
        || in_set("waterway", &["riverbank"])
        || in_set("landuse", &["reservoir", "basin"])
    {
        return Some(Kind::WaterArea);
    }
    if is_way && tag(tags, "waterway").is_some() {
        return Some(Kind::WaterLine);
    }
    if in_set("natural", &["wood", "scrub", "grassland", "heath"])
        || in_set(
            "landuse",
            &["grass", "forest", "meadow", "recreation_ground", "village_green", "allotments", "cemetery"],
        )
        || in_set("leisure", &["park", "garden", "playground", "pitch", "common", "nature_reserve"])
    {
        return Some(Kind::Green);
    }
    None
}

/// A way's or member's inline `geometry`, projected (null nodes dropped).
fn points(geom: Option<&serde_json::Value>, origin: GeoOrigin) -> Vec<DVec2> {
    geom.and_then(|g| g.as_array())
        .map(|g| {
            g.iter()
                .filter_map(|n| {
                    Some(to_local([n.get("lon")?.as_f64()?, n.get("lat")?.as_f64()?], Some(origin)))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Join open member ways into closed rings by matching end points; pieces
/// that do not close are dropped.
fn stitch(mut pieces: Vec<Vec<DVec2>>) -> Vec<Vec<DVec2>> {
    const EPS: f64 = 1e-3;
    let near = |a: DVec2, b: DVec2| a.distance(b) < EPS;
    pieces.retain(|p| p.len() >= 2);
    let mut rings = Vec::new();
    while let Some(mut ring) = pieces.pop() {
        loop {
            let (first, last) = (ring[0], *ring.last().expect("non-empty"));
            if ring.len() >= 4 && near(first, last) {
                rings.push(ring);
                break;
            }
            let Some(i) = pieces.iter().position(|p| near(p[0], last) || near(*p.last().unwrap(), last)) else {
                break;
            };
            let mut next = pieces.swap_remove(i);
            if !near(next[0], last) {
                next.reverse();
            }
            ring.extend(next.into_iter().skip(1));
        }
    }
    rings
}

/// The outer rings of an area element: a closed way, or a relation's
/// stitched `outer` members.
fn area_rings(el: &serde_json::Value, origin: GeoOrigin) -> Vec<Vec<DVec2>> {
    let rings = if el["type"] == "relation" {
        let outer: Vec<Vec<DVec2>> = el["members"]
            .as_array()
            .map(|ms| {
                ms.iter()
                    .filter(|m| m["type"] == "way" && matches!(m["role"].as_str(), Some("outer" | "")))
                    .map(|m| points(m.get("geometry"), origin))
                    .collect()
            })
            .unwrap_or_default();
        stitch(outer)
    } else {
        vec![points(el.get("geometry"), origin)]
    };
    rings
        .into_iter()
        .filter_map(close_ring)
        .filter(|r| signed_area(r).abs() > 1e-6)
        .collect()
}

fn signed_area(ring: &[DVec2]) -> f64 {
    let n = ring.len();
    (0..n).map(|i| ring[i].perp_dot(ring[(i + 1) % n])).sum::<f64>() / 2.0
}

/// Split a polyline into the runs inside the circle of radius `r` about the
/// origin (the site), with the boundary crossings interpolated.
fn clip_to_circle(pts: &[DVec2], r: f64) -> Vec<Vec<DVec2>> {
    let inside = |a: DVec2, b: DVec2| -> Option<(f64, f64)> {
        let d = b - a;
        let (qa, qb, qc) = (d.length_squared(), 2.0 * a.dot(d), a.length_squared() - r * r);
        if qa < 1e-18 {
            return (qc <= 0.0).then_some((0.0, 1.0));
        }
        let disc = qb * qb - 4.0 * qa * qc;
        if disc < 0.0 {
            return None;
        }
        let s = disc.sqrt();
        let (t0, t1) = (((-qb - s) / (2.0 * qa)).max(0.0), ((-qb + s) / (2.0 * qa)).min(1.0));
        (t0 < t1).then_some((t0, t1))
    };
    let mut runs = Vec::new();
    let mut cur: Vec<DVec2> = Vec::new();
    for w in pts.windows(2) {
        let (a, b) = (w[0], w[1]);
        match inside(a, b) {
            Some((t0, t1)) => {
                if cur.is_empty() || t0 > 1e-12 {
                    if cur.len() >= 2 {
                        runs.push(std::mem::take(&mut cur));
                    }
                    cur.clear();
                    cur.push(a + (b - a) * t0);
                }
                cur.push(a + (b - a) * t1);
                if t1 < 1.0 - 1e-12 {
                    runs.push(std::mem::take(&mut cur));
                }
            }
            None => {
                if cur.len() >= 2 {
                    runs.push(std::mem::take(&mut cur));
                }
                cur.clear();
            }
        }
    }
    if cur.len() >= 2 {
        runs.push(cur);
    }
    runs.retain(|r| r.len() >= 2);
    runs
}

/// Ground height under a point: the terrain's, else 0.
pub type Ground<'a> = &'a dyn Fn(f64, f64) -> Option<f64>;

/// Small lift that keeps draped lines visible above the terrain.
const DRAPE_LIFT_M: f64 = 0.02;

/// The context items in an Overpass JSON answer, in local metres about
/// `origin` (the site). Buildings sit on the ground under their footprint
/// (the lowest terrain point, else z = 0); roads, waterways and area
/// outlines are draped onto `ground`; roads and waterways are clipped to
/// `radius_m`. A server-side runtime error (`remark`) is an error.
pub fn context_items(
    bytes: &[u8],
    origin: GeoOrigin,
    radius_m: f64,
    ground: Ground<'_>,
) -> Result<Vec<ContextItem>, OsmError> {
    let root: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|e| OsmError::Unreachable(format!("unreadable Overpass answer: {e}")))?;
    if let Some(remark) = root.get("remark").and_then(|r| r.as_str())
        && remark.contains("runtime error")
    {
        return Err(if remark.contains("timed out") {
            OsmError::Timeout(remark.to_string())
        } else if remark.contains("out of memory") || remark.contains("maxsize") {
            OsmError::TooLarge(remark.to_string())
        } else {
            OsmError::Unreachable(remark.to_string())
        });
    }
    let elements = root
        .get("elements")
        .and_then(|e| e.as_array())
        .ok_or_else(|| OsmError::Unreachable("Overpass answer has no 'elements'".into()))?;

    let z = |p: DVec2| ground(p.x, p.y);
    let drape = |pts: &[DVec2]| -> Vec<DVec3> {
        pts.iter().map(|p| p.extend(z(*p).map_or(0.0, |h| h + DRAPE_LIFT_M))).collect()
    };
    let mut out = Vec::new();
    for el in elements {
        let tags = el.get("tags");
        let name = tag(tags, "name").map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "));
        let item = |layer: &str, geometry| ContextItem { layer: layer.to_string(), name: name.clone(), geometry };
        match el["type"].as_str() {
            Some("node") => {
                if tag(tags, "natural") == Some("tree")
                    && let (Some(lon), Some(lat)) = (el["lon"].as_f64(), el["lat"].as_f64())
                {
                    let p = to_local([lon, lat], Some(origin));
                    out.push(item(LAYER_TREES, ContextGeometry::Point { at: p.extend(z(p).unwrap_or(0.0)) }));
                }
            }
            Some(t @ ("way" | "relation")) => {
                let Some(k) = kind(tags, t == "way") else { continue };
                match k {
                    Kind::Building => {
                        let height = building_height(tags);
                        for ring in area_rings(el, origin) {
                            let base = ring.iter().filter_map(|p| z(*p)).fold(f64::INFINITY, f64::min);
                            let base = if base.is_finite() { base } else { 0.0 };
                            let mesh = kernel_mesh::extrude_profile(&ring, base, height);
                            if mesh.faces().is_empty() {
                                continue;
                            }
                            out.push(item(
                                LAYER_BUILDINGS,
                                ContextGeometry::Mesh {
                                    positions: mesh.positions().to_vec(),
                                    faces: mesh.faces().to_vec(),
                                },
                            ));
                        }
                    }
                    Kind::Road | Kind::WaterLine => {
                        let layer = if k == Kind::Road { LAYER_ROADS } else { LAYER_WATER };
                        let pts = points(el.get("geometry"), origin);
                        if pts.len() < 2 {
                            continue;
                        }
                        let closed = pts.len() >= 4 && pts[0] == pts[pts.len() - 1];
                        if pts.iter().all(|p| p.length() <= radius_m) {
                            let pts = if closed { &pts[..pts.len() - 1] } else { &pts[..] };
                            out.push(item(layer, ContextGeometry::Polyline { points: drape(pts), closed }));
                        } else {
                            for run in clip_to_circle(&pts, radius_m) {
                                out.push(item(layer, ContextGeometry::Polyline { points: drape(&run), closed: false }));
                            }
                        }
                    }
                    Kind::WaterArea | Kind::Green => {
                        let layer = if k == Kind::Green { LAYER_GREEN } else { LAYER_WATER };
                        for ring in area_rings(el, origin) {
                            out.push(item(layer, ContextGeometry::Polyline { points: drape(&ring), closed: true }));
                        }
                    }
                }
            }
            _ => {}
        }
        if out.len() > MAX_ITEMS {
            return Err(OsmError::TooLarge(format!("more than {MAX_ITEMS} objects")));
        }
    }
    Ok(out)
}

/// Validate `items` before anything is inserted.
pub(crate) fn validate(items: &[ContextItem]) -> Result<(), String> {
    if items.is_empty() {
        return Err("osm_context: no items".into());
    }
    for it in items {
        if !LAYERS.iter().any(|(l, _)| *l == it.layer) {
            return Err(format!("osm_context: unknown layer '{}'", it.layer));
        }
        match &it.geometry {
            ContextGeometry::Mesh { positions, faces } => {
                let n = positions.len() as u32;
                if faces.is_empty() || faces.iter().flatten().any(|&i| i >= n) {
                    return Err("osm_context: bad building mesh".into());
                }
            }
            ContextGeometry::Polyline { points, .. } if points.len() < 2 => {
                return Err("osm_context: polyline needs 2 points".into());
            }
            _ => {}
        }
    }
    Ok(())
}

/// Insert `items` (already validated) with `ids`, creating the context
/// layers they use. Returns the layers this call created.
pub(crate) fn insert(doc: &mut Document, items: &[ContextItem], ids: &[ObjectId]) -> Vec<String> {
    let mut created = Vec::new();
    for (layer, color) in LAYERS {
        if items.iter().any(|i| i.layer == layer) && !doc.layers.contains_key(layer) {
            doc.layers.insert(layer.to_string(), LayerStyle { color: Some(color), ..LayerStyle::default() });
            created.push(layer.to_string());
        }
    }
    for (it, id) in items.iter().zip(ids) {
        let geometry = match &it.geometry {
            ContextGeometry::Mesh { positions, faces } => {
                Geometry::Mesh(kernel_mesh::Mesh::new(positions.clone(), faces.clone()))
            }
            ContextGeometry::Polyline { points, closed } => {
                Geometry::Curve(Curve::Polyline { points: points.clone(), closed: *closed })
            }
            ContextGeometry::Point { at } => Geometry::Points { positions: vec![*at] },
        };
        doc.insert(SceneObject {
            visible: true,
            id: *id,
            name: it.name.clone(),
            layer: it.layer.clone(),
            color: None,
            material: None,
            lineweight_mm: None,
            edge_tags: None,
            street: None,
            face_values: None,
            geometry,
        });
    }
    doc.generation += 1;
    created
}

/// "12 building(s) on 'context', 4 road(s) on 'context-roads', …" for the
/// layers `items` use.
pub(crate) fn summary(items: &[ContextItem]) -> String {
    let nouns = ["building", "road", "water feature", "green area", "tree"];
    LAYERS
        .iter()
        .zip(nouns)
        .filter_map(|((layer, _), noun)| {
            let n = items.iter().filter(|i| i.layer == *layer).count();
            (n > 0).then(|| format!("{n} {noun}(s) on '{layer}'"))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse, Command, Session};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// A recorded Overpass answer (trimmed) around the #215 acceptance
    /// address, 1600 Amphitheatre Pkwy, Mountain View.
    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/osm/overpass-amphitheatre.json");
    const LAT: f64 = 37.4224857;
    const LON: f64 = -122.0855846;

    /// A fetcher answering `body`, recording its requests.
    struct Stub {
        body: Result<Vec<u8>, OsmError>,
        calls: AtomicUsize,
        last: Mutex<Option<OsmRequest>>,
    }

    impl OsmFetcher for Stub {
        fn fetch(&self, req: &OsmRequest) -> Result<Vec<u8>, OsmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last.lock().unwrap() = Some(req.clone());
            self.body.clone()
        }
    }

    fn stub(body: Result<Vec<u8>, OsmError>) -> Arc<Stub> {
        Arc::new(Stub { body, calls: AtomicUsize::new(0), last: Mutex::default() })
    }

    fn site(f: Arc<Stub>) -> Session {
        let mut s = Session::default();
        s.run(parse(&format!("location {LAT} {LON} -7")).unwrap()).unwrap();
        s.set_osm_fetcher(Some(f));
        s
    }

    fn on_layer(s: &Session, layer: &str) -> usize {
        s.doc.objects().filter(|o| o.layer == layer).count()
    }

    #[test]
    fn registry_lists_osmfetch_with_network_note() {
        let spec = crate::registry().iter().find(|s| s.name == "osmfetch").expect("osmfetch");
        assert!(spec.usage.contains("[radius_m]"));
        assert!(spec.summary.contains("network") && spec.summary.contains("400 m"));
        assert!(crate::registry().iter().all(|s| s.name != "osm_context"), "internal op stays out");
    }

    #[test]
    fn parses_radius_and_is_not_logged() {
        assert_eq!(parse("osmfetch").unwrap(), Command::OsmFetch { radius_m: None });
        assert_eq!(parse("osmfetch 250").unwrap(), Command::OsmFetch { radius_m: Some(250.0) });
        assert!(parse("osmfetch 1 2").is_err());
        assert!(!parse("osmfetch").unwrap().is_logged());
        assert!(parse("osmfetch").unwrap().is_side_effecting(), "network: the Deck confirms it");
    }

    #[test]
    fn query_has_limits_and_every_feature_class() {
        let q = overpass_query(LAT, LON, 400.0);
        assert!(q.starts_with("[out:json][timeout:60][maxsize:67108864];"), "{q}");
        assert!(q.contains("(around:400,37.4224857,-122.0855846)"), "{q}");
        for k in ["\"building\"", "\"highway\"", "\"waterway\"", "\"leisure\"", "\"natural\"=\"tree\""] {
            assert!(q.contains(k), "{k} in {q}");
        }
        assert!(q.ends_with("out geom;"));
    }

    #[test]
    fn builds_layers_from_the_recorded_answer_with_the_default_radius() {
        let f = stub(Ok(FIXTURE.to_vec()));
        let mut s = site(f.clone());
        let out = s.run(parse("osmfetch").unwrap()).unwrap();
        assert_eq!(f.calls.load(Ordering::SeqCst), 1, "one request per command");
        let req = f.last.lock().unwrap().clone().unwrap();
        assert_eq!((req.lat_deg, req.lon_deg, req.radius_m), (LAT, LON, DEFAULT_RADIUS_M));
        assert!(req.query.contains("(around:400,"), "{}", req.query);
        assert!(out.message.contains(ATTRIBUTION), "{}", out.message);

        let counts: Vec<usize> = LAYERS.iter().map(|(l, _)| on_layer(&s, l)).collect();
        assert_eq!(counts, vec![4, 5, 1, 2, 1], "buildings, roads, water, green, trees");
        assert_eq!(out.created.len(), counts.iter().sum::<usize>());
        for (layer, _) in LAYERS {
            assert!(s.doc.layers.contains_key(layer), "{layer}");
        }
        assert!(s.doc.objects().filter(|o| o.layer == LAYER_BUILDINGS).all(|o| matches!(o.geometry, Geometry::Mesh(_))));
        assert!(s.doc.objects().filter(|o| o.layer == LAYER_TREES).all(|o| matches!(o.geometry, Geometry::Points { .. })));
        // Roads are centrelines, clipped to the radius.
        for o in s.doc.objects().filter(|o| o.layer == LAYER_ROADS) {
            let Geometry::Curve(Curve::Polyline { points, .. }) = &o.geometry else { panic!("road {o:?}") };
            assert!(points.iter().all(|p| p.truncate().length() <= DEFAULT_RADIUS_M + 1e-6));
        }
    }

    #[test]
    fn radius_is_clipped_and_bounded() {
        let f = stub(Ok(FIXTURE.to_vec()));
        let mut s = site(f.clone());
        s.run(parse("osmfetch 60").unwrap()).unwrap();
        for o in s.doc.objects().filter(|o| o.layer == LAYER_ROADS) {
            let Geometry::Curve(Curve::Polyline { points, .. }) = &o.geometry else { panic!() };
            assert!(points.iter().all(|p| p.truncate().length() <= 60.0 + 1e-6));
        }
        for bad in ["osmfetch 5000", "osmfetch 2", "osmfetch -1"] {
            let err = s.run(parse(bad).unwrap()).unwrap_err().to_string();
            assert!(err.contains("radius"), "{bad}: {err}");
        }
        assert_eq!(f.calls.load(Ordering::SeqCst), 1, "a refused radius sends nothing");
    }

    #[test]
    fn without_location_or_fetcher_it_fails_and_changes_nothing() {
        let mut s = Session::default();
        s.set_osm_fetcher(Some(stub(Ok(FIXTURE.to_vec()))));
        let err = s.run(parse("osmfetch").unwrap()).unwrap_err().to_string();
        assert!(err.contains("geocode \"<address>\"") && err.contains("location <lat> <lon>"), "{err}");
        let mut s = Session::default();
        s.run(parse("location 40 0 0").unwrap()).unwrap();
        let err = s.run(parse("osmfetch").unwrap()).unwrap_err().to_string();
        assert!(err.contains("no OpenStreetMap fetcher"), "{err}");
        assert_eq!(s.doc.len(), 0);
        assert_eq!(s.save_log().len(), 1);
    }

    #[test]
    fn network_errors_are_clear_and_change_nothing() {
        let cases = [
            (OsmError::classify("request failed: dns error"), "cannot reach the Overpass API"),
            (OsmError::classify("server returned 504 Gateway Timeout"), "timed out"),
            (OsmError::classify("server returned 429 Too Many Requests"), "busy"),
            (OsmError::classify("answer too large"), "too large"),
        ];
        for (e, want) in cases {
            let mut s = site(stub(Err(e)));
            let err = s.run(parse("osmfetch").unwrap()).unwrap_err().to_string();
            assert!(err.contains(want) && err.contains("nothing added"), "{err}");
            assert_eq!(s.doc.len(), 0);
        }
        let remark = br#"{"elements":[],"remark":"runtime error: Query timed out in \"query\" at line 1 after 61 seconds."}"#;
        let err = site(stub(Ok(remark.to_vec()))).run(parse("osmfetch").unwrap()).unwrap_err().to_string();
        assert!(err.contains("timed out"), "{err}");
        let remark = br#"{"elements":[],"remark":"runtime error: Query run out of memory using about 64 MB of RAM."}"#;
        let err = site(stub(Ok(remark.to_vec()))).run(parse("osmfetch").unwrap()).unwrap_err().to_string();
        assert!(err.contains("too large"), "{err}");
        let err = site(stub(Ok(br#"{"elements":[]}"#.to_vec()))).run(parse("osmfetch").unwrap()).unwrap_err().to_string();
        assert!(err.contains("no OpenStreetMap context"), "{err}");
    }

    #[test]
    fn replays_offline_identically_and_undoes_in_one_step() {
        let mut s = site(stub(Ok(FIXTURE.to_vec())));
        s.run(parse("box 0,0,0 4,4,4").unwrap()).unwrap();
        s.run(parse("osmfetch").unwrap()).unwrap();
        let log = s.save_log();
        assert_eq!(log.len(), 3, "location, box, one osm_context op: {log:?}");
        assert!(matches!(log[2], Command::OsmContext { .. }));
        let json = serde_json::to_string(&log[2]).unwrap();
        assert!(!json.contains("elements") && json.contains("\"cmd\":\"osm_context\""), "geometry only");

        let reopened = crate::io::from_json(&crate::io::to_json(&s)).unwrap();
        assert!(!reopened.has_osm_fetcher(), "no network on replay");
        assert_eq!(reopened.doc, s.doc, "offline replay is identical");

        let before = s.doc.layers.len();
        s.run(Command::Undo).unwrap();
        assert_eq!(s.doc.len(), 1, "only the box is left");
        assert_eq!(s.doc.layers.len(), before - LAYERS.len(), "context layers dropped");
        s.run(Command::Redo).unwrap();
        let objects = |s: &Session| s.doc.objects().cloned().collect::<Vec<_>>();
        assert_eq!(objects(&s), objects(&reopened), "redo restores the same objects");
        assert_eq!(s.doc.layers, reopened.doc.layers);
    }

    /// A context building between the sun and a box shades it: analyses read
    /// the `context` layer as occluders (only overlay layers are skipped).
    #[test]
    fn sunhours_sees_context_buildings_as_occluders() {
        // A 60 m tower 10-30 m south of the site (lat 0.0001° ≈ 11 m).
        let tower = br#"{"elements":[{"type":"way","id":1,"tags":{"building":"yes","height":"60"},
          "geometry":[{"lat":39.99973,"lon":-0.0001},{"lat":39.99973,"lon":0.00015},
                      {"lat":39.99991,"lon":0.00015},{"lat":39.99991,"lon":-0.0001},
                      {"lat":39.99973,"lon":-0.0001}]}]}"#;
        let hours = |with_context: bool| {
            let mut s = Session::default();
            s.run(parse("box 0,0,0 4,4,4").unwrap()).unwrap();
            s.run(parse("location 40.0 0.0 0").unwrap()).unwrap();
            if with_context {
                s.set_osm_fetcher(Some(stub(Ok(tower.to_vec()))));
                s.run(parse("osmfetch 100").unwrap()).unwrap();
                assert_eq!(on_layer(&s, LAYER_BUILDINGS), 1);
            }
            let box_id = s.doc.objects().find(|o| o.layer != LAYER_BUILDINGS).unwrap().id;
            s.run(Command::FaceSunHours {
                targets: crate::Selector::Ids { ids: vec![box_id] },
                ids: None,
                year: 2024,
                month: 12,
                day: 21,
                include_locked: true,
                skip_analysis: true,
                skip_shadows: true,
                per_mesh: true,
            })
            .unwrap();
            s.doc.analysis_reports["facesunhours"].avg
        };
        let (open, shaded) = (hours(false), hours(true));
        assert!(shaded < open, "context tower shades the box: {shaded} vs {open}");
    }

    #[test]
    fn stitches_relation_outer_members_into_a_ring() {
        let rel = br#"{"elements":[{"type":"relation","id":9,"tags":{"type":"multipolygon","leisure":"park"},
          "members":[
            {"type":"way","ref":1,"role":"outer","geometry":[{"lat":0,"lon":0},{"lat":0,"lon":0.001}]},
            {"type":"way","ref":2,"role":"outer","geometry":[{"lat":0.001,"lon":0},{"lat":0,"lon":0}]},
            {"type":"way","ref":3,"role":"outer","geometry":[{"lat":0.001,"lon":0},{"lat":0.001,"lon":0.001},{"lat":0,"lon":0.001}]},
            {"type":"way","ref":4,"role":"inner","geometry":[{"lat":0.0004,"lon":0.0004},{"lat":0.0005,"lon":0.0004}]}
          ]}]}"#;
        let items = context_items(rel, GeoOrigin { lat_deg: 0.0, lon_deg: 0.0 }, 400.0, &|_, _| None).unwrap();
        assert_eq!(items.len(), 1);
        let ContextGeometry::Polyline { points, closed: true } = &items[0].geometry else { panic!() };
        assert_eq!((items[0].layer.as_str(), points.len()), (LAYER_GREEN, 4));
    }

    #[test]
    fn roads_and_buildings_follow_the_terrain() {
        let osm = br#"{"elements":[
          {"type":"way","id":1,"tags":{"highway":"service"},"geometry":[{"lat":0,"lon":0},{"lat":0,"lon":0.0002}]},
          {"type":"way","id":2,"tags":{"building":"yes"},"geometry":[{"lat":0,"lon":0},{"lat":0,"lon":0.0001},
            {"lat":0.0001,"lon":0.0001},{"lat":0,"lon":0}]}]}"#;
        let ground = |x: f64, _y: f64| Some(x / 10.0);
        let items = context_items(osm, GeoOrigin { lat_deg: 0.0, lon_deg: 0.0 }, 400.0, &ground).unwrap();
        let ContextGeometry::Polyline { points, .. } = &items[0].geometry else { panic!() };
        assert!((points[1].z - (points[1].x / 10.0 + DRAPE_LIFT_M)).abs() < 1e-9, "draped road");
        let ContextGeometry::Mesh { positions, .. } = &items[1].geometry else { panic!() };
        let low = positions.iter().map(|p| p.z).fold(f64::INFINITY, f64::min);
        assert!(low.abs() < 1e-9, "building base at its lowest ground point");
    }

    #[test]
    fn clip_splits_a_line_crossing_the_circle() {
        let pts = [DVec2::new(-20.0, 0.0), DVec2::new(20.0, 0.0), DVec2::new(20.0, 5.0)];
        let runs = clip_to_circle(&pts, 10.0);
        assert_eq!(runs.len(), 1);
        assert!((runs[0][0].x + 10.0).abs() < 1e-9 && (runs[0][1].x - 10.0).abs() < 1e-9);
    }
}
