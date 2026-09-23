// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! DXF R12 (AC1009) ASCII export: the whole document as flat entities.
//! Hand-written — R12 is a simple tagged text format (group code line, value
//! line), which keeps the crate dependency-free. Lines, polylines, circles
//! and arcs export exactly; ellipses and NURBS tessellate (R12 has neither);
//! meshes export their feature edges as LINE entities.

use glam::DVec3;
use itsjustcad_doc::{Annotation, BlockGeometry, Document, Geometry};

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

/// A curve -> one entity (LINE/POLYLINE/CIRCLE/ARC; ellipses & NURBS tessellate).
fn curve_entity(t: &mut Tags, layer: &str, curve: &kernel_curve::Curve) -> usize {
    match curve {
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
    }
}

/// A mesh -> its feature edges as LINE entities. Returns the edge count.
fn mesh_entity(t: &mut Tags, layer: &str, mesh: &kernel_mesh::Mesh) -> usize {
    let edges = mesh_feature_edges(mesh);
    let n = edges.len();
    for (a, b) in edges {
        line(t, layer, a, b);
    }
    n
}

/// An annotation -> one or more entities.
fn annotation_entity(
    t: &mut Tags,
    layer: &str,
    a: &Annotation,
    units: itsjustcad_doc::Units,
    doc: Option<&Document>,
) -> usize {
    match a {
        Annotation::LinearDim { a, b, offset } => {
            // Resolve associative anchors to live points when a document is
            // available (top-level objects); block-baked dims have no document
            // and fall back to their last-known/free points.
            let (a, b) = match doc {
                Some(d) => d.resolve_dim(a, b),
                None => (a.point(), b.point()),
            };
            let (a, b) = (&a, &b);
            // Dimension line offset to the left of a->b, value as TEXT.
            let dir = (*b - *a).normalize_or_zero();
            let left = DVec3::new(-dir.y, dir.x, 0.0) * *offset;
            line(t, layer, *a + left, *b + left);
            let mid = (*a + *b) / 2.0 + left;
            text(t, layer, mid, 0.2, &itsjustcad_doc::format_length(units, (*b - *a).length()));
            2
        }
        Annotation::AngularDim { vertex, p1, p2, radius } => {
            // Two legs (as LINE) + arc (as POLYLINE) + the degree label as TEXT.
            // Leg endpoints come from the shared helper (its first two segments
            // are the legs); the arc stays a single POLYLINE entity for DXF.
            let scaffold = itsjustcad_doc::angular_dim_segments(*vertex, *p1, *p2, *radius);
            for &(a, b) in scaffold.iter().take(2) {
                line(t, layer, a, b);
            }
            let arc = itsjustcad_doc::angular_arc_points(*vertex, *p1, *p2, *radius);
            polyline(t, layer, &arc, false);
            let label_at = arc.get(arc.len() / 2).copied().unwrap_or(*vertex);
            text(
                t,
                layer,
                label_at,
                0.2,
                &itsjustcad_doc::format_angle(itsjustcad_doc::angle_degrees(*vertex, *p1, *p2)),
            );
            3
        }
        // A field exports like text — `text` is the resolved field value.
        Annotation::Text { pos, text: s, height }
        | Annotation::Field { pos, text: s, height, .. } => {
            // Tessellate via Hershey stroke font to world-space polylines so
            // the text renders at world scale consistently across all outputs.
            let strokes = itsjustcad_doc::hershey::text_strokes(s, [pos.x, pos.y], *height);
            let n = strokes.len();
            for poly in strokes {
                let pts: Vec<DVec3> = poly.iter().map(|p| DVec3::new(p[0], p[1], pos.z)).collect();
                polyline(t, layer, &pts, false);
            }
            n
        }
        Annotation::Hatch { boundary, .. } => {
            // Pattern dropped; the boundary survives as a closed polyline.
            polyline(t, layer, boundary, true);
            1
        }
    }
}

/// One block-definition geometry -> entities, written into a BLOCK body. Shares
/// the exact per-kind writers used for top-level entities so a block's contents
/// round-trip identically to loose geometry.
fn block_entity(
    t: &mut Tags,
    layer: &str,
    g: &BlockGeometry,
    units: itsjustcad_doc::Units,
) -> usize {
    match g {
        BlockGeometry::Curve(c) => curve_entity(t, layer, c),
        BlockGeometry::Mesh(m) => mesh_entity(t, layer, m),
        BlockGeometry::Annotation(a) => annotation_entity(t, layer, a, units, None),
    }
}

/// Emit an `INSERT` entity referencing block `name` (already sanitized) with the
/// instance's insertion point, uniform scale (41/42/43) and rotation (50).
fn insert(t: &mut Tags, layer: &str, name: &str, position: DVec3, rotation_deg: f64, scale: f64) {
    t.tag(0, "INSERT");
    t.tag(8, layer);
    t.tag(2, name);
    t.point(position);
    t.num(41, scale);
    t.num(42, scale);
    t.num(43, scale);
    t.num(50, rotation_deg.rem_euclid(360.0));
}

/// One document object -> zero or more entities. Returns entities written.
fn entity(
    t: &mut Tags,
    layer: &str,
    geometry: &Geometry,
    units: itsjustcad_doc::Units,
    doc: &Document,
) -> usize {
    match geometry {
        Geometry::Curve(curve) => curve_entity(t, layer, curve),
        Geometry::Mesh(mesh)
        | Geometry::Frame { mesh, .. }
        | Geometry::Area { mesh, .. }
        | Geometry::Parametric { mesh, .. } => mesh_entity(t, layer, mesh),
        Geometry::Annotation(a) => annotation_entity(t, layer, a, units, Some(doc)),
        // Block instances export as an INSERT referencing the sanitized block
        // name written in the BLOCKS section (R12 supports internal blocks; only
        // XREFs are unsupported). Parametric instances are baked: their geometry
        // already lives in `doc.blocks` under `block`, so they export as a plain
        // static block + INSERT like any other instance.
        Geometry::Instance { block, position, rotation_deg, scale, clip, .. } => {
            // Without an XCLIP boundary, export as a plain INSERT (compact, and
            // receivers keep the block reference). With a clip rect, an INSERT
            // can't be culled by the receiver, so expand the block geometry to
            // world coords and emit only the in-rect primitives — same
            // all-or-nothing per-primitive cull the viewport (snapshot.rs) uses:
            // a segment survives if both endpoints are inside the rect; a face if
            // all its verts are inside. No border splitting (approximate MVP).
            match clip {
                None => {
                    insert(t, layer, &dxf_layer(block), *position, *rotation_deg, *scale);
                    1
                }
                Some(rect) => clipped_instance_entities(
                    t, layer, block, *position, *rotation_deg, *scale, rect, doc,
                ),
            }
        }
        // Point clouds are not representable as DXF entities in this exporter.
        Geometry::Points { .. } => 0,
    }
}

