// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! SAF — Structural Analysis Format — export.
//!
//! SAF is an Excel (.xlsx) workbook schema published by the Nemetschek /
//! SCIA group (saf.guide, v2.2.0). The spec defines a fixed set of
//! worksheet names with typed column headers; receiving software (RFEM, SCIA
//! Engineer, AxisVM, FEM-Design, etc.) reads the workbook directly.
//!
//! ## Format deviation — ZIP-of-CSV instead of .xlsx
//!
//! Writing a valid OOXML / xlsx file requires generating XML parts inside a
//! ZIP with SharedStrings, Styles, workbook.xml relationships, and so on.
//! That is ~500 lines of boiler-plate with zero structural insight.  The
//! project's minimal-deps ethos (no Excel writer crate) pushes us toward an
//! equivalent representation:
//!
//! **We emit a ZIP archive whose entries are one CSV file per SAF sheet**
//! (`StructuralPointConnection.csv`, `StructuralCurveMember.csv`, …).  The
//! sheet names and column headers match the SAF 2.2.0 spec *exactly*, so a
//! converter script (`saf_csv_to_xlsx.py`, trivial with openpyxl/xlsxwriter)
//! can reassemble the true .xlsx in < 20 lines.  The file is given the
//! `.saf` extension; the ZIP magic bytes (`PK\x03\x04`) identify the
//! container unambiguously.
//!
//! This deviation is **noted and intentional**.  If a native xlsx writer is
//! added as a workspace dep in the future the internal tables below plug
//! straight in.
//!
//! ## Sheets emitted
//!
//! | Sheet name                  | SAF entity                  |
//! |-----------------------------|-----------------------------|
//! | `StructuralPointConnection` | Nodes (X, Y, Z)             |
//! | `StructuralCurveMember`     | 1D members (beams/columns)  |
//! | `StructuralSurfaceMember`   | 2D members (slabs/walls)    |
//! | `StructuralCrossSection`    | Named sections              |
//! | `StructuralMaterial`        | Named materials             |
//! | `StructuralPointSupport`    | Nodal supports / BCs        |
//! | `StructuralLoadCase`        | Load cases (one per load)   |
//! | `StructuralPointAction`     | Point loads                 |
//! | `StructuralCurveAction`     | Line / distributed loads    |
//!
//! ## Sources
//! * <https://www.saf.guide/> — official SAF documentation v2.2.0
//! * <https://github.com/StructuralAnalysisFormat> — GitHub organisation
//! * <https://community.osarch.org/discussion/252/structural-analysis-format-saf>

use std::collections::BTreeMap;

use glam::DVec3;
use itsjustcad_doc::{AreaKind, Document, FrameKind, Geometry, LoadGeometry, RestraintKind};
use kernel_mesh::StructSection as Section;

// ============================================================================
// CSV helpers
// ============================================================================

