//! SVG export: 2D projection of the current view using the same orthographic
//! projection and edge-extraction helpers as the PDF exporter.  Each layer
//! becomes a `<g>` element with stroke colour from the layer colour and
//! stroke-width from `lineweight_mm` (converted to SVG user-units via the
//! viewBox scale). Annotations are emitted as `<text>` elements.

use glam::{DVec2, DVec3};
use mydrafter_doc::{Annotation, Document, Geometry, LayerStyle, ViewDirection};

/// Chord tolerance for tessellating curves (meters), matching the other exporters.
const SVG_TOL: f64 = 0.005;

/// Default view direction when none is stored in the document.
const DEFAULT_DIR: ViewDirection = ViewDirection::Top;

// Reuse the projection logic from pdf.rs.
fn project(dir: ViewDirection, p: DVec3) -> DVec2 {
    match dir {
        ViewDirection::Top => DVec2::new(p.x, p.y),
        ViewDirection::Front => DVec2::new(p.x, p.z),
        ViewDirection::Right => DVec2::new(p.y, p.z),
        ViewDirection::Iso => {
            let (c30, s30) = (30f64.to_radians().cos(), 30f64.to_radians().sin());
            DVec2::new((p.x - p.y) * c30, (p.x + p.y) * s30 + p.z)
        }
    }
}

/// World-space line segments for one geometry object (mirrors pdf.rs `geometry_segments`).
fn collect_segments(geometry: &Geometry) -> Vec<(DVec3, DVec3)> {
    let mut segs = Vec::new();
    match geometry {
        Geometry::Curve(curve) => {
            let pts = curve.tessellate(SVG_TOL);
            for pair in pts.windows(2) {
                segs.push((pair[0], pair[1]));
            }
            if curve.is_closed() && let (Some(&first), Some(&last)) = (pts.first(), pts.last()) {
                segs.push((last, first));
            }
        }
        Geometry::Mesh(mesh) => {
            segs.extend(crate::dxf::mesh_feature_edges(mesh));
        }
        Geometry::Annotation(Annotation::LinearDim { a, b, offset }) => {
            let dir = (*b - *a).normalize_or_zero();
            let perp = DVec3::new(-dir.y, dir.x, 0.0) * *offset;
            let a_off = *a + perp;
            let b_off = *b + perp;
            let gap = offset.abs() * 0.05;
            let ga = *a + perp.normalize_or_zero() * gap;
            let gb = *b + perp.normalize_or_zero() * gap;
            segs.push((ga, a_off));
            segs.push((gb, b_off));
            segs.push((a_off, b_off));
        }
        Geometry::Annotation(_) => {}
        // Block instances are not directly renderable in SVG export.
        Geometry::Instance { .. } => {}
    }
    segs
}

/// Escape XML attribute and text content.
fn xml_escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '&' => "&amp;".chars().collect::<Vec<_>>(),
            '<' => "&lt;".chars().collect(),
            '>' => "&gt;".chars().collect(),
            '"' => "&quot;".chars().collect(),
            c => vec![c],
        })
        .collect()
}

/// Format an optional RGBA colour as CSS `rgb(R,G,B)`. `None` → black.
fn css_color(c: Option<[f32; 4]>) -> String {
    let [r, g, b, _] = c.unwrap_or([0.0, 0.0, 0.0, 1.0]);
    format!(
        "rgb({},{},{})",
        (r * 255.0).round() as u8,
        (g * 255.0).round() as u8,
        (b * 255.0).round() as u8,
    )
}

/// Compact float: 4 decimal places, trailing zeros trimmed.
fn svg_num(v: f64) -> String {
    let mut s = format!("{v:.4}");
    while s.ends_with('0') { s.pop(); }
    if s.ends_with('.') { s.push('0'); }
    s
}

