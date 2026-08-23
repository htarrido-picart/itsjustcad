//! Vector PDF export for sheets: each viewport projects visible geometry
//! edges through an orthographic camera at the view's scale into PDF line
//! art. The writer is hand-rolled — the output is uncompressed line/text
//! operators only, which keeps the crate dependency-free.

use glam::{DVec2, DVec3};
use mydrafter_doc::{Document, Geometry, Sheet, SheetView, ViewDirection};

/// Chord tolerance for tessellating curves at print time (meters).
const PRINT_TOL: f64 = 0.005;
/// Sheet margin around the viewport area (mm).
const MARGIN_MM: f64 = 10.0;
/// Title strip height at the bottom of the sheet (mm).
const TITLE_MM: f64 = 12.0;
/// Gutter between viewports (mm).
const GUTTER_MM: f64 = 5.0;
/// PDF user-space points per millimeter.
const PT_PER_MM: f64 = 72.0 / 25.4;

/// Drawing-scale math: millimeters on paper for a world length in meters.
/// At 1:100, 1 m = 10 mm.
pub fn world_to_paper_mm(world_m: f64, scale_denominator: f64) -> f64 {
    world_m * 1000.0 / scale_denominator
}

/// Orthographic projection of a world point onto a view plane, in meters.
fn project(dir: ViewDirection, p: DVec3) -> DVec2 {
    match dir {
        ViewDirection::Top => DVec2::new(p.x, p.y),
        ViewDirection::Front => DVec2::new(p.x, p.z),
        ViewDirection::Right => DVec2::new(p.y, p.z),
        ViewDirection::Iso => {
            // 30° axonometric: x recedes right, y recedes left, z is up.
            let (c30, s30) = (30f64.to_radians().cos(), 30f64.to_radians().sin());
            DVec2::new((p.x - p.y) * c30, (p.x + p.y) * s30 + p.z)
        }
    }
}

/// World-space line segments worth drawing for one object.
fn geometry_segments(geometry: &Geometry, out: &mut Vec<(DVec3, DVec3)>) {
    match geometry {
        Geometry::Curve(curve) => {
            let pts = curve.tessellate(PRINT_TOL);
            for pair in pts.windows(2) {
                out.push((pair[0], pair[1]));
            }
            if curve.is_closed()
                && let (Some(&first), Some(&last)) = (pts.first(), pts.last())
            {
                out.push((last, first));
            }
        }
        Geometry::Mesh(mesh) => {
            out.extend(crate::dxf::mesh_feature_edges(mesh));
        }
        // Annotations are screen-styled (arrows, glyphs); skipped in PDF v1.
        Geometry::Annotation(_) => {}
    }
}

/// Liang-Barsky clip of a 2D segment to an axis-aligned rect. Returns the
/// clipped endpoints, or `None` when fully outside.
fn clip(a: DVec2, b: DVec2, min: DVec2, max: DVec2) -> Option<(DVec2, DVec2)> {
    let d = b - a;
    let (mut t0, mut t1) = (0.0f64, 1.0f64);
    for (p, q) in [
        (-d.x, a.x - min.x),
        (d.x, max.x - a.x),
        (-d.y, a.y - min.y),
        (d.y, max.y - a.y),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return None;
            }
        } else {
            let r = q / p;
            if p < 0.0 {
                t0 = t0.max(r);
            } else {
                t1 = t1.min(r);
            }
        }
    }
    if t0 > t1 {
        return None;
    }
    Some((a + d * t0, a + d * t1))
}

fn escape_pdf_text(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii() && !c.is_ascii_control())
        .flat_map(|c| match c {
            '(' | ')' | '\\' => vec!['\\', c],
            c => vec![c],
        })
        .collect()
}

fn mm(v: f64) -> String {
    format!("{:.2}", v * PT_PER_MM)
}

/// Render one viewport's content (border + clipped scaled line art + label)
/// into `content`. Returns the number of geometry lines drawn.
fn render_view(
    doc: &Document,
    view: &SheetView,
    rect: (DVec2, DVec2), // (min, max) in mm, PDF coords (origin bottom-left)
    content: &mut String,
) -> usize {
    let (rmin, rmax) = rect;
    // Border.
    content.push_str(&format!(
        "0.6 w {} {} {} {} re S\n",
        mm(rmin.x),
        mm(rmin.y),
        mm(rmax.x - rmin.x),
        mm(rmax.y - rmin.y)
    ));
    // Label: "top 1:100" under the top border.
    content.push_str(&format!(
        "BT /F1 8 Tf {} {} Td ({} 1:{}) Tj ET\n",
        mm(rmin.x + 2.0),
        mm(rmax.y - 5.0),
        view.direction.label(),
        view.scale
    ));

    // Project all visible geometry, scale to mm, center in the rect.
    let mut segments = Vec::new();
    for obj in doc.objects() {
        if obj.visible && doc.layer_visible(&obj.layer) {
            geometry_segments(&obj.geometry, &mut segments);
        }
    }
    let projected: Vec<(DVec2, DVec2)> = segments
        .iter()
        .map(|(a, b)| {
            let pa = project(view.direction, *a);
            let pb = project(view.direction, *b);
            (
                DVec2::new(
                    world_to_paper_mm(pa.x, view.scale),
                    world_to_paper_mm(pa.y, view.scale),
                ),
                DVec2::new(
                    world_to_paper_mm(pb.x, view.scale),
                    world_to_paper_mm(pb.y, view.scale),
                ),
            )
        })
        .collect();
    if projected.is_empty() {
        return 0;
    }
    let (mut lo, mut hi) = (DVec2::MAX, DVec2::MIN);
    for (a, b) in &projected {
        lo = lo.min(a.min(*b));
        hi = hi.max(a.max(*b));
    }
    let offset = (rmin + rmax) / 2.0 - (lo + hi) / 2.0;

    let pad = 1.0; // keep line art off the border stroke
    let (cmin, cmax) = (
        rmin + DVec2::splat(pad),
        rmax - DVec2::splat(pad),
    );
    let mut drawn = 0usize;
    content.push_str("0.35 w\n");
    for (a, b) in projected {
        if let Some((a, b)) = clip(a + offset, b + offset, cmin, cmax) {
            if (b - a).length() < 1e-6 {
                continue;
            }
            content.push_str(&format!(
                "{} {} m {} {} l S\n",
                mm(a.x),
                mm(a.y),
                mm(b.x),
                mm(b.y)
            ));
            drawn += 1;
        }
    }
    drawn
}

