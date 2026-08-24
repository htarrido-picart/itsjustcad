// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Vector PDF export for sheets: each viewport projects visible geometry
//! edges through an orthographic camera at the view's scale into PDF line
//! art. The writer is hand-rolled — the output is uncompressed line/text
//! operators only, which keeps the crate dependency-free.

use glam::{DVec2, DVec3};
use itsjustcad_doc::{
    Annotation, Document, Geometry, LayerStyle, ScheduleRow, Sheet, SheetDim, SheetView,
    ViewDirection,
};

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
        Geometry::Mesh(mesh)
        | Geometry::Frame { mesh, .. }
        | Geometry::Area { mesh, .. } => {
            out.extend(crate::dxf::mesh_feature_edges(mesh));
        }
        // LinearDim annotations: render the three dim-line segments (two witness
        // lines + one dim line) so they appear inside viewport projections.
        Geometry::Annotation(Annotation::LinearDim { a, b, offset }) => {
            // Perpendicular direction in the XY plane (dim offset is already in
            // model-space meters; positive = left of a→b).
            let dir = (*b - *a).normalize_or_zero();
            let perp = DVec3::new(-dir.y, dir.x, 0.0) * *offset;
            let a_off = *a + perp;
            let b_off = *b + perp;
            // Witness lines (with a small gap at the anchor point).
            let gap = offset.abs() * 0.05;
            let ga = *a + perp.normalize_or_zero() * gap;
            let gb = *b + perp.normalize_or_zero() * gap;
            out.push((ga, a_off));
            out.push((gb, b_off));
            // Dim line.
            out.push((a_off, b_off));
        }
        Geometry::Annotation(Annotation::Hatch { boundary, pattern }) => {
            use itsjustcad_doc::{
                hatch::{hatch_brick, hatch_concrete, hatch_earth, hatch_insulation, hatch_lines},
                HatchPattern,
            };
            let segs: Vec<[glam::DVec3; 2]> = match pattern {
                HatchPattern::Solid => {
                    // Solid fill approximated as a closed boundary outline.
                    let n = boundary.len();
                    (0..n).map(|i| [boundary[i], boundary[(i + 1) % n]]).collect()
                }
                HatchPattern::Lines { angle_deg, spacing } => {
                    hatch_lines(boundary, *angle_deg, *spacing)
                }
                HatchPattern::Crosshatch { angle_deg, spacing } => {
                    let mut s = hatch_lines(boundary, *angle_deg, *spacing);
                    s.extend(hatch_lines(boundary, *angle_deg + 90.0, *spacing));
                    s
                }
                HatchPattern::Brick { spacing } => hatch_brick(boundary, *spacing),
                HatchPattern::Concrete { spacing } => hatch_concrete(boundary, *spacing),
                HatchPattern::Insulation { spacing } => hatch_insulation(boundary, *spacing),
                HatchPattern::Earth { spacing } => hatch_earth(boundary, *spacing),
            };
            for [a, b] in segs {
                out.push((a, b));
            }
        }
        // Text annotations: tessellate via Hershey stroke font into world-space
        // segments. This makes them appear in viewports at world scale and in
        // PDF/SVG/DXF exports consistently (same path as geometry).
        Geometry::Annotation(Annotation::Text { pos, text, height }) => {
            let strokes = itsjustcad_doc::hershey::text_strokes(text, [pos.x, pos.y], *height);
            for poly in strokes {
                for pair in poly.windows(2) {
                    let a = DVec3::new(pair[0][0], pair[0][1], pos.z);
                    let b = DVec3::new(pair[1][0], pair[1][1], pos.z);
                    out.push((a, b));
                }
            }
        }
        // Block instances: resolve in the renderer. PDF export skips them
        // (no block definition expansion at PDF time yet).
        Geometry::Instance { .. } => {}
        // Point clouds are not rendered in PDF/print export.
        Geometry::Points { .. } => {}
    }
}

