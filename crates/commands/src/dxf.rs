//! DXF R12 (AC1009) ASCII export: the whole document as flat entities.
//! Hand-written — R12 is a simple tagged text format (group code line, value
//! line), which keeps the crate dependency-free. Lines, polylines, circles
//! and arcs export exactly; ellipses and NURBS tessellate (R12 has neither);
//! meshes export their feature edges as LINE entities.

use glam::DVec3;
use mydrafter_doc::{Annotation, Document, Geometry};

/// Chord tolerance for tessellating curves R12 cannot represent (meters).
const EXPORT_TOL: f64 = 0.005;

/// Feature edges of a mesh (see [`kernel_mesh::feature_edges`]). Shared with
/// the PDF exporter; the viewport wireframe uses the kernel function directly.
pub(crate) use kernel_mesh::feature_edges as mesh_feature_edges;

/// Tag writer: one "group code, value" pair per call, each on its own line.
struct Tags(String);

impl Tags {
    fn tag(&mut self, code: i32, value: &str) {
        self.0.push_str(&format!("{code}\n{value}\n"));
    }

    fn num(&mut self, code: i32, value: f64) {
        // Enough digits to round-trip drafting coordinates; trailing zeros
        // trimmed so files stay small and diffs readable.
        let mut s = format!("{value:.9}");
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.push('0');
        }
        self.tag(code, &s);
    }

    fn point(&mut self, p: DVec3) {
        self.num(10, p.x);
        self.num(20, p.y);
        self.num(30, p.z);
    }
}

/// DXF layer names: letters, digits and a few punctuation marks only.
fn dxf_layer(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '$') {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    if clean.is_empty() { "0".to_string() } else { clean }
}

fn line(t: &mut Tags, layer: &str, a: DVec3, b: DVec3) {
    t.tag(0, "LINE");
    t.tag(8, layer);
    t.point(a);
    t.num(11, b.x);
    t.num(21, b.y);
    t.num(31, b.z);
}

/// R12 3D polyline: POLYLINE header + one VERTEX per point + SEQEND.
fn polyline(t: &mut Tags, layer: &str, points: &[DVec3], closed: bool) {
    t.tag(0, "POLYLINE");
    t.tag(8, layer);
    t.tag(66, "1"); // vertices follow
    // 8 = 3D polyline; +1 when closed.
    t.tag(70, if closed { "9" } else { "8" });
    for p in points {
        t.tag(0, "VERTEX");
        t.tag(8, layer);
        t.point(*p);
        t.tag(70, "32"); // 3D polyline vertex
    }
    t.tag(0, "SEQEND");
    t.tag(8, layer);
}

fn text(t: &mut Tags, layer: &str, pos: DVec3, height: f64, content: &str) {
    t.tag(0, "TEXT");
    t.tag(8, layer);
    t.point(pos);
    t.num(40, height);
    t.tag(1, content);
}

/// One document object -> zero or more entities. Returns entities written.
fn entity(t: &mut Tags, layer: &str, geometry: &Geometry, units: mydrafter_doc::Units) -> usize {
    match geometry {
        Geometry::Curve(curve) => match curve {
            kernel_curve::Curve::Line { a, b } => {
                line(t, layer, *a, *b);
                1
            }
            kernel_curve::Curve::Polyline { points, closed } => {
                polyline(t, layer, points, *closed);
                1
            }
            kernel_curve::Curve::Arc { center, radius, start, end } => {
                if curve.is_closed() {
                    t.tag(0, "CIRCLE");
                    t.tag(8, layer);
                    t.point(*center);
                    t.num(40, *radius);
                } else {
                    t.tag(0, "ARC");
                    t.tag(8, layer);
                    t.point(*center);
                    t.num(40, *radius);
                    // DXF arcs run CCW from 50 to 51, degrees.
                    t.num(50, start.to_degrees().rem_euclid(360.0));
                    t.num(51, end.to_degrees().rem_euclid(360.0));
                }
                1
            }
            // R12 has no ELLIPSE or SPLINE: tessellate to a closed/open polyline.
            kernel_curve::Curve::Ellipse { .. } | kernel_curve::Curve::Nurbs { .. } => {
                polyline(t, layer, &curve.tessellate(EXPORT_TOL), curve.is_closed());
                1
            }
        },
        Geometry::Mesh(mesh) => {
            let edges = mesh_feature_edges(mesh);
            let n = edges.len();
            for (a, b) in edges {
                line(t, layer, a, b);
            }
            n
        }
        Geometry::Annotation(a) => match a {
            Annotation::LinearDim { a, b, offset } => {
                // Dimension line offset to the left of a->b, value as TEXT.
                let dir = (*b - *a).normalize_or_zero();
                let left = DVec3::new(-dir.y, dir.x, 0.0) * *offset;
                line(t, layer, *a + left, *b + left);
                let mid = (*a + *b) / 2.0 + left;
                text(t, layer, mid, 0.2, &mydrafter_doc::format_length(units, (*b - *a).length()));
                2
            }
            Annotation::Text { pos, text: s, height } => {
                text(t, layer, *pos, *height, s);
                1
            }
            Annotation::Hatch { boundary, .. } => {
                // Pattern dropped; the boundary survives as a closed polyline.
                polyline(t, layer, boundary, true);
                1
            }
        },
        // Block instances: exported as the boundary box only (block definitions
        // are resolved by the renderer, not by DXF export — R12 has no XREF).
        Geometry::Instance { .. } => 0,
    }
}

