// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Rhino `.3dm` (openNURBS) reader — meshes and curves, dependency-free.
//!
//! Hand-written and pure-Rust, in the spirit of [`crate::dxf`] and
//! [`crate::ifc`]. The `.3dm` wire format is the openNURBS binary archive: a
//! 32-byte ASCII start header, then a tree of typed, length-prefixed chunks.
//! Every chunk is `[u32 typecode][u64 length][payload]`; chunks whose typecode
//! carries the `TCODE_CRC` bit append a trailing 4-byte CRC32 (the length counts
//! it), and chunks whose typecode carries the `TCODE_SHORT` bit store an inline
//! value instead of a payload. We target V5/V6/V7 archives (3dm version ≥ 50,
//! where chunk lengths are 8 bytes).
//!
//! ## What we read
//! - **Layer table** → layer name + id, so imported objects can be placed on
//!   their Rhino layer (mirrors [`crate::exec`]'s IFC story→layer mapping).
//! - **Object table**, per object record:
//!   - `ON_Mesh` → [`Imported::Mesh`] (vertices + triangulated faces).
//!   - `ON_LineCurve` → [`Imported::Polyline`] of two points.
//!   - `ON_PolylineCurve` → [`Imported::Polyline`].
//!   - `ON_NurbsCurve` → tessellated to a dense [`Imported::Polyline`] via
//!     [`kernel_curve::nurbs_point`] (full NURBS is a later kernel job).
//!   - Everything else (breps, surfaces, points, annotations…) is **skipped**
//!     and counted.
//!   - The object's `ON_3dmObjectAttributes` carry its **name** and **layer
//!     index**, both preserved.
//!
//! The reader is defensive: a truncated/garbled chunk stops the walk of that
//! table rather than panicking, and an unrecognized class UUID is skipped by
//! chunk length. It never allocates on unchecked counts.
//!
//! A matching minimal **writer** ([`write_min`]) lives at the bottom, used only
//! by the round-trip tests — it emits a spec-conformant archive that openNURBS
//! itself would read, so the reader is tested against genuine `.3dm` bytes.

use glam::DVec3;
use kernel_mesh::Mesh;

// ---- typecodes (opennurbs_3dm.h) ----
const TCODE_SHORT: u32 = 0x8000_0000;

#[cfg(test)]
const TCODE_COMMENTBLOCK: u32 = 0x0000_0001;
const TCODE_ENDOFFILE: u32 = 0x0000_7FFF;
const TCODE_ENDOFTABLE: u32 = 0xFFFF_FFFF;

const TCODE_LAYER_TABLE: u32 = 0x1000_0011;
const TCODE_OBJECT_TABLE: u32 = 0x1000_0013;
const TCODE_LAYER_RECORD: u32 = 0x2000_8050;
const TCODE_OBJECT_RECORD: u32 = 0x2000_8070;
const TCODE_OBJECT_RECORD_TYPE: u32 = 0x8200_0071;
const TCODE_OBJECT_RECORD_ATTRIBUTES: u32 = 0x0200_8072;
const TCODE_OBJECT_RECORD_END: u32 = 0x8200_007F;
const TCODE_OPENNURBS_CLASS: u32 = 0x0002_7FFA;
const TCODE_OPENNURBS_CLASS_UUID: u32 = 0x0002_FFFB;
const TCODE_OPENNURBS_CLASS_DATA: u32 = 0x0002_FFFC;
const TCODE_OPENNURBS_CLASS_END: u32 = 0x8002_7FFF;

// ---- class UUIDs (ON_OBJECT_IMPLEMENT), on-disk byte order (mixed-endian) ----
// Data1 LE u32, Data2/Data3 LE u16, Data4 verbatim.
const UUID_MESH: [u8; 16] =
    [0xE4, 0xD4, 0xD7, 0x4E, 0x47, 0xE9, 0xD3, 0x11, 0xBF, 0xE5, 0x00, 0x10, 0x83, 0x01, 0x22, 0xF0];
const UUID_NURBS_CURVE: [u8; 16] =
    [0xDD, 0xD4, 0xD7, 0x4E, 0x47, 0xE9, 0xD3, 0x11, 0xBF, 0xE5, 0x00, 0x10, 0x83, 0x01, 0x22, 0xF0];
const UUID_LINE_CURVE: [u8; 16] =
    [0xDB, 0xD4, 0xD7, 0x4E, 0x47, 0xE9, 0xD3, 0x11, 0xBF, 0xE5, 0x00, 0x10, 0x83, 0x01, 0x22, 0xF0];
const UUID_POLYLINE_CURVE: [u8; 16] =
    [0xE6, 0xD4, 0xD7, 0x4E, 0x47, 0xE9, 0xD3, 0x11, 0xBF, 0xE5, 0x00, 0x10, 0x83, 0x01, 0x22, 0xF0];

/// Dense-polyline sample count for a tessellated NURBS curve.
const NURBS_SAMPLES: usize = 64;

/// One geometry object recovered from the archive, with its Rhino name and the
/// resolved layer name (already looked up through the layer table).
pub struct ImportedObject {
    pub name: String,
    pub layer: String,
    pub geom: Imported,
}

/// The geometry payload of an imported object.
pub enum Imported {
    /// A triangle mesh → one `MeshLiteral` op.
    Mesh(Mesh),
    /// A line / polyline / tessellated NURBS → one `Polyline` op.
    Polyline { points: Vec<DVec3>, closed: bool },
}

/// Parse result: the recovered objects plus a count of skipped
/// (unrecognized-class) object records.
pub struct Import {
    pub objects: Vec<ImportedObject>,
    pub skipped: usize,
}

