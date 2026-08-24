//! 3D interchange export by file extension: binary STL, OBJ, and glTF 2.0 GLB.
//!
//! Hand-written and dependency-free, in the spirit of [`crate::dxf`]. These are
//! surface/solid formats: triangle meshes export their faces; curves (lines,
//! polylines, tessellated arcs/ellipses/NURBS) export only where the format has
//! a place for them — OBJ carries them as `l` polylines, STL and GLB (which
//! have no line primitive on our path) skip them. Annotations are dropped: none
//! of these formats model dimensions or text as geometry we care to round-trip.

use glam::DVec3;
use mydrafter_doc::{Document, Geometry};

/// Chord tolerance for tessellating curved curves (meters), matching the DXF
/// exporter so wireframe output is consistent across formats.
const EXPORT_TOL: f64 = 0.005;

/// A triangle mesh flattened from one document object: world-space f64
/// positions and the triangle corner indices into them.
struct MeshPart {
    name: String,
    positions: Vec<DVec3>,
    faces: Vec<[u32; 3]>,
}

/// A polyline flattened from one curve object: an ordered point list. `closed`
/// repeats the first point at the end so OBJ `l` draws the closing segment.
struct LinePart {
    name: String,
    points: Vec<DVec3>,
    closed: bool,
}

/// Split the document into triangle meshes and polylines. Object order is
/// preserved so exports are deterministic and diff-friendly.
fn collect(doc: &Document) -> (Vec<MeshPart>, Vec<LinePart>) {
    let mut meshes = Vec::new();
    let mut lines = Vec::new();
    for obj in doc.objects() {
        // SceneObject names are optional; fall back to a stable per-object label.
        let name = obj.name.clone().unwrap_or_else(|| format!("object_{}", meshes.len() + lines.len()));
        match &obj.geometry {
            Geometry::Mesh(m) => meshes.push(MeshPart {
                name,
                positions: m.positions().to_vec(),
                faces: m.faces().to_vec(),
            }),
            Geometry::Curve(c) => {
                let points = match c {
                    kernel_curve::Curve::Line { a, b } => vec![*a, *b],
                    kernel_curve::Curve::Polyline { points, .. } => points.clone(),
                    _ => c.tessellate(EXPORT_TOL),
                };
                if points.len() >= 2 {
                    lines.push(LinePart { name, points, closed: c.is_closed() });
                }
            }
            // Annotations and instances have no place in a solid/surface
            // interchange format.
            Geometry::Annotation(_) | Geometry::Instance { .. } => {}
        }
    }
    (meshes, lines)
}

/// Pick the exporter by lowercased extension. Returns the file bytes and a
/// short human count string for the command echo, or an error naming the
/// supported extensions.
pub fn export(doc: &Document, path: &str) -> Result<(Vec<u8>, String), String> {
    let ext = path
        .rsplit('.')
        .next()
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let (meshes, lines) = collect(doc);
    match ext.as_str() {
        "stl" => {
            let tris: usize = meshes.iter().map(|m| m.faces.len()).sum();
            Ok((write_stl(&meshes), format!("{tris} triangles")))
        }
        "obj" => {
            let (bytes, tris, segs) = write_obj(&meshes, &lines);
            Ok((bytes, format!("{tris} triangles, {segs} line segments")))
        }
        "gltf" | "glb" => {
            let (bytes, tris) = write_glb(&meshes);
            Ok((bytes, format!("{tris} triangles")))
        }
        other => Err(format!(
            "unknown 3D export extension '.{other}' (use .stl, .obj, .gltf or .glb)"
        )),
    }
}

// ---------------- binary STL ----------------

/// Binary STL: 80-byte header, u32 little-endian triangle count, then 50 bytes
/// per triangle (normal + 3 vertices as 12 f32 LE, plus a u16 attribute count).
/// Total size is therefore exactly 84 + 50 * triangles.
fn write_stl(meshes: &[MeshPart]) -> Vec<u8> {
    let tri_count: usize = meshes.iter().map(|m| m.faces.len()).sum();
    let mut out = Vec::with_capacity(84 + 50 * tri_count);
    let mut header = [0u8; 80];
    let tag = b"mydrafter binary STL";
    header[..tag.len()].copy_from_slice(tag);
    out.extend_from_slice(&header);
    out.extend_from_slice(&(tri_count as u32).to_le_bytes());
    for m in meshes {
        for f in &m.faces {
            let [a, b, c] = f.map(|i| m.positions[i as usize]);
            let n = (b - a).cross(c - a).normalize_or_zero();
            for v in [n, a, b, c] {
                out.extend_from_slice(&(v.x as f32).to_le_bytes());
                out.extend_from_slice(&(v.y as f32).to_le_bytes());
                out.extend_from_slice(&(v.z as f32).to_le_bytes());
            }
            out.extend_from_slice(&0u16.to_le_bytes()); // attribute byte count
        }
    }
    out
}