/// Build the complete DXF text for a document. Returns the file text and the
/// number of entities written.
pub fn document_dxf(doc: &Document) -> (String, usize) {
    let mut t = Tags(String::new());

    t.tag(0, "SECTION");
    t.tag(2, "HEADER");
    t.tag(9, "$ACADVER");
    t.tag(1, "AC1009"); // R12
    t.tag(0, "ENDSEC");

    // Layer table so receivers list the document's layers by name.
    t.tag(0, "SECTION");
    t.tag(2, "TABLES");
    t.tag(0, "TABLE");
    t.tag(2, "LAYER");
    t.tag(70, &doc.layers.len().to_string());
    for (name, style) in &doc.layers {
        t.tag(0, "LAYER");
        t.tag(2, &dxf_layer(name));
        t.tag(70, "0");
        t.tag(62, "7");
        t.tag(6, "CONTINUOUS");
        // Code 370: lineweight in hundredths of a mm (ISO standard values).
        // Round to the nearest hundredth and clamp to sane drafting range.
        let lw_hundredths = (style.lineweight_mm * 100.0).round().clamp(0.0, 211.0) as i32;
        t.tag(370, &lw_hundredths.to_string());
    }
    t.tag(0, "ENDTAB");
    t.tag(0, "ENDSEC");

    t.tag(0, "SECTION");
    t.tag(2, "ENTITIES");
    let mut count = 0usize;
    for obj in doc.objects() {
        count += entity(&mut t, &dxf_layer(&obj.layer), &obj.geometry, doc.units);
    }
    t.tag(0, "ENDSEC");
    t.tag(0, "EOF");

    (t.0, count)
}

// ---------- import ----------

/// Entities scanned from a DXF ENTITIES section: one substrate command per
/// supported entity, tagged with its (lowercased) layer name, plus the count
/// of entities skipped because their kind is unsupported or their record is
/// malformed.
pub struct DxfEntities {
    pub entities: Vec<(String, crate::Command)>,
    pub skipped: usize,
}

