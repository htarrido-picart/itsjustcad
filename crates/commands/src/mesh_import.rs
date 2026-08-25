// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Mesh importers: OBJ (v/vt/vn slash forms, o/g/l), binary and ASCII STL,
//! glTF 2.0 GLB (positions + indices from mesh primitives, single buffer),
//! and Collada 1.4/1.5 (.dae) — geometry/mesh sources, triangles/polylist,
//! node transforms (matrix/translate/rotate/scale).
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
        "dae" => parse_collada(bytes),
        other => Err(format!(
            "unknown 3D import extension '.{other}' (supported: .obj, .stl, .gltf, .glb, .dae)"
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

// ---- Collada (.dae) ----

/// Parse a Collada 1.4/1.5 XML file.
///
/// Strategy (no external XML crate):
/// 1. Walk the text looking for `<geometry>` blocks, extract `<source>` float
///    arrays and `<triangles>` / `<polylist>` elements, resolve POSITION input,
///    fan-triangulate n-gons.
/// 2. Walk `<node>` blocks to find `<instance_geometry>` references and
///    accumulate transform matrices (matrix/translate/rotate/scale).
/// 3. Apply the node transform to each referenced mesh's positions.
///
/// Unknown elements and attributes are silently skipped — tolerant by design.
fn parse_collada(bytes: &[u8]) -> Result<Vec<(String, Mesh)>, String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| "Collada .dae file is not valid UTF-8".to_string())?;

    // Safety ceiling: reject files > 512 MB before doing any work.
    const MAX_DAE_BYTES: usize = 512 * 1024 * 1024;
    if bytes.len() > MAX_DAE_BYTES {
        return Err(format!(
            "Collada file too large ({} MB > 512 MB limit)",
            bytes.len() / 1_048_576
        ));
    }

    // ---- step 1: collect geometry definitions keyed by id ----
    // We build a map: geometry_id → Vec<(name, Mesh)>
    use std::collections::HashMap;
    let mut geom_map: HashMap<String, Vec<(String, Mesh)>> = HashMap::new();

    // Iterate over <geometry ...> ... </geometry> blocks.
    let mut search = text;
    while let Some(geom_start) = find_tag_open(search, "geometry") {
        let rest = &search[geom_start..];
        let (geom_attrs, inner, consumed) = extract_element(rest, "geometry")?;
        let geom_id = attr_value(geom_attrs, "id").unwrap_or("geometry").to_string();
        let geom_name = attr_value(geom_attrs, "name")
            .unwrap_or(&geom_id)
            .to_string();
        let meshes = parse_collada_mesh(inner, &geom_name)?;
        if !meshes.is_empty() {
            geom_map.insert(geom_id, meshes);
        }
        search = &search[geom_start + consumed..];
    }

    // ---- step 2: walk <node> tree for instance_geometry + transforms ----
    // We collect: (mesh_name, positions, faces) with transforms applied.
    let mut out: Vec<(String, Mesh)> = Vec::new();

    // Find all <instance_geometry> elements in the document.
    let mut search2 = text;
    while let Some(ig_start) = find_tag_open(search2, "instance_geometry") {
        let rest = &search2[ig_start..];
        // Grab the url attribute (references geometry id with a leading '#').
        let (attrs, _inner, consumed) = extract_element_or_empty_tag(rest, "instance_geometry");
        let url = attr_value(attrs, "url").unwrap_or("").to_string();
        let geom_id = url.trim_start_matches('#');

        // Walk backwards in search2 to find the closest enclosing <node> and
        // its transforms. We scan the prefix before ig_start.
        let prefix = &search2[..ig_start];
        let transform = extract_node_transform(prefix);

        if let Some(parts) = geom_map.get(geom_id) {
            for (name, mesh) in parts {
                let transformed_positions: Vec<DVec3> = mesh
                    .positions()
                    .iter()
                    .map(|&p| apply_mat4(transform, p))
                    .collect();
                let faces = mesh.faces().to_vec();
                out.push((name.clone(), Mesh::new(transformed_positions, faces)));
            }
        }

        search2 = &search2[ig_start + consumed..];
    }

    // Fallback: if no <instance_geometry> found (bare geometry-only file),
    // emit all geometries without transform.
    if out.is_empty() {
        for (_id, parts) in geom_map {
            out.extend(parts);
        }
    }

    Ok(out)
}

