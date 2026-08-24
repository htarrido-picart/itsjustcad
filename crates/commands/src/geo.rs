//! Site geometry importers: GeoJSON features, CSV survey points, and Overpass
//! (OpenStreetMap) building footprints. Pure parsing + geometry — no I/O, no
//! network. The `exec` layer reads a file's bytes and calls these, then expands
//! the result into substrate ops (MeshLiteral / Polyline / Circle) so the
//! op-log — not the source file — is the record.
//!
//! Coordinate heuristic: GeoJSON is nominally lon/lat degrees. If the document
//! has a geo origin (set by EPW/`sun`/`location`), coordinates within ±180° are
//! projected to local meters by a simple equirectangular map about the origin.
//! Coordinates whose magnitude exceeds ±180 are treated as already-local xy
//! meters and pass through unchanged. With no origin, everything is local xy.

use glam::{DVec2, DVec3};
use kernel_mesh::{triangulate, Mesh};

const EARTH_RADIUS_M: f64 = 6_378_137.0;

/// A geo origin used to project lon/lat degrees to local meters.
#[derive(Clone, Copy, Debug)]
pub struct GeoOrigin {
    pub lat_deg: f64,
    pub lon_deg: f64,
}

impl GeoOrigin {
    /// Equirectangular projection of a lon/lat point to local meters, with the
    /// origin mapping to (0,0). Good enough for a site-scale context model.
    fn project(&self, lon_deg: f64, lat_deg: f64) -> DVec2 {
        let lat0 = self.lat_deg.to_radians();
        let x = (lon_deg - self.lon_deg).to_radians() * EARTH_RADIUS_M * lat0.cos();
        let y = (lat_deg - self.lat_deg).to_radians() * EARTH_RADIUS_M;
        DVec2::new(x, y)
    }
}

/// Map one raw GeoJSON `[x, y]` coordinate to local meters using the heuristic.
fn to_local(coord: [f64; 2], origin: Option<GeoOrigin>) -> DVec2 {
    match origin {
        Some(o) if coord[0].abs() <= 180.0 && coord[1].abs() <= 90.0 => {
            o.project(coord[0], coord[1])
        }
        // No origin, or coords beyond degree range → treat as local xy meters.
        _ => DVec2::new(coord[0], coord[1]),
    }
}

// ---- GeoJSON feature import ----

/// One imported feature, already projected to local meters.
#[derive(Clone, Debug, PartialEq)]
pub enum GeoFeature {
    /// Closed ring (Polygon outer boundary). First point is not repeated.
    Polygon { name: Option<String>, ring: Vec<DVec2> },
    /// Open polyline (LineString).
    Line { name: Option<String>, points: Vec<DVec2> },
    /// A single point (Point geometry).
    Point { name: Option<String>, at: DVec2 },
}

/// Parse a GeoJSON document into features projected to local meters.
///
/// Supports FeatureCollection, single Feature, and bare geometry objects.
/// Polygon (outer ring only), LineString, and Point are handled; MultiPolygon /
/// MultiLineString expand into one feature per member. `properties.name`
/// becomes the feature name. Unsupported geometry types are skipped.
pub fn parse_geojson(bytes: &[u8], origin: Option<GeoOrigin>) -> Result<Vec<GeoFeature>, String> {
    let root: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("GeoJSON parse error: {e}"))?;

    let mut out = Vec::new();
    match root.get("type").and_then(|t| t.as_str()) {
        Some("FeatureCollection") => {
            let features = root
                .get("features")
                .and_then(|f| f.as_array())
                .ok_or("FeatureCollection missing 'features' array")?;
            for f in features {
                push_feature(f, origin, &mut out);
            }
        }
        Some("Feature") => push_feature(&root, origin, &mut out),
        // Bare geometry object.
        Some(_) => push_geometry(&root, None, origin, &mut out),
        None => return Err("GeoJSON has no 'type'".to_string()),
    }
    Ok(out)
}

fn feature_name(f: &serde_json::Value) -> Option<String> {
    f.get("properties")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .map(|s| s.to_string())
}