/// Expand a clipped block instance to world-space LINE entities, culling
/// primitives outside `rect`. Returns the number of entities written. Mirrors
/// the viewport cull (snapshot.rs): a mesh face is kept only if all its verts
/// are inside the rect; a curve segment only if both endpoints are inside. No
/// border splitting — an out-of-rect primitive is dropped whole (approximate,
/// matches the display).
#[allow(clippy::too_many_arguments)]
fn clipped_instance_entities(
    t: &mut Tags,
    layer: &str,
    block: &str,
    position: DVec3,
    rotation_deg: f64,
    scale: f64,
    rect: &itsjustcad_doc::ClipRect,
    doc: &Document,
) -> usize {
    let Some(defs) = doc.blocks.get(block) else { return 0 };
    let rot = rotation_deg.to_radians();
    let (sin_r, cos_r) = rot.sin_cos();
    let transform = |p: DVec3| -> DVec3 {
        let ps = p * scale;
        DVec3::new(
            ps.x * cos_r - ps.y * sin_r + position.x,
            ps.x * sin_r + ps.y * cos_r + position.y,
            ps.z + position.z,
        )
    };
    let mut count = 0usize;
    for def in defs {
        match def {
            BlockGeometry::Mesh(m) => {
                // feature_edges returns endpoints in the def frame; transform to world.
                for (ea, eb) in mesh_feature_edges(m) {
                    let (a, b) = (transform(ea), transform(eb));
                    if rect.contains_xy(a) && rect.contains_xy(b) {
                        line(t, layer, a, b);
                        count += 1;
                    }
                }
            }
            BlockGeometry::Curve(c) => {
                let mut pts: Vec<DVec3> = c.tessellate(EXPORT_TOL).iter().map(|&p| transform(p)).collect();
                if c.is_closed() && let Some(first) = pts.first().copied() {
                    pts.push(first);
                }
                for pair in pts.windows(2) {
                    if rect.contains_xy(pair[0]) && rect.contains_xy(pair[1]) {
                        line(t, layer, pair[0], pair[1]);
                        count += 1;
                    }
                }
            }
            BlockGeometry::Annotation(_) => {
                // Annotations in clipped block defs are not exported (matches viewport).
            }
        }
    }
    count
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

    // BLOCKS section: one BLOCK/ENDBLK per definition in `doc.blocks`. Parametric
    // (dynamic) blocks have no DXF equivalent, but each parametric instance keeps
    // its baked geometry in `doc.blocks` under its per-instance key, so those keys
    // export as ordinary static blocks here — no special handling needed. Block
    // bodies are written at the origin (base point 0,0,0); INSERT places them.
    t.tag(0, "SECTION");
    t.tag(2, "BLOCKS");
    for (name, geoms) in &doc.blocks {
        let bname = dxf_layer(name);
        t.tag(0, "BLOCK");
        t.tag(8, "0");
        t.tag(2, &bname);
        t.tag(70, "0"); // 0 = a normal (non-anonymous) block
        t.point(DVec3::ZERO); // base point at origin
        t.tag(3, &bname); // block name repeated (R12 convention)
        for g in geoms {
            block_entity(&mut t, "0", g, doc.units);
        }
        t.tag(0, "ENDBLK");
        t.tag(8, "0");
    }
    t.tag(0, "ENDSEC");

    t.tag(0, "SECTION");
    t.tag(2, "ENTITIES");
    let mut count = 0usize;
    for obj in doc.objects() {
        count += entity(&mut t, &dxf_layer(&obj.layer), &obj.geometry, doc.units, doc);
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
    // The whole file is "group code line, value line" pairs. Be RESILIENT: real
    // exporters (e.g. LibreDWG's DWG→DXF) emit multi-line string values (an MTEXT
    // group-1/3 disclaimer, a wrapped TEXT). Those continuation lines land where a
    // group code is expected and are NOT integers. A strict parser aborts the
    // whole import on the first one; instead we SKIP a non-integer "code" line and
    // resync on the next — losing at most the tail of one string, never the file.
    let mut pairs: Vec<(i32, &str)> = Vec::new();
    let mut lines = text.lines();
    while let Some(code) = lines.next() {
        let code = code.trim();
        if code.is_empty() {
            continue; // tolerate blank lines anywhere
        }
        let Ok(code) = code.parse::<i32>() else {
            // Stray line: a continuation of a preceding multi-line string value
            // (or junk). Drop it and try the next line as a code — this resyncs
            // after one skip rather than failing the whole import.
            continue;
        };
        let Some(value) = lines.next() else {
            break; // truncated tail — keep everything parsed so far
        };
        pairs.push((code, value.trim()));
    }

    // Cut the ENTITIES and BLOCKS sections into records (each starts at a 0 code).
    // ENTITIES holds the drawing; BLOCKS holds block DEFINITIONS that INSERT
    // entities reference — we need both to instance blocks.
    #[derive(Clone, Copy, PartialEq)]
    enum Sec {
        None,
        Entities,
        Blocks,
    }
    let mut records: Vec<(&str, Vec<(i32, &str)>)> = Vec::new();
    let mut blk_records: Vec<(&str, Vec<(i32, &str)>)> = Vec::new();
    let mut sec = Sec::None;
    let mut awaiting_section_name = false;
    let mut saw_entities = false;
    for &(code, value) in &pairs {
        match code {
            0 if value == "SECTION" => awaiting_section_name = true,
            2 if awaiting_section_name => {
                sec = match value {
                    "ENTITIES" => Sec::Entities,
                    "BLOCKS" => Sec::Blocks,
                    _ => Sec::None,
                };
                saw_entities |= sec == Sec::Entities;
                awaiting_section_name = false;
            }
            0 if value == "ENDSEC" => sec = Sec::None,
            0 => match sec {
                Sec::Entities => records.push((value, Vec::new())),
                Sec::Blocks => blk_records.push((value, Vec::new())),
                Sec::None => {}
            },
            _ => match sec {
                Sec::Entities => {
                    if let Some((_, fields)) = records.last_mut() {
                        fields.push((code, value));
                    }
                }
                Sec::Blocks => {
                    if let Some((_, fields)) = blk_records.last_mut() {
                        fields.push((code, value));
                    }
                }
                Sec::None => {}
            },
        }
    }

    // Resilience vs. gibberish: an empty-but-valid DXF (SECTION/ENTITIES/ENDSEC
    // with no entities) is fine, but a file that never even reached an ENTITIES
    // section is not a DXF — surface a friendly error instead of importing nothing.
    if !saw_entities {
        return Err(
            "not a DXF: no ENTITIES section found (wrong format or corrupt file?)".to_string(),
        );
    }

    // Block definitions, keyed by name. INSERT entities are instanced against
    // these; a BlockDefine op is emitted (before any insert) for each.
    let blocks = parse_blocks(blk_records).map_err(|e| e.to_string())?;

    // Fold records into commands; VERTEX/SEQEND attach to the open POLYLINE.
    let mut out = DxfEntities { entities: Vec::new(), skipped: 0 };
    let mut open_poly: Option<(String, bool, Vec<DVec3>)> = None; // layer, closed, points
    // POINT and 3DFACE are AGGREGATED per layer: a survey DXF has tens of
    // thousands of each, so one point cloud / one mesh per layer beats creating
    // 50k individual objects (and 50k logged ops). Emitted after the fold.
    use std::collections::BTreeMap;
    let mut points_by_layer: BTreeMap<String, Vec<DVec3>> = BTreeMap::new();
    let mut mesh_by_layer: BTreeMap<String, (Vec<DVec3>, Vec<[u32; 3]>)> = BTreeMap::new();
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
        // Aggregated kinds handled here (record_entity returns one command each;
        // these fold into shared per-layer buffers instead).
        match name {
            "INSERT" => {
                // Instance a block definition. Name (2) + insertion point (10) +
                // uniform scale (41; DXF's non-uniform 42/43 collapse to it) +
                // rotation (50). Unknown/empty blocks are skipped.
                let bname = fields.iter().find(|(c, _)| *c == 2).map(|(_, v)| v.to_string());
                match bname {
                    Some(bn) if blocks.contains_key(&bn) => {
                        let position = record_point(&fields, 10).unwrap_or(DVec3::ZERO);
                        let scale = record_num(&fields, 41).filter(|s| *s > 0.0).unwrap_or(1.0);
                        let rotation_deg = record_num(&fields, 50).unwrap_or(0.0);
                        out.entities.push((
                            record_layer(&fields),
                            crate::Command::BlockInsert {
                                id: None,
                                name: bn,
                                position,
                                rotation_deg: Some(rotation_deg),
                                scale: Some(scale),
                                params: Default::default(),
                            },
                        ));
                    }
                    _ => out.skipped += 1,
                }
                continue;
            }
            "POINT" => {
                match record_point(&fields, 10) {
                    Some(p) => points_by_layer.entry(record_layer(&fields)).or_default().push(p),
                    None => out.skipped += 1,
                }
                continue;
            }
            "3DFACE" => {
                // Corners 10/11/12/13; a triangle when the 4th equals the 3rd.
                match (
                    record_point(&fields, 10),
                    record_point(&fields, 11),
                    record_point(&fields, 12),
                ) {
                    (Some(a), Some(b), Some(c)) => {
                        let (pos, faces) =
                            mesh_by_layer.entry(record_layer(&fields)).or_default();
                        let base = pos.len() as u32;
                        pos.extend([a, b, c]);
                        faces.push([base, base + 1, base + 2]);
                        if let Some(d) = record_point(&fields, 13)
                            && (d - c).length() > 1e-9
                        {
                            let di = pos.len() as u32;
                            pos.push(d);
                            faces.push([base, base + 2, di]);
                        }
                    }
                    _ => out.skipped += 1,
                }
                continue;
            }
            _ => {}
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
    // Emit the aggregates: one point cloud and one mesh per layer.
    for (layer, positions) in points_by_layer {
        if !positions.is_empty() {
            out.entities
                .push((layer, crate::Command::PointLiteral { id: None, positions }));
        }
    }
    for (layer, (positions, faces)) in mesh_by_layer {
        if !faces.is_empty() {
            out.entities.push((
                layer,
                crate::Command::MeshLiteral {
                    id: None,
                    positions,
                    faces,
                    name: Some("dxf 3dfaces".to_string()),
                },
            ));
        }
    }
    // Prepend one BlockDefine per block so every BlockInsert resolves against an
    // already-defined block (import applies commands in order). Empty selector —
    // exec's replay path takes the geometry verbatim, no source objects needed.
    if !blocks.is_empty() {
        let mut defs: Vec<(String, crate::Command)> = blocks
            .into_iter()
            .map(|(name, geometries)| {
                (
                    "0".to_string(),
                    crate::Command::BlockDefine {
                        targets: crate::Selector::Ids { ids: Vec::new() },
                        name,
                        geometries: Some(geometries),
                    },
                )
            })
            .collect();
        defs.append(&mut out.entities);
        out.entities = defs;
    }
    Ok(out)
}

/// A nested `INSERT` inside a block body, captured during the scan and resolved
/// in a second pass (the referenced block may be defined later in the file).
struct NestedInsert {
    /// Name of the block this INSERT references.
    block: String,
    position: DVec3,
    rotation_deg: f64,
    scale: f64,
}

/// A raw (pre-resolution) block definition: its directly-committed geometry plus
/// any nested INSERTs to be baked once every definition is known.
struct RawBlock {
    base: DVec3,
    geoms: Vec<itsjustcad_doc::BlockGeometry>,
    nested: Vec<NestedInsert>,
}

/// Parse the BLOCKS-section records into block definitions. Each `BLOCK`…`ENDBLK`
/// span is one named definition; its entities become [`BlockGeometry`] translated
/// so the block base point (group 10 of `BLOCK`) sits at the origin (INSERT's
/// insertion point then places it).
///
/// Block-body coverage (M-dwg-bridge — real architectural blocks were coming in
/// empty): flat LINE/POLYLINE/LWPOLYLINE/CIRCLE/ARC/TEXT/MTEXT/DIMENSION/ELLIPSE
/// **plus** HATCH (→ boundary polyline), SPLINE (→ tessellated polyline) and
/// NESTED INSERTs (→ the referenced block's geometry BAKED in at the insert's
/// transform, since [`BlockGeometry`] is deliberately flat and cannot hold a
/// nested reference). Point clouds and meshes inside a block are still dropped
/// (blocks are 2D symbols in practice). A block whose body mixes mappable and
/// unmappable bodies keeps what maps rather than being discarded wholesale.
fn parse_blocks(
    records: Vec<(&str, Vec<(i32, &str)>)>,
) -> Result<std::collections::BTreeMap<String, Vec<itsjustcad_doc::BlockGeometry>>, crate::ExecError>
{
    use itsjustcad_doc::BlockGeometry;
    use kernel_curve::Curve;
    // Preserve definition order for stable, deterministic baking.
    let mut raw: Vec<(String, RawBlock)> = Vec::new();
    let mut cur: Option<(String, RawBlock)> = None;
    let mut open_poly: Option<(String, bool, Vec<DVec3>)> = None;
    for (name, fields) in records {
        match name {
            "BLOCK" => {
                let bname = fields
                    .iter()
                    .find(|(c, _)| *c == 2)
                    .map(|(_, v)| v.to_string())
                    .unwrap_or_default();
                let base = record_point(&fields, 10).unwrap_or(DVec3::ZERO);
                cur = Some((bname, RawBlock { base, geoms: Vec::new(), nested: Vec::new() }));
                open_poly = None;
            }
            "ENDBLK" => {
                // Flush a still-open POLYLINE, then stash the raw block.
                if let (Some((_, blk)), Some((_, closed, pts))) =
                    (cur.as_mut(), open_poly.take())
                    && pts.len() >= 2
                {
                    blk.geoms.push(BlockGeometry::Curve(Curve::Polyline { points: pts, closed }));
                }
                if let Some((bname, blk)) = cur.take()
                    && !bname.is_empty()
                {
                    raw.push((bname, blk));
                }
            }
            _ => {
                let Some((_, blk)) = cur.as_mut() else {
                    continue;
                };
                // POLYLINE vertex folding, mirroring the ENTITIES path.
                if let Some((_, closed, pts)) = open_poly.as_mut() {
                    match name {
                        "VERTEX" => {
                            if let Some(p) = record_point(&fields, 10) {
                                pts.push(p);
                            }
                            continue;
                        }
                        "SEQEND" => {
                            let (closed, pts) = (*closed, std::mem::take(pts));
                            open_poly = None;
                            if pts.len() >= 2 {
                                blk.geoms.push(BlockGeometry::Curve(Curve::Polyline {
                                    points: pts,
                                    closed,
                                }));
                            }
                            continue;
                        }
                        _ => open_poly = None, // unterminated: drop, fall through
                    }
                }
                if name == "POLYLINE" {
                    let closed = record_num(&fields, 70).unwrap_or(0.0) as i64 & 1 != 0;
                    open_poly = Some((String::new(), closed, Vec::new()));
                    continue;
                }
                // A NESTED insert: record it for baking in the resolution pass.
                if name == "INSERT" {
                    if let Some(bn) = fields.iter().find(|(c, _)| *c == 2).map(|(_, v)| v.to_string())
                    {
                        blk.nested.push(NestedInsert {
                            block: bn,
                            position: record_point(&fields, 10).unwrap_or(DVec3::ZERO),
                            rotation_deg: record_num(&fields, 50).unwrap_or(0.0),
                            scale: record_num(&fields, 41).filter(|s| *s > 0.0).unwrap_or(1.0),
                        });
                    }
                    continue;
                }
                let mut dummy = None;
                if let RecordOutcome::Entity(_, cmd) = record_entity(name, &fields, &mut dummy)
                    && let Some(g) = command_to_block_geometry(&cmd)?
                {
                    blk.geoms.push(g);
                }
            }
        }
    }
    Ok(resolve_blocks(raw))
}

/// Hard caps to keep a crafted (malicious) DXF from exhausting memory or the
/// stack during nested-block baking:
/// - `MAX_BLOCK_DEPTH` bounds recursion on a deep *acyclic* nested-block chain
///   (the cycle guard alone does not — an acyclic chain never repeats a name).
/// - `MAX_TOTAL_BAKED_GEOMS` bounds a diamond DAG, where a shared child is baked
///   into every parent, so geometry can grow ~2^depth (exponential blow-up / OOM).
const MAX_BLOCK_DEPTH: usize = 48;
const MAX_TOTAL_BAKED_GEOMS: usize = 300_000;

/// Second pass: re-origin each raw block on its base point and BAKE nested
/// INSERTs by copying the referenced block's (already re-origined) geometry in
/// at the insert's position/rotation/scale. Three guards keep hostile input safe:
/// a **visited** set breaks reference *cycles*, a **depth** cap bounds deep
/// acyclic nesting chains, and a running **geometry budget** bounds diamond DAGs
/// (exponential fan-out). Past any cap the offending edge is skipped, not baked.
fn resolve_blocks(
    raw: Vec<(String, RawBlock)>,
) -> std::collections::BTreeMap<String, Vec<itsjustcad_doc::BlockGeometry>> {
    use std::collections::BTreeMap;
    let by_name: BTreeMap<String, &RawBlock> =
        raw.iter().map(|(n, b)| (n.clone(), b)).collect();
    let mut out: BTreeMap<String, Vec<itsjustcad_doc::BlockGeometry>> = BTreeMap::new();
    // Total baked geometry across ALL blocks in this file — the diamond-DAG cap.
    let mut total_baked: usize = 0;
    for (name, _blk) in &raw {
        let mut visiting = std::collections::BTreeSet::new();
        let geoms = bake_block(name, &by_name, &mut visiting, 0, &mut total_baked);
        if !geoms.is_empty() {
            out.insert(name.clone(), geoms);
        }
        if total_baked >= MAX_TOTAL_BAKED_GEOMS {
            // Budget exhausted: stop resolving further blocks rather than risk OOM.
            break;
        }
    }
    out
}

/// Recursively build one block's re-origined geometry, baking nested inserts.
/// `visiting` holds the ancestry to break cycles; `depth` bounds acyclic nesting;
/// `total_baked` is the shared running geometry budget (diamond-DAG guard).
fn bake_block(
    name: &str,
    by_name: &std::collections::BTreeMap<String, &RawBlock>,
    visiting: &mut std::collections::BTreeSet<String>,
    depth: usize,
    total_baked: &mut usize,
) -> Vec<itsjustcad_doc::BlockGeometry> {
    if depth >= MAX_BLOCK_DEPTH || *total_baked >= MAX_TOTAL_BAKED_GEOMS {
        return Vec::new(); // depth/budget cap — skip this edge
    }
    let Some(blk) = by_name.get(name) else {
        return Vec::new();
    };
    if !visiting.insert(name.to_string()) {
        return Vec::new(); // cycle — skip this edge
    }
    // Direct geometry, re-origined on the base point.
    let mut geoms = blk.geoms.clone();
    for g in geoms.iter_mut() {
        translate_block_geom(g, -blk.base);
    }
    // Count retained geometry against the budget. Children counted as they are
    // pushed below (a diamond DAG copies a shared child into each parent, so each
    // copy is real retained geometry and must count separately).
    *total_baked = total_baked.saturating_add(geoms.len());
    // Baked nested inserts: the child's geometry (already re-origined) placed at
    // the insert transform, then shifted so the PARENT'S base sits at the origin.
    for ins in &blk.nested {
        if *total_baked >= MAX_TOTAL_BAKED_GEOMS {
            break; // budget exhausted — stop baking further children
        }
        let child = bake_block(&ins.block, by_name, visiting, depth + 1, total_baked);
        for mut g in child {
            transform_block_geom(&mut g, ins.scale, ins.rotation_deg.to_radians(), ins.position);
            translate_block_geom(&mut g, -blk.base);
            geoms.push(g);
            *total_baked = total_baked.saturating_add(1);
        }
    }
    visiting.remove(name);
    geoms
}

/// Apply a uniform scale, CCW rotation about +Z (radians) and translation to
/// block geometry, in that order (scale, rotate, then translate) — matching how
/// an INSERT places a block. Points are transformed about the origin.
fn transform_block_geom(g: &mut itsjustcad_doc::BlockGeometry, scale: f64, rot: f64, off: DVec3) {
    let (s, c) = rot.sin_cos();
    let xf = |p: DVec3| -> DVec3 {
        let x = p.x * scale;
        let y = p.y * scale;
        DVec3::new(x * c - y * s + off.x, x * s + y * c + off.y, p.z * scale + off.z)
    };
    map_block_geom_points(g, xf, scale);
}

/// Apply a point map (and radius scale, for arcs) to every point of a block
/// geometry in place. `rscale` scales radii/heights that don't come from points.
fn map_block_geom_points(
    g: &mut itsjustcad_doc::BlockGeometry,
    xf: impl Fn(DVec3) -> DVec3,
    rscale: f64,
) {
    use itsjustcad_doc::{Annotation, BlockGeometry};
    use kernel_curve::Curve;
    match g {
        BlockGeometry::Curve(Curve::Line { a, b }) => {
            *a = xf(*a);
            *b = xf(*b);
        }
        BlockGeometry::Curve(Curve::Polyline { points, .. }) => {
            for p in points.iter_mut() {
                *p = xf(*p);
            }
        }
        BlockGeometry::Curve(Curve::Arc { center, radius, .. }) => {
            *center = xf(*center);
            *radius *= rscale.abs();
        }
        BlockGeometry::Curve(Curve::Ellipse { center, rx, ry, .. }) => {
            *center = xf(*center);
            *rx *= rscale.abs();
            *ry *= rscale.abs();
        }
        BlockGeometry::Curve(Curve::Nurbs { control, .. }) => {
            for p in control.iter_mut() {
                *p = xf(*p);
            }
        }
        BlockGeometry::Annotation(Annotation::Text { pos, height, .. })
        | BlockGeometry::Annotation(Annotation::Field { pos, height, .. }) => {
            *pos = xf(*pos);
            *height *= rscale.abs();
        }
        BlockGeometry::Annotation(Annotation::LinearDim { a, b, .. }) => {
            // Block dims are baked/free; transform their stored points in place.
            if let itsjustcad_doc::DimAnchor::Free(p) = a {
                *p = xf(*p);
            }
            if let itsjustcad_doc::DimAnchor::Free(p) = b {
                *p = xf(*p);
            }
        }
        BlockGeometry::Annotation(Annotation::AngularDim { vertex, p1, p2, radius }) => {
            *vertex = xf(*vertex);
            *p1 = xf(*p1);
            *p2 = xf(*p2);
            *radius *= rscale.abs();
        }
        BlockGeometry::Annotation(Annotation::Hatch { boundary, .. }) => {
            for p in boundary.iter_mut() {
                *p = xf(*p);
            }
        }
        BlockGeometry::Mesh(_) => {}
    }
}

/// Convert a parsed entity command into block geometry (curve/annotation).
///
/// Returns `Ok(None)` for commands with no block representation (point clouds,
/// meshes) — those are silently dropped from block definitions. Returns `Err`
/// for a command that *should* be a block body but cannot be baked losslessly:
/// an associative (object-bound) dimension carries a live binding that a static
/// block definition has no document to resolve, so it is rejected rather than
/// silently fabricating a bogus origin point.
fn command_to_block_geometry(
    cmd: &crate::Command,
) -> Result<Option<itsjustcad_doc::BlockGeometry>, crate::ExecError> {
    use crate::Command;
    use itsjustcad_doc::{Annotation, BlockGeometry};
    use kernel_curve::Curve;
    Ok(Some(match cmd {
        Command::Line { a, b, .. } => BlockGeometry::Curve(Curve::Line { a: *a, b: *b }),
        Command::Polyline { points, closed, .. } => {
            BlockGeometry::Curve(Curve::Polyline { points: points.clone(), closed: *closed })
        }
        Command::Circle { center, radius, .. } => BlockGeometry::Curve(Curve::Arc {
            center: *center,
            radius: *radius,
            start: 0.0,
            end: std::f64::consts::TAU,
        }),
        Command::Arc { center, radius, start_deg, end_deg, .. } => BlockGeometry::Curve(Curve::Arc {
            center: *center,
            radius: *radius,
            start: start_deg.to_radians(),
            end: end_deg.to_radians(),
        }),
        Command::Text { pos, text, height, .. } => {
            BlockGeometry::Annotation(Annotation::Text { pos: *pos, text: text.clone(), height: *height })
        }
        Command::Dim { a, b, offset, .. } => {
            // Block-baked dims are static: a block definition carries no live
            // document, so only *free-point* anchors can be baked losslessly.
            // An object-bound (associative) anchor has no point to fold to here
            // — reject it rather than fabricate a meaningless origin point.
            use itsjustcad_doc::DimAnchor;
            let bake = |spec: &crate::DimAnchorSpec| -> Result<DimAnchor, crate::ExecError> {
                match spec {
                    crate::DimAnchorSpec::Free(p) => Ok(DimAnchor::Free(*p)),
                    crate::DimAnchorSpec::Object { .. } => Err(crate::ExecError::Invalid(
                        "associative dimensions cannot be baked into a block — \
                         use free points (dim x,y,z ...)"
                            .into(),
                    )),
                }
            };
            BlockGeometry::Annotation(Annotation::LinearDim {
                a: bake(a)?,
                b: bake(b)?,
                offset: *offset,
            })
        }
        _ => return Ok(None),
    }))
}

/// Shift block geometry by `d` (used to re-origin a block on its base point).
fn translate_block_geom(g: &mut itsjustcad_doc::BlockGeometry, d: DVec3) {
    use itsjustcad_doc::{Annotation, BlockGeometry};
    match g {
        BlockGeometry::Curve(c) => c.translate(d),
        BlockGeometry::Annotation(Annotation::Text { pos, .. })
        | BlockGeometry::Annotation(Annotation::Field { pos, .. }) => *pos += d,
        BlockGeometry::Annotation(Annotation::LinearDim { a, b, .. }) => {
            // Free anchors translate; object bindings follow their referent.
            a.translate(d);
            b.translate(d);
        }
        BlockGeometry::Annotation(Annotation::AngularDim { vertex, p1, p2, .. }) => {
            *vertex += d;
            *p1 += d;
            *p2 += d;
        }
        BlockGeometry::Annotation(Annotation::Hatch { boundary, .. }) => {
            boundary.iter_mut().for_each(|p| *p += d);
        }
        BlockGeometry::Mesh(_) => {}
    }
}

// Short-lived per-record return value (never stored in bulk), so the size gap
// between Entity and the unit variants is harmless — boxing would add an
// allocation per DXF entity for a cosmetic lint.
#[allow(clippy::large_enum_variant)]
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
        "MTEXT" => match (record_point(fields, 10), record_num(fields, 40)) {
            // Insertion point (10), char height (40). Body is the group-3 chunks
            // (250-char continuations) followed by the group-1 tail; inline MTEXT
            // formatting codes (\P, {\f…;}, \H…;) are stripped to plain text.
            (Some(pos), Some(height)) if height > 0.0 => {
                let mut raw = String::new();
                for (c, v) in fields {
                    if *c == 3 {
                        raw.push_str(v);
                    }
                }
                if let Some((_, v)) = fields.iter().find(|(c, _)| *c == 1) {
                    raw.push_str(v);
                }
                let text = mtext_plain(&raw);
                (!text.is_empty()).then_some(Command::Text { id: None, pos, text, height })
            }
            _ => None,
        },
        "DIMENSION" => {
            // Linear/aligned only (group-70 low 3 bits: 0 = rotated, 1 = aligned).
            // Extension-line origins are 13/14; the dimension-line point is 10.
            // Offset = signed perpendicular distance from 10 to the 13→14 line.
            let dtype = record_num(fields, 70).unwrap_or(0.0) as i64 & 7;
            match (
                dtype,
                record_point(fields, 13),
                record_point(fields, 14),
                record_point(fields, 10),
            ) {
                (0 | 1, Some(a), Some(b), Some(dimline)) => {
                    let ab = b - a;
                    let offset = if ab.length() > 1e-9 {
                        let n = DVec3::new(-ab.y, ab.x, 0.0).normalize();
                        (dimline - a).dot(n)
                    } else {
                        0.0
                    };
                    Some(Command::Dim {
                        id: None,
                        a: crate::DimAnchorSpec::free(a),
                        b: crate::DimAnchorSpec::free(b),
                        offset,
                    })
                }
                _ => None,
            }
        }
        "ELLIPSE" => {
            // center (10), major-axis endpoint RELATIVE to center (11/21), ratio
            // minor/major (40), param range (41/42, radians; default full turn).
            // Our Ellipse command is axis-aligned, so a possibly-rotated DXF
            // ellipse is tessellated to a polyline (also handles partial arcs).
            match (
                record_point(fields, 10),
                record_num(fields, 11),
                record_num(fields, 21),
                record_num(fields, 40),
            ) {
                (Some(center), Some(mx), Some(my), Some(ratio)) => {
                    let start = record_num(fields, 41).unwrap_or(0.0);
                    let end = record_num(fields, 42).unwrap_or(std::f64::consts::TAU);
                    let major = DVec3::new(mx, my, 0.0);
                    let minor = DVec3::new(-major.y, major.x, 0.0) * ratio;
                    let closed = (end - start - std::f64::consts::TAU).abs() < 1e-6;
                    let steps = 64usize;
                    let n = if closed { steps } else { steps + 1 };
                    let pts: Vec<DVec3> = (0..n)
                        .map(|i| {
                            let t = start + (end - start) * (i as f64 / steps as f64);
                            center + major * t.cos() + minor * t.sin()
                        })
                        .collect();
                    (pts.len() >= 2).then_some(Command::Polyline { id: None, points: pts, closed })
                }
                _ => None,
            }
        }
        "SPLINE" => {
            // A NURBS curve. We have no exact spline primitive, so tessellate to
            // a polyline: prefer the FIT points (11/21/31 — the on-curve points
            // the spline interpolates), else fall back to the control points
            // (10/20/30), which form the spline's control polygon and give a
            // reasonable coarse approximation. Group 70 bit 1 = closed. This is
            // the same "unsupported curve → polyline" stance the exporter takes.
            let flags = record_num(fields, 70).unwrap_or(0.0) as i64;
            let closed = flags & 1 != 0;
            let mut fit = Vec::new();
            let mut ctrl = Vec::new();
            let mut fx: Option<f64> = None;
            let mut cx: Option<f64> = None;
            for (code, value) in fields {
                match (code, value.parse::<f64>()) {
                    (11, Ok(v)) => fx = Some(v),
                    (21, Ok(y)) => {
                        if let Some(x) = fx.take() {
                            fit.push(DVec3::new(x, y, 0.0));
                        }
                    }
                    (10, Ok(v)) => cx = Some(v),
                    (20, Ok(y)) => {
                        if let Some(x) = cx.take() {
                            ctrl.push(DVec3::new(x, y, 0.0));
                        }
                    }
                    _ => {}
                }
            }
            let points = if fit.len() >= 2 { fit } else { ctrl };
            (points.len() >= 2).then_some(Command::Polyline { id: None, points, closed })
        }
        "HATCH" => {
            // Import the hatch BOUNDARY as a closed polyline (the fill pattern is
            // dropped — Command::Hatch fills by selector, not raw geometry). Verts
            // are the 10/20 pairs inside the boundary-path block: after group 91
            // (path count) and before group 75 (hatch style, which begins the
            // pattern/seed section that ALSO uses 10/20).
            let mut pts = Vec::new();
            let mut in_boundary = false;
            let mut x: Option<f64> = None;
            for (code, value) in fields {
                match code {
                    91 => in_boundary = true,
                    75 => in_boundary = false,
                    10 if in_boundary => x = value.parse::<f64>().ok(),
                    20 if in_boundary => {
                        if let (Some(px), Ok(py)) = (x.take(), value.parse::<f64>()) {
                            pts.push(DVec3::new(px, py, 0.0));
                        }
                    }
                    _ => {}
                }
            }
            (pts.len() >= 2).then_some(Command::Polyline { id: None, points: pts, closed: true })
        }
        _ => None,
    };
    match cmd {
        Some(cmd) => RecordOutcome::Entity(layer, cmd),
        None => RecordOutcome::Skipped,
    }
}