/// Build SVG bytes for the current document view.
/// Returns the SVG bytes and a summary string for the command echo.
pub fn export_svg(doc: &Document) -> (Vec<u8>, String) {
    let dir = DEFAULT_DIR; // always top-down for now (no live camera in export path)

    // Collect all projected 2D segments per layer.
    struct LayerData {
        name: String,
        style: LayerStyle,
        segs: Vec<(DVec2, DVec2)>,
        texts: Vec<(DVec2, String, f64)>, // (pos, text, height_m)
    }

    // Gather layers in document layer order (alphabetical + default first).
    let fallback = LayerStyle::default();
    let layer_names: Vec<String> = {
        let mut names: Vec<String> = doc.layers.keys().cloned().collect();
        names.sort();
        names
    };

    let mut layers: Vec<LayerData> = layer_names
        .iter()
        .map(|n| LayerData {
            name: n.clone(),
            style: doc.layers.get(n).unwrap_or(&fallback).clone(),
            segs: Vec::new(),
            texts: Vec::new(),
        })
        .collect();
    // Objects whose layer is not in the layer table go to an implicit fallback.
    let mut orphan = LayerData {
        name: "__orphan__".to_string(),
        style: fallback.clone(),
        segs: Vec::new(),
        texts: Vec::new(),
    };

    let mut all_pts: Vec<DVec2> = Vec::new();
    let mut total_segs = 0usize;

    for obj in doc.objects() {
        if !obj.visible || !doc.layer_visible(&obj.layer) {
            continue;
        }
        let layer_data = layers
            .iter_mut()
            .find(|l| l.name == obj.layer)
            .unwrap_or(&mut orphan);

        // Text annotations.
        if let Geometry::Annotation(Annotation::Text { pos, text, height }) = &obj.geometry {
            let p2 = project(dir, *pos);
            layer_data.texts.push((p2, text.clone(), *height));
            all_pts.push(p2);
            continue;
        }

        let world_segs = collect_segments(&obj.geometry);
        for (a, b) in &world_segs {
            let pa = project(dir, *a);
            let pb = project(dir, *b);
            all_pts.push(pa);
            all_pts.push(pb);
            layer_data.segs.push((pa, pb));
        }
        total_segs += world_segs.len();
    }

    // --- viewBox from scene bounds ---
    let (mut min_x, mut min_y, mut max_x, mut max_y) =
        (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for p in &all_pts {
        min_x = min_x.min(p.x);
        min_y = min_y.min(p.y);
        max_x = max_x.max(p.x);
        max_y = max_y.max(p.y);
    }
    // Guard empty doc.
    if !min_x.is_finite() || min_x >= max_x || min_y >= max_y {
        min_x = -1.0; min_y = -1.0; max_x = 1.0; max_y = 1.0;
    }

    let pad = ((max_x - min_x).max(max_y - min_y)) * 0.02;
    let vx = min_x - pad;
    let vy = min_y - pad;
    let vw = (max_x - min_x) + 2.0 * pad;
    let vh = (max_y - min_y) + 2.0 * pad;

    // SVG user-units = meters; lineweight stays in mm so divide by 1000 to
    // convert to meter-scale user-units.
    let mut svg = String::new();
    svg.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    svg.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" \
         viewBox=\"{} {} {} {}\" \
         width=\"{}\" height=\"{}\">\n",
        svg_num(vx),
        svg_num(vy),
        svg_num(vw),
        svg_num(vh),
        svg_num(vw * 1000.0), // display width in mm (1 m = 1000 mm)
        svg_num(vh * 1000.0),
    ));

    // Emit layers (only those with content).
    let mut layer_count = 0usize;
    let mut path_count = 0usize;

    let all_layers = layers.iter().chain(std::iter::once(&orphan));
    for layer in all_layers {
        if layer.segs.is_empty() && layer.texts.is_empty() {
            continue;
        }
        layer_count += 1;
        let stroke = css_color(layer.style.color);
        let sw = layer.style.lineweight_mm / 1000.0; // mm → meters (SVG user-units)
        svg.push_str(&format!(
            "  <g id=\"{}\" stroke=\"{}\" stroke-width=\"{}\" fill=\"none\">\n",
            xml_escape(&layer.name),
            stroke,
            svg_num(sw),
        ));

        // Emit each segment as a separate `<line>`.
        for (a, b) in &layer.segs {
            svg.push_str(&format!(
                "    <line x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\"/>\n",
                svg_num(a.x), svg_num(a.y),
                svg_num(b.x), svg_num(b.y),
            ));
            path_count += 1;
        }

        // Text annotations.
        for (pos, text, height) in &layer.texts {
            svg.push_str(&format!(
                "    <text x=\"{}\" y=\"{}\" font-size=\"{}\" fill=\"{}\">{}</text>\n",
                svg_num(pos.x),
                svg_num(pos.y),
                svg_num(*height),
                stroke,
                xml_escape(text),
            ));
        }

        svg.push_str("  </g>\n");
    }

    svg.push_str("</svg>\n");

    let summary = format!(
        "{total_segs} segments across {layer_count} layer(s), {path_count} SVG elements"
    );
    (svg.into_bytes(), summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse, Session};

    #[test]
    fn svg_contains_required_structure() {
        let mut s = Session::default();
        s.run(parse("layer walls").unwrap()).unwrap();
        s.run(parse("layercolor walls 1,0,0").unwrap()).unwrap();
        s.run(parse("box 0,0,0 2,2,2").unwrap()).unwrap();
        s.run(parse("layer default").unwrap()).unwrap();
        s.run(parse("line 0,0,0 5,0,0").unwrap()).unwrap();

        let (bytes, _summary) = export_svg(&s.doc);
        let svg = String::from_utf8(bytes).unwrap();

        assert!(svg.contains("<svg"), "should contain svg element");
        assert!(svg.contains("viewBox"), "should contain viewBox");
        // Two layers with geometry => two <g> groups.
        assert!(svg.matches("<g ").count() >= 2, "at least 2 <g> groups\n{svg}");
        // box has feature edges, line has 1 segment.
        assert!(svg.contains("<line "), "should contain <line> elements");
    }

    #[test]
    fn svg_parses_viewbox() {
        let mut s = Session::default();
        s.run(parse("box 0,0,0 1,1,1").unwrap()).unwrap();

        let (bytes, _) = export_svg(&s.doc);
        let svg = String::from_utf8(bytes).unwrap();

        // Extract viewBox attribute value (between the quotes).
        let prefix = "viewBox=\"";
        let vb_start = svg.find(prefix).expect("viewBox attr") + prefix.len();
        let vb_end = svg[vb_start..].find('"').unwrap() + vb_start;
        let vb_str = &svg[vb_start..vb_end];
        let nums: Vec<f64> = vb_str
            .split_whitespace()
            .filter_map(|t| t.parse().ok())
            .collect();
        assert_eq!(nums.len(), 4, "viewBox has 4 numbers: '{vb_str}'");
        let width = nums[2];
        let height = nums[3];
        assert!(width > 0.0 && height > 0.0, "viewBox dimensions positive");
    }

    #[test]
    fn svg_empty_doc_is_valid() {
        let doc = Document::default();
        let (bytes, summary) = export_svg(&doc);
        let svg = String::from_utf8(bytes).unwrap();
        assert!(svg.starts_with("<?xml"), "starts with XML declaration");
        assert!(svg.contains("<svg"), "contains svg element");
        assert!(summary.contains("0 segments"), "{summary}");
    }

    #[test]
    fn svg_per_layer_groups() {
        let mut s = Session::default();
        s.run(parse("layer a").unwrap()).unwrap();
        s.run(parse("line 0,0,0 1,0,0").unwrap()).unwrap();
        s.run(parse("layer b").unwrap()).unwrap();
        s.run(parse("line 2,0,0 3,0,0").unwrap()).unwrap();

        let (bytes, _) = export_svg(&s.doc);
        let svg = String::from_utf8(bytes).unwrap();
        assert!(svg.contains("id=\"a\""), "layer a group");
        assert!(svg.contains("id=\"b\""), "layer b group");
    }
}