fn push_feature(f: &serde_json::Value, origin: Option<GeoOrigin>, out: &mut Vec<GeoFeature>) {
    let name = feature_name(f);
    if let Some(geom) = f.get("geometry") {
        push_geometry(geom, name, origin, out);
    }
}

fn push_geometry(
    geom: &serde_json::Value,
    name: Option<String>,
    origin: Option<GeoOrigin>,
    out: &mut Vec<GeoFeature>,
) {
    let Some(gtype) = geom.get("type").and_then(|t| t.as_str()) else {
        return;
    };
    let coords = geom.get("coordinates");
    match gtype {
        "Point" => {
            if let Some(c) = coords.and_then(coord_pair) {
                out.push(GeoFeature::Point { name, at: to_local(c, origin) });
            }
        }
        "LineString" => {
            if let Some(pts) = coords.and_then(|c| coord_seq(c, origin))
                && pts.len() >= 2
            {
                out.push(GeoFeature::Line { name, points: pts });
            }
        }
        "Polygon" => {
            // First ring is the outer boundary; holes are ignored.
            if let Some(ring) = coords
                .and_then(|c| c.as_array())
                .and_then(|rings| rings.first())
                .and_then(|r| coord_seq(r, origin))
                .and_then(close_ring)
            {
                out.push(GeoFeature::Polygon { name, ring });
            }
        }
        "MultiLineString" => {
            if let Some(parts) = coords.and_then(|c| c.as_array()) {
                for p in parts {
                    if let Some(pts) = coord_seq(p, origin)
                        && pts.len() >= 2
                    {
                        out.push(GeoFeature::Line { name: name.clone(), points: pts });
                    }
                }
            }
        }
        "MultiPolygon" => {
            if let Some(polys) = coords.and_then(|c| c.as_array()) {
                for poly in polys {
                    if let Some(ring) = poly
                        .as_array()
                        .and_then(|rings| rings.first())
                        .and_then(|r| coord_seq(r, origin))
                        .and_then(close_ring)
                    {
                        out.push(GeoFeature::Polygon { name: name.clone(), ring });
                    }
                }
            }
        }
        _ => { /* GeometryCollection, MultiPoint, etc.: skipped */ }
    }
}

/// A ring from GeoJSON repeats its first vertex at the end; drop the duplicate
/// so it matches our unrepeated-first-point convention. Returns `None` for
/// rings with fewer than 3 distinct points.
fn close_ring(mut ring: Vec<DVec2>) -> Option<Vec<DVec2>> {
    if ring.len() >= 2 && ring.first() == ring.last() {
        ring.pop();
    }
    if ring.len() >= 3 { Some(ring) } else { None }
}

fn coord_pair(v: &serde_json::Value) -> Option<[f64; 2]> {
    let arr = v.as_array()?;
    Some([arr.first()?.as_f64()?, arr.get(1)?.as_f64()?])
}

fn coord_seq(v: &serde_json::Value, origin: Option<GeoOrigin>) -> Option<Vec<DVec2>> {
    let arr = v.as_array()?;
    Some(arr.iter().filter_map(coord_pair).map(|c| to_local(c, origin)).collect())
}

// ---- terrain from CSV points ----

/// Parse a CSV of `x,y,z` survey points (header row optional; non-numeric first
/// row is skipped). Blank lines and `#` comments are ignored.
pub fn parse_csv_points(text: &str) -> Result<Vec<DVec3>, String> {
    let mut pts = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split(&[',', ';', '\t'][..]).map(|c| c.trim()).collect();
        if cols.len() < 3 {
            return Err(format!("line {}: expected x,y,z (got {} columns)", i + 1, cols.len()));
        }
        let parse3 = || -> Option<DVec3> {
            Some(DVec3::new(
                cols[0].parse().ok()?,
                cols[1].parse().ok()?,
                cols[2].parse().ok()?,
            ))
        };
        match parse3() {
            Some(p) => pts.push(p),
            None if i == 0 => continue, // header row
            None => return Err(format!("line {}: non-numeric coordinate", i + 1)),
        }
    }
    Ok(pts)
}