// ---------------- OBJ ----------------

/// Trim trailing zeros so coordinates stay compact but exact enough to
/// round-trip drafting values.
fn obj_num(v: f64) -> String {
    let mut s = format!("{v:.6}");
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.push('0');
    }
    s
}

/// Wavefront OBJ: shared 1-based vertex index space. Meshes emit `o <name>` +
/// `v` + `f`; curves emit `o <name>` + `v` + a single `l` polyline. Returns the
/// text bytes, triangle count and line-segment count.
fn write_obj(meshes: &[MeshPart], lines: &[LinePart]) -> (Vec<u8>, usize, usize) {
    let mut s = String::from("# mydrafter OBJ export\n");
    let mut base = 1u32; // OBJ vertex indices are 1-based
    let mut tris = 0usize;
    let mut segs = 0usize;

    for m in meshes {
        s.push_str(&format!("o {}\n", obj_name(&m.name)));
        for p in &m.positions {
            s.push_str(&format!("v {} {} {}\n", obj_num(p.x), obj_num(p.y), obj_num(p.z)));
        }
        for f in &m.faces {
            s.push_str(&format!("f {} {} {}\n", base + f[0], base + f[1], base + f[2]));
        }
        base += m.positions.len() as u32;
        tris += m.faces.len();
    }

    for l in lines {
        s.push_str(&format!("o {}\n", obj_name(&l.name)));
        for p in &l.points {
            s.push_str(&format!("v {} {} {}\n", obj_num(p.x), obj_num(p.y), obj_num(p.z)));
        }
        let n = l.points.len() as u32;
        s.push('l');
        for i in 0..n {
            s.push_str(&format!(" {}", base + i));
        }
        if l.closed {
            s.push_str(&format!(" {base}")); // close back to the first vertex
        }
        s.push('\n');
        segs += (n - 1) as usize + usize::from(l.closed);
        base += n;
    }

    (s.into_bytes(), tris, segs)
}

/// OBJ names cannot contain whitespace; blanks become a placeholder.
fn obj_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_whitespace() { '_' } else { c })
        .collect();
    if cleaned.is_empty() { "object".to_string() } else { cleaned }
}

// ---------------- glTF 2.0 GLB ----------------