/// Parse DXF text (R12 and later — extra 2000+ codes like handles and 100
/// subclass markers are ignored) into substrate commands. Supported: LINE,
/// LWPOLYLINE, POLYLINE/VERTEX/SEQEND, CIRCLE, ARC, TEXT. Everything else is
/// skipped silently and counted.
pub fn parse_dxf(text: &str) -> Result<DxfEntities, String> {
    // The whole file is "group code line, value line" pairs.
    let mut pairs: Vec<(i32, &str)> = Vec::new();
    let mut lines = text.lines();
    while let Some(code) = lines.next() {
        let code = code.trim();
        if code.is_empty() && lines.clone().next().is_none() {
            break; // tolerate a trailing blank line
        }
        let Some(value) = lines.next() else {
            return Err("truncated DXF: group code without a value line".to_string());
        };
        let code: i32 = code
            .parse()
            .map_err(|_| format!("not a DXF: expected an integer group code, got '{code}'"))?;
        pairs.push((code, value.trim()));
    }

    // Cut the ENTITIES section into records: each starts at a 0 code.
    let mut records: Vec<(&str, Vec<(i32, &str)>)> = Vec::new();
    let mut in_entities = false;
    let mut awaiting_section_name = false;
    for &(code, value) in &pairs {
        match code {
            0 if value == "SECTION" => awaiting_section_name = true,
            2 if awaiting_section_name => {
                in_entities = value == "ENTITIES";
                awaiting_section_name = false;
            }
            0 if value == "ENDSEC" => in_entities = false,
            0 if in_entities => records.push((value, Vec::new())),
            _ if in_entities => {
                if let Some((_, fields)) = records.last_mut() {
                    fields.push((code, value));
                }
            }
            _ => {}
        }
    }

    // Fold records into commands; VERTEX/SEQEND attach to the open POLYLINE.
    let mut out = DxfEntities { entities: Vec::new(), skipped: 0 };
    let mut open_poly: Option<(String, bool, Vec<DVec3>)> = None; // layer, closed, points
    for (name, fields) in records {
        if let Some((layer, closed, points)) = &mut open_poly {
            match name {
                "VERTEX" => {
                    if let Some(p) = record_point(&fields, 10) {
                        points.push(p);
                    }
                    continue;
                }
                "SEQEND" => {
                    let (layer, closed, points) =
                        (layer.clone(), *closed, std::mem::take(points));
                    open_poly = None;
                    if points.len() >= 2 {
                        out.entities.push((
                            layer,
                            crate::Command::Polyline { id: None, points, closed },
                        ));
                    } else {
                        out.skipped += 1;
                    }
                    continue;
                }
                // Unterminated POLYLINE: drop it, fall through to `name`.
                _ => {
                    open_poly = None;
                    out.skipped += 1;
                }
            }
        }
        match record_entity(name, &fields, &mut open_poly) {
            RecordOutcome::Entity(layer, cmd) => out.entities.push((layer, cmd)),
            RecordOutcome::PolyOpened => {}
            RecordOutcome::Skipped => out.skipped += 1,
        }
    }
    if open_poly.is_some() {
        out.skipped += 1; // POLYLINE never closed by SEQEND before EOF
    }
    Ok(out)
}

enum RecordOutcome {
    Entity(String, crate::Command),
    PolyOpened,
    Skipped,
}

/// One non-VERTEX record -> command (or open a POLYLINE / skip it).
fn record_entity(
    name: &str,
    fields: &[(i32, &str)],
    open_poly: &mut Option<(String, bool, Vec<DVec3>)>,
) -> RecordOutcome {
    use crate::Command;
    let layer = record_layer(fields);
    let cmd = match name {
        "LINE" => match (record_point(fields, 10), record_point(fields, 11)) {
            (Some(a), Some(b)) => Some(Command::Line { id: None, a, b }),
            _ => None,
        },
        "CIRCLE" => match (record_point(fields, 10), record_num(fields, 40)) {
            (Some(center), Some(radius)) if radius > 0.0 => {
                Some(Command::Circle { id: None, center, radius })
            }
            _ => None,
        },
        "ARC" => match (
            record_point(fields, 10),
            record_num(fields, 40),
            record_num(fields, 50),
            record_num(fields, 51),
        ) {
            (Some(center), Some(radius), Some(start), Some(mut end)) if radius > 0.0 => {
                if end <= start {
                    end += 360.0; // DXF arcs run CCW from 50 to 51
                }
                Some(Command::Arc { id: None, center, radius, start_deg: start, end_deg: end })
            }
            _ => None,
        },
        "TEXT" => match (record_point(fields, 10), record_num(fields, 40)) {
            (Some(pos), Some(height)) if height > 0.0 => {
                fields.iter().find(|(c, _)| *c == 1).map(|(_, v)| Command::Text {
                    id: None,
                    pos,
                    text: (*v).to_string(),
                    height,
                })
            }
            _ => None,
        },
        "LWPOLYLINE" => {
            // Vertices are repeated 10/20 pairs in order; 38 = elevation (z).
            let z = record_num(fields, 38).unwrap_or(0.0);
            let mut points = Vec::new();
            let mut x: Option<f64> = None;
            for (code, value) in fields {
                match (code, value.parse::<f64>()) {
                    (10, Ok(v)) => x = Some(v),
                    (20, Ok(y)) => {
                        if let Some(x) = x.take() {
                            points.push(DVec3::new(x, y, z));
                        }
                    }
                    _ => {}
                }
            }
            let closed = record_num(fields, 70).unwrap_or(0.0) as i64 & 1 != 0;
            (points.len() >= 2).then_some(Command::Polyline { id: None, points, closed })
        }
        "POLYLINE" => {
            let closed = record_num(fields, 70).unwrap_or(0.0) as i64 & 1 != 0;
            *open_poly = Some((layer, closed, Vec::new()));
            return RecordOutcome::PolyOpened;
        }
        _ => None,
    };
    match cmd {
        Some(cmd) => RecordOutcome::Entity(layer, cmd),
        None => RecordOutcome::Skipped,
    }
}