/// Dim-line text (label only, no arrows): emits BT...ET PDF text for a
/// dimension line between `p1` and `p2` (paper mm) with the given value string.
/// Text is centred above the midpoint, horizontal only (simplified).
fn emit_dim_text(p1: DVec2, p2: DVec2, label: &str, content: &mut String) {
    let mid = (p1 + p2) / 2.0;
    let font_pt = 7.0;
    // Rough char width in mm at 7pt Helvetica (≈ 4pt/char).
    let text_w_mm = label.len() as f64 * 4.0 / PT_PER_MM;
    content.push_str(&format!(
        "BT /F1 {font_pt} Tf {} {} Td ({}) Tj ET\n",
        mm(mid.x - text_w_mm / 2.0),
        mm(mid.y + 1.5), // 1.5 mm gap above the dim line
        escape_pdf_text(label)
    ));
}

/// Render a paper-space dimension (stored in Sheet.dims) into the content
/// stream. Draws witness lines, a dim line, and the label.
///
/// The model distance is recovered from the paper distance + view scale so the
/// label exactly reflects the geometry being measured.
fn render_sheet_dim(d: &SheetDim, view_scale: f64, content: &mut String) {
    let a = DVec2::new(d.a_mm[0], d.a_mm[1]);
    let b = DVec2::new(d.b_mm[0], d.b_mm[1]);

    let dir = (b - a).normalize_or_zero();
    // Perpendicular: rotate 90° CCW.
    let perp = DVec2::new(-dir.y, dir.x) * d.offset_mm;

    let a_off = a + perp;
    let b_off = b + perp;
    // Short witness lines with 1 mm gap.
    let gap_mm = 1.0_f64;
    let gap = perp.normalize_or_zero() * gap_mm;
    let a_start = a + gap;
    let b_start = b + gap;
    // 1 mm extension past the dim line.
    let ext = perp.normalize_or_zero() * 1.0;

    content.push_str(&format!(
        "0.25 w\n\
         {} {} m {} {} l S\n\
         {} {} m {} {} l S\n\
         {} {} m {} {} l S\n",
        mm(a_start.x), mm(a_start.y), mm((a_off + ext).x), mm((a_off + ext).y),
        mm(b_start.x), mm(b_start.y), mm((b_off + ext).x), mm((b_off + ext).y),
        mm(a_off.x),   mm(a_off.y),   mm(b_off.x),          mm(b_off.y),
    ));

    // Label: paper distance → model distance via scale.
    let paper_dist_mm = (b - a).length();
    let model_m = paper_dist_mm * view_scale / 1000.0;
    let label = format!("{:.3}m", model_m);
    emit_dim_text(a_off, b_off, &label, content);
}