/// Delaunay-triangulate scattered 3D points (by their XY) into a terrain mesh,
/// keeping each vertex's z. Fewer than 3 points → error.
pub fn terrain_from_points(pts: &[DVec3]) -> Result<Mesh, String> {
    if pts.len() < 3 {
        return Err(format!("terrain needs at least 3 points, got {}", pts.len()));
    }
    let xy: Vec<DVec2> = pts.iter().map(|p| p.truncate()).collect();
    let faces = triangulate(&xy);
    if faces.is_empty() {
        return Err("terrain points are collinear or coincident — no surface".to_string());
    }
    Ok(Mesh::new(pts.to_vec(), faces))
}

/// Build a terrain mesh from GeoJSON contour LineStrings, each carrying an
/// elevation. `elevation` reads the z from a feature's properties (e.g. an
/// "elevation"/"ele" tag); vertices from every contour are pooled and
/// triangulated by XY. Returns the sampled 3D points and the mesh.
pub fn terrain_from_contours(
    bytes: &[u8],
    origin: Option<GeoOrigin>,
    elevation_key: &str,
) -> Result<Mesh, String> {
    let root: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("GeoJSON parse error: {e}"))?;
    let features = match root.get("type").and_then(|t| t.as_str()) {
        Some("FeatureCollection") => root
            .get("features")
            .and_then(|f| f.as_array())
            .ok_or("FeatureCollection missing 'features'")?
            .clone(),
        Some("Feature") => vec![root.clone()],
        _ => return Err("terrain contours need a FeatureCollection or Feature".to_string()),
    };

    let mut pts: Vec<DVec3> = Vec::new();
    for f in &features {
        let ele = f
            .get("properties")
            .and_then(|p| p.get(elevation_key))
            .and_then(|e| e.as_f64())
            .ok_or_else(|| format!("a contour feature is missing numeric '{elevation_key}'"))?;
        let Some(geom) = f.get("geometry") else { continue };
        let gtype = geom.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let coords = geom.get("coordinates");
        let mut lines: Vec<Vec<DVec2>> = Vec::new();
        match gtype {
            "LineString" => {
                if let Some(seq) = coords.and_then(|c| coord_seq(c, origin)) {
                    lines.push(seq);
                }
            }
            "MultiLineString" => {
                if let Some(parts) = coords.and_then(|c| c.as_array()) {
                    for p in parts {
                        if let Some(seq) = coord_seq(p, origin) {
                            lines.push(seq);
                        }
                    }
                }
            }
            _ => {}
        }
        for seq in lines {
            for p in seq {
                pts.push(DVec3::new(p.x, p.y, ele));
            }
        }
    }
    terrain_from_points(&pts)
}

// ---- OSM / Overpass building footprints ----

/// One building footprint recovered from an Overpass JSON export: a closed
/// outer ring (local meters) and its extrusion height in meters.
#[derive(Clone, Debug, PartialEq)]
pub struct Building {
    pub name: Option<String>,
    pub ring: Vec<DVec2>,
    pub height_m: f64,
}

/// Default storey height when a building has no height/levels tag.
pub const DEFAULT_BUILDING_HEIGHT_M: f64 = 9.0;

/// Parse an Overpass API JSON export (`overpass-api.de/api/interpreter` output)
/// into building footprints. Handles the `elements` array: `way` elements with
/// a `building` tag and inline `geometry` (lat/lon per node). Height comes from
/// the `height` tag, else `building:levels` × 3 m, else the default.
pub fn parse_overpass(bytes: &[u8], origin: Option<GeoOrigin>) -> Result<Vec<Building>, String> {
    let root: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("Overpass JSON parse error: {e}"))?;
    let elements = root
        .get("elements")
        .and_then(|e| e.as_array())
        .ok_or("Overpass JSON missing 'elements' array")?;

    let mut out = Vec::new();
    for el in elements {
        if el.get("type").and_then(|t| t.as_str()) != Some("way") {
            continue;
        }
        let tags = el.get("tags");
        let is_building = tags
            .and_then(|t| t.get("building"))
            .is_some_and(|b| !b.is_null());
        if !is_building {
            continue;
        }
        let Some(geom) = el.get("geometry").and_then(|g| g.as_array()) else {
            continue; // needs `out geom;` in the Overpass query
        };
        let ring: Vec<DVec2> = geom
            .iter()
            .filter_map(|n| {
                Some(to_local(
                    [n.get("lon")?.as_f64()?, n.get("lat")?.as_f64()?],
                    origin,
                ))
            })
            .collect();
        let Some(ring) = close_ring(ring) else { continue };

        let height_m = building_height(tags);
        let name = tags
            .and_then(|t| t.get("name"))
            .and_then(|n| n.as_str())
            .map(|s| s.to_string());
        out.push(Building { name, ring, height_m });
    }
    Ok(out)
}