/// Strip MTEXT inline formatting to plain text. Handles the common codes: `\P`
/// (paragraph → space), `\~` (nbsp → space), escaped `\\ \{ \}`, `{ }` grouping
/// braces, and arg-bearing commands (`\A1;`, `\fArial|…;`, `\H2.5x;`, `\C1;`, …)
/// whose payload runs to the next `;`. Non-arg toggles (`\L`, `\O`, `\K`) drop the
/// letter only. Not a full MTEXT parser — good enough to recover readable labels.
fn mtext_plain(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.peek().copied() {
                Some('P') | Some('p') | Some('~') => {
                    out.push(' ');
                    chars.next();
                }
                Some('\\') | Some('{') | Some('}') => {
                    out.push(chars.next().unwrap());
                }
                Some(cmd) => {
                    chars.next(); // consume the command letter
                    // Arg-bearing commands consume up to (and including) a ';'.
                    if matches!(cmd, 'A' | 'C' | 'c' | 'H' | 'W' | 'T' | 'Q' | 'F' | 'f' | 'p' | 'S') {
                        for n in chars.by_ref() {
                            if n == ';' {
                                break;
                            }
                        }
                    }
                }
                None => {}
            },
            '{' | '}' => {} // drop grouping braces
            _ => out.push(c),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
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
        // Text annotations are tessellated as Hershey stroke polylines — no DXF
        // TEXT entities. The count reflects the number of stroke polylines.
        run(&mut s, "text 5,3,0 hello 0.3");
        let (dxf, count) = document_dxf(&s.doc);
        let entities = scan_entities(&dxf);
        // LINE, closed polyline, circle, arc, ellipse→polyline, plus N Hershey
        // stroke polylines for "hello". At minimum more than 5 entities.
        assert!(count > 5, "expected >5 logical entities, got {count}");
        assert_eq!(entities.iter().filter(|e| *e == "LINE").count(), 1);
        // closed polyline + ellipse + hershey strokes for "hello" (≥5 letters)
        assert!(entities.iter().filter(|e| *e == "POLYLINE").count() >= 2);
        assert_eq!(entities.iter().filter(|e| *e == "CIRCLE").count(), 1);
        assert_eq!(entities.iter().filter(|e| *e == "ARC").count(), 1);
        // Text annotation no longer emits DXF TEXT entities.
        assert_eq!(entities.iter().filter(|e| *e == "TEXT").count(), 0);
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
    use itsjustcad_doc::Geometry as G;

    fn curve_of(obj: &itsjustcad_doc::SceneObject) -> &kernel_curve::Curve {
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
        // Text annotations are excluded: they export as Hershey stroke polylines
        // and cannot round-trip back to Annotation::Text (by design — the DXF
        // format receives vector geometry, not string metadata).
        let mut s = Session::default();
        run(&mut s, "layer courtyard");
        run(&mut s, "rect 0,0,0 10 8");
        run(&mut s, "rect 3,3,0 4 2");
        run(&mut s, "layer site");
        run(&mut s, "line 0,0,0 10,0,3");
        run(&mut s, "circle 20,0,0 2.5");
        run(&mut s, "arc 30,0,0 5 30 120");
        let path = std::env::temp_dir().join("itsjustcad_import_roundtrip.dxf");
        run(&mut s, &format!("export {}", path.display()));

        let mut s2 = Session::default();
        let out = s2
            .run(Command::Import { path: path.display().to_string() })
            .unwrap();
        // 2 rects (polylines) + line + circle + arc = 5 curve entities.
        assert!(out.message.contains("imported 5 entities"), "{}", out.message);
        assert!(out.message.contains("(0 skipped)"), "{}", out.message);
        assert_eq!(out.created.len(), 5);
        assert_eq!(s2.doc.len(), s.doc.len());
        assert_eq!(s2.doc.current_layer, itsjustcad_doc::DEFAULT_LAYER);

        // Same order, same layers (lowercased round-trip), same geometry.
        for (a, b) in s.doc.objects().zip(s2.doc.objects()) {
            assert_eq!(a.layer, b.layer);
            match (&a.geometry, &b.geometry) {
                (G::Curve(ca), G::Curve(cb)) => assert_curve_close(ca, cb),
                other => panic!("geometry kinds differ: {other:?}"),
            }
        }
    }

    /// Text annotation exports as Hershey stroke polylines (not DXF TEXT).
    #[test]
    fn text_annotation_exports_as_hershey_polylines() {
        let mut s = Session::default();
        run(&mut s, "text 0,0,0 Hi 1.0");
        let (dxf, count) = document_dxf(&s.doc);
        // Hershey strokes for "Hi" — both letters have strokes.
        assert!(count >= 2, "expected ≥2 Hershey polylines for 'Hi', got {count}");
        // No DXF TEXT entity emitted.
        assert_eq!(
            scan_entities(&dxf).iter().filter(|e| *e == "TEXT").count(),
            0,
            "text annotation must not emit DXF TEXT (use Hershey polylines)"
        );
        // But POLYLINE entities must exist.
        assert!(
            scan_entities(&dxf).iter().filter(|e| *e == "POLYLINE").count() >= 1,
            "expected POLYLINE entities from Hershey strokes"
        );
    }

    #[test]
    fn imported_session_saves_and_replays_stably() {
        let mut s = Session::default();
        run(&mut s, "layer walls");
        run(&mut s, "polyline 0,0 5,0 5,5 closed");
        run(&mut s, "circle 8,0,0 1.5");
        let path = std::env::temp_dir().join("itsjustcad_import_replay.dxf");
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
        // SPLINE + INSERT are still unsupported → skipped. POINT is now imported
        // (aggregated into a cloud), so it is no longer counted as skipped.
        assert_eq!(parsed.skipped, 2);
        // The LINE (folded in place) is first; the aggregated POINT cloud is
        // appended after the fold.
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
        // The lone POINT became a one-point cloud.
        assert!(
            parsed.entities.iter().any(|(_, c)| matches!(
                c,
                Command::PointLiteral { positions, .. } if positions.len() == 1
            )),
            "POINT should aggregate into a 1-point cloud"
        );
    }

    #[test]
    fn mtext_imports_as_text_with_formatting_stripped() {
        let text = "0\nSECTION\n2\nENTITIES\n\
            0\nMTEXT\n8\nNOTES\n10\n5\n20\n7\n30\n0\n40\n2.5\n\
            1\n{\\fArial|b1;\\H2.5;Hello}\\PWorld\n\
            0\nENDSEC\n0\nEOF\n";
        let parsed = parse_dxf(text).unwrap();
        assert_eq!(parsed.entities.len(), 1, "MTEXT should import");
        let (layer, cmd) = &parsed.entities[0];
        assert_eq!(layer, "notes");
        match cmd {
            Command::Text { pos, text, height, .. } => {
                assert_eq!(*pos, DVec3::new(5.0, 7.0, 0.0));
                assert!((height - 2.5).abs() < 1e-9);
                assert_eq!(text, "Hello World", "formatting codes must be stripped");
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn dimension_linear_imports_as_dim() {
        // Horizontal dim: extension origins (13,14) at y=0, dim line (10) at y=3.
        let text = "0\nSECTION\n2\nENTITIES\n\
            0\nDIMENSION\n8\nDIMS\n70\n0\n\
            13\n0\n23\n0\n33\n0\n14\n10\n24\n0\n34\n0\n\
            10\n5\n20\n3\n30\n0\n\
            0\nENDSEC\n0\nEOF\n";
        let parsed = parse_dxf(text).unwrap();
        assert_eq!(parsed.entities.len(), 1, "linear DIMENSION should import");
        match &parsed.entities[0].1 {
            Command::Dim { a, b, offset, .. } => {
                assert_eq!(*a, crate::DimAnchorSpec::free(DVec3::new(0.0, 0.0, 0.0)));
                assert_eq!(*b, crate::DimAnchorSpec::free(DVec3::new(10.0, 0.0, 0.0)));
                assert!((offset - 3.0).abs() < 1e-9, "offset {offset}");
            }
            other => panic!("expected Dim, got {other:?}"),
        }
    }

    #[test]
    fn hatch_boundary_imports_as_closed_polyline() {
        // A solid hatch over a triangular polyline boundary; seed point after 75
        // must NOT be picked up as a boundary vertex.
        let text = "0\nSECTION\n2\nENTITIES\n\
            0\nHATCH\n8\nFILLS\n10\n0\n20\n0\n30\n0\n2\nSOLID\n70\n1\n71\n0\n\
            91\n1\n92\n2\n72\n0\n73\n1\n93\n3\n\
            10\n0\n20\n0\n10\n4\n20\n0\n10\n2\n20\n3\n\
            75\n0\n76\n1\n98\n1\n10\n2\n20\n1\n\
            0\nENDSEC\n0\nEOF\n";
        let parsed = parse_dxf(text).unwrap();
        assert_eq!(parsed.entities.len(), 1, "HATCH boundary should import");
        match &parsed.entities[0].1 {
            Command::Polyline { points, closed, .. } => {
                assert!(closed, "hatch boundary is closed");
                assert_eq!(
                    *points,
                    vec![
                        DVec3::new(0.0, 0.0, 0.0),
                        DVec3::new(4.0, 0.0, 0.0),
                        DVec3::new(2.0, 3.0, 0.0),
                    ],
                    "only the 3 boundary verts, not the seed point (2,1)"
                );
            }
            other => panic!("expected Polyline, got {other:?}"),
        }
    }

    #[test]
    fn points_aggregate_into_one_cloud_per_layer() {
        let text = "0\nSECTION\n2\nENTITIES\n\
            0\nPOINT\n8\nSURVEY\n10\n1\n20\n2\n30\n0\n\
            0\nPOINT\n8\nSURVEY\n10\n3\n20\n4\n30\n0\n\
            0\nPOINT\n8\nGRID\n10\n5\n20\n6\n30\n0\n\
            0\nENDSEC\n0\nEOF\n";
        let parsed = parse_dxf(text).unwrap();
        // One cloud per layer (2 layers), NOT 3 separate point objects.
        let clouds: Vec<_> = parsed
            .entities
            .iter()
            .filter_map(|(l, c)| match c {
                Command::PointLiteral { positions, .. } => Some((l.as_str(), positions.len())),
                _ => None,
            })
            .collect();
        assert!(clouds.contains(&("survey", 2)), "survey cloud of 2: {clouds:?}");
        assert!(clouds.contains(&("grid", 1)), "grid cloud of 1: {clouds:?}");
        assert_eq!(clouds.len(), 2, "exactly one cloud per layer");
    }

    #[test]
    fn threedface_aggregates_into_one_mesh() {
        // One quad (4 distinct corners) → 4 verts, 2 triangles; one triangle
        // (4th == 3rd) → 3 verts, 1 triangle. Both fold into ONE mesh per layer.
        let text = "0\nSECTION\n2\nENTITIES\n\
            0\n3DFACE\n8\nTIN\n10\n0\n20\n0\n30\n0\n11\n1\n21\n0\n31\n0\n12\n1\n22\n1\n32\n0\n13\n0\n23\n1\n33\n0\n\
            0\n3DFACE\n8\nTIN\n10\n2\n20\n0\n30\n0\n11\n3\n21\n0\n31\n0\n12\n3\n22\n1\n32\n0\n13\n3\n23\n1\n33\n0\n\
            0\nENDSEC\n0\nEOF\n";
        let parsed = parse_dxf(text).unwrap();
        let meshes: Vec<_> = parsed
            .entities
            .iter()
            .filter_map(|(l, c)| match c {
                Command::MeshLiteral { positions, faces, .. } => {
                    Some((l.as_str(), positions.len(), faces.len()))
                }
                _ => None,
            })
            .collect();
        // quad=4v/2f + tri=3v/1f → 7 verts, 3 faces, ONE mesh.
        assert_eq!(meshes, vec![("tin", 7, 3)], "one aggregated mesh: {meshes:?}");
    }

    #[test]
    fn ellipse_tessellates_to_closed_polyline() {
        // Full ellipse: center (5,5), major axis 4 along +x, ratio 0.5 → ry=2.
        let text = "0\nSECTION\n2\nENTITIES\n\
            0\nELLIPSE\n8\nE\n10\n5\n20\n5\n30\n0\n11\n4\n21\n0\n31\n0\n40\n0.5\n41\n0\n42\n6.283185307\n\
            0\nENDSEC\n0\nEOF\n";
        let parsed = parse_dxf(text).unwrap();
        assert_eq!(parsed.entities.len(), 1);
        match &parsed.entities[0].1 {
            Command::Polyline { points, closed, .. } => {
                assert!(closed, "full ellipse is a closed loop");
                assert!(points.len() >= 32, "densely tessellated");
                // Extents: x in [1,9], y in [3,7] (rx=4, ry=2 about (5,5)).
                let xmax = points.iter().map(|p| p.x).fold(f64::MIN, f64::max);
                let ymax = points.iter().map(|p| p.y).fold(f64::MIN, f64::max);
                assert!((xmax - 9.0).abs() < 0.1, "xmax {xmax}");
                assert!((ymax - 7.0).abs() < 0.1, "ymax {ymax}");
            }
            other => panic!("expected Polyline, got {other:?}"),
        }
    }

    #[test]
    fn insert_instances_a_block_definition() {
        // A block "TREE" (one circle) defined in BLOCKS, referenced twice by
        // INSERT in ENTITIES. Expect: one BlockDefine (first) + two BlockInsert.
        let text = "0\nSECTION\n2\nBLOCKS\n\
            0\nBLOCK\n2\nTREE\n10\n0\n20\n0\n30\n0\n\
            0\nCIRCLE\n8\n0\n10\n0\n20\n0\n40\n1\n\
            0\nENDBLK\n\
            0\nENDSEC\n\
            0\nSECTION\n2\nENTITIES\n\
            0\nINSERT\n8\nSYMBOLS\n2\nTREE\n10\n5\n20\n5\n30\n0\n41\n2\n50\n90\n\
            0\nINSERT\n8\nSYMBOLS\n2\nTREE\n10\n8\n20\n1\n30\n0\n\
            0\nENDSEC\n0\nEOF\n";
        let parsed = parse_dxf(text).unwrap();
        // First command must be the block definition.
        match &parsed.entities[0].1 {
            Command::BlockDefine { name, geometries, .. } => {
                assert_eq!(name, "TREE");
                assert_eq!(geometries.as_ref().unwrap().len(), 1, "one circle in TREE");
            }
            other => panic!("expected BlockDefine first, got {other:?}"),
        }
        // Then two inserts, with transform carried through.
        let inserts: Vec<_> = parsed
            .entities
            .iter()
            .filter_map(|(l, c)| match c {
                Command::BlockInsert { name, position, rotation_deg, scale, .. } => {
                    Some((l.as_str(), name.as_str(), *position, *rotation_deg, *scale))
                }
                _ => None,
            })
            .collect();
        assert_eq!(inserts.len(), 2, "two TREE inserts");
        assert_eq!(inserts[0].0, "symbols");
        assert_eq!(inserts[0].2, DVec3::new(5.0, 5.0, 0.0));
        assert_eq!(inserts[0].3, Some(90.0));
        assert_eq!(inserts[0].4, Some(2.0));
        assert_eq!(inserts[1].4, Some(1.0), "default scale 1 when 41 absent");
    }

    /// M-dwg-bridge: a block whose body is a HATCH now imports with the hatch
    /// boundary as a closed polyline (was dropped → block came in empty).
    #[test]
    fn block_with_hatch_body_imports_boundary() {
        let text = "0\nSECTION\n2\nBLOCKS\n\
            0\nBLOCK\n2\nFILLSYM\n10\n0\n20\n0\n30\n0\n\
            0\nHATCH\n8\n0\n10\n0\n20\n0\n2\nSOLID\n70\n1\n71\n0\n\
            91\n1\n92\n2\n72\n0\n73\n1\n93\n3\n\
            10\n0\n20\n0\n10\n4\n20\n0\n10\n2\n20\n3\n\
            75\n0\n76\n1\n98\n1\n10\n2\n20\n1\n\
            0\nENDBLK\n0\nENDSEC\n\
            0\nSECTION\n2\nENTITIES\n0\nINSERT\n2\nFILLSYM\n10\n0\n20\n0\n0\nENDSEC\n0\nEOF\n";
        let parsed = parse_dxf(text).unwrap();
        let def = parsed.entities.iter().find_map(|(_, c)| match c {
            Command::BlockDefine { name, geometries, .. } if name == "FILLSYM" => {
                geometries.clone()
            }
            _ => None,
        });
        let geoms = def.expect("FILLSYM block must be defined, not dropped");
        assert_eq!(geoms.len(), 1, "hatch boundary → one polyline: {geoms:?}");
        match &geoms[0] {
            itsjustcad_doc::BlockGeometry::Curve(kernel_curve::Curve::Polyline { points, closed }) => {
                assert!(closed);
                assert_eq!(points.len(), 3, "3 boundary verts, not the seed");
            }
            other => panic!("expected closed polyline, got {other:?}"),
        }
    }

    /// M-dwg-bridge: a block whose body is a SPLINE now imports as a tessellated
    /// polyline (from its fit points) instead of being dropped.
    #[test]
    fn block_with_spline_body_imports_polyline() {
        // SPLINE with 3 fit points (11/21) — should read back as a 3-pt polyline.
        let text = "0\nSECTION\n2\nBLOCKS\n\
            0\nBLOCK\n2\nCURVY\n10\n0\n20\n0\n30\n0\n\
            0\nSPLINE\n8\n0\n70\n0\n71\n3\n\
            11\n0\n21\n0\n11\n2\n21\n3\n11\n5\n21\n0\n\
            0\nENDBLK\n0\nENDSEC\n\
            0\nSECTION\n2\nENTITIES\n0\nINSERT\n2\nCURVY\n10\n0\n20\n0\n0\nENDSEC\n0\nEOF\n";
        let parsed = parse_dxf(text).unwrap();
        let geoms = parsed
            .entities
            .iter()
            .find_map(|(_, c)| match c {
                Command::BlockDefine { name, geometries, .. } if name == "CURVY" => {
                    geometries.clone()
                }
                _ => None,
            })
            .expect("CURVY block must be defined");
        assert_eq!(geoms.len(), 1);
        match &geoms[0] {
            itsjustcad_doc::BlockGeometry::Curve(kernel_curve::Curve::Polyline { points, .. }) => {
                assert_eq!(points.len(), 3, "spline fit points → polyline");
                assert!((points[1] - DVec3::new(2.0, 3.0, 0.0)).length() < 1e-9);
            }
            other => panic!("expected polyline from spline, got {other:?}"),
        }
    }

    /// Top-level SPLINE also tessellates to a polyline (from control points when
    /// no fit points are present).
    #[test]
    fn top_level_spline_imports_from_control_points() {
        let text = "0\nSECTION\n2\nENTITIES\n\
            0\nSPLINE\n8\nS\n70\n0\n\
            10\n0\n20\n0\n10\n1\n20\n2\n10\n4\n20\n0\n\
            0\nENDSEC\n0\nEOF\n";
        let parsed = parse_dxf(text).unwrap();
        assert_eq!(parsed.skipped, 0, "spline with points must not skip");
        match &parsed.entities[0].1 {
            Command::Polyline { points, .. } => assert_eq!(points.len(), 3),
            other => panic!("expected polyline, got {other:?}"),
        }
    }

    /// M-dwg-bridge: a block containing a NESTED INSERT of another block bakes
    /// the referenced block's geometry in at the insert transform (BlockGeometry
    /// is flat, so we bake rather than keep a live reference).
    #[test]
    fn block_with_nested_insert_bakes_child_geometry() {
        // LEAF = one line 0,0→1,0. TREE contains a nested INSERT of LEAF at
        // (10,0) with scale 1, rotation 0 → the baked line lands at 10,0→11,0.
        let text = "0\nSECTION\n2\nBLOCKS\n\
            0\nBLOCK\n2\nLEAF\n10\n0\n20\n0\n30\n0\n\
            0\nLINE\n8\n0\n10\n0\n20\n0\n11\n1\n21\n0\n\
            0\nENDBLK\n\
            0\nBLOCK\n2\nTREE\n10\n0\n20\n0\n30\n0\n\
            0\nINSERT\n2\nLEAF\n10\n10\n20\n0\n\
            0\nENDBLK\n0\nENDSEC\n\
            0\nSECTION\n2\nENTITIES\n0\nINSERT\n2\nTREE\n10\n0\n20\n0\n0\nENDSEC\n0\nEOF\n";
        let parsed = parse_dxf(text).unwrap();
        let tree = parsed
            .entities
            .iter()
            .find_map(|(_, c)| match c {
                Command::BlockDefine { name, geometries, .. } if name == "TREE" => {
                    geometries.clone()
                }
                _ => None,
            })
            .expect("TREE block must be defined");
        assert_eq!(tree.len(), 1, "TREE bakes the one nested LEAF line");
        match &tree[0] {
            itsjustcad_doc::BlockGeometry::Curve(kernel_curve::Curve::Line { a, b }) => {
                assert!((*a - DVec3::new(10.0, 0.0, 0.0)).length() < 1e-9, "a {a}");
                assert!((*b - DVec3::new(11.0, 0.0, 0.0)).length() < 1e-9, "b {b}");
            }
            other => panic!("expected baked line, got {other:?}"),
        }
    }

    /// A self-referential (cyclic) block must not recurse forever — the cycle
    /// edge is skipped, the block still imports with its direct geometry.
    #[test]
    fn cyclic_nested_block_is_safe() {
        let text = "0\nSECTION\n2\nBLOCKS\n\
            0\nBLOCK\n2\nLOOP\n10\n0\n20\n0\n30\n0\n\
            0\nLINE\n8\n0\n10\n0\n20\n0\n11\n1\n21\n0\n\
            0\nINSERT\n2\nLOOP\n10\n0\n20\n0\n\
            0\nENDBLK\n0\nENDSEC\n\
            0\nSECTION\n2\nENTITIES\n0\nINSERT\n2\nLOOP\n10\n0\n20\n0\n0\nENDSEC\n0\nEOF\n";
        let parsed = parse_dxf(text).unwrap();
        let loop_def = parsed.entities.iter().find_map(|(_, c)| match c {
            Command::BlockDefine { name, geometries, .. } if name == "LOOP" => geometries.clone(),
            _ => None,
        });
        assert!(loop_def.is_some(), "cyclic block still defines (no hang)");
        // Only the direct line survives; the cyclic nested insert is skipped.
        assert_eq!(loop_def.unwrap().len(), 1);
    }

    /// Security: a DEEP acyclic nested-block chain (B0→B1→…→Bn) must not overflow
    /// the stack — the cycle guard alone would not stop it (no name ever repeats).
    /// The depth cap bounds recursion; import returns cleanly and bounded.
    #[test]
    fn deep_acyclic_nested_chain_is_bounded_no_overflow() {
        // Build 400 blocks: each B{i} has one line + a nested INSERT of B{i+1};
        // the last just has a line. 400 > MAX_BLOCK_DEPTH, so the cap must engage.
        let n = 400usize;
        let mut s = String::from("0\nSECTION\n2\nBLOCKS\n");
        for i in 0..n {
            s.push_str(&format!(
                "0\nBLOCK\n2\nB{i}\n10\n0\n20\n0\n30\n0\n\
                 0\nLINE\n8\n0\n10\n0\n20\n0\n11\n1\n21\n0\n"
            ));
            if i + 1 < n {
                s.push_str(&format!("0\nINSERT\n2\nB{}\n10\n1\n20\n0\n", i + 1));
            }
            s.push_str("0\nENDBLK\n");
        }
        s.push_str(
            "0\nENDSEC\n0\nSECTION\n2\nENTITIES\n0\nINSERT\n2\nB0\n10\n0\n20\n0\n0\nENDSEC\n0\nEOF\n",
        );
        // Must not panic / overflow the stack.
        let parsed = parse_dxf(&s).unwrap();
        let b0 = parsed.entities.iter().find_map(|(_, c)| match c {
            Command::BlockDefine { name, geometries, .. } if name == "B0" => geometries.clone(),
            _ => None,
        });
        // B0 imports; its baked geometry is bounded by the depth cap (≤ depth+1
        // lines), never the full 400.
        let g = b0.expect("B0 must define");
        assert!(g.len() <= super::MAX_BLOCK_DEPTH + 1, "unbounded bake: {}", g.len());
    }

    /// Security: a diamond DAG where a shared child is referenced by every level
    /// can bake ~2^depth geometry (exponential OOM). The running geometry budget
    /// must cap the total; import returns cleanly and bounded.
    #[test]
    fn diamond_dag_geometry_is_budget_capped_no_oom() {
        // D{i} contains a line + TWO inserts of D{i+1}; leaf D{depth} is one line.
        // Uncapped total ≈ 2^depth lines — 30 levels would be ~10^9. The budget
        // (MAX_TOTAL_BAKED_GEOMS) must clamp it well below that.
        let depth = 30usize;
        let mut s = String::from("0\nSECTION\n2\nBLOCKS\n");
        for i in 0..=depth {
            s.push_str(&format!(
                "0\nBLOCK\n2\nD{i}\n10\n0\n20\n0\n30\n0\n\
                 0\nLINE\n8\n0\n10\n0\n20\n0\n11\n1\n21\n0\n"
            ));
            if i < depth {
                s.push_str(&format!("0\nINSERT\n2\nD{}\n10\n1\n20\n0\n", i + 1));
                s.push_str(&format!("0\nINSERT\n2\nD{}\n10\n2\n20\n0\n", i + 1));
            }
            s.push_str("0\nENDBLK\n");
        }
        s.push_str(
            "0\nENDSEC\n0\nSECTION\n2\nENTITIES\n0\nINSERT\n2\nD0\n10\n0\n20\n0\n0\nENDSEC\n0\nEOF\n",
        );
        // Must not OOM / hang. Every defined block's baked geometry is bounded.
        let parsed = parse_dxf(&s).unwrap();
        for (_, c) in &parsed.entities {
            if let Command::BlockDefine { geometries: Some(g), .. } = c {
                assert!(
                    g.len() <= super::MAX_TOTAL_BAKED_GEOMS,
                    "block baked {} geoms — budget breached",
                    g.len()
                );
            }
        }
    }

    #[test]
    fn insert_of_unknown_block_is_skipped() {
        let text = "0\nSECTION\n2\nENTITIES\n\
            0\nINSERT\n8\nA\n2\nNOPE\n10\n0\n20\n0\n\
            0\nENDSEC\n0\nEOF\n";
        let parsed = parse_dxf(text).unwrap();
        assert!(parsed.entities.is_empty(), "unknown block insert imports nothing");
        assert_eq!(parsed.skipped, 1);
    }

    /// Collect the block-instance geometries of a session's document.
    fn instances_of(
        doc: &Document,
    ) -> Vec<(String, DVec3, f64, f64)> {
        doc.objects()
            .filter_map(|o| match &o.geometry {
                G::Instance { block, position, rotation_deg, scale, .. } => {
                    Some((block.clone(), *position, *rotation_deg, *scale))
                }
                _ => None,
            })
            .collect()
    }

    /// Full block round-trip: define a block, place two instances with distinct
    /// position/rotation/scale, export to DXF, re-import, and assert the block
    /// definition survives with both instances and their transforms.
    #[test]
    fn block_definition_and_instances_round_trip() {
        let mut s = Session::default();
        // Define a block "WIDGET" from a circle + a line.
        run(&mut s, "circle 0,0,0 1.5");
        run(&mut s, "line -2,0,0 2,0,0");
        run(&mut s, "block last 2 widget");
        // Two instances on a non-default layer, distinct transforms.
        run(&mut s, "layer symbols");
        run(&mut s, "insert widget 5,5,0 90 2");
        run(&mut s, "insert widget 8,1,0 0 1");

        let path = std::env::temp_dir().join("itsjustcad_block_roundtrip.dxf");
        run(&mut s, &format!("export {}", path.display()));

        let mut s2 = Session::default();
        run(&mut s2, &format!("import {}", path.display()));

        // Block definition survives (keyed by the sanitized/uppercased name the
        // exporter writes; the importer reads the group-2 name verbatim).
        assert!(
            s2.doc.blocks.contains_key("WIDGET"),
            "block def must round-trip, have: {:?}",
            s2.doc.blocks.keys().collect::<Vec<_>>()
        );
        let def = &s2.doc.blocks["WIDGET"];
        assert_eq!(def.len(), 2, "widget = circle + line");

        // Both instances survive, on the (lowercased) symbols layer.
        let mut got = instances_of(&s2.doc);
        got.sort_by(|a, b| a.1.x.partial_cmp(&b.1.x).unwrap());
        assert_eq!(got.len(), 2, "two instances survive: {got:?}");
        assert_eq!(got[0].0, "WIDGET");
        assert!((got[0].1 - DVec3::new(5.0, 5.0, 0.0)).length() < 1e-9, "pos {:?}", got[0].1);
        assert!((got[0].2 - 90.0).abs() < 1e-9, "rotation {}", got[0].2);
        assert!((got[0].3 - 2.0).abs() < 1e-9, "scale {}", got[0].3);
        assert!((got[1].1 - DVec3::new(8.0, 1.0, 0.0)).length() < 1e-9);
        assert!((got[1].2 - 0.0).abs() < 1e-9);
        assert!((got[1].3 - 1.0).abs() < 1e-9);
        // Instance layer preserved (lowercased on the way back in).
        for o in s2.doc.objects() {
            if matches!(o.geometry, G::Instance { .. }) {
                assert_eq!(o.layer, "symbols", "instance keeps its layer");
            }
        }
    }

    /// Two distinct block definitions round-trip together.
    #[test]
    fn multiple_block_definitions_round_trip() {
        let mut s = Session::default();
        run(&mut s, "circle 0,0,0 1");
        run(&mut s, "block last tree");
        run(&mut s, "rect 0,0,0 1 2");
        run(&mut s, "block last door");
        run(&mut s, "insert tree 0,0,0");
        run(&mut s, "insert door 5,0,0");

        let path = std::env::temp_dir().join("itsjustcad_two_blocks.dxf");
        run(&mut s, &format!("export {}", path.display()));

        let mut s2 = Session::default();
        run(&mut s2, &format!("import {}", path.display()));
        assert!(s2.doc.blocks.contains_key("TREE"), "TREE def survives");
        assert!(s2.doc.blocks.contains_key("DOOR"), "DOOR def survives");
        let names: std::collections::BTreeSet<_> =
            instances_of(&s2.doc).into_iter().map(|i| i.0).collect();
        assert!(names.contains("TREE") && names.contains("DOOR"), "both inserts: {names:?}");
    }

    /// A parametric (dynamic) block instance exports its BAKED geometry as a
    /// static DXF block + INSERT: DXF has no dynamic-block concept, so the
    /// instance re-imports as a plain static block instance (params are lost, the
    /// geometry at the current param values is preserved).
    #[test]
    fn parametric_block_instance_bakes_to_static() {
        let mut s = Session::default();
        run(&mut s, "pblock pdoor width=0.9 : rect 0,0,0 {width} 0.05");
        run(&mut s, "insert pdoor 2,0,0 width=1.2");

        let path = std::env::temp_dir().join("itsjustcad_pblock.dxf");
        run(&mut s, &format!("export {}", path.display()));

        let mut s2 = Session::default();
        run(&mut s2, &format!("import {}", path.display()));

        // Exactly one instance re-imports, referencing a static baked block whose
        // geometry (a closed rect polyline) is preserved. It is NOT parametric —
        // no `source`/`params` survive DXF (documented bake behavior).
        let inst: Vec<_> = s2
            .doc
            .objects()
            .filter_map(|o| match &o.geometry {
                G::Instance { block, source, params, position, .. } => {
                    Some((block.clone(), source.clone(), params.clone(), *position))
                }
                _ => None,
            })
            .collect();
        assert_eq!(inst.len(), 1, "one baked instance re-imports: {inst:?}");
        assert!(inst[0].1.is_none(), "re-imported instance is static, not parametric");
        assert!(inst[0].2.is_empty(), "no params survive DXF");
        assert!((inst[0].3 - DVec3::new(2.0, 0.0, 0.0)).length() < 1e-9);
        // The baked block definition exists and carries the rect geometry.
        assert!(
            s2.doc.blocks.contains_key(&inst[0].0),
            "baked block def present: have {:?}",
            s2.doc.blocks.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn multiline_string_value_does_not_abort_import() {
        // Regression: a real DWG→DXF (LibreDWG) wrote a TEXT/MTEXT whose value is a
        // multi-line disclaimer. The wrapped continuation lines are NOT integer
        // group codes; the old strict parser errored the WHOLE file on the first
        // one ("expected an integer group code, got 'OR USE OF ... PROHIBITED'").
        // The parser must skip stray continuation lines, resync, and still import
        // the geometry that follows.
        let text = "0\nSECTION\n2\nENTITIES\n\
            0\nTEXT\n8\nNOTES\n10\n0\n20\n0\n40\n2.5\n\
            1\nCopyright notice line one\n\
            UNAUTHORIZED REPRODUCTION OR USE\n\
            IS THEREFORE EXPRESSLY PROHIBITED\n\
            0\nLINE\n8\nWALLS\n10\n0\n20\n0\n30\n0\n11\n5\n21\n1\n31\n0\n\
            0\nCIRCLE\n8\nHOLES\n10\n3\n20\n3\n40\n2\n\
            0\nENDSEC\n0\nEOF\n";
        let parsed = parse_dxf(text).expect("multi-line string must not fail the parse");
        // The LINE and CIRCLE after the runaway text still import — proof of resync.
        let has_line = parsed.entities.iter().any(|(l, c)| {
            l == "walls" && matches!(c, Command::Line { .. })
        });
        let has_circle = parsed.entities.iter().any(|(l, c)| {
            l == "holes" && matches!(c, Command::Circle { .. })
        });
        assert!(has_line, "LINE after multi-line text should import: {:?}", parsed.entities);
        assert!(has_circle, "CIRCLE after multi-line text should import");
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
        let path = std::env::temp_dir().join("itsjustcad_import_arcs.dxf");
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
        let path = std::env::temp_dir().join("itsjustcad_import_box.dxf");
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

        // A file named .dxf but with garbage content should fail with a friendly
        // parse error (no ENTITIES section) — the resilient parser skips stray
        // lines, so genuine gibberish yields "not a DXF", not a group-code abort.
        let path = std::env::temp_dir().join("itsjustcad_not_a_dxf.dxf");
        std::fs::write(&path, "hello\nworld\nagain\n").unwrap();
        let err = s
            .run(Command::Import { path: path.display().to_string() })
            .unwrap_err();
        assert!(err.to_string().contains("not a DXF"), "{err}");
        assert_eq!(s.doc.len(), 0, "failed import leaves nothing behind");
    }

    /// A free-point dim bakes into block geometry (unchanged behaviour): the
    /// stored points survive verbatim as `DimAnchor::Free`.
    #[test]
    fn block_bakes_free_point_dim() {
        use itsjustcad_doc::{Annotation, BlockGeometry, DimAnchor};
        let cmd = Command::Dim {
            id: None,
            a: crate::DimAnchorSpec::free(DVec3::new(1.0, 2.0, 0.0)),
            b: crate::DimAnchorSpec::free(DVec3::new(4.0, 2.0, 0.0)),
            offset: 0.5,
        };
        let g = command_to_block_geometry(&cmd).expect("free-point dim bakes");
        match g {
            Some(BlockGeometry::Annotation(Annotation::LinearDim { a, b, offset })) => {
                assert_eq!(a, DimAnchor::Free(DVec3::new(1.0, 2.0, 0.0)));
                assert_eq!(b, DimAnchor::Free(DVec3::new(4.0, 2.0, 0.0)));
                assert_eq!(offset, 0.5);
            }
            other => panic!("expected a baked LinearDim, got {other:?}"),
        }
    }

    /// An associative (object-bound) dim CANNOT be baked into a static block —
    /// the binding has no live document to resolve against, so baking it would
    /// have to fabricate a bogus point. It must surface a clean error instead.
    #[test]
    fn block_rejects_object_bound_dim() {
        use itsjustcad_doc::EndpointRef;
        let cmd = Command::Dim {
            id: None,
            a: crate::DimAnchorSpec::Object {
                target: crate::Selector::Named { name: "wall".into() },
                which: EndpointRef::Start,
            },
            b: crate::DimAnchorSpec::free(DVec3::new(4.0, 2.0, 0.0)),
            offset: 0.5,
        };
        let err = command_to_block_geometry(&cmd)
            .expect_err("object-bound dim must not bake into a block");
        let msg = err.to_string();
        assert!(
            msg.contains("associative dimensions cannot be baked into a block"),
            "unexpected error message: {msg}"
        );
    }

    #[test]
    fn dxf_export_honors_instance_clip() {
        use itsjustcad_doc::{ClipRect, ObjectId, SceneObject};
        // A block with two line segments: one inside the clip rect, one outside.
        let inside = kernel_curve::Curve::Line { a: DVec3::new(0.5, 0.5, 0.0), b: DVec3::new(0.9, 0.9, 0.0) };
        let outside = kernel_curve::Curve::Line { a: DVec3::new(5.0, 5.0, 0.0), b: DVec3::new(6.0, 6.0, 0.0) };
        let mut doc = Document::default();
        doc.blocks.insert(
            "seg".to_string(),
            vec![BlockGeometry::Curve(inside), BlockGeometry::Curve(outside)],
        );
        let make = |clip: Option<ClipRect>| {
            let mut d = doc.clone();
            d.insert(SceneObject {
                visible: true,
                id: ObjectId::new(),
                name: None,
                layer: "0".into(),
                color: None,
                material: None,
                lineweight_mm: None,
                geometry: Geometry::Instance {
                    block: "seg".into(),
                    position: DVec3::ZERO,
                    rotation_deg: 0.0,
                    scale: 1.0,
                    source: None,
                    params: Default::default(),
                    clip,
                },
            });
            d
        };

        // Without a clip: exported as a single INSERT (1 entity).
        let (_txt, n_noclip) = document_dxf(&make(None));

        // With a clip rect around [0,1]²: the inside segment survives, the
        // outside one is culled. Exported as expanded LINE entities, so the
        // count reflects only the in-rect segment.
        let rect = ClipRect::new(glam::DVec2::ZERO, glam::DVec2::new(1.0, 1.0));
        let (txt, n_clip) = document_dxf(&make(Some(rect)));
        // Only ENTITIES-section entities count (the BLOCKS section still carries
        // the full block body). scan_entities already scopes to ENTITIES.
        let lines = scan_entities(&txt).into_iter().filter(|e| e == "LINE").count();
        assert_eq!(lines, 1, "only the in-rect segment is exported as an entity\n{txt}");
        // The clipped instance expands to LINE entities (not an INSERT).
        assert!(!scan_entities(&txt).iter().any(|e| e == "INSERT"), "clipped → no INSERT");
        assert!(n_clip >= 1);
        // The unclipped export references the block via a single INSERT.
        assert!(n_noclip >= 1);
    }
}