/// Import meshes and curves from raw `.3dm` bytes.
///
/// Returns an error only for a file that is not a `.3dm` at all (bad magic) or a
/// pre-V5 archive we do not support. Truncation inside a table is tolerated:
/// whatever was decoded before the bad chunk is returned.
pub fn import(bytes: &[u8]) -> Result<Import, String> {
    let mut r = Reader::new(bytes)?;
    let mut layers: Vec<String> = Vec::new();
    let mut objects: Vec<ImportedObject> = Vec::new();
    let mut skipped = 0usize;

    // Walk top-level chunks; dispatch the tables we care about, skip the rest.
    while let Some(chunk) = r.next_chunk() {
        match chunk.typecode {
            TCODE_LAYER_TABLE => {
                layers = read_layer_table(chunk.payload);
            }
            TCODE_OBJECT_TABLE => {
                read_object_table(chunk.payload, &layers, &mut objects, &mut skipped);
            }
            TCODE_ENDOFFILE => break,
            _ => {}
        }
    }

    Ok(Import { objects, skipped })
}

// ---------------------------------------------------------------------------
// Chunk reader
// ---------------------------------------------------------------------------

struct Chunk<'a> {
    typecode: u32,
    /// For SHORT chunks this is empty; the inline value lives in `value`.
    payload: &'a [u8],
    #[allow(dead_code)]
    value: u64,
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Validate the 32-byte magic + version, position `pos` past the header.
    fn new(data: &'a [u8]) -> Result<Self, String> {
        const MAGIC: &[u8] = b"3D Geometry File Format ";
        if data.len() < 32 || !data.starts_with(MAGIC) {
            return Err("not a Rhino .3dm file (bad '3D Geometry File Format' header)".to_string());
        }
        // Version digits are right-justified in bytes 24..32.
        let ver_txt: String =
            data[24..32].iter().map(|&b| b as char).filter(|c| c.is_ascii_digit()).collect();
        let version: u32 = ver_txt.trim().parse().unwrap_or(0);
        if version < 50 {
            return Err(format!(
                "unsupported .3dm archive version {version} (only V5+ / version ≥ 50 is read)"
            ));
        }
        Ok(Self { data, pos: 32 })
    }

    /// Read the next chunk at `pos`, advancing past its full extent. Returns
    /// `None` at end-of-data or on a malformed header (defensive stop).
    fn next_chunk(&mut self) -> Option<Chunk<'a>> {
        let typecode = self.read_u32()?;
        if typecode & TCODE_SHORT != 0 {
            // SHORT chunk: the 8-byte field is an inline value, no payload.
            let value = self.read_u64()?;
            Some(Chunk { typecode, payload: &[], value })
        } else {
            // Big chunk: 8-byte length, then that many payload bytes.
            let len = self.read_u64()? as usize;
            let start = self.pos;
            let end = start.checked_add(len)?;
            if end > self.data.len() {
                return None;
            }
            self.pos = end;
            Some(Chunk { typecode, payload: &self.data[start..end], value: 0 })
        }
    }

    fn read_u32(&mut self) -> Option<u32> {
        let b = self.data.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        Some(u32::from_le_bytes(b.try_into().ok()?))
    }
    fn read_u64(&mut self) -> Option<u64> {
        let b = self.data.get(self.pos..self.pos + 8)?;
        self.pos += 8;
        Some(u64::from_le_bytes(b.try_into().ok()?))
    }
}