fn quote(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn row(fields: &[&str]) -> String {
    let mut s = fields.iter().map(|f| quote(f)).collect::<Vec<_>>().join(",");
    s.push_str("\r\n");
    s
}

fn fmtf(v: f64) -> String {
    // SAF coordinates are in metres, 6 decimal places is sub-μm precision.
    format!("{v:.6}")
}

// ============================================================================
// ZIP writer (store mode, no compression — text CSVs stay small enough)
// ============================================================================

/// Minimal ZIP archive writer emitting DEFLATE-method-0 (Stored) entries.
/// We hand-write the ZIP format to stay dependency-free; it is valid
/// ZIP 2.0 (all unzippers and `unzip(1)` handle it).
struct ZipWriter {
    buf: Vec<u8>,
    entries: Vec<ZipEntry>,
}

struct ZipEntry {
    name: Vec<u8>,
    offset: u32,
    crc32: u32,
    size: u32,
}

impl ZipWriter {
    fn new() -> Self {
        Self { buf: Vec::new(), entries: Vec::new() }
    }

    fn add_file(&mut self, name: &str, data: &[u8]) {
        let offset = self.buf.len() as u32;
        let crc = crc32(data);
        let size = data.len() as u32;

        // Local file header
        self.buf.extend_from_slice(&[0x50, 0x4B, 0x03, 0x04]); // signature
        self.buf.extend_from_slice(&[0x14, 0x00]); // version needed: 2.0
        self.buf.extend_from_slice(&[0x00, 0x00]); // flags
        self.buf.extend_from_slice(&[0x00, 0x00]); // compression: stored
        self.buf.extend_from_slice(&[0x00, 0x00]); // mod time
        self.buf.extend_from_slice(&[0x00, 0x00]); // mod date
        self.buf.extend_from_slice(&crc.to_le_bytes());
        self.buf.extend_from_slice(&size.to_le_bytes()); // compressed size
        self.buf.extend_from_slice(&size.to_le_bytes()); // uncompressed size
        let name_bytes = name.as_bytes();
        self.buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        self.buf.extend_from_slice(&[0x00, 0x00]); // extra field length
        self.buf.extend_from_slice(name_bytes);
        self.buf.extend_from_slice(data);

        self.entries.push(ZipEntry {
            name: name_bytes.to_vec(),
            offset,
            crc32: crc,
            size,
        });
    }

    fn finish(mut self) -> Vec<u8> {
        let cd_offset = self.buf.len() as u32;
        let mut cd_size: u32 = 0;

        for entry in &self.entries {
            let start = self.buf.len();
            self.buf.extend_from_slice(&[0x50, 0x4B, 0x01, 0x02]); // CD header sig
            self.buf.extend_from_slice(&[0x14, 0x00]); // version made by
            self.buf.extend_from_slice(&[0x14, 0x00]); // version needed
            self.buf.extend_from_slice(&[0x00, 0x00]); // flags
            self.buf.extend_from_slice(&[0x00, 0x00]); // method: stored
            self.buf.extend_from_slice(&[0x00, 0x00]); // mod time
            self.buf.extend_from_slice(&[0x00, 0x00]); // mod date
            self.buf.extend_from_slice(&entry.crc32.to_le_bytes());
            self.buf.extend_from_slice(&entry.size.to_le_bytes()); // compressed
            self.buf.extend_from_slice(&entry.size.to_le_bytes()); // uncompressed
            self.buf.extend_from_slice(&(entry.name.len() as u16).to_le_bytes());
            self.buf.extend_from_slice(&[0x00, 0x00]); // extra len
            self.buf.extend_from_slice(&[0x00, 0x00]); // comment len
            self.buf.extend_from_slice(&[0x00, 0x00]); // disk number start
            self.buf.extend_from_slice(&[0x00, 0x00]); // int attributes
            self.buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // ext attributes
            self.buf.extend_from_slice(&entry.offset.to_le_bytes());
            self.buf.extend_from_slice(&entry.name);
            cd_size += (self.buf.len() - start) as u32;
        }

        let num = self.entries.len() as u16;
        // End of central directory record
        self.buf.extend_from_slice(&[0x50, 0x4B, 0x05, 0x06]); // EOCD sig
        self.buf.extend_from_slice(&[0x00, 0x00]); // disk number
        self.buf.extend_from_slice(&[0x00, 0x00]); // disk with CD
        self.buf.extend_from_slice(&num.to_le_bytes()); // entries on disk
        self.buf.extend_from_slice(&num.to_le_bytes()); // total entries
        self.buf.extend_from_slice(&cd_size.to_le_bytes());
        self.buf.extend_from_slice(&cd_offset.to_le_bytes());
        self.buf.extend_from_slice(&[0x00, 0x00]); // comment length
        self.buf
    }
}

/// CRC-32 (ISO 3309 / ITU-T V.42) — required by the ZIP format.
fn crc32(data: &[u8]) -> u32 {
    // Standard CRC-32 table driven by the polynomial 0xEDB88320.
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, entry) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *entry = c;
        }
        t
    });
    let mut crc = !0u32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

// ============================================================================
// Node deduplication
// ============================================================================

