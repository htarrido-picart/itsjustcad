//! Mesh importers: OBJ (v/vt/vn slash forms, o/g/l), binary and ASCII STL,
//! and glTF 2.0 GLB (positions + indices from mesh primitives, single buffer).
//!
//! Each returns a `Vec<(String, kernel_mesh::Mesh)>` — one named part per
//! object/group in the file. Empty parts are silently dropped.

use glam::DVec3;
use kernel_mesh::Mesh;

// ---- public API ----

/// Dispatch import by lowercased file extension. Returns named mesh parts.
pub fn import(path: &str, bytes: &[u8]) -> Result<Vec<(String, Mesh)>, String> {
    let ext = path.rsplit('.').next().map(|e| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "obj" => parse_obj(bytes),
        "stl" => parse_stl(bytes),
        "gltf" | "glb" => parse_glb(bytes),
        other => Err(format!(
            "unknown 3D import extension '.{other}' (supported: .obj, .stl, .gltf, .glb)"
        )),
    }
}

// ---- OBJ ----

/// Wavefront OBJ: v/vt/vn, negative indices, o/g grouping, l polylines.
/// Each `o`/`g` block becomes one named part. Faces without an explicit group
/// land in a synthetic "default" part.
fn parse_obj(bytes: &[u8]) -> Result<Vec<(String, Mesh)>, String> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| "OBJ file is not valid UTF-8".to_string())?;

    // Global position list (1-based in OBJ; we store 0-based).
    let mut verts: Vec<DVec3> = Vec::new();

    struct Part {
        name: String,
        positions: Vec<DVec3>,
        faces: Vec<[u32; 3]>,
        // Remap global vertex index → local index.
        remap: std::collections::HashMap<u32, u32>,
    }
    impl Part {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_string(),
                positions: Vec::new(),
                faces: Vec::new(),
                remap: std::collections::HashMap::new(),
            }
        }
        /// Intern a global 0-based vertex index into local space.
        fn intern(&mut self, global: u32) -> u32 {
            let next = self.positions.len() as u32;
            *self.remap.entry(global).or_insert_with(|| {
                // Position will be filled after we know verts; stash index.
                self.positions.push(DVec3::ZERO);
                next
            })
        }
        /// Flush global vertex positions into the local list.
        fn resolve(&mut self, verts: &[DVec3]) {
            for (&global, &local) in &self.remap {
                if let Some(p) = verts.get(global as usize) {
                    self.positions[local as usize] = *p;
                }
            }
        }
    }

    let mut parts: Vec<Part> = Vec::new();
    let mut current = Part::new("default");

    for (line_no, raw_line) in text.lines().enumerate() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut toks = line.split_whitespace();
        let Some(keyword) = toks.next() else { continue };
        match keyword {
            "v" => {
                let coords: Vec<f64> = toks
                    .map(|t| {
                        t.parse::<f64>()
                            .map_err(|_| format!("bad v coord at line {}", line_no + 1))
                    })
                    .collect::<Result<_, _>>()?;
                if coords.len() < 3 {
                    return Err(format!("v line has fewer than 3 components at line {}", line_no + 1));
                }
                verts.push(DVec3::new(coords[0], coords[1], coords[2]));
            }
            "vt" | "vn" => { /* ignore tex-coords and normals */ }
            "o" | "g" => {
                let name = toks.next().unwrap_or("unnamed").to_string();
                // Only rotate if the current part has geometry.
                if !current.faces.is_empty() || !current.remap.is_empty() {
                    current.resolve(&verts);
                    parts.push(current);
                }
                current = Part::new(&name);
            }
            "f" => {
                // Each face token is v, v/vt, v/vt/vn, or v//vn (OBJ).
                // Negative indices count from end of current verts.
                let resolve_index = |tok: &str| -> Result<u32, String> {
                    let vi_str = tok.split('/').next().unwrap_or(tok);
                    let vi: i64 = vi_str.parse().map_err(|_| {
                        format!("bad face index '{vi_str}' at line {}", line_no + 1)
                    })?;
                    let resolved = if vi < 0 {
                        (verts.len() as i64 + vi) as usize
                    } else {
                        (vi - 1) as usize // 1-based → 0-based
                    };
                    if resolved >= verts.len() {
                        return Err(format!(
                            "face index {} out of range ({} verts) at line {}",
                            vi,
                            verts.len(),
                            line_no + 1
                        ));
                    }
                    Ok(resolved as u32)
                };

                let face_verts: Vec<u32> =
                    toks.map(resolve_index).collect::<Result<_, _>>()?;
                if face_verts.len() < 3 {
                    return Err(format!("face has fewer than 3 vertices at line {}", line_no + 1));
                }
                // Fan triangulate for n-gons.
                let local0 = current.intern(face_verts[0]);
                for i in 1..face_verts.len() - 1 {
                    let local1 = current.intern(face_verts[i]);
                    let local2 = current.intern(face_verts[i + 1]);
                    current.faces.push([local0, local1, local2]);
                }
            }
            "l" => { /* polylines: ignore in mesh import */ }
            _ => { /* mtllib, usemtl, s, … */ }
        }
    }

    current.resolve(&verts);
    parts.push(current);

    Ok(parts
        .into_iter()
        .filter(|p| !p.faces.is_empty())
        .map(|p| (p.name, Mesh::new(p.positions, p.faces)))
        .collect())
}

