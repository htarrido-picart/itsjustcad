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
    for name in doc.layers.keys() {
        t.tag(0, "LAYER");
        t.tag(2, &dxf_layer(name));
        t.tag(70, "0");
        t.tag(62, "7");
        t.tag(6, "CONTINUOUS");
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
    fn layer_names_sanitized() {
        assert_eq!(dxf_layer("walls"), "WALLS");
        assert_eq!(dxf_layer("ground floor/α"), "GROUND_FLOOR__");
        assert_eq!(dxf_layer(""), "0");
    }
}