/// glTF 2.0 binary container (GLB). A single binary buffer holds, per mesh,
/// flat-shaded positions + normals (VEC3 f32) and triangle indices (u32
/// SCALAR); each mesh becomes one node + one mesh + one primitive. Position
/// accessors carry the spec-required min/max. Returns the GLB bytes and the
/// total triangle count.
fn write_glb(meshes: &[MeshPart]) -> (Vec<u8>, usize) {
    let mut bin: Vec<u8> = Vec::new();
    let mut buffer_views = String::new();
    let mut accessors = String::new();
    let mut mesh_json = String::new();
    let mut node_json = String::new();
    let mut node_indices = String::new();

    let mut acc_index = 0u32;
    let mut view_index = 0u32;
    let mut total_tris = 0usize;

    let sep = |s: &str| if s.is_empty() { "" } else { "," };

    for (mesh_i, m) in meshes.iter().enumerate() {
        // Flat shading: one vertex per face corner so triangles read crisply,
        // matching the viewport's massing look.
        let mut positions: Vec<[f32; 3]> = Vec::with_capacity(m.faces.len() * 3);
        let mut normals: Vec<[f32; 3]> = Vec::with_capacity(m.faces.len() * 3);
        let mut indices: Vec<u32> = Vec::with_capacity(m.faces.len() * 3);
        for f in &m.faces {
            let [a, b, c] = f.map(|i| m.positions[i as usize]);
            let n = (b - a).cross(c - a).normalize_or_zero();
            for p in [a, b, c] {
                indices.push(positions.len() as u32);
                positions.push([p.x as f32, p.y as f32, p.z as f32]);
                normals.push([n.x as f32, n.y as f32, n.z as f32]);
            }
        }
        total_tris += m.faces.len();

        // Component min/max over the position accessor (spec-required for
        // POSITION). Guard the empty case (no faces) with a zero box.
        let (mut min, mut max) = ([f32::MAX; 3], [f32::MIN; 3]);
        for p in &positions {
            for k in 0..3 {
                min[k] = min[k].min(p[k]);
                max[k] = max[k].max(p[k]);
            }
        }
        if positions.is_empty() {
            min = [0.0; 3];
            max = [0.0; 3];
        }

        // --- POSITION buffer view + accessor ---
        let pos_offset = align4(&mut bin);
        for p in &positions {
            for c in p {
                bin.extend_from_slice(&c.to_le_bytes());
            }
        }
        let pos_len = positions.len() * 12;
        buffer_views.push_str(&format!(
            "{}{{\"buffer\":0,\"byteOffset\":{pos_offset},\"byteLength\":{pos_len},\"target\":34962}}",
            sep(&buffer_views)
        ));
        let pos_view = view_index;
        view_index += 1;
        accessors.push_str(&format!(
            "{}{{\"bufferView\":{pos_view},\"componentType\":5126,\"count\":{},\"type\":\"VEC3\",\"min\":[{},{},{}],\"max\":[{},{},{}]}}",
            sep(&accessors),
            positions.len(),
            f(min[0]), f(min[1]), f(min[2]),
            f(max[0]), f(max[1]), f(max[2]),
        ));
        let pos_acc = acc_index;
        acc_index += 1;

        // --- NORMAL buffer view + accessor ---
        let nrm_offset = align4(&mut bin);
        for n in &normals {
            for c in n {
                bin.extend_from_slice(&c.to_le_bytes());
            }
        }
        let nrm_len = normals.len() * 12;
        buffer_views.push_str(&format!(
            "{}{{\"buffer\":0,\"byteOffset\":{nrm_offset},\"byteLength\":{nrm_len},\"target\":34962}}",
            sep(&buffer_views)
        ));
        let nrm_view = view_index;
        view_index += 1;
        accessors.push_str(&format!(
            "{}{{\"bufferView\":{nrm_view},\"componentType\":5126,\"count\":{},\"type\":\"VEC3\"}}",
            sep(&accessors),
            normals.len(),
        ));
        let nrm_acc = acc_index;
        acc_index += 1;

        // --- indices buffer view + accessor (u32 SCALAR, target 34963) ---
        let idx_offset = align4(&mut bin);
        for i in &indices {
            bin.extend_from_slice(&i.to_le_bytes());
        }
        let idx_len = indices.len() * 4;
        buffer_views.push_str(&format!(
            "{}{{\"buffer\":0,\"byteOffset\":{idx_offset},\"byteLength\":{idx_len},\"target\":34963}}",
            sep(&buffer_views)
        ));
        let idx_view = view_index;
        view_index += 1;
        accessors.push_str(&format!(
            "{}{{\"bufferView\":{idx_view},\"componentType\":5125,\"count\":{},\"type\":\"SCALAR\"}}",
            sep(&accessors),
            indices.len(),
        ));
        let idx_acc = acc_index;
        acc_index += 1;

        mesh_json.push_str(&format!(
            "{}{{\"name\":{},\"primitives\":[{{\"attributes\":{{\"POSITION\":{pos_acc},\"NORMAL\":{nrm_acc}}},\"indices\":{idx_acc},\"mode\":4}}]}}",
            sep(&mesh_json),
            json_str(&m.name),
        ));
        node_json.push_str(&format!(
            "{}{{\"mesh\":{mesh_i},\"name\":{}}}",
            sep(&node_json),
            json_str(&m.name),
        ));
        node_indices.push_str(&format!("{}{mesh_i}", sep(&node_indices)));
    }

    let total_len = bin.len();
    let json = format!(
        "{{\"asset\":{{\"version\":\"2.0\",\"generator\":\"mydrafter\"}},\
         \"scene\":0,\"scenes\":[{{\"nodes\":[{node_indices}]}}],\
         \"nodes\":[{node_json}],\"meshes\":[{mesh_json}],\
         \"accessors\":[{accessors}],\"bufferViews\":[{buffer_views}],\
         \"buffers\":[{{\"byteLength\":{total_len}}}]}}"
    );

    (assemble_glb(&json, &bin), total_tris)
}