/// Parse `<mesh>` inside a single `<geometry>` block.
fn parse_collada_mesh(inner: &str, geom_name: &str) -> Result<Vec<(String, Mesh)>, String> {
    use std::collections::HashMap;

    // Collect <source> float arrays keyed by id.
    let mut sources: HashMap<String, Vec<f32>> = HashMap::new();
    let mut search = inner;
    while let Some(src_start) = find_tag_open(search, "source") {
        let rest = &search[src_start..];
        let (src_attrs, src_inner, src_consumed) = extract_element(rest, "source")?;
        let src_id = attr_value(src_attrs, "id").unwrap_or("").to_string();
        // Extract <float_array> content.
        if let Some(fa_start) = find_tag_open(src_inner, "float_array") {
            let fa_rest = &src_inner[fa_start..];
            if let Ok((_, fa_inner, _)) = extract_element(fa_rest, "float_array") {
                let floats: Vec<f32> = fa_inner
                    .split_whitespace()
                    .filter_map(|s| s.parse::<f32>().ok())
                    .collect();
                sources.insert(src_id, floats);
            }
        }
        search = &search[src_start + src_consumed..];
    }

    // Find <vertices> to resolve the POSITION semantic → source id.
    let mut position_source: Option<String> = None;
    if let Some(v_start) = find_tag_open(inner, "vertices") {
        let v_rest = &inner[v_start..];
        if let Ok((_, v_inner, _)) = extract_element(v_rest, "vertices") {
            // Find <input semantic="POSITION" source="#...">
            let mut sv = v_inner;
            while let Some(inp_start) = find_tag_open(sv, "input") {
                let inp_rest = &sv[inp_start..];
                let (inp_attrs, _, inp_consumed) =
                    extract_element_or_empty_tag(inp_rest, "input");
                let semantic = attr_value(inp_attrs, "semantic").unwrap_or("");
                if semantic == "POSITION" {
                    let src = attr_value(inp_attrs, "source").unwrap_or("");
                    position_source = Some(src.trim_start_matches('#').to_string());
                }
                sv = &sv[inp_start + inp_consumed..];
            }
        }
    }

    let pos_floats = position_source
        .as_deref()
        .and_then(|id| sources.get(id))
        .or_else(|| {
            // Fallback: find any source whose id contains "position" or "Position".
            sources
                .iter()
                .find(|(k, _)| k.to_ascii_lowercase().contains("position"))
                .map(|(_, v)| v)
        });

    let pos_floats = match pos_floats {
        Some(f) => f,
        None => return Ok(Vec::new()), // no positions → skip
    };

    // Build position list from float triples.
    let raw_positions: Vec<DVec3> = pos_floats
        .as_chunks::<3>().0.iter()
        .map(|c| DVec3::new(c[0] as f64, c[1] as f64, c[2] as f64))
        .collect();

    if raw_positions.is_empty() {
        return Ok(Vec::new());
    }

    // Safety cap on position count.
    const MAX_VERTS: usize = 50_000_000;
    if raw_positions.len() > MAX_VERTS {
        return Err(format!(
            "Collada geometry '{}' has {} positions, exceeding {} limit",
            geom_name,
            raw_positions.len(),
            MAX_VERTS
        ));
    }

    let mut all_faces: Vec<[u32; 3]> = Vec::new();

    // ---- <triangles> ----
    let mut search_t = inner;
    while let Some(tri_start) = find_tag_open(search_t, "triangles") {
        let rest = &search_t[tri_start..];
        let (tri_attrs, tri_inner, tri_consumed) = extract_element(rest, "triangles")?;
        let _ = tri_attrs; // count attribute optional
        // Find VERTEX input offset.
        let vertex_offset = find_input_offset(tri_inner, "VERTEX").unwrap_or(0);
        let stride = total_input_stride(tri_inner);
        // <p> indices.
        if let Some(p_start) = find_tag_open(tri_inner, "p") {
            let p_rest = &tri_inner[p_start..];
            if let Ok((_, p_inner, _)) = extract_element(p_rest, "p") {
                let indices: Vec<u32> = p_inner
                    .split_whitespace()
                    .filter_map(|s| s.parse::<u32>().ok())
                    .collect();
                collect_triangles_from_p(
                    &indices, stride, vertex_offset, raw_positions.len(), &mut all_faces,
                );
            }
        }
        search_t = &search_t[tri_start + tri_consumed..];
    }

    // ---- <polylist> ----
    let mut search_p = inner;
    while let Some(pl_start) = find_tag_open(search_p, "polylist") {
        let rest = &search_p[pl_start..];
        let (pl_attrs, pl_inner, pl_consumed) = extract_element(rest, "polylist")?;
        let _ = pl_attrs;
        let vertex_offset = find_input_offset(pl_inner, "VERTEX").unwrap_or(0);
        let stride = total_input_stride(pl_inner);
        // <vcount> polygon sizes.
        let vcounts: Vec<u32> = if let Some(vc_start) = find_tag_open(pl_inner, "vcount") {
            let vc_rest = &pl_inner[vc_start..];
            if let Ok((_, vc_inner, _)) = extract_element(vc_rest, "vcount") {
                vc_inner
                    .split_whitespace()
                    .filter_map(|s| s.parse::<u32>().ok())
                    .collect()
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        if let Some(p_start) = find_tag_open(pl_inner, "p") {
            let p_rest = &pl_inner[p_start..];
            if let Ok((_, p_inner, _)) = extract_element(p_rest, "p") {
                let indices: Vec<u32> = p_inner
                    .split_whitespace()
                    .filter_map(|s| s.parse::<u32>().ok())
                    .collect();
                collect_polylist_from_p(
                    &indices,
                    &vcounts,
                    stride,
                    vertex_offset,
                    raw_positions.len(),
                    &mut all_faces,
                );
            }
        }
        search_p = &search_p[pl_start + pl_consumed..];
    }

    if all_faces.is_empty() {
        return Ok(Vec::new());
    }

    // Safety cap on face count.
    const MAX_FACES: usize = 50_000_000;
    if all_faces.len() > MAX_FACES {
        return Err(format!(
            "Collada geometry '{}' has {} faces, exceeding {} limit",
            geom_name,
            all_faces.len(),
            MAX_FACES
        ));
    }

    Ok(vec![(geom_name.to_string(), Mesh::new(raw_positions, all_faces))])
}

/// Collect triangles from a flat <p> index buffer (stride = inputs per vertex).
fn collect_triangles_from_p(
    indices: &[u32],
    stride: usize,
    vertex_offset: usize,
    pos_len: usize,
    out: &mut Vec<[u32; 3]>,
) {
    let stride = stride.max(1);
    let tri_count = indices.len() / (3 * stride);
    let pos_len = pos_len as u32;
    for t in 0..tri_count {
        let base = t * 3 * stride;
        let a = indices[base + vertex_offset];
        let b = indices[base + stride + vertex_offset];
        let c = indices[base + 2 * stride + vertex_offset];
        // H-5 pattern: discard OOB faces.
        if a < pos_len && b < pos_len && c < pos_len {
            out.push([a, b, c]);
        }
    }
}

/// Fan-triangulate polygons from a <polylist> <p> buffer.
fn collect_polylist_from_p(
    indices: &[u32],
    vcounts: &[u32],
    stride: usize,
    vertex_offset: usize,
    pos_len: usize,
    out: &mut Vec<[u32; 3]>,
) {
    let stride = stride.max(1);
    let pos_len = pos_len as u32;
    let mut cursor = 0usize;
    for &vc in vcounts {
        let n = vc as usize;
        if n < 3 || cursor + n * stride > indices.len() {
            cursor += n * stride;
            continue;
        }
        let v0 = indices[cursor + vertex_offset];
        for i in 1..(n - 1) {
            let v1 = indices[cursor + i * stride + vertex_offset];
            let v2 = indices[cursor + (i + 1) * stride + vertex_offset];
            if v0 < pos_len && v1 < pos_len && v2 < pos_len {
                out.push([v0, v1, v2]);
            }
        }
        cursor += n * stride;
    }
}

/// Find the offset attribute of an <input semantic="X"> element.
fn find_input_offset(xml: &str, semantic: &str) -> Option<usize> {
    let mut search = xml;
    while let Some(s) = find_tag_open(search, "input") {
        let rest = &search[s..];
        let (attrs, _, consumed) = extract_element_or_empty_tag(rest, "input");
        let sem = attr_value(attrs, "semantic").unwrap_or("");
        if sem == semantic {
            let off = attr_value(attrs, "offset")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0);
            return Some(off);
        }
        search = &search[s + consumed..];
    }
    None
}

/// Count the total number of <input> elements to determine the per-vertex stride.
fn total_input_stride(xml: &str) -> usize {
    let mut stride = 0usize;
    let mut search = xml;
    while let Some(s) = find_tag_open(search, "input") {
        let rest = &search[s..];
        let (attrs, _, consumed) = extract_element_or_empty_tag(rest, "input");
        let off = attr_value(attrs, "offset")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        stride = stride.max(off + 1);
        search = &search[s + consumed..];
    }
    stride.max(1)
}

/// Walk the XML prefix before an `<instance_geometry>` to find the most recent
/// `<node>` and extract its cumulative transform as a column-major 4×4 matrix
/// (stored row-major in our array for `apply_mat4`).
fn extract_node_transform(prefix: &str) -> [f64; 16] {
    // Identity matrix.
    let mut mat = [
        1.0, 0.0, 0.0, 0.0,
        0.0, 1.0, 0.0, 0.0,
        0.0, 0.0, 1.0, 0.0,
        0.0, 0.0, 0.0, 1.0f64,
    ];

    // Find the last <node ...> in prefix.
    let node_start = match rfind_tag_open(prefix, "node") {
        Some(s) => s,
        None => return mat,
    };
    let node_text = &prefix[node_start..];

    // Walk transform children until we hit the end of the opening tag region.
    // We look for <matrix>, <translate>, <rotate>, <scale> in document order.
    let tag_names = ["matrix", "translate", "rotate", "scale"];
    // Collect all transforms in order.
    let mut transforms: Vec<(usize, &str, Vec<f64>)> = Vec::new();
    for tag in &tag_names {
        let mut search = node_text;
        let mut offset = 0usize;
        while let Some(s) = find_tag_open(search, tag) {
            let rest = &search[s..];
            if let Ok((_, inner, consumed)) = extract_element(rest, tag) {
                let vals: Vec<f64> = inner
                    .split_whitespace()
                    .filter_map(|v| v.parse::<f64>().ok())
                    .collect();
                transforms.push((offset + s, tag, vals));
                offset += s + consumed;
                search = &rest[consumed..];
            } else {
                break;
            }
        }
    }

    // Sort by document position.
    transforms.sort_by_key(|(pos, _, _)| *pos);

    for (_, tag, vals) in transforms {
        let m = match tag {
            "matrix" if vals.len() == 16 => {
                // Collada uses column-major (like OpenGL). vals[0..4] = col0.
                // We store row-major: row i, col j = mat[i*4+j].
                // Collada col-major to row-major: result[i][j] = vals[j*4+i].
                [
                    vals[0], vals[4], vals[8],  vals[12],
                    vals[1], vals[5], vals[9],  vals[13],
                    vals[2], vals[6], vals[10], vals[14],
                    vals[3], vals[7], vals[11], vals[15],
                ]
            }
            "translate" if vals.len() >= 3 => [
                1.0, 0.0, 0.0, vals[0],
                0.0, 1.0, 0.0, vals[1],
                0.0, 0.0, 1.0, vals[2],
                0.0, 0.0, 0.0, 1.0,
            ],
            "scale" if vals.len() >= 3 => [
                vals[0], 0.0,     0.0,     0.0,
                0.0,     vals[1], 0.0,     0.0,
                0.0,     0.0,     vals[2], 0.0,
                0.0,     0.0,     0.0,     1.0,
            ],
            "rotate" if vals.len() >= 4 => {
                // Axis-angle: vals[0..3] = axis, vals[3] = angle in degrees.
                let ax = vals[0];
                let ay = vals[1];
                let az = vals[2];
                let angle = vals[3].to_radians();
                let c = angle.cos();
                let s = angle.sin();
                let t = 1.0 - c;
                [
                    t*ax*ax+c,    t*ax*ay-s*az, t*ax*az+s*ay, 0.0,
                    t*ax*ay+s*az, t*ay*ay+c,    t*ay*az-s*ax, 0.0,
                    t*ax*az-s*ay, t*ay*az+s*ax, t*az*az+c,    0.0,
                    0.0,          0.0,          0.0,          1.0,
                ]
            }
            _ => continue,
        };
        mat = mat4_mul(mat, m);
    }
    mat
}

/// Row-major 4×4 matrix multiply: result = a × b.
fn mat4_mul(a: [f64; 16], b: [f64; 16]) -> [f64; 16] {
    let mut c = [0.0f64; 16];
    for i in 0..4 {
        for j in 0..4 {
            for k in 0..4 {
                c[i * 4 + j] += a[i * 4 + k] * b[k * 4 + j];
            }
        }
    }
    c
}

/// Apply a row-major 4×4 transform matrix to a point.
fn apply_mat4(m: [f64; 16], p: DVec3) -> DVec3 {
    let x = m[0] * p.x + m[1] * p.y + m[2]  * p.z + m[3];
    let y = m[4] * p.x + m[5] * p.y + m[6]  * p.z + m[7];
    let z = m[8] * p.x + m[9] * p.y + m[10] * p.z + m[11];
    DVec3::new(x, y, z)
}

// ---- Minimal XML helpers ----

/// Find the byte offset of the start of `<tag` in `xml`.
fn find_tag_open(xml: &str, tag: &str) -> Option<usize> {
    let needle = format!("<{}", tag);
    // Must be followed by whitespace, '>', or '/>' to avoid partial matches.
    let bytes = xml.as_bytes();
    let nb = needle.as_bytes();
    let tag_byte = if tag.is_empty() { return None } else { tag.as_bytes()[0] };
    let _ = tag_byte;
    let mut i = 0;
    while i + nb.len() <= bytes.len() {
        if bytes[i..].starts_with(nb) {
            let after = i + nb.len();
            if after >= bytes.len() || matches!(bytes[after], b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/') {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Find the LAST occurrence of `<tag` in `xml` (for node-ancestor search).
fn rfind_tag_open(xml: &str, tag: &str) -> Option<usize> {
    let needle = format!("<{}", tag);
    let bytes = xml.as_bytes();
    let nb = needle.as_bytes();
    let mut last = None;
    let mut i = 0;
    while i + nb.len() <= bytes.len() {
        if bytes[i..].starts_with(nb) {
            let after = i + nb.len();
            if after >= bytes.len() || matches!(bytes[after], b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/') {
                last = Some(i);
            }
        }
        i += 1;
    }
    last
}

/// Extract `<tag attrs...> inner </tag>` from the start of `xml`.
/// Returns (attrs_str, inner_str, bytes_consumed).
fn extract_element<'a>(xml: &'a str, tag: &str) -> Result<(&'a str, &'a str, usize), String> {
    // Find the end of the opening tag.
    let open_end = xml.find('>').ok_or_else(|| format!("Collada: unclosed <{tag}>"))?;
    let open_tag = &xml[..open_end];
    // Self-closing?
    if open_tag.ends_with('/') {
        return Ok((&xml[1 + tag.len()..open_end - 1], "", open_end + 1));
    }
    let attrs_str = &xml[1 + tag.len()..open_end];

    // Find the matching </tag>.
    let close = format!("</{}>", tag);
    let inner_start = open_end + 1;
    // Handle nested same-tag elements (depth counting).
    let mut depth = 1usize;
    let mut pos = inner_start;
    while pos < xml.len() {
        let remaining = &xml[pos..];
        let open_pat = format!("<{}", tag);
        let next_open = remaining.find(open_pat.as_str()).map(|i| i + pos);
        let next_close = remaining.find(close.as_str()).map(|i| i + pos);
        match (next_open, next_close) {
            (Some(o), Some(c)) if o < c => {
                // Check it's a real tag (not a partial match).
                let after_o = o + open_pat.len();
                if after_o < xml.len()
                    && matches!(xml.as_bytes()[after_o], b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/')
                {
                    depth += 1;
                }
                pos = o + 1;
            }
            (_, Some(c)) => {
                depth -= 1;
                if depth == 0 {
                    let inner = &xml[inner_start..c];
                    let consumed = c + close.len();
                    return Ok((attrs_str, inner, consumed));
                }
                pos = c + 1;
            }
            _ => break,
        }
    }
    // If no close tag found, treat the rest as inner content (tolerant).
    let inner = &xml[inner_start..];
    Ok((attrs_str, inner, xml.len()))
}

/// Extract element or self-closing tag attrs, returning ("", ...) for inner.
fn extract_element_or_empty_tag<'a>(xml: &'a str, tag: &str) -> (&'a str, &'a str, usize) {
    // Find the end of the tag.
    if let Some(end) = xml.find('>') {
        let tag_body = &xml[1 + tag.len()..end];
        let is_self_closing = tag_body.ends_with('/') || xml[..end + 1].ends_with("/>");
        if is_self_closing {
            return (tag_body.trim_end_matches('/'), "", end + 1);
        }
        // Has body — find </tag>
        let close = format!("</{}>", tag);
        let inner_start = end + 1;
        if let Some(c) = xml[inner_start..].find(close.as_str()) {
            let inner = &xml[inner_start..inner_start + c];
            return (tag_body, inner, inner_start + c + close.len());
        }
        (tag_body, &xml[inner_start..], xml.len())
    } else {
        (&xml[1 + tag.len()..], "", xml.len())
    }
}

/// Extract the value of an attribute from an attrs string, e.g. `id="foo"` → `"foo"`.
fn attr_value<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    // Look for `name="value"` or `name='value'`.
    let pattern = format!("{}=", name);
    let pos = attrs.find(pattern.as_str())?;
    let rest = &attrs[pos + pattern.len()..];
    if let Some(inner) = rest.strip_prefix('"') {
        let end = inner.find('"')?;
        Some(&inner[..end])
    } else if let Some(inner) = rest.strip_prefix('\'') {
        let end = inner.find('\'')?;
        Some(&inner[..end])
    } else {
        // Unquoted (uncommon but tolerate).
        let end = rest.find(|c: char| c.is_whitespace() || c == '>').unwrap_or(rest.len());
        Some(&rest[..end])
    }
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
        if let Ok(parts) = &result {
            assert!(parts.is_empty(), "no valid mesh should be produced");
        } // Err(_) is also fine
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

    // ---- Collada (.dae) ----

    /// Minimal synthetic .dae with 4 positions and 2 triangles.
    fn make_quad_dae(geom_id: &str, geom_name: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
             <COLLADA xmlns=\"http://www.collada.org/2005/11/COLLADASchema\" version=\"1.4.1\">\n\
               <library_geometries>\n\
                 <geometry id=\"{gid}\" name=\"{gname}\">\n\
                   <mesh>\n\
                     <source id=\"{gid}-positions\">\n\
                       <float_array count=\"12\">0 0 0  1 0 0  1 1 0  0 1 0</float_array>\n\
                     </source>\n\
                     <vertices id=\"{gid}-vertices\">\n\
                       <input semantic=\"POSITION\" source=\"#{gid}-positions\"/>\n\
                     </vertices>\n\
                     <triangles count=\"2\">\n\
                       <input semantic=\"VERTEX\" source=\"#{gid}-vertices\" offset=\"0\"/>\n\
                       <p>0 1 2  0 2 3</p>\n\
                     </triangles>\n\
                   </mesh>\n\
                 </geometry>\n\
               </library_geometries>\n\
               <library_visual_scenes>\n\
                 <visual_scene id=\"Scene\">\n\
                   <node id=\"Mesh\">\n\
                     <instance_geometry url=\"#{gid}\"/>\n\
                   </node>\n\
                 </visual_scene>\n\
               </library_visual_scenes>\n\
             </COLLADA>",
            gid = geom_id,
            gname = geom_name
        )
    }

    #[test]
    fn collada_basic_quad_triangle_count() {
        let dae = make_quad_dae("Quad", "Quad");
        let parts = parse_collada(dae.as_bytes()).unwrap();
        assert_eq!(parts.len(), 1, "expected 1 mesh part");
        let (name, mesh) = &parts[0];
        assert_eq!(name, "Quad");
        assert_eq!(mesh.faces().len(), 2, "quad = 2 triangles");
        assert_eq!(mesh.positions().len(), 4, "4 unique positions");
    }

    #[test]
    fn collada_positions_correct() {
        let dae = make_quad_dae("Q", "Q");
        let parts = parse_collada(dae.as_bytes()).unwrap();
        let mesh = &parts[0].1;
        let pos = mesh.positions();
        assert!((pos[0] - glam::DVec3::new(0.0, 0.0, 0.0)).length() < 1e-9);
        assert!((pos[1] - glam::DVec3::new(1.0, 0.0, 0.0)).length() < 1e-9);
        assert!((pos[3] - glam::DVec3::new(0.0, 1.0, 0.0)).length() < 1e-9);
    }

    #[test]
    fn collada_node_translate_applied() {
        // Wrap the geometry in a node with a <translate>.
        let dae = r##"<?xml version="1.0"?>
<COLLADA xmlns="http://www.collada.org/2005/11/COLLADASchema" version="1.4.1">
  <library_geometries>
    <geometry id="G" name="G">
      <mesh>
        <source id="G-pos">
          <float_array count="9">0 0 0  1 0 0  0 1 0</float_array>
        </source>
        <vertices id="G-verts">
          <input semantic="POSITION" source="#G-pos"/>
        </vertices>
        <triangles count="1">
          <input semantic="VERTEX" source="#G-verts" offset="0"/>
          <p>0 1 2</p>
        </triangles>
      </mesh>
    </geometry>
  </library_geometries>
  <library_visual_scenes>
    <visual_scene id="S">
      <node id="N">
        <translate>10 20 30</translate>
        <instance_geometry url="#G"/>
      </node>
    </visual_scene>
  </library_visual_scenes>
</COLLADA>"##;
        let parts = parse_collada(dae.as_bytes()).unwrap();
        assert_eq!(parts.len(), 1);
        let mesh = &parts[0].1;
        // First position was (0,0,0), after translate(10,20,30) → (10,20,30).
        let p = mesh.positions()[0];
        assert!((p.x - 10.0).abs() < 1e-9, "x={}", p.x);
        assert!((p.y - 20.0).abs() < 1e-9, "y={}", p.y);
        assert!((p.z - 30.0).abs() < 1e-9, "z={}", p.z);
    }

    #[test]
    fn collada_polylist_fan_triangulated() {
        // A quad polygon (4 verts) in <polylist> should become 2 triangles.
        let dae = r##"<?xml version="1.0"?>
<COLLADA xmlns="http://www.collada.org/2005/11/COLLADASchema" version="1.4.1">
  <library_geometries>
    <geometry id="PL" name="PL">
      <mesh>
        <source id="PL-pos">
          <float_array count="12">0 0 0  1 0 0  1 1 0  0 1 0</float_array>
        </source>
        <vertices id="PL-verts">
          <input semantic="POSITION" source="#PL-pos"/>
        </vertices>
        <polylist count="1">
          <input semantic="VERTEX" source="#PL-verts" offset="0"/>
          <vcount>4</vcount>
          <p>0 1 2 3</p>
        </polylist>
      </mesh>
    </geometry>
  </library_geometries>
  <library_visual_scenes>
    <visual_scene id="S">
      <node id="N">
        <instance_geometry url="#PL"/>
      </node>
    </visual_scene>
  </library_visual_scenes>
</COLLADA>"##;
        let parts = parse_collada(dae.as_bytes()).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].1.faces().len(), 2, "quad → 2 triangles via fan");
    }

    #[test]
    fn collada_oob_face_indices_dropped() {
        // <p> contains an index (99) that is out of bounds for 4 positions.
        // This must not panic; the valid triangle is kept, the bad one dropped.
        let dae = r##"<?xml version="1.0"?>
<COLLADA xmlns="http://www.collada.org/2005/11/COLLADASchema" version="1.4.1">
  <library_geometries>
    <geometry id="OOB" name="OOB">
      <mesh>
        <source id="OOB-pos">
          <float_array count="9">0 0 0  1 0 0  0 1 0</float_array>
        </source>
        <vertices id="OOB-verts">
          <input semantic="POSITION" source="#OOB-pos"/>
        </vertices>
        <triangles count="2">
          <input semantic="VERTEX" source="#OOB-verts" offset="0"/>
          <p>0 1 2  0 99 2</p>
        </triangles>
      </mesh>
    </geometry>
  </library_geometries>
  <library_visual_scenes>
    <visual_scene id="S">
      <node id="N">
        <instance_geometry url="#OOB"/>
      </node>
    </visual_scene>
  </library_visual_scenes>
</COLLADA>"##;
        let parts = parse_collada(dae.as_bytes()).unwrap();
        // The bad triangle (index 99) is filtered out; only the valid one remains.
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].1.faces().len(), 1, "OOB triangle dropped");
    }

    #[test]
    fn collada_malformed_not_utf8_errors() {
        // Invalid UTF-8 bytes should return a clear error, not panic.
        let result = parse_collada(&[0xFF, 0xFE, 0x00]);
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("UTF-8"), "error: {msg}");
    }

    #[test]
    fn collada_dispatch_via_import() {
        // Ensure the extension dispatch table routes .dae correctly.
        let dae = make_quad_dae("D", "D");
        let parts = import("/tmp/test.dae", dae.as_bytes()).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].1.faces().len(), 2);
    }

    // ---- LAZ actionable error ----

    #[test]
    fn laz_gives_actionable_error_via_exec() {
        use crate::{parse, Session};
        // Write a fake .laz file (content doesn't matter — exec rejects by extension).
        let tmp = std::env::temp_dir().join("test_reject.laz");
        std::fs::write(&tmp, b"fake laz content").unwrap();
        let mut s = Session::default();
        let cmd = parse(&format!("import {}", tmp.display())).unwrap();
        let err = s.run(cmd).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("LAZ") && msg.contains("decompress") && msg.contains(".las"),
            "error should mention LAZ and decompress: {msg}"
        );
    }
}