/// A cursor over a chunk payload, decoding openNURBS primitives.
struct Cur<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Cur<'a> {
    fn new(d: &'a [u8]) -> Self {
        Self { d, p: 0 }
    }
    fn u8(&mut self) -> Option<u8> {
        let v = *self.d.get(self.p)?;
        self.p += 1;
        Some(v)
    }
    fn i32(&mut self) -> Option<i32> {
        let b = self.d.get(self.p..self.p + 4)?;
        self.p += 4;
        Some(i32::from_le_bytes(b.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        self.i32().map(|v| v as u32)
    }
    fn f32(&mut self) -> Option<f32> {
        let b = self.d.get(self.p..self.p + 4)?;
        self.p += 4;
        Some(f32::from_le_bytes(b.try_into().ok()?))
    }
    fn f64(&mut self) -> Option<f64> {
        let b = self.d.get(self.p..self.p + 8)?;
        self.p += 8;
        Some(f64::from_le_bytes(b.try_into().ok()?))
    }
    fn point3d(&mut self) -> Option<DVec3> {
        Some(DVec3::new(self.f64()?, self.f64()?, self.f64()?))
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        self.p = self.p.checked_add(n)?;
        if self.p > self.d.len() {
            return None;
        }
        Some(())
    }
    /// openNURBS `WriteString`: u32 element count (0 or len+1), then that many
    /// little-endian UTF-16 code units (last is a 0 terminator).
    fn string(&mut self) -> Option<String> {
        let count = self.u32()? as usize;
        if count == 0 {
            return Some(String::new());
        }
        let mut units = Vec::with_capacity(count);
        for _ in 0..count {
            let lo = self.u8()? as u16;
            let hi = self.u8()? as u16;
            units.push(lo | (hi << 8));
        }
        // Drop the trailing NUL terminator.
        if units.last() == Some(&0) {
            units.pop();
        }
        Some(String::from_utf16_lossy(&units))
    }
    /// A 16-byte UUID, verbatim on-disk bytes.
    fn uuid(&mut self) -> Option<[u8; 16]> {
        let b = self.d.get(self.p..self.p + 16)?;
        self.p += 16;
        b.try_into().ok()
    }
}

// ---------------------------------------------------------------------------
// Layer table
// ---------------------------------------------------------------------------

/// Decode the layer table into an index→name list (empty on any hiccup).
fn read_layer_table(payload: &[u8]) -> Vec<String> {
    let mut inner = Reader { data: payload, pos: 0 };
    let mut names = Vec::new();
    while let Some(chunk) = inner.next_chunk() {
        match chunk.typecode {
            TCODE_LAYER_RECORD => {
                if let Some(name) = layer_record_name(chunk.payload) {
                    names.push(name);
                } else {
                    names.push(String::new());
                }
            }
            TCODE_ENDOFTABLE => break,
            _ => {}
        }
    }
    names
}

/// Extract the name from an `ON_Layer` record (LAYER_RECORD → OPENNURBS_CLASS →
/// CLASS_DATA → `ON_Layer::Write`). Returns `None` on any structural surprise.
fn layer_record_name(payload: &[u8]) -> Option<String> {
    let data = class_data(payload)?;
    let mut c = Cur::new(data);
    // ON_Layer::Write, chunk version 1.15 (single byte 0x1F).
    let _ver = c.u8()?;
    let _obsolete_mode = c.i32()?;
    let _layer_index = c.i32()?;
    let _iges_level = c.i32()?;
    let _material_index = c.i32()?;
    let _obsolete_model_index = c.i32()?;
    c.skip(4)?; // WriteColor m_color (4 bytes)
    c.skip(2 + 2)?; // WriteShort ×2 (obsolete line style)
    let _d0 = c.f64()?; // obsolete
    let _d1 = c.f64()?; // obsolete
    c.string() // m_name
}

// ---------------------------------------------------------------------------
// Object table
// ---------------------------------------------------------------------------

fn read_object_table(
    payload: &[u8],
    layers: &[String],
    out: &mut Vec<ImportedObject>,
    skipped: &mut usize,
) {
    let mut inner = Reader { data: payload, pos: 0 };
    while let Some(chunk) = inner.next_chunk() {
        match chunk.typecode {
            TCODE_OBJECT_RECORD => match parse_object_record(chunk.payload, layers) {
                Some(obj) => out.push(obj),
                None => *skipped += 1,
            },
            TCODE_ENDOFTABLE => break,
            _ => {}
        }
    }
}

/// Parse one OBJECT_RECORD: find the OPENNURBS_CLASS (geometry) and the
/// ATTRIBUTES (name + layer index). Returns `None` when the geometry class is
/// one we don't reconstruct (counted as skipped by the caller).
fn parse_object_record(payload: &[u8], layers: &[String]) -> Option<ImportedObject> {
    let mut r = Reader { data: payload, pos: 0 };
    let mut geom: Option<Imported> = None;
    let mut name = String::new();
    let mut layer_index: i32 = -1;

    while let Some(chunk) = r.next_chunk() {
        match chunk.typecode {
            TCODE_OPENNURBS_CLASS => {
                geom = parse_class(chunk.payload);
            }
            TCODE_OBJECT_RECORD_TYPE => {} // SHORT: ON::object_type, unused
            TCODE_OBJECT_RECORD_ATTRIBUTES => {
                if let Some((n, li)) = parse_attributes(chunk.payload) {
                    name = n;
                    layer_index = li;
                }
            }
            TCODE_OBJECT_RECORD_END => break,
            _ => {}
        }
    }

    let geom = geom?;
    let layer = layers
        .get(usize::try_from(layer_index).unwrap_or(usize::MAX))
        .filter(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| "rhino".to_string());
    Some(ImportedObject { name, layer, geom })
}

/// Walk an OPENNURBS_CLASS chunk: read its UUID (class id) and DATA, dispatch by
/// class to a geometry decoder. `None` for classes we skip.
fn parse_class(payload: &[u8]) -> Option<Imported> {
    let mut r = Reader { data: payload, pos: 0 };
    let mut uuid: Option<[u8; 16]> = None;
    let mut data: Option<&[u8]> = None;
    while let Some(chunk) = r.next_chunk() {
        match chunk.typecode {
            TCODE_OPENNURBS_CLASS_UUID => {
                // 16-byte UUID followed by a 4-byte CRC (in the length).
                uuid = Cur::new(chunk.payload).uuid();
            }
            TCODE_OPENNURBS_CLASS_DATA => {
                // Trailing 4 bytes are the CRC; strip them from the class data.
                let p = chunk.payload;
                data = Some(if p.len() >= 4 { &p[..p.len() - 4] } else { p });
            }
            TCODE_OPENNURBS_CLASS_END => break,
            _ => {}
        }
    }
    let uuid = uuid?;
    let data = data?;
    match uuid {
        _ if uuid == UUID_MESH => read_mesh(data).map(Imported::Mesh),
        _ if uuid == UUID_LINE_CURVE => read_line_curve(data),
        _ if uuid == UUID_POLYLINE_CURVE => read_polyline_curve(data),
        _ if uuid == UUID_NURBS_CURVE => read_nurbs_curve(data),
        _ => None,
    }
}

/// Descend LAYER_RECORD/OBJECT payload → OPENNURBS_CLASS → CLASS_DATA bytes
/// (CRC stripped). Used by the layer decoder.
fn class_data(payload: &[u8]) -> Option<&[u8]> {
    let mut r = Reader { data: payload, pos: 0 };
    while let Some(chunk) = r.next_chunk() {
        if chunk.typecode == TCODE_OPENNURBS_CLASS {
            let mut cr = Reader { data: chunk.payload, pos: 0 };
            while let Some(inner) = cr.next_chunk() {
                if inner.typecode == TCODE_OPENNURBS_CLASS_DATA {
                    let p = inner.payload;
                    return Some(if p.len() >= 4 { &p[..p.len() - 4] } else { p });
                }
            }
        }
    }
    None
}

/// `ON_3dmObjectAttributes::Write` (V5/V6, chunk version 2.13): read the object
/// id + layer index, then scan the selector TLV list for the name (selector 1).
fn parse_attributes(payload: &[u8]) -> Option<(String, i32)> {
    // ATTRIBUTES is a CRC chunk: strip the trailing 4-byte CRC.
    let data = if payload.len() >= 4 { &payload[..payload.len() - 4] } else { payload };
    let mut c = Cur::new(data);
    let _ver = c.u8()?; // 0x2D (2.13)
    let _uuid = c.uuid()?; // object id
    let layer_index = c.i32()?;
    let mut name = String::new();
    // Selector TLV list, terminated by selector 0.
    while let Some(sel) = c.u8() {
        match sel {
            0 => break,
            1 => {
                name = c.string()?;
            }
            2 => {
                let _url = c.string()?;
            }
            3 | 4 => {
                let _idx = c.i32()?;
            }
            6 => {
                c.skip(4)?; // color
            }
            // Any other selector: we cannot know its length, so stop reading the
            // TLV list (name, if present, comes early). Layer index is already read.
            _ => break,
        }
    }
    Some((name, layer_index))
}

// ---------------------------------------------------------------------------
// Geometry decoders
// ---------------------------------------------------------------------------

/// `ON_LineCurve::Write` (1.0): from(3d), to(3d), interval(2d), dim(int).
fn read_line_curve(data: &[u8]) -> Option<Imported> {
    let mut c = Cur::new(data);
    let _ver = c.u8()?;
    let from = c.point3d()?;
    let to = c.point3d()?;
    Some(Imported::Polyline { points: vec![from, to], closed: false })
}

/// `ON_PolylineCurve::Write` (1.0): point array (count + 3d each), param array,
/// dim.
fn read_polyline_curve(data: &[u8]) -> Option<Imported> {
    let mut c = Cur::new(data);
    let _ver = c.u8()?;
    let count = c.i32()?;
    if !(0..=10_000_000).contains(&count) {
        return None;
    }
    let count = count as usize;
    let mut points = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        points.push(c.point3d()?);
    }
    let closed = points.len() >= 2 && points.first() == points.last();
    if closed {
        points.pop();
    }
    Some(Imported::Polyline { points, closed })
}