fn building_height(tags: Option<&serde_json::Value>) -> f64 {
    let Some(tags) = tags else {
        return DEFAULT_BUILDING_HEIGHT_M;
    };
    // `height` may be "12" or "12 m"; take the leading number.
    if let Some(h) = tags.get("height").and_then(|v| v.as_str())
        && let Some(n) = h.split_whitespace().next().and_then(|s| s.parse::<f64>().ok())
        && n > 0.0
    {
        return n;
    }
    if let Some(levels) = tags.get("building:levels").and_then(|v| v.as_str())
        && let Ok(n) = levels.trim().parse::<f64>()
        && n > 0.0
    {
        return n * 3.0;
    }
    DEFAULT_BUILDING_HEIGHT_M
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geojson_polygon_line_point_counts() {
        let gj = br#"{
          "type": "FeatureCollection",
          "features": [
            {"type":"Feature","properties":{"name":"lot"},
             "geometry":{"type":"Polygon","coordinates":[[[0,0],[10,0],[10,10],[0,10],[0,0]]]}},
            {"type":"Feature","properties":{},
             "geometry":{"type":"LineString","coordinates":[[0,0],[5,5],[10,0]]}},
            {"type":"Feature","properties":{"name":"marker"},
             "geometry":{"type":"Point","coordinates":[3,3]}}
          ]
        }"#;
        let feats = parse_geojson(gj, None).unwrap();
        assert_eq!(feats.len(), 3);
        match &feats[0] {
            GeoFeature::Polygon { name, ring } => {
                assert_eq!(name.as_deref(), Some("lot"));
                assert_eq!(ring.len(), 4, "closing vertex dropped");
            }
            _ => panic!("first feature should be a polygon"),
        }
        assert!(matches!(feats[1], GeoFeature::Line { .. }));
        match &feats[2] {
            GeoFeature::Point { name, .. } => assert_eq!(name.as_deref(), Some("marker")),
            _ => panic!("third feature should be a point"),
        }
    }

    #[test]
    fn geojson_local_xy_passthrough_beyond_degree_range() {
        // Coords > 180 pass through as local meters even with an origin set.
        let gj = br#"{"type":"Feature","properties":{},
          "geometry":{"type":"Point","coordinates":[500,600]}}"#;
        let origin = Some(GeoOrigin { lat_deg: 40.0, lon_deg: -74.0 });
        let feats = parse_geojson(gj, origin).unwrap();
        match &feats[0] {
            GeoFeature::Point { at, .. } => assert_eq!(*at, DVec2::new(500.0, 600.0)),
            _ => panic!("expected point"),
        }
    }

    #[test]
    fn geojson_projects_lonlat_with_origin() {
        // A point 0.001° east of the origin projects to a small positive x.
        let gj = br#"{"type":"Feature","properties":{},
          "geometry":{"type":"Point","coordinates":[-73.999,40.0]}}"#;
        let origin = Some(GeoOrigin { lat_deg: 40.0, lon_deg: -74.0 });
        let feats = parse_geojson(gj, origin).unwrap();
        match &feats[0] {
            GeoFeature::Point { at, .. } => {
                assert!(at.x > 50.0 && at.x < 120.0, "0.001° east ≈ 85 m: {}", at.x);
                assert!(at.y.abs() < 1e-6, "same latitude → y≈0");
            }
            _ => panic!("expected point"),
        }
    }

    #[test]
    fn csv_points_with_header() {
        let csv = "x,y,z\n0,0,0\n10,0,1\n0,10,2\n5,5,3\n";
        let pts = parse_csv_points(csv).unwrap();
        assert_eq!(pts.len(), 4);
        assert_eq!(pts[0], DVec3::new(0.0, 0.0, 0.0));
    }

    #[test]
    fn csv_points_no_header() {
        let csv = "0,0,0\n10,0,1\n0,10,2\n";
        assert_eq!(parse_csv_points(csv).unwrap().len(), 3);
    }

    #[test]
    fn terrain_square_center_four_triangles() {
        let pts = vec![
            DVec3::new(0.0, 0.0, 0.0),
            DVec3::new(1.0, 0.0, 0.0),
            DVec3::new(1.0, 1.0, 0.0),
            DVec3::new(0.0, 1.0, 0.0),
            DVec3::new(0.5, 0.5, 2.0),
        ];
        let mesh = terrain_from_points(&pts).unwrap();
        assert_eq!(mesh.faces().len(), 4);
        assert_eq!(mesh.positions().len(), 5, "z preserved on all vertices");
        assert_eq!(mesh.positions()[4].z, 2.0, "peak keeps its elevation");
    }

    #[test]
    fn terrain_needs_three_points() {
        let pts = vec![DVec3::ZERO, DVec3::X];
        assert!(terrain_from_points(&pts).is_err());
    }

    #[test]
    fn terrain_from_geojson_contours() {
        // Two elevation contours; pooled vertices triangulate to a surface.
        let gj = br#"{
          "type":"FeatureCollection",
          "features":[
            {"type":"Feature","properties":{"ele":0},
             "geometry":{"type":"LineString","coordinates":[[0,0],[10,0],[10,10]]}},
            {"type":"Feature","properties":{"ele":5},
             "geometry":{"type":"LineString","coordinates":[[2,2],[8,2],[8,8]]}}
          ]
        }"#;
        let mesh = terrain_from_contours(gj, None, "ele").unwrap();
        assert!(!mesh.faces().is_empty());
        // The high contour's z must appear in the mesh.
        assert!(mesh.positions().iter().any(|p| (p.z - 5.0).abs() < 1e-9));
    }

    #[test]
    fn overpass_building_footprint_counts() {
        let osm = br#"{
          "elements":[
            {"type":"way","id":1,"tags":{"building":"yes","height":"12"},
             "geometry":[{"lat":0,"lon":0},{"lat":0,"lon":0.0001},
                         {"lat":0.0001,"lon":0.0001},{"lat":0.0001,"lon":0},
                         {"lat":0,"lon":0}]},
            {"type":"way","id":2,"tags":{"highway":"residential"},
             "geometry":[{"lat":0,"lon":0},{"lat":0,"lon":0.001}]},
            {"type":"way","id":3,"tags":{"building":"house","building:levels":"3"},
             "geometry":[{"lat":1,"lon":1},{"lat":1,"lon":1.0001},
                         {"lat":1.0001,"lon":1},{"lat":1,"lon":1}]}
          ]
        }"#;
        let origin = Some(GeoOrigin { lat_deg: 0.0, lon_deg: 0.0 });
        let bldgs = parse_overpass(osm, origin).unwrap();
        assert_eq!(bldgs.len(), 2, "only the two building ways, not the highway");
        assert_eq!(bldgs[0].height_m, 12.0, "explicit height tag");
        assert_eq!(bldgs[1].height_m, 9.0, "3 levels × 3 m");
        assert_eq!(bldgs[0].ring.len(), 4, "closing vertex dropped");
    }

    #[test]
    fn overpass_default_height_when_untagged() {
        let osm = br#"{"elements":[
          {"type":"way","id":1,"tags":{"building":"yes"},
           "geometry":[{"lat":0,"lon":0},{"lat":0,"lon":0.0001},
                       {"lat":0.0001,"lon":0},{"lat":0,"lon":0}]}
        ]}"#;
        let bldgs = parse_overpass(osm, Some(GeoOrigin { lat_deg: 0.0, lon_deg: 0.0 })).unwrap();
        assert_eq!(bldgs.len(), 1);
        assert_eq!(bldgs[0].height_m, DEFAULT_BUILDING_HEIGHT_M);
    }
}