/// Pad the binary buffer to a 4-byte boundary (glTF requires each accessor's
/// data to be 4-byte aligned within the buffer). Returns the offset after
/// padding, i.e. where the next data starts.
fn align4(bin: &mut Vec<u8>) -> usize {
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    bin.len()
}

/// Wrap JSON + binary chunks into the 12-byte GLB header + two chunks. Each
/// chunk is length-prefixed and padded to 4 bytes (JSON with spaces, BIN with
/// zeros), as the GLB spec mandates.
fn assemble_glb(json: &str, bin: &[u8]) -> Vec<u8> {
    let mut json_bytes = json.as_bytes().to_vec();
    while !json_bytes.len().is_multiple_of(4) {
        json_bytes.push(b' ');
    }
    let mut bin_bytes = bin.to_vec();
    while !bin_bytes.len().is_multiple_of(4) {
        bin_bytes.push(0);
    }
    let total = 12 + 8 + json_bytes.len() + 8 + bin_bytes.len();

    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"glTF"); // magic
    out.extend_from_slice(&2u32.to_le_bytes()); // version
    out.extend_from_slice(&(total as u32).to_le_bytes()); // total length
    // JSON chunk
    out.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&0x4E4F_534Au32.to_le_bytes()); // "JSON"
    out.extend_from_slice(&json_bytes);
    // BIN chunk
    out.extend_from_slice(&(bin_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&0x004E_4942u32.to_le_bytes()); // "BIN\0"
    out.extend_from_slice(&bin_bytes);
    out
}

/// Compact float for JSON: finite values print trimmed, non-finite clamp to 0.
fn f(v: f32) -> String {
    if !v.is_finite() {
        return "0".to_string();
    }
    let mut s = format!("{v:.6}");
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.push('0');
        }
    }
    s
}