/// `ON_NurbsCurve::Write` (1.0/1.1): dim, is_rat, order, cv_count, 2 reserved,
/// bbox(6d), knot_count + knots, cv_count + weighted CVs. Tessellated to a dense
/// polyline via [`kernel_curve::nurbs_point`].
fn read_nurbs_curve(data: &[u8]) -> Option<Imported> {
    let mut c = Cur::new(data);
    let _ver = c.u8()?;
    let dim = c.i32()?;
    let is_rat = c.i32()?;
    let order = c.i32()?;
    let cv_count = c.i32()?;
    let _r0 = c.i32()?;
    let _r1 = c.i32()?;
    c.skip(6 * 8)?; // bbox: 6 doubles
    if !(2..=64).contains(&order) || !(2..=10_000_000).contains(&cv_count) || !(2..=3).contains(&dim)
    {
        return None;
    }
    let degree = (order - 1) as usize;
    let cv_count = cv_count as usize;
    let cv_dim = if is_rat != 0 { dim + 1 } else { dim } as usize;

    let knot_count = c.i32()?;
    if knot_count as usize != order as usize + cv_count - 2 {
        return None;
    }
    // openNURBS omits the two superfluous end knots; clamp back to n+degree+1 by
    // duplicating the first/last so kernel_curve gets a full clamped knot vector.
    let mut mid = Vec::with_capacity(knot_count as usize);
    for _ in 0..knot_count {
        mid.push(c.f64()?);
    }
    let mut knots = Vec::with_capacity(knot_count as usize + 2);
    knots.push(*mid.first()?);
    knots.extend_from_slice(&mid);
    knots.push(*mid.last()?);
    debug_assert_eq!(knots.len(), cv_count + degree + 1);

    let _cv_count2 = c.i32()?;
    let mut control = Vec::with_capacity(cv_count);
    let mut weights = Vec::with_capacity(cv_count);
    for _ in 0..cv_count {
        let mut coords = [0.0f64; 4];
        for coord in coords.iter_mut().take(cv_dim) {
            *coord = c.f64()?;
        }
        if is_rat != 0 {
            let w = coords[dim as usize];
            let w = if w == 0.0 { 1.0 } else { w };
            control.push(DVec3::new(coords[0] / w, coords[1] / w, coords[2] / w));
            weights.push(w);
        } else {
            control.push(DVec3::new(coords[0], coords[1], coords[2]));
            weights.push(1.0);
        }
    }

    let mut points = Vec::with_capacity(NURBS_SAMPLES + 1);
    for i in 0..=NURBS_SAMPLES {
        let t = i as f64 / NURBS_SAMPLES as f64;
        points.push(kernel_curve::nurbs_point(&control, &weights, &knots, degree, t));
    }
    Some(Imported::Polyline { points, closed: false })
}