/// Build the complete PDF for a sheet. Returns the file bytes and the number
/// of geometry lines drawn across all viewports.
pub fn sheet_pdf(doc: &Document, sheet: &Sheet) -> (Vec<u8>, usize) {
    let (paper_w, paper_h) = sheet.paper.landscape_mm();
    let mut content = String::new();
    let mut drawn = 0usize;

    // Sheet frame + title.
    content.push_str(&format!(
        "0.8 w {} {} {} {} re S\n",
        mm(MARGIN_MM / 2.0),
        mm(MARGIN_MM / 2.0),
        mm(paper_w - MARGIN_MM),
        mm(paper_h - MARGIN_MM)
    ));
    content.push_str(&format!(
        "BT /F1 12 Tf {} {} Td ({} - {}) Tj ET\n",
        mm(MARGIN_MM),
        mm(MARGIN_MM / 2.0 + 3.0),
        escape_pdf_text(&sheet.name),
        sheet.paper.label()
    ));

    // Equal horizontal slices between the margins, above the title strip.
    let n = sheet.views.len();
    if n > 0 {
        let area_w = paper_w - 2.0 * MARGIN_MM;
        let view_w = (area_w - GUTTER_MM * (n as f64 - 1.0)) / n as f64;
        let (y0, y1) = (MARGIN_MM + TITLE_MM, paper_h - MARGIN_MM);
        for (i, view) in sheet.views.iter().enumerate() {
            let x0 = MARGIN_MM + i as f64 * (view_w + GUTTER_MM);
            drawn += render_view(
                doc,
                view,
                (DVec2::new(x0, y0), DVec2::new(x0 + view_w, y1)),
                &mut content,
            );
        }
    }

    (write_pdf(paper_w, paper_h, content.as_bytes()), drawn)
}

/// Minimal single-page PDF: catalog, page tree, page, content stream, and a
/// built-in Helvetica font. Cross-reference offsets are exact.
fn write_pdf(paper_w_mm: f64, paper_h_mm: f64, content: &[u8]) -> Vec<u8> {
    let objects: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {} {}] \
             /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>",
            mm(paper_w_mm),
            mm(paper_h_mm)
        )
        .into_bytes(),
        {
            let mut s = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
            s.extend_from_slice(content);
            s.extend_from_slice(b"\nendstream");
            s
        },
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ];

    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_at = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for off in offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            objects.len() + 1,
            xref_at
        )
        .as_bytes(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_math_1_to_100() {
        // 1:100 => 1 m of world reads as 10 mm on paper.
        assert_eq!(world_to_paper_mm(1.0, 100.0), 10.0);
        assert_eq!(world_to_paper_mm(2.5, 50.0), 50.0);
        assert_eq!(world_to_paper_mm(0.001, 1.0), 1.0);
    }

    #[test]
    fn projections() {
        let p = DVec3::new(1.0, 2.0, 3.0);
        assert_eq!(project(ViewDirection::Top, p), DVec2::new(1.0, 2.0));
        assert_eq!(project(ViewDirection::Front, p), DVec2::new(1.0, 3.0));
        assert_eq!(project(ViewDirection::Right, p), DVec2::new(2.0, 3.0));
        let iso = project(ViewDirection::Iso, p);
        assert!((iso.y - (3.0 * 0.5 + 3.0)).abs() < 1e-12); // (x+y)·sin30 + z
    }

    #[test]
    fn clip_keeps_inside_drops_outside() {
        let (min, max) = (DVec2::ZERO, DVec2::splat(10.0));
        // fully inside
        let (a, b) = clip(DVec2::new(1.0, 1.0), DVec2::new(9.0, 9.0), min, max).unwrap();
        assert_eq!((a, b), (DVec2::new(1.0, 1.0), DVec2::new(9.0, 9.0)));
        // crossing: clipped to the border
        let (a, b) = clip(DVec2::new(-5.0, 5.0), DVec2::new(15.0, 5.0), min, max).unwrap();
        assert_eq!((a.x, b.x), (0.0, 10.0));
        // fully outside
        assert!(clip(DVec2::new(-5.0, -5.0), DVec2::new(-1.0, -1.0), min, max).is_none());
    }

    #[test]
    fn text_escaping() {
        assert_eq!(escape_pdf_text("a(b)c\\d"), "a\\(b\\)c\\\\d");
    }

    #[test]
    fn empty_sheet_is_valid_pdf() {
        let doc = Document::default();
        let sheet = Sheet {
            name: "empty".into(),
            paper: mydrafter_doc::PaperSize::A3,
            views: vec![],
        };
        let (bytes, drawn) = sheet_pdf(&doc, &sheet);
        assert!(bytes.starts_with(b"%PDF"));
        assert!(bytes.ends_with(b"%%EOF\n"));
        assert_eq!(drawn, 0);
    }
}