/// Unique nodes, keyed by rounded-to-mm position to handle floating-point
/// near-coincident points from modelling.  Returns a node name and a lookup
/// closure.
struct NodeTable {
    map: BTreeMap<(i64, i64, i64), String>,
    count: usize,
}

impl NodeTable {
    fn new() -> Self {
        Self { map: BTreeMap::new(), count: 0 }
    }

    /// Round a coordinate to the nearest millimetre for deduplication.
    fn key(p: DVec3) -> (i64, i64, i64) {
        (
            (p.x * 1000.0).round() as i64,
            (p.y * 1000.0).round() as i64,
            (p.z * 1000.0).round() as i64,
        )
    }

    /// Insert or return the existing node name for position `p`.
    fn intern(&mut self, p: DVec3) -> String {
        let k = Self::key(p);
        if let Some(name) = self.map.get(&k) {
            return name.clone();
        }
        self.count += 1;
        let name = format!("N{}", self.count);
        self.map.insert(k, name.clone());
        name
    }

    /// Iterate in insertion order (BTreeMap gives sorted key order; good enough
    /// — SAF does not require a specific node order).
    fn iter(&self) -> impl Iterator<Item = (DVec3, &str)> {
        self.map.iter().map(|((ix, iy, iz), name)| {
            let p = DVec3::new(*ix as f64 / 1000.0, *iy as f64 / 1000.0, *iz as f64 / 1000.0);
            (p, name.as_str())
        })
    }

    fn len(&self) -> usize {
        self.map.len()
    }
}

// ============================================================================
// Cross-section helpers
// ============================================================================

fn section_name(sec: &Section) -> String {
    match *sec {
        Section::Rectangular { w, h } => format!("RECT_{:.0}x{:.0}", w * 1000.0, h * 1000.0),
        Section::Circular { d } => format!("CIRC_{:.0}", d * 1000.0),
        Section::IWideFlange { d, bf, .. } => {
            format!("IWF_{:.0}x{:.0}", d * 1000.0, bf * 1000.0)
        }
        Section::Pipe { d, t } => format!("PIPE_{:.0}x{:.0}", d * 1000.0, t * 1000.0),
    }
}

/// SAF cross-section type string (one of: "Parametric", "Manufactured", "General").
fn section_type(sec: &Section) -> &'static str {
    match sec {
        Section::Rectangular { .. }
        | Section::Circular { .. }
        | Section::Pipe { .. }
        | Section::IWideFlange { .. } => "Parametric",
    }
}

/// SAF shape string for parametric sections.
fn section_shape(sec: &Section) -> &'static str {
    match sec {
        Section::Rectangular { .. } => "Rectangle",
        Section::Circular { .. } => "Circle",
        Section::Pipe { .. } => "Circle hollow",
        Section::IWideFlange { .. } => "I or H",
    }
}

/// Parameters string: SAF wants "b;h" (mm) for Rectangle, "d" for Circle, etc.
fn section_params(sec: &Section) -> String {
    match *sec {
        Section::Rectangular { w, h } => format!("{:.1};{:.1}", w * 1000.0, h * 1000.0),
        Section::Circular { d } => format!("{:.1}", d * 1000.0),
        Section::Pipe { d, t } => format!("{:.1};{:.1}", d * 1000.0, t * 1000.0),
        Section::IWideFlange { d, bf, tf, tw } => format!(
            "{:.1};{:.1};{:.1};{:.1}",
            d * 1000.0,
            bf * 1000.0,
            tf * 1000.0,
            tw * 1000.0
        ),
    }
}

// ============================================================================
// Public export entry point
// ============================================================================