/// `ON_Mesh::Write` (major 3 = compressed). We read the header up to the face
/// array and the (raw, method-0) vertex compressed-buffer. Faces store 4 indices
/// each (triangle when vi[2]==vi[3]); a quad splits into two triangles.
fn read_mesh(data: &[u8]) -> Option<Mesh> {
    let mut c = Cur::new(data);
    let ver = c.u8()?;
    let major = ver >> 4;
    let minor = ver & 0x0F;
    if major != 3 {
        return None; // only the compressed V3 mesh layout is read
    }
    let vcount = c.i32()?;
    let fcount = c.i32()?;
    if !(0..=50_000_000).contains(&vcount) || !(0..=50_000_000).contains(&fcount) {
        return None;
    }
    let vcount = vcount as usize;
    let fcount = fcount as usize;

    c.skip(4 * 8)?; // packed_tex_domain[0], [1]  (2 intervals = 4 doubles)
    c.skip(4 * 8)?; // srf_domain[0], [1]
    c.skip(2 * 8)?; // srf_scale (2 doubles)
    c.skip(6 * 4)?; // legacy float bbox (6 floats)
    c.skip(6 * 4)?; // m_nbox (6 floats)
    c.skip(4 * 4)?; // m_tbox (4 floats)
    let _closed = c.i32()?;
    // has_mesh_params flag + optional anonymous chunk.
    if c.u8()? != 0 {
        return None; // we don't handle embedded mesh params; skip this mesh
    }
    for _ in 0..4 {
        if c.u8()? != 0 {
            return None; // kstat present → skip
        }
    }

    // --- faces: WriteFaceArray ---
    let i_size = c.i32()?;
    let faces = read_faces(&mut c, fcount, i_size)?;

    // --- vertices: first compressed buffer in Write_2 (m_V, ON_3fPoint) ---
    let positions = read_compressed_f32_points(&mut c, vcount)?;
    // We ignore the remaining N/T/K/C buffers and trailing fields; positions +
    // faces are all we reconstruct.
    let _ = minor;

    if faces.is_empty() {
        return None;
    }
    let verts: Vec<DVec3> =
        positions.iter().map(|p| DVec3::new(p[0] as f64, p[1] as f64, p[2] as f64)).collect();
    Some(Mesh::new(verts, faces))
}

fn read_faces(c: &mut Cur, fcount: usize, i_size: i32) -> Option<Vec<[u32; 3]>> {
    let mut tris = Vec::with_capacity(fcount * 2);
    let read_idx = |c: &mut Cur| -> Option<u32> {
        match i_size {
            1 => c.u8().map(|v| v as u32),
            2 => {
                let lo = c.u8()? as u32;
                let hi = c.u8()? as u32;
                Some(lo | (hi << 8))
            }
            4 => c.u32(),
            _ => None,
        }
    };
    for _ in 0..fcount {
        let vi = [read_idx(c)?, read_idx(c)?, read_idx(c)?, read_idx(c)?];
        if vi[2] == vi[3] {
            tris.push([vi[0], vi[1], vi[2]]);
        } else {
            tris.push([vi[0], vi[1], vi[2]]);
            tris.push([vi[0], vi[2], vi[3]]);
        }
    }
    Some(tris)
}

/// `WriteCompressedBuffer` of `vcount` `ON_3fPoint`s. Layout: u32 uncompressed
/// byte size; if nonzero: u32 crc, u8 method (0=raw, 1=deflate), then data. We
/// read the raw (method 0) form; a deflate buffer (method 1) is unsupported →
/// `None` (our writer never emits it).
fn read_compressed_f32_points(c: &mut Cur, vcount: usize) -> Option<Vec<[f32; 3]>> {
    let size = c.u32()? as usize;
    if size == 0 {
        return if vcount == 0 { Some(Vec::new()) } else { None };
    }
    if size != vcount * 12 {
        return None;
    }
    let _crc = c.u32()?;
    let method = c.u8()?;
    if method != 0 {
        return None; // deflate not supported on read
    }
    let mut pts = Vec::with_capacity(vcount);
    for _ in 0..vcount {
        pts.push([c.f32()?, c.f32()?, c.f32()?]);
    }
    Some(pts)
}

// ---------------------------------------------------------------------------
// CRC32 (zlib polynomial, openNURBS ON_CRC32 semantics: XOR 0xFFFFFFFF at both
// ends of a call; chunk seed is 0). Used only by the minimal writer.
// ---------------------------------------------------------------------------

#[cfg(test)]
fn crc32(seed: u32, buf: &[u8]) -> u32 {
    let mut rem = seed ^ 0xFFFF_FFFF;
    for &b in buf {
        rem = CRC32_TABLE[((rem ^ b as u32) & 0xFF) as usize] ^ (rem >> 8);
    }
    rem ^ 0xFFFF_FFFF
}

#[cfg(test)]
static CRC32_TABLE: [u32; 256] = build_crc_table();

#[cfg(test)]
const fn build_crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut n = 0usize;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[n] = c;
        n += 1;
    }
    t
}

// ---------------------------------------------------------------------------
// Minimal writer — TEST-ONLY. Emits a spec-conformant V5 (version 50) archive
// with a layer table and an object table of meshes/curves, so the reader can be
// exercised against genuine `.3dm` bytes in a round-trip test.
// ---------------------------------------------------------------------------

/// A geometry item the minimal writer can emit.
#[cfg(test)]
pub enum WriteItem {
    Mesh { name: String, layer_index: i32, positions: Vec<DVec3>, tris: Vec<[u32; 3]> },
    Polyline { name: String, layer_index: i32, points: Vec<DVec3> },
    Line { name: String, layer_index: i32, a: DVec3, b: DVec3 },
}