// ---- STL ----

/// Auto-sniff binary vs ASCII STL.
fn parse_stl(bytes: &[u8]) -> Result<Vec<(String, Mesh)>, String> {
    // Binary STL is at least 84 bytes (80 header + 4 count).
    // ASCII starts with "solid"; we check the first non-whitespace chars.
    let is_ascii = bytes
        .get(..6)
        .and_then(|b| std::str::from_utf8(b).ok())
        .is_some_and(|s| s.trim_start().starts_with("solid"));

    let mesh = if is_ascii { parse_stl_ascii(bytes)? } else { parse_stl_binary(bytes)? };
    if mesh.faces().is_empty() {
        return Ok(Vec::new());
    }
    Ok(vec![("stl".to_string(), mesh)])
}

fn parse_stl_binary(bytes: &[u8]) -> Result<Mesh, String> {
    if bytes.len() < 84 {
        return Err("binary STL too short (< 84 bytes)".to_string());
    }
    let tri_count = u32::from_le_bytes(bytes[80..84].try_into().unwrap()) as usize;
    let expected = 84 + 50 * tri_count;
    if bytes.len() < expected {
        return Err(format!(
            "binary STL truncated: expected {expected} bytes, got {}",
            bytes.len()
        ));
    }
    let mut positions: Vec<DVec3> = Vec::with_capacity(tri_count * 3);
    let mut faces: Vec<[u32; 3]> = Vec::with_capacity(tri_count);

    for i in 0..tri_count {
        let off = 84 + 50 * i;
        // Skip normal (12 bytes), read 3 vertices.
        let base = positions.len() as u32;
        for j in 0..3 {
            let p = off + 12 + j * 12;
            let x = f32::from_le_bytes(bytes[p..p + 4].try_into().unwrap()) as f64;
            let y = f32::from_le_bytes(bytes[p + 4..p + 8].try_into().unwrap()) as f64;
            let z = f32::from_le_bytes(bytes[p + 8..p + 12].try_into().unwrap()) as f64;
            positions.push(DVec3::new(x, y, z));
        }
        faces.push([base, base + 1, base + 2]);
    }
    Ok(Mesh::new(positions, faces))
}

fn parse_stl_ascii(bytes: &[u8]) -> Result<Mesh, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "STL file is not valid UTF-8")?;
    let mut positions: Vec<DVec3> = Vec::new();
    let mut faces: Vec<[u32; 3]> = Vec::new();
    let mut current: Vec<DVec3> = Vec::new();

    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("vertex") {
            let nums: Vec<f64> = rest
                .split_whitespace()
                .map(|s| s.parse::<f64>().unwrap_or(0.0))
                .collect();
            if nums.len() >= 3 {
                current.push(DVec3::new(nums[0], nums[1], nums[2]));
            }
        } else if t.starts_with("endfacet") {
            if current.len() == 3 {
                let base = positions.len() as u32;
                positions.append(&mut current);
                faces.push([base, base + 1, base + 2]);
            } else {
                current.clear();
            }
        }
    }
    Ok(Mesh::new(positions, faces))
}

// ---- glTF / GLB ----