/// Export the document as a SAF ZIP-of-CSV archive.
///
/// The returned `Vec<u8>` is a valid ZIP file.  Each entry is a CSV whose
/// name matches a SAF 2.2.0 worksheet name exactly.  Column headers also
/// match the spec so a converter can rebuild the true `.xlsx`.
///
/// Returns `(bytes, summary_string)`.
pub fn export(doc: &Document) -> Result<(Vec<u8>, String), String> {
    // -----------------------------------------------------------------------
    // 1. Collect structural data from the document
    // -----------------------------------------------------------------------

    struct FrameRow {
        name: String,
        kind: FrameKind,
        node_a: String,
        node_b: String,
        section_name: String,
        material: Option<String>,
    }

    struct AreaRow {
        name: String,
        kind: AreaKind,
        nodes: Vec<String>,
        thickness: f64,
        #[allow(dead_code)] // stored for future surface load cross-referencing
        material: Option<String>,
    }

    let mut nodes = NodeTable::new();

    // Collect materials and cross-sections from frame/area members in the doc
    // (including inline ones not in the named tables — belt-and-suspenders).
    let mut sec_set: BTreeMap<String, Section> = BTreeMap::new();
    let mut mat_set: BTreeMap<String, itsjustcad_doc::Material> = BTreeMap::new();

    // Named tables from the document
    for (name, sec) in &doc.sections {
        sec_set.insert(name.clone(), *sec);
    }
    for (name, mat) in &doc.materials {
        mat_set.insert(name.clone(), *mat);
    }

    let mut frame_rows: Vec<FrameRow> = Vec::new();
    let mut area_rows: Vec<AreaRow> = Vec::new();

    for obj in doc.objects() {
        match &obj.geometry {
            Geometry::Frame { kind, a, b, section, material, .. } => {
                let node_a = nodes.intern(*a);
                let node_b = nodes.intern(*b);
                let sname = section_name(section);
                sec_set.insert(sname.clone(), *section);
                let obj_name = obj
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("{}_{}", kind.label(), frame_rows.len() + 1));
                frame_rows.push(FrameRow {
                    name: obj_name,
                    kind: *kind,
                    node_a,
                    node_b,
                    section_name: sname,
                    material: material.clone(),
                });
            }
            Geometry::Area { kind, boundary, thickness, material, .. } => {
                let node_names: Vec<String> = boundary.iter().map(|p| nodes.intern(*p)).collect();
                let obj_name = obj
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("{}_{}", kind.label(), area_rows.len() + 1));
                area_rows.push(AreaRow {
                    name: obj_name,
                    kind: *kind,
                    nodes: node_names,
                    thickness: *thickness,
                    material: material.clone(),
                });
            }
            _ => {}
        }
    }

    // Support nodes must also exist in the node table.
    for (i, sup) in doc.supports.iter().enumerate() {
        let _ = nodes.intern(sup.position);
        let _ = i; // suppress lint
    }
    for load in &doc.loads {
        if let LoadGeometry::Point { position } = load.geometry {
            let _ = nodes.intern(position);
        }
    }

    // -----------------------------------------------------------------------
    // 2. Build CSV content for each SAF sheet
    // -----------------------------------------------------------------------

    // -- StructuralPointConnection (nodes) -----------------------------------
    let mut node_csv = String::new();
    node_csv.push_str(&row(&["Name", "Coordinate X [m]", "Coordinate Y [m]", "Coordinate Z [m]"]));
    for (p, name) in nodes.iter() {
        node_csv.push_str(&row(&[name, &fmtf(p.x), &fmtf(p.y), &fmtf(p.z)]));
    }
    let node_count = nodes.len();

    // -- StructuralMaterial --------------------------------------------------
    let mut mat_csv = String::new();
    mat_csv.push_str(&row(&[
        "Name",
        "Type",
        "Subtype",
        "Quality",
        "Unit mass [kg/m3]",
        "E modulus [MPa]",
        "G modulus [MPa]",
        "Poisson Coefficient",
        "Thermal expansion [1/K]",
    ]));
    for (name, mat) in &mat_set {
        let e_mpa = mat.elastic_modulus_e / 1e6;
        // G = E / (2(1+ν)); assume ν = 0.3 if not stored (structural default).
        let nu = 0.3_f64;
        let g_mpa = e_mpa / (2.0 * (1.0 + nu));
        mat_csv.push_str(&row(&[
            name,
            "Other",
            "",
            "",
            &format!("{:.3}", mat.density),
            &format!("{:.3}", e_mpa),
            &format!("{:.3}", g_mpa),
            &format!("{nu:.3}"),
            "0.000012",
        ]));
    }

    // -- StructuralCrossSection ----------------------------------------------
    let mut sec_csv = String::new();
    sec_csv.push_str(&row(&[
        "Name",
        "Material",
        "Cross-section type",
        "Shape",
        "Parameters [mm]",
        "A [m2]",
    ]));
    for (name, sec) in &sec_set {
        // Try to find an associated material name from any frame that uses
        // this section name (best-effort: first match wins).
        let mat_name = frame_rows
            .iter()
            .find(|f| &f.section_name == name)
            .and_then(|f| f.material.clone())
            .unwrap_or_default();
        sec_csv.push_str(&row(&[
            name,
            &mat_name,
            section_type(sec),
            section_shape(sec),
            &section_params(sec),
            &format!("{:.8}", sec.area()),
        ]));
    }

    // -- StructuralCurveMember (1D frame members) ----------------------------
    let mut curve_csv = String::new();
    curve_csv.push_str(&row(&[
        "Name",
        "Type",
        "Cross section",
        "Nodes",
        "LCS",
        "LCS Rotation [deg]",
        "System line",
        "Behaviour in analysis",
        "Layer",
    ]));
    let member_count = frame_rows.len();
    for fr in &frame_rows {
        let type_str = match fr.kind {
            FrameKind::Beam => "Beam",
            FrameKind::Column => "Column",
        };
        let nodes_str = format!("{};{}", fr.node_a, fr.node_b);
        curve_csv.push_str(&row(&[
            &fr.name,
            type_str,
            &fr.section_name,
            &nodes_str,
            "ZAxis",
            "0",
            "Centre",
            "Standard",
            "",
        ]));
    }

    // -- StructuralSurfaceMember (2D area members) ---------------------------
    let mut surf_csv = String::new();
    surf_csv.push_str(&row(&[
        "Name",
        "Type",
        "Thickness [m]",
        "Nodes",
        "System plane",
        "Behaviour in analysis",
        "Layer",
    ]));
    for ar in &area_rows {
        let type_str = match ar.kind {
            AreaKind::Slab => "Plate",
            AreaKind::Wall => "Wall",
        };
        let nodes_str = ar.nodes.join(";");
        surf_csv.push_str(&row(&[
            &ar.name,
            type_str,
            &fmtf(ar.thickness),
            &nodes_str,
            "Top",
            "Standard",
            "",
        ]));
    }

    // -- StructuralPointSupport ----------------------------------------------
    let mut supp_csv = String::new();
    supp_csv.push_str(&row(&[
        "Name",
        "Node",
        "Type",
        "ux",
        "uy",
        "uz",
        "fix",
        "fiy",
        "fiz",
    ]));
    for (i, sup) in doc.supports.iter().enumerate() {
        let sname = format!("SUP{}", i + 1);
        let node_name = {
            let k = NodeTable::key(sup.position);
            // The support position was already interned above; look it up.
            nodes.map.get(&k).cloned().unwrap_or_else(|| format!("N?{i}"))
        };
        let (ux, uy, uz, fix, fiy, fiz) = match sup.kind {
            RestraintKind::Pinned => ("Fixed", "Fixed", "Fixed", "Free", "Free", "Free"),
            RestraintKind::Fixed => ("Fixed", "Fixed", "Fixed", "Fixed", "Fixed", "Fixed"),
            RestraintKind::Roller => {
                // Free along one translational axis; we map the first
                // non-zero component of roller_axis (if any) to the free DOF.
                let axis = sup.roller_axis.unwrap_or(DVec3::X);
                if axis.x.abs() > 0.5 {
                    ("Free", "Fixed", "Fixed", "Free", "Free", "Free")
                } else if axis.y.abs() > 0.5 {
                    ("Fixed", "Free", "Fixed", "Free", "Free", "Free")
                } else {
                    ("Fixed", "Fixed", "Free", "Free", "Free", "Free")
                }
            }
        };
        supp_csv.push_str(&row(&[&sname, &node_name, "Nodal", ux, uy, uz, fix, fiy, fiz]));
    }

    // -- StructuralLoadCase + loads ------------------------------------------
    // SAF keeps load cases and loads in separate sheets.  We create one load
    // case per unique load name, then reference it from the action rows.
    let mut lc_names: BTreeMap<String, usize> = BTreeMap::new();
    for load in &doc.loads {
        let idx = lc_names.len() + 1;
        lc_names.entry(load.name.clone()).or_insert(idx);
    }

    let mut lc_csv = String::new();
    lc_csv.push_str(&row(&["Name", "Description", "Action type", "Load type"]));
    for name in lc_names.keys() {
        // Default: Permanent (dead load) — the user can change in the solver.
        lc_csv.push_str(&row(&[name, name, "Permanent", "Self weight"]));
    }

    let mut pt_action_csv = String::new();
    pt_action_csv.push_str(&row(&[
        "Name",
        "Load case",
        "Node",
        "Direction",
        "Value [kN]",
    ]));

    let mut ln_action_csv = String::new();
    ln_action_csv.push_str(&row(&[
        "Name",
        "Load case",
        "Node 1",
        "Node 2",
        "Direction",
        "Value [kN/m]",
        "Value 2 [kN/m]",
    ]));

    let mut pt_action_count = 0usize;
    let mut ln_action_count = 0usize;

    for (i, load) in doc.loads.iter().enumerate() {
        let lc = &load.name;
        // Force in kN (doc stores N).
        let val_kn = load.magnitude / 1000.0;
        let dir = format_dir(load.direction);

        match &load.geometry {
            LoadGeometry::Point { position } => {
                pt_action_count += 1;
                let act_name = format!("PA{}", i + 1);
                let k = NodeTable::key(*position);
                let nname =
                    nodes.map.get(&k).cloned().unwrap_or_else(|| format!("N?{i}"));
                pt_action_csv.push_str(&row(&[
                    &act_name,
                    lc,
                    &nname,
                    &dir,
                    &format!("{val_kn:.4}"),
                ]));
            }
            LoadGeometry::Line { a, b } => {
                ln_action_count += 1;
                let act_name = format!("LA{}", i + 1);
                let ka = NodeTable::key(*a);
                let kb = NodeTable::key(*b);
                let na = nodes.map.get(&ka).cloned().unwrap_or_else(|| format!("N?{i}a"));
                let nb = nodes.map.get(&kb).cloned().unwrap_or_else(|| format!("N?{i}b"));
                // Uniform line load: value1 == value2 == magnitude (N/m → kN/m).
                ln_action_csv.push_str(&row(&[
                    &act_name,
                    lc,
                    &na,
                    &nb,
                    &dir,
                    &format!("{val_kn:.4}"),
                    &format!("{val_kn:.4}"),
                ]));
            }
            LoadGeometry::Area { .. } => {
                // Surface loads would go in StructuralSurfaceAction; scope-cut:
                // area loads reference a 2D member ID which we don't track here.
                // Noted: area loads are omitted from the SAF export.
            }
        }
    }

    // -----------------------------------------------------------------------
    // 3. Pack into ZIP
    // -----------------------------------------------------------------------

    let mut zip = ZipWriter::new();
    zip.add_file("StructuralPointConnection.csv", node_csv.as_bytes());
    zip.add_file("StructuralMaterial.csv", mat_csv.as_bytes());
    zip.add_file("StructuralCrossSection.csv", sec_csv.as_bytes());
    zip.add_file("StructuralCurveMember.csv", curve_csv.as_bytes());
    zip.add_file("StructuralSurfaceMember.csv", surf_csv.as_bytes());
    zip.add_file("StructuralPointSupport.csv", supp_csv.as_bytes());
    zip.add_file("StructuralLoadCase.csv", lc_csv.as_bytes());
    zip.add_file("StructuralPointAction.csv", pt_action_csv.as_bytes());
    zip.add_file("StructuralCurveAction.csv", ln_action_csv.as_bytes());

    let bytes = zip.finish();
    let summary = format!(
        "SAF (ZIP-of-CSV), {node_count} nodes, {member_count} members, {} sections, {} materials, {} supports, {} load cases, {} point loads, {} line loads",
        sec_set.len(),
        mat_set.len(),
        doc.supports.len(),
        lc_names.len(),
        pt_action_count,
        ln_action_count,
    );
    Ok((bytes, summary))
}