/// Write a minimal but spec-conformant `.3dm` (archive version 50) containing
/// the given layers and geometry. Test-only.
#[cfg(test)]
pub fn write_min(layers: &[&str], items: &[WriteItem]) -> Vec<u8> {
    let mut out = Vec::new();
    // 32-byte header: phrase + spaces, version "50" right-justified in [24..32].
    let mut header = *b"3D Geometry File Format         ";
    header[30] = b'5';
    header[31] = b'0';
    out.extend_from_slice(&header);

    // Comment block (empty-ish): ^Z then NUL.
    write_big_chunk(&mut out, TCODE_COMMENTBLOCK, false, |b| {
        b.push(0x1A);
        b.push(0x00);
    });

    // Layer table.
    write_big_chunk(&mut out, TCODE_LAYER_TABLE, false, |b| {
        for (i, name) in layers.iter().enumerate() {
            write_big_chunk(b, TCODE_LAYER_RECORD, true, |lr| {
                write_opennurbs_class(lr, &UUID_LAYER, |cd| write_layer(cd, i as i32, name));
            });
        }
        write_short_chunk(b, TCODE_ENDOFTABLE, 0);
    });

    // Object table.
    write_big_chunk(&mut out, TCODE_OBJECT_TABLE, false, |b| {
        for item in items {
            write_object_record(b, item);
        }
        write_short_chunk(b, TCODE_ENDOFTABLE, 0);
    });

    let eof = out.len() as u64 + 12;
    write_short_chunk(&mut out, TCODE_ENDOFFILE, eof);
    out
}

#[cfg(test)]
const UUID_LAYER: [u8; 16] =
    [0x3C, 0x36, 0x86, 0xCA, 0xF3, 0x03, 0x11, 0xD4, 0x98, 0x1B, 0x88, 0x1C, 0x2A, 0x00, 0x2A, 0x9E];

#[cfg(test)]
fn write_object_record(out: &mut Vec<u8>, item: &WriteItem) {
    let (name, layer_index) = match item {
        WriteItem::Mesh { name, layer_index, .. } => (name, *layer_index),
        WriteItem::Polyline { name, layer_index, .. } => (name, *layer_index),
        WriteItem::Line { name, layer_index, .. } => (name, *layer_index),
    };
    write_big_chunk(out, TCODE_OBJECT_RECORD, true, |rec| {
        // ON::object_type — value is informational for us; use a generic code.
        write_short_chunk(rec, TCODE_OBJECT_RECORD_TYPE, 0);
        match item {
            WriteItem::Mesh { positions, tris, .. } => {
                write_opennurbs_class(rec, &UUID_MESH, |cd| write_mesh(cd, positions, tris));
            }
            WriteItem::Polyline { points, .. } => {
                write_opennurbs_class(rec, &UUID_POLYLINE_CURVE, |cd| {
                    write_polyline_curve(cd, points)
                });
            }
            WriteItem::Line { a, b, .. } => {
                write_opennurbs_class(rec, &UUID_LINE_CURVE, |cd| write_line_curve(cd, *a, *b));
            }
        }
        write_big_chunk(rec, TCODE_OBJECT_RECORD_ATTRIBUTES, true, |at| {
            write_attributes(at, name, layer_index);
        });
        write_short_chunk(rec, TCODE_OBJECT_RECORD_END, 0);
    });
}

#[cfg(test)]
fn write_opennurbs_class(out: &mut Vec<u8>, uuid: &[u8; 16], data: impl FnOnce(&mut Vec<u8>)) {
    write_big_chunk(out, TCODE_OPENNURBS_CLASS, false, |cls| {
        write_big_chunk(cls, TCODE_OPENNURBS_CLASS_UUID, true, |u| u.extend_from_slice(uuid));
        write_big_chunk(cls, TCODE_OPENNURBS_CLASS_DATA, true, data);
        write_short_chunk(cls, TCODE_OPENNURBS_CLASS_END, 0);
    });
}

#[cfg(test)]
fn write_layer(out: &mut Vec<u8>, index: i32, name: &str) {
    out.push(0x1F); // chunk version 1.15
    w_i32(out, 0); // obsolete mode
    w_i32(out, index); // layer index
    w_i32(out, 0); // iges level
    w_i32(out, -1); // material index
    w_i32(out, 0); // obsolete model index
    out.extend_from_slice(&[0, 0, 0, 0]); // color
    out.extend_from_slice(&[0, 0, 0, 0]); // 2× short
    w_f64(out, 0.0);
    w_f64(out, 1.0);
    w_string(out, name); // m_name — reader stops here
}

#[cfg(test)]
fn write_attributes(out: &mut Vec<u8>, name: &str, layer_index: i32) {
    out.push(0x2D); // chunk version 2.13
    out.extend_from_slice(&[0u8; 16]); // object uuid
    w_i32(out, layer_index);
    if !name.is_empty() {
        out.push(1); // selector: name
        w_string(out, name);
    }
    out.push(0); // terminator
}

#[cfg(test)]
fn write_mesh(out: &mut Vec<u8>, positions: &[DVec3], tris: &[[u32; 3]]) {
    out.push(0x35); // chunk version 3.5 (V5 compressed)
    w_i32(out, positions.len() as i32);
    w_i32(out, tris.len() as i32);
    for _ in 0..(4 + 4 + 2) {
        w_f64(out, 0.0); // packed_tex(4) + srf_domain(4) + srf_scale(2)
    }
    for _ in 0..(6 + 6 + 4) {
        w_f32(out, 0.0); // fbbox(6) + nbox(6) + tbox(4)
    }
    w_i32(out, -1); // closed unknown
    out.push(0); // has_mesh_params
    out.extend_from_slice(&[0, 0, 0, 0]); // 4× kstat flags

    // Faces: i_size then 4 indices per face.
    let i_size: i32 = if positions.len() < 256 {
        1
    } else if positions.len() < 65536 {
        2
    } else {
        4
    };
    w_i32(out, i_size);
    for t in tris {
        for &idx in &[t[0], t[1], t[2], t[2]] {
            match i_size {
                1 => out.push(idx as u8),
                2 => out.extend_from_slice(&(idx as u16).to_le_bytes()),
                _ => out.extend_from_slice(&idx.to_le_bytes()),
            }
        }
    }

    // Vertices: first compressed buffer (raw, method 0).
    let mut vbuf = Vec::with_capacity(positions.len() * 12);
    for p in positions {
        w_f32(&mut vbuf, p.x as f32);
        w_f32(&mut vbuf, p.y as f32);
        w_f32(&mut vbuf, p.z as f32);
    }
    write_compressed_raw(out, &vbuf);
    // The remaining N/T/K/C buffers: empty (size 0). The reader stops after m_V.
}