/// Default fallback style when a layer has no explicit entry.
fn default_style() -> LayerStyle {
    LayerStyle::default()
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

/// Render one viewport's content (border + clipped scaled line art + label
/// + model-space dim text) into `content`.
///
/// Returns the number of geometry lines drawn.
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

    // Collect (lineweight_mm, world-segment) pairs, preserving layer weight.
    let mut weighted_segs: Vec<(f64, DVec3, DVec3)> = Vec::new();
    for obj in doc.objects() {
        if obj.visible && doc.layer_visible(&obj.layer) {
            let fallback = default_style();
            let style = doc.layers.get(&obj.layer).unwrap_or(&fallback);
            let w = style.lineweight_mm;
            let base = weighted_segs.len();
            let mut tmp = Vec::new();
            geometry_segments(&obj.geometry, &mut tmp);
            for (a, b) in tmp {
                weighted_segs.push((w, a, b));
            }
            let _ = base; // silence unused-variable
        }
    }
    if weighted_segs.is_empty() {
        return 0;
    }

    // Compute bounds for centering (weight-independent).
    let (mut lo, mut hi) = (DVec2::MAX, DVec2::MIN);
    for &(_, a, b) in &weighted_segs {
        let pa = {
            let p = project(view.direction, a);
            DVec2::new(world_to_paper_mm(p.x, view.scale), world_to_paper_mm(p.y, view.scale))
        };
        let pb = {
            let p = project(view.direction, b);
            DVec2::new(world_to_paper_mm(p.x, view.scale), world_to_paper_mm(p.y, view.scale))
        };
        lo = lo.min(pa.min(pb));
        hi = hi.max(pa.max(pb));
    }
    let offset = (rmin + rmax) / 2.0 - (lo + hi) / 2.0;
    let pad = 1.0;
    let (cmin, cmax) = (rmin + DVec2::splat(pad), rmax - DVec2::splat(pad));

    // Sort by lineweight so we can batch `w` operators.
    weighted_segs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut drawn = 0usize;
    // Use a sentinel that guarantees the first segment always emits a `w` op.
    let mut cur_w = -1.0_f64;
    for (w, wa, wb) in weighted_segs {
        let pa = {
            let p = project(view.direction, wa);
            DVec2::new(world_to_paper_mm(p.x, view.scale), world_to_paper_mm(p.y, view.scale))
        };
        let pb = {
            let p = project(view.direction, wb);
            DVec2::new(world_to_paper_mm(p.x, view.scale), world_to_paper_mm(p.y, view.scale))
        };
        if let Some((a, b)) = clip(pa + offset, pb + offset, cmin, cmax) {
            if (b - a).length() < 1e-6 {
                continue;
            }
            // Emit a `w` (line width) operator only on weight change.
            // PDF line width is in points; 1 mm = PT_PER_MM pt.
            if (w - cur_w).abs() > 1e-6 {
                content.push_str(&format!("{:.4} w\n", w * PT_PER_MM));
                cur_w = w;
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

    // Second pass: emit dim text labels for LinearDim annotations.
    for obj in doc.objects() {
        if !obj.visible || !doc.layer_visible(&obj.layer) {
            continue;
        }
        if let Geometry::Annotation(Annotation::LinearDim { a: da, b: db, offset: dim_off }) =
            &obj.geometry
        {
            let dir = (*db - *da).normalize_or_zero();
            let perp = DVec3::new(-dir.y, dir.x, 0.0) * *dim_off;
            // Position label at the midpoint of the dim line.
            let mid_world = (*da + *db) / 2.0 + perp;
            let projected = project(view.direction, mid_world);
            let paper_pos = DVec2::new(
                world_to_paper_mm(projected.x, view.scale),
                world_to_paper_mm(projected.y, view.scale),
            );
            // Apply the same viewport centering offset as the geometry pass.
            let text_pos = paper_pos + offset;
            // Only emit label when the midpoint falls inside the viewport.
            if text_pos.x > cmin.x && text_pos.x < cmax.x
                && text_pos.y > cmin.y && text_pos.y < cmax.y
            {
                let dist_m = (*db - *da).length();
                let label = format!("{:.3}m", dist_m);
                content.push_str(&format!(
                    "BT /F1 7 Tf {} {} Td ({}) Tj ET\n",
                    mm(text_pos.x),
                    mm(text_pos.y),
                    escape_pdf_text(&label)
                ));
            }
        }
    }

    drawn
}

/// Column headers and alignment for schedule tables.
const TABLE_COLS: [&str; 6] = ["Name", "ID", "Layer", "Type", "Area", "Volume"];
/// Table font size in points.
const TABLE_FONT_PT: f64 = 7.0;
/// Row height in mm.
const TABLE_ROW_MM: f64 = 4.5;
/// Left margin for the table block in mm.
const TABLE_LEFT_MM: f64 = 12.0;

/// Render a schedule table into `content` starting at `y_mm` (bottom of table
/// area, PDF coords). Returns the height consumed in mm.
fn render_table(rows: &[ScheduleRow], y_start_mm: f64, content: &mut String) -> f64 {
    if rows.is_empty() {
        return 0.0;
    }

    // Build cell text (no unit conversion; always meters).
    let cells: Vec<[String; 6]> = rows
        .iter()
        .map(|r| {
            [
                r.name.clone(),
                r.id.clone(),
                r.layer.clone(),
                r.kind.clone(),
                format!("{:.2}", r.area_m2),
                format!("{:.2}", r.volume_m3),
            ]
        })
        .collect();

    // Compute column widths in characters, then convert to mm (7pt Helvetica
    // average ~4.2pt per char, conservatively use 4pt so table fits A3).
    const CHAR_PT: f64 = 4.2;
    let mut widths = [0usize; 6];
    for (i, h) in TABLE_COLS.iter().enumerate() {
        widths[i] = h.len();
    }
    for row in &cells {
        for (i, c) in row.iter().enumerate() {
            widths[i] = widths[i].max(c.len());
        }
    }
    let col_mm: Vec<f64> = widths.iter().map(|w| (*w as f64 * CHAR_PT + 4.0) / PT_PER_MM).collect();
    let total_w: f64 = col_mm.iter().sum();

    let n_rows = 1 + rows.len(); // header + data
    let total_h = n_rows as f64 * TABLE_ROW_MM;

    // Draw header row background (light grey) for visual separation.
    let header_y = y_start_mm + (rows.len() as f64) * TABLE_ROW_MM;
    content.push_str(&format!(
        "0.85 g {} {} {} {} re f 0 g\n",
        mm(TABLE_LEFT_MM),
        mm(header_y),
        mm(total_w),
        mm(TABLE_ROW_MM)
    ));

    // Draw outer border.
    content.push_str(&format!(
        "0.5 w {} {} {} {} re S\n",
        mm(TABLE_LEFT_MM),
        mm(y_start_mm),
        mm(total_w),
        mm(total_h)
    ));

    // Draw all rows (header + data).
    let all_rows: Vec<[String; 6]> = std::iter::once(TABLE_COLS.map(str::to_string))
        .chain(cells)
        .collect();

    for (row_i, row) in all_rows.iter().enumerate() {
        // y of this row (header at top = high y, data rows below).
        let row_y = y_start_mm + (n_rows - 1 - row_i) as f64 * TABLE_ROW_MM;
        // Horizontal divider above this row (skip bottom-most).
        if row_i > 0 {
            content.push_str(&format!(
                "0.3 w {} {} m {} {} l S\n",
                mm(TABLE_LEFT_MM),
                mm(row_y + TABLE_ROW_MM),
                mm(TABLE_LEFT_MM + total_w),
                mm(row_y + TABLE_ROW_MM)
            ));
        }
        // Cell text.
        let mut x = TABLE_LEFT_MM;
        for (col_i, cell) in row.iter().enumerate() {
            content.push_str(&format!(
                "BT /F1 {TABLE_FONT_PT} Tf {} {} Td ({}) Tj ET\n",
                mm(x + 1.0),
                mm(row_y + 1.2),
                escape_pdf_text(cell)
            ));
            // Vertical separator.
            if col_i < 5 {
                let sep_x = x + col_mm[col_i];
                content.push_str(&format!(
                    "0.3 w {} {} m {} {} l S\n",
                    mm(sep_x),
                    mm(y_start_mm),
                    mm(sep_x),
                    mm(y_start_mm + total_h)
                ));
            }
            x += col_mm[col_i];
        }
    }

    total_h
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

    // Render schedule table first to know how much vertical space it takes.
    let table_h = if let Some(tbl) = &sheet.table {
        render_table(&tbl.rows, MARGIN_MM + TITLE_MM, &mut content)
    } else {
        0.0
    };
    let view_y0 = MARGIN_MM + TITLE_MM + if table_h > 0.0 { table_h + GUTTER_MM } else { 0.0 };

    // Equal horizontal slices between the margins, above the title strip (and table).
    let n = sheet.views.len();
    if n > 0 {
        let area_w = paper_w - 2.0 * MARGIN_MM;
        let view_w = (area_w - GUTTER_MM * (n as f64 - 1.0)) / n as f64;
        let (y0, y1) = (view_y0, paper_h - MARGIN_MM);
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

    // Paper-space dims (SheetDim): render after viewports so they overlay cleanly.
    for d in &sheet.dims {
        let scale = sheet.views.get(d.view_index).map(|v| v.scale).unwrap_or(100.0);
        render_sheet_dim(d, scale, &mut content);
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
            paper: itsjustcad_doc::PaperSize::A3,
            views: vec![],
            table: None,
            dims: vec![],
        };
        let (bytes, drawn) = sheet_pdf(&doc, &sheet);
        assert!(bytes.starts_with(b"%PDF"));
        assert!(bytes.ends_with(b"%%EOF\n"));
        assert_eq!(drawn, 0);
    }

    #[test]
    fn two_layer_weights_produce_distinct_w_ops_in_pdf() {
        use crate::{parse, Session};
        let mut s = Session::default();
        // Thin layer: default 0.18 mm. Use tiny geometry (mm-scale in meter
        // world-space) at 1:1000 so they map to a few mm on the A3 sheet.
        s.run(parse("layer thin").unwrap()).unwrap();
        s.run(parse("line 0,0,0 0.001,0,0").unwrap()).unwrap();
        // Heavy layer: 0.50 mm.
        s.run(parse("layer heavy").unwrap()).unwrap();
        s.run(parse("layerweight heavy 0.50").unwrap()).unwrap();
        s.run(parse("line 0.002,0,0 0.003,0,0").unwrap()).unwrap();
        s.run(parse("sheet s1 a3").unwrap()).unwrap();
        s.run(parse("sheetview s1 top 1000").unwrap()).unwrap();

        let sheet = s.doc.sheet("s1").unwrap().clone();
        let (bytes, drawn) = sheet_pdf(&s.doc, &sheet);
        assert_eq!(drawn, 2, "both lines drawn");
        // The content stream is ASCII inside the PDF binary. Scan it for
        // standalone `<number> w` lines (line-width operators) that come from
        // our per-layer weight batching; other `w`-containing lines include
        // `re S` and composite border ops which have trailing tokens.
        let content = String::from_utf8_lossy(&bytes);
        let w_ops: Vec<&str> = content
            .split('\n')
            .filter(|l| {
                let t = l.trim();
                // A standalone PDF `w` op: one number token then the `w` keyword.
                matches!(t.split_whitespace().collect::<Vec<_>>().as_slice(),
                    [_num, "w"])
            })
            .collect();
        assert!(w_ops.len() >= 2, "expected ≥2 standalone w ops, got: {w_ops:?}");
        // The two w values must differ.
        let w_values: std::collections::HashSet<&str> = w_ops.iter().copied().collect();
        assert!(w_values.len() >= 2, "w ops must have distinct values: {w_ops:?}");
    }

    /// Paper→model math at 1:100: 100 mm paper = 10 m model.
    #[test]
    fn paper_to_model_math_1_to_100() {
        // At 1:100: paper_mm * scale / 1000 = model_m
        let paper_mm = 100.0_f64;
        let scale = 100.0_f64;
        let model_m = paper_mm * scale / 1000.0;
        assert!((model_m - 10.0).abs() < 1e-12, "100mm @ 1:100 should be 10m, got {model_m}");

        // At 1:50: 50 mm = 2.5 m
        let model_m2: f64 = 50.0 * 50.0 / 1000.0;
        assert!((model_m2 - 2.5).abs() < 1e-12);

        // Inverse: world_to_paper_mm(10.0, 100.0) = 100 mm
        assert!((world_to_paper_mm(10.0, 100.0) - 100.0).abs() < 1e-12);
    }

    /// `sheetdim` is logged, stored in Sheet.dims, and replays identically.
    #[test]
    fn sheetdim_replay_stability() {
        use crate::{parse, Session};

        let mut s = Session::default();
        s.run(parse("sheet plan a3").unwrap()).unwrap();
        s.run(parse("sheetview plan top 100").unwrap()).unwrap();
        s.run(parse("sheetdim plan 20,30 120,30 10").unwrap()).unwrap();

        let sheet = s.doc.sheet("plan").unwrap();
        assert_eq!(sheet.dims.len(), 1, "dim stored on sheet");
        let d = &sheet.dims[0];
        assert!((d.a_mm[0] - 20.0).abs() < 1e-6);
        assert!((d.b_mm[0] - 120.0).abs() < 1e-6);
        assert!((d.offset_mm - 10.0).abs() < 1e-6);

        // Replay stability: save as JSON and reload; state must be identical.
        let json = crate::io::to_json(&s);
        let s2 = crate::io::from_json(&json).unwrap();
        let sheet2 = s2.doc.sheet("plan").unwrap();
        assert_eq!(sheet2.dims.len(), 1);
        assert_eq!(sheet2.dims[0], sheet.dims[0]);
        // Round-trip JSON must be identical (no non-determinism).
        assert_eq!(crate::io::to_json(&s2), json, "replay-stable JSON");
    }

    /// `sheetdim` undo removes the dim.
    #[test]
    fn sheetdim_undo_removes_dim() {
        use crate::{parse, Session};

        let mut s = Session::default();
        s.run(parse("sheet s1 a3").unwrap()).unwrap();
        s.run(parse("sheetview s1 top 100").unwrap()).unwrap();
        s.run(parse("sheetdim s1 10,10 110,10").unwrap()).unwrap();
        assert_eq!(s.doc.sheet("s1").unwrap().dims.len(), 1);

        s.run(parse("undo").unwrap()).unwrap();
        assert_eq!(s.doc.sheet("s1").unwrap().dims.len(), 0, "dim removed by undo");
    }

    /// PDF output contains the measured value string for a sheetdim.
    #[test]
    fn sheetdim_pdf_contains_dim_text() {
        use crate::{parse, Session};

        let mut s = Session::default();
        s.run(parse("sheet s1 a3").unwrap()).unwrap();
        s.run(parse("sheetview s1 top 100").unwrap()).unwrap();
        // 100 mm paper at 1:100 = 10 m model.
        s.run(parse("sheetdim s1 20,50 120,50 8").unwrap()).unwrap();

        let sheet = s.doc.sheet("s1").unwrap().clone();
        let (bytes, _) = sheet_pdf(&s.doc, &sheet);
        let content = String::from_utf8_lossy(&bytes);
        // The label "10.000m" should appear verbatim in the PDF content stream.
        assert!(
            content.contains("10.000m"),
            "PDF should contain dim label '10.000m'"
        );
    }

    /// Text annotation "HELLO" at 0.01m height renders as Hershey strokes in PDF.
    ///
    /// At 1:1 scale, 0.01m → 10mm on paper (fits comfortably on A3).
    /// "HELLO" has strokes: H(3), E(3), L(1), L(1), O(1) → ≥9 stroke segments.
    #[test]
    fn text_annotation_renders_as_hershey_strokes_in_pdf() {
        use crate::{parse, Session};

        let mut s = Session::default();
        // Place "HELLO" at origin. Height 0.01m = 10mm at 1:1 scale → fits on A3.
        s.run(parse("text 0,0,0 HELLO 0.01").unwrap()).unwrap();
        s.run(parse("sheet s1 a3").unwrap()).unwrap();
        // 1:1 scale: 0.01m world → 10mm on paper.
        s.run(parse("sheetview s1 top 1").unwrap()).unwrap();

        let sheet = s.doc.sheet("s1").unwrap().clone();
        let (bytes, drawn) = sheet_pdf(&s.doc, &sheet);
        // "HELLO" has 5 letters each with multiple strokes (H=3, E=3, L=1, L=1, O=1)
        // total ≥ 9 stroke segments drawn.
        assert!(drawn >= 9, "expected ≥9 stroke segments for HELLO, got {drawn}");
        // The PDF content must contain line-draw operators.
        let content = String::from_utf8_lossy(&bytes);
        // Count "l S" occurrences (each segment ends with " l S").
        let seg_count = content.matches(" l S\n").count();
        assert!(seg_count >= 9, "expected ≥9 PDF line segments for HELLO, got {seg_count}");
        // Confirm PDF structure is valid.
        assert!(bytes.starts_with(b"%PDF"), "valid PDF header");
        assert!(bytes.ends_with(b"%%EOF\n"), "valid PDF trailer");
    }

    /// Model-space LinearDim lines render inside a viewport (segments drawn > 0).
    #[test]
    fn model_space_dim_renders_in_viewport() {
        use crate::{parse, Session};

        let mut s = Session::default();
        // Create a dim between (0,0,0) and (5,0,0) with offset 0.5 m.
        s.run(parse("dim 0,0,0 5,0,0 0.5").unwrap()).unwrap();
        s.run(parse("sheet s1 a3").unwrap()).unwrap();
        s.run(parse("sheetview s1 top 100").unwrap()).unwrap();

        let sheet = s.doc.sheet("s1").unwrap().clone();
        let (bytes, drawn) = sheet_pdf(&s.doc, &sheet);
        // Dim generates 3 segments (2 witness + 1 dim line).
        assert!(drawn >= 3, "expected ≥3 segments for dim, got {drawn}");
        let content = String::from_utf8_lossy(&bytes);
        // The model distance label "5.000m" should appear in the PDF.
        assert!(content.contains("5.000m"), "PDF should contain dim label '5.000m'");
    }
}