/// Layer (code 8), lowercased to match the document's naming style — our own
/// exporter uppercases, so export -> import round-trips layer names.
fn record_layer(fields: &[(i32, &str)]) -> String {
    fields
        .iter()
        .find(|(c, _)| *c == 8)
        .map(|(_, v)| v.to_ascii_lowercase())
        .unwrap_or_else(|| "0".to_string())
}

fn record_num(fields: &[(i32, &str)], code: i32) -> Option<f64> {
    fields.iter().find(|(c, _)| *c == code).and_then(|(_, v)| v.parse().ok())
}

/// Point at `base`/(base+10)/(base+20); a missing z reads as 0 (2D files).
fn record_point(fields: &[(i32, &str)], base: i32) -> Option<DVec3> {
    Some(DVec3::new(
        record_num(fields, base)?,
        record_num(fields, base + 10)?,
        record_num(fields, base + 20).unwrap_or(0.0),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse, Session};

    fn run(s: &mut Session, line: &str) {
        s.run(parse(line).unwrap()).unwrap();
    }

    /// Minimal reimport check: pair up "code\nvalue" lines and collect the
    /// entity names that follow each `0` code inside ENTITIES.
    fn scan_entities(dxf: &str) -> Vec<String> {
        let lines: Vec<&str> = dxf.lines().collect();
        assert!(lines.len().is_multiple_of(2), "tags come in pairs");
        let mut entities = Vec::new();
        let mut in_entities = false;
        for pair in lines.chunks(2) {
            let (code, value) = (pair[0].trim(), pair[1].trim());
            code.parse::<i32>().expect("group codes are integers");
            if code == "2" && value == "ENTITIES" {
                in_entities = true;
            }
            if code == "0" {
                match value {
                    "ENDSEC" => in_entities = false,
                    v if in_entities => entities.push(v.to_string()),
                    _ => {}
                }
            }
        }
        entities
    }

    #[test]
    fn empty_document_has_sections_and_eof() {
        let (dxf, count) = document_dxf(&Document::default());
        assert_eq!(count, 0);
        for needle in ["HEADER", "AC1009", "TABLES", "ENTITIES", "EOF"] {
            assert!(dxf.contains(needle), "missing {needle}");
        }
        assert!(dxf.ends_with("0\nEOF\n"));
        assert!(scan_entities(&dxf).is_empty());
    }

    #[test]
    fn known_doc_entity_counts_and_kinds() {
        let mut s = Session::default();
        run(&mut s, "line 0,0,0 10,0,0");
        run(&mut s, "polyline 0,0 5,0 5,5 closed");
        run(&mut s, "circle 20,0,0 2.5");
        run(&mut s, "arc 30,0,0 5 0 90");
        run(&mut s, "ellipse 40,0,0 4 2");
        run(&mut s, "text 5,3,0 hello 0.3");
        let (dxf, count) = document_dxf(&s.doc);
        let entities = scan_entities(&dxf);
        // polyline: POLYLINE + 3 VERTEX + SEQEND; ellipse tessellates to a
        // polyline with N vertices. Entity count reports logical entities.
        assert_eq!(count, 6);
        assert_eq!(entities.iter().filter(|e| *e == "LINE").count(), 1);
        assert_eq!(entities.iter().filter(|e| *e == "POLYLINE").count(), 2);
        assert_eq!(entities.iter().filter(|e| *e == "SEQEND").count(), 2);
        assert_eq!(entities.iter().filter(|e| *e == "CIRCLE").count(), 1);
        assert_eq!(entities.iter().filter(|e| *e == "ARC").count(), 1);
        assert_eq!(entities.iter().filter(|e| *e == "TEXT").count(), 1);
    }

    #[test]
    fn coordinates_and_arc_angles_written() {
        let mut s = Session::default();
        run(&mut s, "line 1.5,2,0 10,0,3");
        run(&mut s, "arc 0,0,0 5 30 120");
        let (dxf, _) = document_dxf(&s.doc);
        // line endpoints via 10/20/30 and 11/21/31 (3D via 30/31)
        for tag in [
            "10\n1.5\n", "20\n2.0\n", "30\n0.0\n", "11\n10.0\n", "21\n0.0\n", "31\n3.0\n",
            "40\n5.0\n", "50\n30.0\n", "51\n120.0\n",
        ] {
            assert!(dxf.contains(tag), "missing tag pair {tag:?}");
        }
    }

    #[test]
    fn full_circle_arc_exports_as_circle() {
        let mut s = Session::default();
        run(&mut s, "circle 0,0,0 3");
        let (dxf, count) = document_dxf(&s.doc);
        assert_eq!(count, 1);
        assert_eq!(scan_entities(&dxf), vec!["CIRCLE"]);
    }

    #[test]
    fn mesh_exports_feature_edges_as_lines() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 2,2,2");
        let (dxf, count) = document_dxf(&s.doc);
        assert_eq!(count, 12, "a box has 12 feature edges");
        assert_eq!(scan_entities(&dxf).iter().filter(|e| *e == "LINE").count(), 12);
    }

    #[test]
    fn dim_exports_line_plus_text() {
        let mut s = Session::default();
        run(&mut s, "dim 0,0 10,0 0.5");
        let (dxf, count) = document_dxf(&s.doc);
        assert_eq!(count, 2);
        assert_eq!(scan_entities(&dxf), vec!["LINE", "TEXT"]);
        assert!(dxf.contains("1\n10.00 m\n"), "measured value as TEXT content");
    }

    #[test]
    fn layers_appear_in_table_and_on_entities() {
        let mut s = Session::default();
        run(&mut s, "layer walls");
        run(&mut s, "line 0,0,0 1,0,0");
        let (dxf, _) = document_dxf(&s.doc);
        assert!(dxf.contains("2\nWALLS\n"), "layer table entry");
        assert!(dxf.contains("8\nWALLS\n"), "entity layer reference");
        assert!(dxf.contains("2\nDEFAULT\n"), "default layer still listed");
    }

    #[test]
    fn dxf_layer_table_carries_lineweight_code_370() {
        let mut s = Session::default();
        run(&mut s, "layer thin");   // default 0.18 mm -> 18 hundredths
        run(&mut s, "layer heavy");
        run(&mut s, "layerweight heavy 0.35"); // 35 hundredths
        let (dxf, _) = document_dxf(&s.doc);
        // Each LAYER record must carry code 370.
        // Default layer: 0.18 mm -> 18.
        assert!(dxf.contains("370\n18\n"), "default 0.18 mm -> code 18");
        // heavy layer: 0.35 mm -> 35.
        assert!(dxf.contains("370\n35\n"), "0.35 mm -> code 35");
    }

    #[test]
    fn layer_names_sanitized() {
        assert_eq!(dxf_layer("walls"), "WALLS");
        assert_eq!(dxf_layer("ground floor/α"), "GROUND_FLOOR__");
        assert_eq!(dxf_layer(""), "0");
    }

    // ---------- import ----------

    use crate::Command;
    use mydrafter_doc::Geometry as G;

    fn curve_of(obj: &mydrafter_doc::SceneObject) -> &kernel_curve::Curve {
        match &obj.geometry {
            G::Curve(c) => c,
            other => panic!("expected a curve, got {other:?}"),
        }
    }

    /// Structural equality with tolerance: arc angles pass through
    /// degrees<->radians on the way out and back.
    fn assert_curve_close(a: &kernel_curve::Curve, b: &kernel_curve::Curve) {
        use kernel_curve::Curve::*;
        match (a, b) {
            (Line { a: a1, b: b1 }, Line { a: a2, b: b2 }) => {
                assert!((*a1 - *a2).length() < 1e-9 && (*b1 - *b2).length() < 1e-9);
            }
            (
                Polyline { points: p1, closed: c1 },
                Polyline { points: p2, closed: c2 },
            ) => {
                assert_eq!(c1, c2);
                assert_eq!(p1.len(), p2.len());
                for (q1, q2) in p1.iter().zip(p2) {
                    assert!((*q1 - *q2).length() < 1e-9, "{q1} vs {q2}");
                }
            }
            (
                Arc { center: c1, radius: r1, start: s1, end: e1 },
                Arc { center: c2, radius: r2, start: s2, end: e2 },
            ) => {
                assert!((*c1 - *c2).length() < 1e-9);
                assert!((r1 - r2).abs() < 1e-9);
                assert!((s1 - s2).abs() < 1e-9, "start {s1} vs {s2}");
                assert!((e1 - e2).abs() < 1e-9, "end {e1} vs {e2}");
            }
            other => panic!("curve kinds differ: {other:?}"),
        }
    }

    #[test]
    fn import_round_trips_our_export() {
        // Courtyard (outer + inner rects) plus one of every exact curve kind.
        let mut s = Session::default();
        run(&mut s, "layer courtyard");
        run(&mut s, "rect 0,0,0 10 8");
        run(&mut s, "rect 3,3,0 4 2");
        run(&mut s, "layer site");
        run(&mut s, "line 0,0,0 10,0,3");
        run(&mut s, "circle 20,0,0 2.5");
        run(&mut s, "arc 30,0,0 5 30 120");
        run(&mut s, "text 5,3,0 hello 0.3");
        let path = std::env::temp_dir().join("mydrafter_import_roundtrip.dxf");
        run(&mut s, &format!("export {}", path.display()));

        let mut s2 = Session::default();
        let out = s2
            .run(Command::Import { path: path.display().to_string() })
            .unwrap();
        assert!(out.message.contains("imported 6 entities"), "{}", out.message);
        assert!(out.message.contains("(0 skipped)"), "{}", out.message);
        assert_eq!(out.created.len(), 6);
        assert_eq!(s2.doc.len(), s.doc.len());
        assert_eq!(s2.doc.current_layer, mydrafter_doc::DEFAULT_LAYER);

        // Same order, same layers (lowercased round-trip), same geometry.
        for (a, b) in s.doc.objects().zip(s2.doc.objects()) {
            assert_eq!(a.layer, b.layer);
            match (&a.geometry, &b.geometry) {
                (G::Curve(ca), G::Curve(cb)) => assert_curve_close(ca, cb),
                (
                    G::Annotation(Annotation::Text { pos, text, height }),
                    G::Annotation(Annotation::Text { pos: p2, text: t2, height: h2 }),
                ) => {
                    assert!((*pos - *p2).length() < 1e-9);
                    assert_eq!(text, t2);
                    assert!((height - h2).abs() < 1e-9);
                }
                other => panic!("geometry kinds differ: {other:?}"),
            }
        }
    }

    #[test]
    fn imported_session_saves_and_replays_stably() {
        let mut s = Session::default();
        run(&mut s, "layer walls");
        run(&mut s, "polyline 0,0 5,0 5,5 closed");
        run(&mut s, "circle 8,0,0 1.5");
        let path = std::env::temp_dir().join("mydrafter_import_replay.dxf");
        run(&mut s, &format!("export {}", path.display()));

        let mut s2 = Session::default();
        run(&mut s2, &format!("import {}", path.display()));
        // The log holds plain entity + layer ops — never the import itself —
        // so replay needs no access to the DXF file.
        let json = crate::io::to_json(&s2);
        assert!(!json.contains("\"cmd\": \"import\""), "{json}");
        let loaded = crate::io::from_json(&json).unwrap();
        assert_eq!(crate::io::to_json(&loaded), json, "replay-stable");
        let ids: Vec<_> = s2.doc.objects().map(|o| o.id).collect();
        let loaded_ids: Vec<_> = loaded.doc.objects().map(|o| o.id).collect();
        assert_eq!(ids, loaded_ids, "identical objects after replay");
    }

    #[test]
    fn unknown_entities_skipped_silently() {
        // 2000+-style noise (handles, subclass markers) around one LINE, plus
        // entity kinds we do not import.
        let text = "0\nSECTION\n2\nENTITIES\n\
            0\nSPLINE\n8\nA\n70\n8\n\
            0\nLINE\n5\n1AF\n100\nAcDbEntity\n8\nWALLS\n100\nAcDbLine\n\
            10\n0\n20\n0\n30\n0\n11\n5\n21\n1\n31\n0\n\
            0\nPOINT\n10\n1\n20\n1\n\
            0\nINSERT\n2\nCHAIR\n10\n0\n20\n0\n\
            0\nENDSEC\n0\nEOF\n";
        let parsed = parse_dxf(text).unwrap();
        assert_eq!(parsed.skipped, 3);
        assert_eq!(parsed.entities.len(), 1);
        let (layer, cmd) = &parsed.entities[0];
        assert_eq!(layer, "walls");
        assert_eq!(
            *cmd,
            Command::Line {
                id: None,
                a: DVec3::new(0.0, 0.0, 0.0),
                b: DVec3::new(5.0, 1.0, 0.0),
            }
        );
    }

    #[test]
    fn lwpolyline_reads_vertices_flags_and_elevation() {
        let text = "0\nSECTION\n2\nENTITIES\n\
            0\nLWPOLYLINE\n8\nDECK\n90\n3\n70\n1\n38\n2.5\n\
            10\n0\n20\n0\n10\n5\n20\n0\n10\n5\n20\n5\n\
            0\nENDSEC\n0\nEOF\n";
        let parsed = parse_dxf(text).unwrap();
        assert_eq!(parsed.skipped, 0);
        let (layer, cmd) = &parsed.entities[0];
        assert_eq!(layer, "deck");
        assert_eq!(
            *cmd,
            Command::Polyline {
                id: None,
                points: vec![
                    DVec3::new(0.0, 0.0, 2.5),
                    DVec3::new(5.0, 0.0, 2.5),
                    DVec3::new(5.0, 5.0, 2.5),
                ],
                closed: true,
            }
        );
    }

    #[test]
    fn full_circle_and_wrapped_arc_import() {
        let mut s = Session::default();
        run(&mut s, "circle 0,0,0 3");
        run(&mut s, "arc 10,0,0 2 300 60"); // wraps through 0 degrees
        let path = std::env::temp_dir().join("mydrafter_import_arcs.dxf");
        run(&mut s, &format!("export {}", path.display()));
        let mut s2 = Session::default();
        run(&mut s2, &format!("import {}", path.display()));
        assert_eq!(s2.doc.len(), 2);
        let objs: Vec<_> = s2.doc.objects().collect();
        assert!(curve_of(objs[0]).is_closed(), "circle imports closed");
        match curve_of(objs[1]) {
            kernel_curve::Curve::Arc { start, end, .. } => {
                // 300..60 reads back as 300..420: same CCW sweep.
                assert!((start.to_degrees() - 300.0).abs() < 1e-9);
                assert!((end.to_degrees() - 420.0).abs() < 1e-9);
            }
            other => panic!("expected arc, got {other:?}"),
        }
    }

    #[test]
    fn mesh_feature_edges_import_as_lines_one_op_each() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 2,2,2");
        let path = std::env::temp_dir().join("mydrafter_import_box.dxf");
        run(&mut s, &format!("export {}", path.display()));
        let mut s2 = Session::default();
        run(&mut s2, &format!("import {}", path.display()));
        // 1 entity = 1 op: 12 edges land as 12 line objects and 12 logged ops
        // (no layer switches — everything is on the default layer).
        assert_eq!(s2.doc.len(), 12);
        assert_eq!(s2.save_log().len(), 12);
        assert!(s2
            .doc
            .objects()
            .all(|o| matches!(curve_of(o), kernel_curve::Curve::Line { .. })));
        // Entities undo one at a time.
        run(&mut s2, "undo");
        assert_eq!(s2.doc.len(), 11);
    }

    #[test]
    fn import_errors_are_friendly() {
        let mut s = Session::default();
        let err = s.run(parse("import /nonexistent/nope.dxf").unwrap()).unwrap_err();
        assert!(err.to_string().contains("cannot read"), "{err}");

        // A file named .dxf but with garbage content should fail with a parse error.
        let path = std::env::temp_dir().join("mydrafter_not_a_dxf.dxf");
        std::fs::write(&path, "hello\nworld\nagain\n").unwrap();
        let err = s
            .run(Command::Import { path: path.display().to_string() })
            .unwrap_err();
        assert!(err.to_string().contains("group code"), "{err}");
        assert_eq!(s.doc.len(), 0, "failed import leaves nothing behind");
    }
}