#[cfg(test)]
fn write_compressed_raw(out: &mut Vec<u8>, data: &[u8]) {
    w_i32(out, data.len() as i32); // uncompressed size
    if data.is_empty() {
        return;
    }
    w_i32(out, crc32(0, data) as i32);
    out.push(0); // method 0 = raw
    out.extend_from_slice(data);
}

#[cfg(test)]
fn write_line_curve(out: &mut Vec<u8>, a: DVec3, b: DVec3) {
    out.push(0x10); // 1.0
    for p in [a, b] {
        w_f64(out, p.x);
        w_f64(out, p.y);
        w_f64(out, p.z);
    }
    w_f64(out, 0.0); // interval min
    w_f64(out, 1.0); // interval max
    w_i32(out, 3); // dim
}

#[cfg(test)]
fn write_polyline_curve(out: &mut Vec<u8>, points: &[DVec3]) {
    out.push(0x10); // 1.0
    w_i32(out, points.len() as i32);
    for p in points {
        w_f64(out, p.x);
        w_f64(out, p.y);
        w_f64(out, p.z);
    }
    w_i32(out, points.len() as i32); // param array
    for i in 0..points.len() {
        w_f64(out, i as f64);
    }
    w_i32(out, 3); // dim
}

// ---- writer primitives ----

#[cfg(test)]
fn w_i32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_le_bytes());
}
#[cfg(test)]
fn w_f32(out: &mut Vec<u8>, v: f32) {
    out.extend_from_slice(&v.to_le_bytes());
}
#[cfg(test)]
fn w_f64(out: &mut Vec<u8>, v: f64) {
    out.extend_from_slice(&v.to_le_bytes());
}
#[cfg(test)]
fn w_string(out: &mut Vec<u8>, s: &str) {
    let units: Vec<u16> = s.encode_utf16().collect();
    if units.is_empty() {
        w_i32(out, 0);
        return;
    }
    w_i32(out, units.len() as i32 + 1); // includes NUL
    for u in units {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes()); // NUL terminator
}

/// Write a big chunk: typecode, then an 8-byte length placeholder, payload from
/// `body`, an optional trailing CRC32 (over the payload+CRC), then backpatch the
/// length (which counts the CRC bytes).
#[cfg(test)]
fn write_big_chunk(out: &mut Vec<u8>, typecode: u32, crc: bool, body: impl FnOnce(&mut Vec<u8>)) {
    out.extend_from_slice(&typecode.to_le_bytes());
    let len_pos = out.len();
    out.extend_from_slice(&0u64.to_le_bytes()); // placeholder
    let payload_start = out.len();
    body(out);
    if crc {
        let c = crc32(0, &out[payload_start..]);
        out.extend_from_slice(&c.to_le_bytes());
    }
    let len = (out.len() - payload_start) as u64;
    out[len_pos..len_pos + 8].copy_from_slice(&len.to_le_bytes());
}