/// Minimal JSON string escaping for object names.
fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse, Session};

    fn run(s: &mut Session, line: &str) {
        s.run(parse(line).unwrap()).unwrap();
    }

    /// A session with one box (12 triangles) and one polyline curve.
    fn box_and_line() -> Session {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 2,2,2");
        run(&mut s, "polyline 0,0 5,0 5,5 closed");
        s
    }

    #[test]
    fn stl_byte_count_math() {
        let s = box_and_line();
        let (bytes, count) = export(&s.doc, "/tmp/x.stl").unwrap();
        // A box is 12 triangles; the polyline is not a solid and is dropped.
        assert_eq!(count, "12 triangles");
        let tri = u32::from_le_bytes(bytes[80..84].try_into().unwrap());
        assert_eq!(tri, 12);
        assert_eq!(bytes.len(), 84 + 50 * 12, "84 header + 50 per triangle");
        assert!(bytes[..20].starts_with(b"mydrafter"));
    }

    #[test]
    fn stl_empty_document() {
        let (bytes, count) = export(&Document::default(), "/tmp/e.stl").unwrap();
        assert_eq!(count, "0 triangles");
        assert_eq!(bytes.len(), 84);
        assert_eq!(u32::from_le_bytes(bytes[80..84].try_into().unwrap()), 0);
    }

    #[test]
    fn obj_counts_vertices_faces_and_lines() {
        let s = box_and_line();
        let (bytes, count) = export(&s.doc, "/tmp/x.obj").unwrap();
        let text = String::from_utf8(bytes).unwrap();
        // Box: 8 verts, 12 faces. Closed polyline: 3 verts, 1 `l` with 4 refs
        // (3 points + closing back to first) = 3 segments.
        let v = text.lines().filter(|l| l.starts_with("v ")).count();
        let faces = text.lines().filter(|l| l.starts_with("f ")).count();
        let l_lines: Vec<&str> = text.lines().filter(|l| l.starts_with("l ")).collect();
        assert_eq!(v, 8 + 3);
        assert_eq!(faces, 12);
        assert_eq!(l_lines.len(), 1);
        // l references are 1-based and continue past the mesh's 8 verts.
        assert_eq!(l_lines[0], "l 9 10 11 9");
        assert_eq!(text.lines().filter(|l| l.starts_with("o ")).count(), 2);
        assert_eq!(count, "12 triangles, 3 line segments");
    }

    #[test]
    fn obj_face_indices_within_range() {
        let s = box_and_line();
        let (bytes, _) = export(&s.doc, "/tmp/x.obj").unwrap();
        let text = String::from_utf8(bytes).unwrap();
        for line in text.lines().filter(|l| l.starts_with("f ")) {
            for tok in line[2..].split_whitespace() {
                let idx: u32 = tok.parse().unwrap();
                assert!((1..=8).contains(&idx), "box face index {idx} out of range");
            }
        }
    }

    #[test]
    fn glb_magic_chunks_and_accessors_for_a_box() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 2,2,2");
        let (bytes, count) = export(&s.doc, "/tmp/x.glb").unwrap();
        assert_eq!(count, "12 triangles");

        // --- header ---
        assert_eq!(&bytes[0..4], b"glTF");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 2);
        let total = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        assert_eq!(total, bytes.len(), "declared length matches file");
        assert_eq!(total % 4, 0, "GLB total length is 4-byte aligned");

        // --- JSON chunk ---
        let json_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        assert_eq!(&bytes[16..20], b"JSON");
        assert_eq!(json_len % 4, 0, "JSON chunk padded to 4 bytes");
        let json = std::str::from_utf8(&bytes[20..20 + json_len]).unwrap().trim();

        // --- BIN chunk follows ---
        let bin_off = 20 + json_len;
        let bin_len = u32::from_le_bytes(bytes[bin_off..bin_off + 4].try_into().unwrap()) as usize;
        assert_eq!(&bytes[bin_off + 4..bin_off + 8], b"BIN\0");
        assert_eq!(bin_len % 4, 0, "BIN chunk padded to 4 bytes");
        assert_eq!(bin_off + 8 + bin_len, total, "chunks fill the file exactly");

        // --- JSON parses and has the expected structure ---
        let doc: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(doc["asset"]["version"], "2.0");
        assert_eq!(doc["meshes"].as_array().unwrap().len(), 1);
        // 12 flat-shaded triangles -> 36 vertices, 36 indices.
        let accs = doc["accessors"].as_array().unwrap();
        assert_eq!(accs.len(), 3, "POSITION, NORMAL, indices");
        let pos = &accs[0];
        assert_eq!(pos["type"], "VEC3");
        assert_eq!(pos["componentType"], 5126); // f32
        assert_eq!(pos["count"], 36);
        // Position accessor must carry a 3-component min/max spanning the box.
        assert_eq!(pos["min"], serde_json::json!([0.0, 0.0, 0.0]));
        assert_eq!(pos["max"], serde_json::json!([2.0, 2.0, 2.0]));
        let idx = &accs[2];
        assert_eq!(idx["type"], "SCALAR");
        assert_eq!(idx["componentType"], 5125); // u32
        assert_eq!(idx["count"], 36);

        // buffer byteLength equals the actual BIN chunk payload (before padding).
        let declared = doc["buffers"][0]["byteLength"].as_u64().unwrap() as usize;
        assert!(declared <= bin_len && bin_len - declared < 4, "buffer length within pad");
    }

    #[test]
    fn glb_empty_document_is_valid() {
        let (bytes, count) = export(&Document::default(), "/tmp/e.glb").unwrap();
        assert_eq!(count, "0 triangles");
        assert_eq!(&bytes[0..4], b"glTF");
        let json_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let json = std::str::from_utf8(&bytes[20..20 + json_len]).unwrap().trim();
        let doc: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(doc["meshes"].as_array().unwrap().len(), 0);
        assert_eq!(doc["accessors"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn unknown_extension_errors() {
        let err = export(&Document::default(), "/tmp/x.foo").unwrap_err();
        assert!(err.contains(".stl"), "{err}");
        assert!(err.contains("foo"), "{err}");
    }

    #[test]
    fn extension_is_case_insensitive() {
        assert!(export(&Document::default(), "/tmp/X.STL").is_ok());
        assert!(export(&Document::default(), "/tmp/X.Glb").is_ok());
        assert!(export(&Document::default(), "/tmp/X.Obj").is_ok());
    }
}