/// Convert a force direction vector to a SAF direction string.
/// SAF uses "X", "Y", "Z", "-X", "-Y", "-Z" or "Vector".
fn format_dir(d: DVec3) -> String {
    // Snap to cardinal axis if within 5° (cos > 0.996).
    let axes = [
        (DVec3::X, "X"),
        (-DVec3::X, "-X"),
        (DVec3::Y, "Y"),
        (-DVec3::Y, "-Y"),
        (DVec3::Z, "Z"),
        (-DVec3::Z, "-Z"),
    ];
    if let Some((_, label)) = axes.iter().find(|(ax, _)| d.dot(*ax) > 0.996) {
        return label.to_string();
    }
    // Non-cardinal: emit as "Vector" (SAF also accepts vx;vy;vz notation).
    format!("{:.4};{:.4};{:.4}", d.x, d.y, d.z)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use glam::DVec3;
    use itsjustcad_doc::{
        Document, FrameKind, Geometry, LoadGeometry, ObjectId, RestraintKind, SceneObject,
        StructLoad, StructSupport,
    };
    use kernel_mesh::{Mesh, StructSection};

    /// Build a minimal two-node frame: column at (0,0,0)→(0,0,3) and beam
    /// (0,0,3)→(5,0,3), plus a pinned support, a fixed support, and a
    /// point load.
    fn frame_doc() -> Document {
        let mut doc = Document::default();

        // Add a section to the named section table
        doc.sections.insert(
            "RECT_300x300".to_string(),
            StructSection::Rectangular { w: 0.3, h: 0.3 },
        );
        // Add a material
        doc.materials.insert(
            "C30".to_string(),
            itsjustcad_doc::Material { elastic_modulus_e: 30e9, density: 2400.0 },
        );

        let col_mesh = Mesh::new(
            vec![DVec3::ZERO, DVec3::new(0.3, 0.0, 0.0), DVec3::new(0.0, 0.3, 0.0)],
            vec![[0, 1, 2]],
        );
        doc.insert(SceneObject {
            id: ObjectId::new(),
            name: Some("COL1".to_string()),
            layer: "default".to_string(),
            visible: true,
            color: None,
            geometry: Geometry::Frame {
                kind: FrameKind::Column,
                a: DVec3::new(0.0, 0.0, 0.0),
                b: DVec3::new(0.0, 0.0, 3.0),
                section: StructSection::Rectangular { w: 0.3, h: 0.3 },
                material: Some("C30".to_string()),
                orientation_deg: 0.0,
                mesh: col_mesh,
            },
        });

        let beam_mesh = Mesh::new(
            vec![DVec3::ZERO, DVec3::new(0.3, 0.0, 0.0), DVec3::new(0.0, 0.3, 0.0)],
            vec![[0, 1, 2]],
        );
        doc.insert(SceneObject {
            id: ObjectId::new(),
            name: Some("BEAM1".to_string()),
            layer: "default".to_string(),
            visible: true,
            color: None,
            geometry: Geometry::Frame {
                kind: FrameKind::Beam,
                a: DVec3::new(0.0, 0.0, 3.0),
                b: DVec3::new(5.0, 0.0, 3.0),
                section: StructSection::Rectangular { w: 0.3, h: 0.3 },
                material: Some("C30".to_string()),
                orientation_deg: 0.0,
                mesh: beam_mesh,
            },
        });

        // Pinned support at column base
        doc.supports.push(StructSupport {
            position: DVec3::new(0.0, 0.0, 0.0),
            kind: RestraintKind::Pinned,
            roller_axis: None,
        });

        // Point load at beam top
        doc.loads.push(StructLoad {
            name: "DL".to_string(),
            magnitude: 50_000.0, // 50 kN
            direction: -DVec3::Z,
            geometry: LoadGeometry::Point { position: DVec3::new(0.0, 0.0, 3.0) },
        });

        doc
    }

    #[test]
    fn export_produces_valid_zip() {
        let doc = frame_doc();
        let (bytes, summary) = export(&doc).expect("export ok");
        // ZIP magic
        assert_eq!(&bytes[0..4], b"PK\x03\x04", "ZIP local file header magic");
        // Summary mentions tables
        assert!(summary.contains("nodes"), "summary has node count: {summary}");
        assert!(summary.contains("members"), "summary has member count: {summary}");
    }

    #[test]
    fn zip_contains_expected_sheets() {
        let doc = frame_doc();
        let (bytes, _) = export(&doc).expect("export ok");
        let content = String::from_utf8_lossy(&bytes);
        // Central directory entry names are plain ASCII inside the ZIP bytes
        for name in &[
            "StructuralPointConnection.csv",
            "StructuralCurveMember.csv",
            "StructuralCrossSection.csv",
            "StructuralMaterial.csv",
            "StructuralPointSupport.csv",
            "StructuralLoadCase.csv",
            "StructuralPointAction.csv",
        ] {
            assert!(content.contains(name), "ZIP missing sheet '{name}'");
        }
    }

    #[test]
    fn node_csv_contains_three_nodes() {
        // Column: (0,0,0)→(0,0,3); Beam: (0,0,3)→(5,0,3); support at (0,0,0).
        // Unique nodes: (0,0,0), (0,0,3), (5,0,3) → 3.
        let doc = frame_doc();
        let (bytes, summary) = export(&doc).expect("export ok");
        assert!(summary.contains("3 nodes"), "expected 3 nodes in summary: {summary}");

        // Find StructuralPointConnection.csv inside ZIP (stored uncompressed)
        let raw = String::from_utf8_lossy(&bytes);
        let has_z3 = raw.contains("3.000000");
        assert!(has_z3, "node at Z=3.0 should appear in ZIP bytes");
    }

    #[test]
    fn member_csv_has_two_rows() {
        let doc = frame_doc();
        let (_, summary) = export(&doc).expect("export ok");
        assert!(summary.contains("2 members"), "expected 2 members: {summary}");
    }

    #[test]
    fn section_csv_has_correct_entry() {
        let doc = frame_doc();
        let (bytes, _) = export(&doc).expect("export ok");
        let raw = String::from_utf8_lossy(&bytes);
        // The section name RECT_300x300 must appear (300mm x 300mm).
        assert!(raw.contains("RECT_300x300"), "section name in ZIP");
        // Rectangle shape
        assert!(raw.contains("Rectangle"), "shape label in ZIP");
    }

    #[test]
    fn support_csv_pinned_dofs() {
        let doc = frame_doc();
        let (bytes, _) = export(&doc).expect("export ok");
        let raw = String::from_utf8_lossy(&bytes);
        // Pinned: ux=Fixed, uy=Fixed, uz=Fixed, fix=Free, fiy=Free, fiz=Free
        assert!(raw.contains("Fixed,Fixed,Fixed,Free,Free,Free"), "pinned DOF pattern");
    }

    #[test]
    fn load_case_and_point_action_exported() {
        let doc = frame_doc();
        let (bytes, summary) = export(&doc).expect("export ok");
        let raw = String::from_utf8_lossy(&bytes);
        // Load case name "DL" must appear
        assert!(raw.contains("DL"), "load case name in ZIP");
        // 50 kN = 50.0000 in CSV
        assert!(raw.contains("50.0000"), "50 kN point load value");
        assert!(summary.contains("1 load cases"), "load case count: {summary}");
        assert!(summary.contains("1 point loads"), "point load count: {summary}");
    }

    #[test]
    fn crc32_known_value() {
        // CRC-32 of "123456789" is 0xCBF43926
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn replay_stability() {
        // Two exports of the same doc produce identical bytes.
        let doc = frame_doc();
        let (b1, _) = export(&doc).expect("first export");
        let (b2, _) = export(&doc).expect("second export");
        assert_eq!(b1, b2, "export must be deterministic");
    }
}