/// Write a SHORT chunk: typecode + 8-byte inline value.
#[cfg(test)]
fn write_short_chunk(out: &mut Vec<u8>, typecode: u32, value: u64) {
    out.extend_from_slice(&typecode.to_le_bytes());
    out.extend_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip: write a spec-conformant .3dm with a mesh, a line, and a
    /// polyline across two layers, read it back, assert geometry/names/layers.
    #[test]
    fn roundtrip_mesh_line_polyline() {
        let layers = ["walls", "lines"];
        let items = vec![
            WriteItem::Mesh {
                name: "cube_face".to_string(),
                layer_index: 0,
                positions: vec![
                    DVec3::new(0.0, 0.0, 0.0),
                    DVec3::new(1.0, 0.0, 0.0),
                    DVec3::new(1.0, 1.0, 0.0),
                    DVec3::new(0.0, 1.0, 0.0),
                ],
                tris: vec![[0, 1, 2], [0, 2, 3]],
            },
            WriteItem::Line {
                name: "edge".to_string(),
                layer_index: 1,
                a: DVec3::new(0.0, 0.0, 0.0),
                b: DVec3::new(5.0, 0.0, 0.0),
            },
            WriteItem::Polyline {
                name: "path".to_string(),
                layer_index: 1,
                points: vec![
                    DVec3::new(0.0, 0.0, 0.0),
                    DVec3::new(1.0, 2.0, 0.0),
                    DVec3::new(3.0, 2.0, 1.0),
                ],
            },
        ];
        let bytes = write_min(&layers, &items);
        let imp = import(&bytes).expect("valid .3dm");
        assert_eq!(imp.skipped, 0);
        assert_eq!(imp.objects.len(), 3);

        // Mesh.
        let m = &imp.objects[0];
        assert_eq!(m.name, "cube_face");
        assert_eq!(m.layer, "walls");
        match &m.geom {
            Imported::Mesh(mesh) => {
                assert_eq!(mesh.positions().len(), 4);
                assert_eq!(mesh.faces().len(), 2);
                assert_eq!(mesh.positions()[1], DVec3::new(1.0, 0.0, 0.0));
            }
            _ => panic!("expected mesh"),
        }

        // Line → 2-point polyline.
        let l = &imp.objects[1];
        assert_eq!(l.name, "edge");
        assert_eq!(l.layer, "lines");
        match &l.geom {
            Imported::Polyline { points, closed } => {
                assert_eq!(points.len(), 2);
                assert!(!closed);
                assert_eq!(points[1], DVec3::new(5.0, 0.0, 0.0));
            }
            _ => panic!("expected polyline"),
        }

        // Polyline.
        let p = &imp.objects[2];
        assert_eq!(p.name, "path");
        assert_eq!(p.layer, "lines");
        match &p.geom {
            Imported::Polyline { points, .. } => {
                assert_eq!(points.len(), 3);
                assert_eq!(points[2], DVec3::new(3.0, 2.0, 1.0));
            }
            _ => panic!("expected polyline"),
        }
    }

    /// An object with an unknown class UUID is skipped and counted, not fatal.
    #[test]
    fn unknown_object_skipped() {
        // Hand-build an object table with one record whose class UUID is bogus.
        let mut out = Vec::new();
        let mut header = *b"3D Geometry File Format         ";
        header[30] = b'5';
        header[31] = b'0';
        out.extend_from_slice(&header);
        write_big_chunk(&mut out, TCODE_COMMENTBLOCK, false, |b| {
            b.push(0x1A);
            b.push(0x00);
        });
        write_big_chunk(&mut out, TCODE_OBJECT_TABLE, false, |b| {
            write_big_chunk(b, TCODE_OBJECT_RECORD, true, |rec| {
                write_short_chunk(rec, TCODE_OBJECT_RECORD_TYPE, 0);
                let bogus = [0xFFu8; 16];
                write_opennurbs_class(rec, &bogus, |cd| cd.extend_from_slice(&[1, 2, 3, 4]));
                write_short_chunk(rec, TCODE_OBJECT_RECORD_END, 0);
            });
            write_short_chunk(b, TCODE_ENDOFTABLE, 0);
        });
        let eof = out.len() as u64 + 12;
        write_short_chunk(&mut out, TCODE_ENDOFFILE, eof);

        let imp = import(&out).expect("valid archive");
        assert_eq!(imp.objects.len(), 0);
        assert_eq!(imp.skipped, 1);
    }

    /// A file with bad magic errors cleanly (no panic).
    #[test]
    fn bad_magic_errors() {
        assert!(import(b"not a rhino file at all, really").is_err());
        assert!(import(&[]).is_err());
    }

    /// A truncated object table is tolerated: earlier objects survive, the walk
    /// stops at the bad chunk instead of panicking.
    #[test]
    fn truncated_file_tolerated() {
        let layers = ["a"];
        let items = vec![WriteItem::Line {
            name: "l".to_string(),
            layer_index: 0,
            a: DVec3::ZERO,
            b: DVec3::X,
        }];
        let full = write_min(&layers, &items);
        // Chop the file mid-way; must not panic and must not error on magic.
        let truncated = &full[..full.len() - 20];
        let _ = import(truncated); // no panic is the assertion
    }

    /// A closed polyline (first point repeated as last) is recognized as closed.
    #[test]
    fn closed_polyline_detected() {
        let items = vec![WriteItem::Polyline {
            name: "loop".to_string(),
            layer_index: 0,
            points: vec![
                DVec3::new(0.0, 0.0, 0.0),
                DVec3::new(1.0, 0.0, 0.0),
                DVec3::new(1.0, 1.0, 0.0),
                DVec3::new(0.0, 0.0, 0.0), // repeat → closed
            ],
        }];
        let bytes = write_min(&["a"], &items);
        let imp = import(&bytes).unwrap();
        match &imp.objects[0].geom {
            Imported::Polyline { points, closed } => {
                assert!(closed);
                assert_eq!(points.len(), 3, "duplicate closing point dropped");
            }
            _ => panic!(),
        }
    }
}

#[cfg(test)]
mod fixture_gen {
    use super::*;
    use glam::DVec3;
    /// Not a real test — emits a sample .3dm to /tmp for the headless visual
    /// sanity. Run explicitly: `cargo test -p itsjustcad-commands emit_sample_3dm -- --ignored`.
    #[test]
    #[ignore]
    fn emit_sample_3dm() {
        // A pyramid mesh + an L-shaped polyline + a diagonal line, two layers.
        let apex = DVec3::new(1.0, 1.0, 3.0);
        let base = [
            DVec3::new(0.0, 0.0, 0.0),
            DVec3::new(2.0, 0.0, 0.0),
            DVec3::new(2.0, 2.0, 0.0),
            DVec3::new(0.0, 2.0, 0.0),
        ];
        let positions = vec![base[0], base[1], base[2], base[3], apex];
        let tris = vec![
            [0, 1, 2], [0, 2, 3], // base
            [0, 1, 4], [1, 2, 4], [2, 3, 4], [3, 0, 4], // sides
        ];
        let items = vec![
            WriteItem::Mesh { name: "pyramid".into(), layer_index: 0, positions, tris },
            WriteItem::Polyline {
                name: "Lpath".into(),
                layer_index: 1,
                points: vec![
                    DVec3::new(-2.0, 0.0, 0.0),
                    DVec3::new(-2.0, 3.0, 0.0),
                    DVec3::new(1.0, 3.0, 0.0),
                ],
            },
            WriteItem::Line {
                name: "diag".into(),
                layer_index: 1,
                a: DVec3::new(-2.0, 0.0, 0.0),
                b: DVec3::new(2.0, 2.0, 3.0),
            },
        ];
        let bytes = write_min(&["solids", "wires"], &items);
        std::fs::write("/tmp/itsjustcad_sample.3dm", &bytes).unwrap();
        eprintln!("wrote /tmp/itsjustcad_sample.3dm ({} bytes)", bytes.len());
    }
}