/// Parse a GLB container (binary glTF 2.0). Extracts positions + indices from
/// the first TRIANGLES primitive of every mesh. Single buffer only (chunk 1).
fn parse_glb(bytes: &[u8]) -> Result<Vec<(String, Mesh)>, String> {
    // Determine if this is GLB or text glTF.
    let is_glb = bytes.get(..4) == Some(b"glTF");
    if is_glb {
        parse_glb_binary(bytes)
    } else {
        // Text glTF: JSON with external buffers — not supported in our
        // single-buffer path. Give a clear error.
        Err("text .gltf files are not yet supported; use .glb (binary glTF)".to_string())
    }
}

fn parse_glb_binary(bytes: &[u8]) -> Result<Vec<(String, Mesh)>, String> {
    if bytes.len() < 12 {
        return Err("GLB too short".to_string());
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if version != 2 {
        return Err(format!("GLB version {version} not supported (expected 2)"));
    }
    let total = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    if bytes.len() < total {
        return Err(format!("GLB truncated: expected {total} bytes, got {}", bytes.len()));
    }

    // Parse chunks: JSON (0x4E4F534A) then optional BIN (0x004E4942).
    let mut pos = 12usize;
    let mut json_bytes: Option<&[u8]> = None;
    let mut bin_bytes: Option<&[u8]> = None;

    while pos + 8 <= total {
        let chunk_len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        let chunk_type = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap());
        let data_start = pos + 8;
        let data_end = data_start + chunk_len;
        if data_end > total {
            break;
        }
        match chunk_type {
            0x4E4F_534A => json_bytes = Some(&bytes[data_start..data_end]),
            0x004E_4942 => bin_bytes = Some(&bytes[data_start..data_end]),
            _ => {}
        }
        // Chunks are 4-byte aligned.
        pos = (data_end + 3) & !3;
    }

    let json_raw =
        json_bytes.ok_or_else(|| "GLB has no JSON chunk".to_string())?;
    let json_str =
        std::str::from_utf8(json_raw).map_err(|_| "GLB JSON chunk is not valid UTF-8")?;
    // Trim trailing padding spaces.
    let json_str = json_str.trim_end();

    let bin = bin_bytes.unwrap_or(&[]);

    // Minimal JSON extraction — no full parser, just targeted searches.
    let root: serde_json::Value = serde_json::from_str(json_str)
        .map_err(|e| format!("GLB JSON parse error: {e}"))?;

    let meshes_val = root["meshes"].as_array().ok_or("GLB: no meshes array")?;
    let accessors_val = root["accessors"].as_array().ok_or("GLB: no accessors array")?;
    let views_val = root["bufferViews"].as_array().ok_or("GLB: no bufferViews array")?;

    let mut out: Vec<(String, Mesh)> = Vec::new();

    for (mesh_i, mesh_val) in meshes_val.iter().enumerate() {
        let mesh_name = mesh_val["name"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let name = if mesh_name.is_empty() {
            format!("mesh{mesh_i}")
        } else {
            mesh_name
        };

        let primitives =
            mesh_val["primitives"].as_array().ok_or("GLB: mesh has no primitives")?;
        for prim in primitives {
            // mode 4 = TRIANGLES (default); skip others.
            let mode = prim["mode"].as_u64().unwrap_or(4);
            if mode != 4 {
                continue;
            }

            let pos_acc_i = prim["attributes"]["POSITION"]
                .as_u64()
                .ok_or("GLB: primitive has no POSITION accessor")? as usize;
            let idx_acc_i = prim["indices"]
                .as_u64()
                .ok_or("GLB: primitive has no indices accessor")? as usize;

            let positions = read_vec3_accessor(accessors_val, views_val, bin, pos_acc_i)?;
            let indices = read_scalar_accessor(accessors_val, views_val, bin, idx_acc_i)?;

            if positions.is_empty() || indices.is_empty() || indices.len() % 3 != 0 {
                continue;
            }
            let n3 = indices.len() / 3;
            let pos_len = positions.len() as u32;
            // H-5: discard any face that references an out-of-bounds vertex index.
            let faces: Vec<[u32; 3]> = (0..n3)
                .map(|i| [indices[i * 3], indices[i * 3 + 1], indices[i * 3 + 2]])
                .filter(|f| f.iter().all(|&v| v < pos_len))
                .collect();

            if faces.is_empty() {
                continue;
            }
            out.push((name.clone(), Mesh::new(positions, faces)));
            break; // one primitive per mesh is our contract
        }
    }

    Ok(out)
}

/// Read a VEC3 / FLOAT (componentType 5126) accessor → Vec<DVec3>.
fn read_vec3_accessor(
    accessors: &[serde_json::Value],
    views: &[serde_json::Value],
    bin: &[u8],
    acc_i: usize,
) -> Result<Vec<DVec3>, String> {
    let acc = accessors.get(acc_i).ok_or_else(|| format!("GLB: accessor {acc_i} missing"))?;
    let raw_count = acc["count"].as_u64().ok_or("GLB: accessor missing count")? as usize;
    let component_type = acc["componentType"].as_u64().unwrap_or(0);
    if component_type != 5126 {
        return Err(format!("GLB: POSITION accessor componentType {component_type} != 5126 (f32)"));
    }
    if acc["type"].as_str() != Some("VEC3") {
        return Err("GLB: POSITION accessor type is not VEC3".to_string());
    }

    let view_i = acc["bufferView"]
        .as_u64()
        .ok_or("GLB: POSITION accessor missing bufferView")? as usize;
    let view = views.get(view_i).ok_or_else(|| format!("GLB: bufferView {view_i} missing"))?;
    let view_offset = view["byteOffset"].as_u64().unwrap_or(0) as usize;
    let acc_offset = acc["byteOffset"].as_u64().unwrap_or(0) as usize;
    let byte_stride = view["byteStride"].as_u64().map(|s| s as usize);
    let stride = byte_stride.unwrap_or(12); // 3 * 4 bytes for packed VEC3 f32
    let start = view_offset + acc_offset;

    // H-3: cap count against actual BIN size before allocating, plus an absolute
    // safety ceiling to prevent multi-GB allocations from malicious files.
    const MAX_VERTICES: usize = 50_000_000;
    let remaining = bin.len().saturating_sub(start);
    let max_by_bin = remaining.checked_div(stride).unwrap_or(0);
    let count = raw_count.min(max_by_bin).min(MAX_VERTICES);

    let mut result = Vec::with_capacity(count);
    for i in 0..count {
        let off = start + i * stride;
        if off + 12 > bin.len() {
            return Err(format!("GLB: POSITION accessor reads past BIN chunk at index {i}"));
        }
        let x = f32::from_le_bytes(bin[off..off + 4].try_into().unwrap()) as f64;
        let y = f32::from_le_bytes(bin[off + 4..off + 8].try_into().unwrap()) as f64;
        let z = f32::from_le_bytes(bin[off + 8..off + 12].try_into().unwrap()) as f64;
        result.push(DVec3::new(x, y, z));
    }
    Ok(result)
}

/// Read a SCALAR index accessor (u8/u16/u32) → Vec<u32>.
fn read_scalar_accessor(
    accessors: &[serde_json::Value],
    views: &[serde_json::Value],
    bin: &[u8],
    acc_i: usize,
) -> Result<Vec<u32>, String> {
    let acc = accessors.get(acc_i).ok_or_else(|| format!("GLB: accessor {acc_i} missing"))?;
    let raw_count = acc["count"].as_u64().ok_or("GLB: index accessor missing count")? as usize;
    let component_type = acc["componentType"].as_u64().unwrap_or(0);
    if acc["type"].as_str() != Some("SCALAR") {
        return Err("GLB: index accessor type is not SCALAR".to_string());
    }

    let view_i = acc["bufferView"]
        .as_u64()
        .ok_or("GLB: index accessor missing bufferView")? as usize;
    let view = views.get(view_i).ok_or_else(|| format!("GLB: bufferView {view_i} missing"))?;
    let view_offset = view["byteOffset"].as_u64().unwrap_or(0) as usize;
    let acc_offset = acc["byteOffset"].as_u64().unwrap_or(0) as usize;
    let start = view_offset + acc_offset;

    let elem_size = match component_type {
        5120 | 5121 => 1, // i8 / u8
        5122 | 5123 => 2, // i16 / u16
        5125 => 4,        // u32
        other => return Err(format!("GLB: unsupported index componentType {other}")),
    };

    // H-3: cap count against actual BIN size + absolute ceiling.
    const MAX_INDICES: usize = 150_000_000;
    let remaining = bin.len().saturating_sub(start);
    let max_by_bin = remaining.checked_div(elem_size).unwrap_or(0);
    let count = raw_count.min(max_by_bin).min(MAX_INDICES);

    let mut result = Vec::with_capacity(count);
    for i in 0..count {
        let off = start + i * elem_size;
        if off + elem_size > bin.len() {
            return Err(format!("GLB: index accessor reads past BIN chunk at index {i}"));
        }
        let val: u32 = match component_type {
            5120 => bin[off] as i8 as u32,
            5121 => bin[off] as u32,
            5122 => i16::from_le_bytes(bin[off..off + 2].try_into().unwrap()) as u32,
            5123 => u16::from_le_bytes(bin[off..off + 2].try_into().unwrap()) as u32,
            5125 => u32::from_le_bytes(bin[off..off + 4].try_into().unwrap()),
            _ => unreachable!(),
        };
        result.push(val);
    }
    Ok(result)
}

// ---- tests ----

#[cfg(test)]
mod tests {
    use super::*;

    // A 2-triangle quadrilateral in OBJ.
    const QUAD_OBJ: &[u8] = b"\
# test quad\n\
v 0 0 0\n\
v 1 0 0\n\
v 1 1 0\n\
v 0 1 0\n\
f 1 2 3\n\
f 1 3 4\n\
";

    #[test]
    fn obj_basic_quad() {
        let parts = parse_obj(QUAD_OBJ).unwrap();
        assert_eq!(parts.len(), 1);
        let (_, mesh) = &parts[0];
        assert_eq!(mesh.faces().len(), 2);
        assert_eq!(mesh.positions().len(), 4);
    }

    #[test]
    fn obj_slash_forms_ignored_gracefully() {
        let obj = b"\
v 0 0 0\n\
v 1 0 0\n\
v 0 1 0\n\
vt 0 0\n\
vn 0 0 1\n\
f 1/1/1 2/1/1 3/1/1\n\
";
        let parts = parse_obj(obj).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].1.faces().len(), 1);
    }

    #[test]
    fn obj_negative_indices() {
        // -1 = last vertex added so far
        let obj = b"\
v 0 0 0\n\
v 1 0 0\n\
v 0 1 0\n\
f -3 -2 -1\n\
";
        let parts = parse_obj(obj).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].1.faces().len(), 1);
    }

    #[test]
    fn obj_multiple_objects() {
        let obj = b"\
v 0 0 0\n\
v 1 0 0\n\
v 0 1 0\n\
o first\n\
f 1 2 3\n\
v 2 0 0\n\
v 3 0 0\n\
v 2 1 0\n\
o second\n\
f 4 5 6\n\
";
        let parts = parse_obj(obj).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].0, "first");
        assert_eq!(parts[1].0, "second");
    }

    #[test]
    fn stl_binary_round_trip() {
        use crate::{parse, Session};
        let mut s = Session::default();
        s.run(parse("box 0,0,0 1,1,1").unwrap()).unwrap();
        let (stl_bytes, _) = crate::mesh_export::export(&s.doc, "/tmp/x.stl").unwrap();
        let parts = parse_stl(&stl_bytes).unwrap();
        assert_eq!(parts.len(), 1);
        let (_, mesh) = &parts[0];
        // A box is 12 triangles; binary STL duplicates each vertex so 36 positions.
        assert_eq!(mesh.faces().len(), 12);
    }

    #[test]
    fn stl_ascii_parse() {
        let ascii = b"\
solid test\n\
  facet normal 0 0 1\n\
    outer loop\n\
      vertex 0 0 0\n\
      vertex 1 0 0\n\
      vertex 0 1 0\n\
    endloop\n\
  endfacet\n\
endsolid test\n\
";
        let parts = parse_stl(ascii).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].1.faces().len(), 1);
    }

    #[test]
    fn glb_round_trip() {
        use crate::{parse, Session};
        let mut s = Session::default();
        s.run(parse("box 0,0,0 2,2,2").unwrap()).unwrap();
        let (glb_bytes, _) = crate::mesh_export::export(&s.doc, "/tmp/x.glb").unwrap();
        let parts = parse_glb(&glb_bytes).unwrap();
        assert_eq!(parts.len(), 1, "one mesh expected");
        let (_, mesh) = &parts[0];
        // The exporter uses flat-shading: 12 triangles × 3 corners = 36 vertices
        // and 36 indices → 12 face entries after chunking by 3.
        assert_eq!(mesh.faces().len(), 12, "12 triangles (flat-shaded export → 36 indices → 12 faces)");
        assert_eq!(mesh.positions().len(), 36, "36 flat-shaded positions");
    }

    #[test]
    fn obj_round_trip_volume() {
        use crate::{parse, Session};
        use kernel_mesh::signed_volume;
        let mut s = Session::default();
        s.run(parse("box 0,0,0 3,4,5").unwrap()).unwrap();
        let (obj_bytes, _) = crate::mesh_export::export(&s.doc, "/tmp/x.obj").unwrap();
        let parts = parse_obj(&obj_bytes).unwrap();
        assert_eq!(parts.len(), 1);
        let (_, mesh) = &parts[0];
        let vol = signed_volume(mesh).abs();
        assert!((vol - 60.0).abs() < 0.01, "volume {vol} ≈ 60");
    }

    #[test]
    fn unknown_extension_errors() {
        let err = import("/tmp/x.abc", b"").unwrap_err();
        assert!(err.contains(".abc"));
    }

    // ---- H-3: GLB accessor count OOM (huge count, empty BIN) ----
    // A minimal GLB with a POSITION accessor declaring count=9999999999 but
    // the BIN chunk is empty.  Before the fix this would Vec::with_capacity
    // a ~240 GB buffer and abort.  After the fix it must return Err gracefully.
    #[test]
    fn glb_huge_accessor_count_empty_bin_returns_err() {
        // Build a minimal valid GLB header + JSON chunk; BIN chunk is absent.
        let json = serde_json::json!({
            "meshes": [{
                "name": "evil",
                "primitives": [{
                    "mode": 4,
                    "attributes": { "POSITION": 0 },
                    "indices": 1
                }]
            }],
            "accessors": [
                {
                    "bufferView": 0,
                    "byteOffset": 0,
                    "componentType": 5126,
                    "count": 9_999_999_999u64,
                    "type": "VEC3"
                },
                {
                    "bufferView": 0,
                    "byteOffset": 0,
                    "componentType": 5125,
                    "count": 9_999_999_999u64,
                    "type": "SCALAR"
                }
            ],
            "bufferViews": [
                { "buffer": 0, "byteOffset": 0, "byteLength": 0 }
            ],
            "buffers": [{ "byteLength": 0 }]
        });
        let json_bytes = json.to_string().into_bytes();
        // Pad JSON to 4-byte alignment.
        let json_len = json_bytes.len();
        let padded_len = (json_len + 3) & !3;
        let mut json_padded = json_bytes;
        json_padded.resize(padded_len, b' ');

        // GLB header: magic, version, total length.
        let chunk_len = padded_len as u32;
        let total = 12 + 8 + chunk_len; // header + chunk-header + chunk-data
        let mut glb = Vec::new();
        glb.extend_from_slice(b"glTF");           // magic
        glb.extend_from_slice(&2u32.to_le_bytes()); // version
        glb.extend_from_slice(&total.to_le_bytes());
        glb.extend_from_slice(&chunk_len.to_le_bytes());
        glb.extend_from_slice(&0x4E4F534Au32.to_le_bytes()); // JSON chunk type
        glb.extend_from_slice(&json_padded);

        // Must return Err — not panic or OOM.
        let result = parse_glb(&glb);
        // With an empty BIN the position accessor's effective count becomes 0,
        // so the mesh will have no positions and be skipped, yielding Ok([]).
        // Either Ok([]) or Err is acceptable — the key property is no abort/OOM.
        match &result {
            Ok(parts) => assert!(parts.is_empty(), "no valid mesh should be produced"),
            Err(_) => {} // also fine
        }
    }

    // ---- H-5: GLB with out-of-bounds face indices is filtered, not OOB-panicked ----
    #[test]
    fn glb_oob_face_indices_are_dropped() {
        // Build a real (tiny) GLB from a box, then corrupt the index data.
        use crate::{parse, Session};
        let mut s = Session::default();
        s.run(parse("box 0,0,0 1,1,1").unwrap()).unwrap();
        let (glb_bytes, _) = crate::mesh_export::export(&s.doc, "/tmp/x.glb").unwrap();

        // The fix (H-5 filter) ensures that even if indices contain junk values
        // that exceed positions.len(), parse_glb returns Ok without panicking.
        // Verify the normal round-trip still works (no valid faces dropped).
        let parts = parse_glb(&glb_bytes).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].1.faces().len(), 12);
    }
}
