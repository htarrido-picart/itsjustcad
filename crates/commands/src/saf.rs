// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! SAF — Structural Analysis Format — export.
//!
//! SAF is an Excel (.xlsx) workbook schema published by the Nemetschek /
//! SCIA group (saf.guide, v2.2.0). The spec defines a fixed set of
//! worksheet names with typed column headers; receiving software (RFEM, SCIA
//! Engineer, AxisVM, FEM-Design, etc.) reads the workbook directly.
//!
//! ## Container — genuine .xlsx, hand-rolled
//!
//! We emit a **real OOXML .xlsx workbook** (ZIP of XML parts) so analysis
//! packages can open the file directly — no converter step. The writer is
//! hand-rolled on top of the same minimal store-mode ZIP writer used
//! elsewhere in this module (no Excel crate, license-clean, pure Rust):
//!
//! * `[Content_Types].xml`, `_rels/.rels` — OOXML package plumbing
//! * `docProps/core.xml` — creator + the interop disclaimer ("geometry +
//!   topology handoff, **no analysis results**")
//! * `xl/workbook.xml` + `xl/_rels/workbook.xml.rels` — one `<sheet>` per
//!   SAF table, named exactly per the SAF 2.2.0 spec
//! * `xl/worksheets/sheetN.xml` — header row + data rows; numeric fields are
//!   written as native number cells (`<v>`), text as `inlineStr` (so no
//!   sharedStrings part is needed)
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
//! Interop ONLY: the file carries model geometry, topology, sections,
//! materials, supports and loads for handoff to a real analysis package.
//! ItsJustCAD performs **no structural analysis** and the export contains
//! no analysis results.
//!
//! ## Sources
//! * <https://www.saf.guide/> — official SAF documentation v2.2.0
//! * <https://github.com/StructuralAnalysisFormat> — GitHub organisation
//! * <https://community.osarch.org/discussion/252/structural-analysis-format-saf>

use std::collections::BTreeMap;

use glam::DVec3;
use itsjustcad_doc::{AreaKind, Document, FrameKind, Geometry, LoadGeometry, RestraintKind};
use kernel_mesh::StructSection as Section;

/// Disclaimer embedded in the workbook properties (`docProps/core.xml`).
const DISCLAIMER: &str = "SAF 2.2.0 structural model exported by ItsJustCAD — \
geometry + topology handoff only; contains no analysis results. \
ItsJustCAD does not perform structural analysis.";

// ============================================================================
// ZIP writer (store mode, no compression — text XML stays small enough)
// ============================================================================

/// Minimal ZIP archive writer emitting DEFLATE-method-0 (Stored) entries.
/// We hand-write the ZIP format to stay dependency-free; it is valid
/// ZIP 2.0 (all unzippers, Excel, LibreOffice and openpyxl handle it).
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
// xlsx (OOXML spreadsheet) writer
// ============================================================================

/// One SAF worksheet: the SAF-spec sheet name plus rows of cells (row 0 is
/// the header row).
struct Sheet {
    name: &'static str,
    rows: Vec<Vec<String>>,
}

impl Sheet {
    fn new(name: &'static str, header: &[&str]) -> Self {
        Self {
            name,
            rows: vec![header.iter().map(|s| s.to_string()).collect()],
        }
    }

    fn push(&mut self, cells: &[&str]) {
        self.rows.push(cells.iter().map(|s| s.to_string()).collect());
    }
}

/// Minimal XML text escaping for element content.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Spreadsheet column letter(s) for a 0-based column index (A, B, … Z, AA…).
fn col_letters(idx: usize) -> String {
    let mut n = idx;
    let mut s = String::new();
    loop {
        s.insert(0, (b'A' + (n % 26) as u8) as char);
        if n < 26 {
            break;
        }
        n = n / 26 - 1;
    }
    s
}

/// True when a cell value should be written as a native number cell.
/// (Node names like "N1", DOF strings like "Fixed" and multi-value fields
/// like "2;1" all fail the parse and stay inline strings.)
fn is_numeric(s: &str) -> bool {
    !s.is_empty() && s.parse::<f64>().is_ok_and(f64::is_finite)
}

/// Serialize one worksheet part (`xl/worksheets/sheetN.xml`).
fn sheet_xml(sheet: &Sheet) -> String {
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
         <worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">\
         <sheetData>",
    );
    for (ri, row) in sheet.rows.iter().enumerate() {
        let r = ri + 1;
        xml.push_str(&format!("<row r=\"{r}\">"));
        for (ci, cell) in row.iter().enumerate() {
            if cell.is_empty() {
                continue; // skip empty cells entirely
            }
            let cref = format!("{}{r}", col_letters(ci));
            if is_numeric(cell) {
                xml.push_str(&format!("<c r=\"{cref}\"><v>{cell}</v></c>"));
            } else {
                xml.push_str(&format!(
                    "<c r=\"{cref}\" t=\"inlineStr\"><is><t>{}</t></is></c>",
                    xml_escape(cell)
                ));
            }
        }
        xml.push_str("</row>");
    }
    xml.push_str("</sheetData></worksheet>");
    xml
}

/// Assemble the full .xlsx package from the SAF sheets.
fn write_xlsx(sheets: &[Sheet]) -> Vec<u8> {
    let mut zip = ZipWriter::new();

    // -- [Content_Types].xml -------------------------------------------------
    let mut ct = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
         <Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
         <Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
         <Default Extension=\"xml\" ContentType=\"application/xml\"/>\
         <Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>\
         <Override PartName=\"/docProps/core.xml\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/>",
    );
    for i in 1..=sheets.len() {
        ct.push_str(&format!(
            "<Override PartName=\"/xl/worksheets/sheet{i}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>"
        ));
    }
    ct.push_str("</Types>");
    zip.add_file("[Content_Types].xml", ct.as_bytes());

    // -- _rels/.rels ---------------------------------------------------------
    let rels = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
        <Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
        <Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/>\
        <Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties\" Target=\"docProps/core.xml\"/>\
        </Relationships>";
    zip.add_file("_rels/.rels", rels.as_bytes());

    // -- docProps/core.xml (carries the no-analysis disclaimer) --------------
    let core = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
         <cp:coreProperties xmlns:cp=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\" \
         xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\
         <dc:creator>ItsJustCAD</dc:creator>\
         <dc:description>{}</dc:description>\
         </cp:coreProperties>",
        xml_escape(DISCLAIMER)
    );
    zip.add_file("docProps/core.xml", core.as_bytes());

    // -- xl/workbook.xml -----------------------------------------------------
    let mut wb = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
         <workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" \
         xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">\
         <sheets>",
    );
    for (i, sheet) in sheets.iter().enumerate() {
        wb.push_str(&format!(
            "<sheet name=\"{}\" sheetId=\"{}\" r:id=\"rId{}\"/>",
            xml_escape(sheet.name),
            i + 1,
            i + 1
        ));
    }
    wb.push_str("</sheets></workbook>");
    zip.add_file("xl/workbook.xml", wb.as_bytes());

    // -- xl/_rels/workbook.xml.rels ------------------------------------------
    let mut wb_rels = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
         <Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
    );
    for i in 1..=sheets.len() {
        wb_rels.push_str(&format!(
            "<Relationship Id=\"rId{i}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet{i}.xml\"/>"
        ));
    }
    wb_rels.push_str("</Relationships>");
    zip.add_file("xl/_rels/workbook.xml.rels", wb_rels.as_bytes());

    // -- worksheets ----------------------------------------------------------
    for (i, sheet) in sheets.iter().enumerate() {
        let name = format!("xl/worksheets/sheet{}.xml", i + 1);
        zip.add_file(&name, sheet_xml(sheet).as_bytes());
    }

    zip.finish()
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
        Section::Timber { w, h } => format!("TIMBER_{:.0}x{:.0}", w * 1000.0, h * 1000.0),
        Section::Guadua { d, t } => format!("GUADUA_{:.0}x{:.0}", d * 1000.0, t * 1000.0),
    }
}

/// SAF cross-section type string (one of: "Parametric", "Manufactured", "General").
fn section_type(sec: &Section) -> &'static str {
    match sec {
        Section::Rectangular { .. }
        | Section::Circular { .. }
        | Section::Pipe { .. }
        | Section::IWideFlange { .. }
        | Section::Timber { .. }
        | Section::Guadua { .. } => "Parametric",
    }
}

/// SAF shape string for parametric sections.
fn section_shape(sec: &Section) -> &'static str {
    match sec {
        Section::Rectangular { .. } => "Rectangle",
        Section::Circular { .. } => "Circle",
        Section::Pipe { .. } => "Circle hollow",
        Section::IWideFlange { .. } => "I or H",
        Section::Timber { .. } => "Rectangle",
        Section::Guadua { .. } => "Circle hollow",
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
        Section::Timber { w, h } => format!("{:.1};{:.1}", w * 1000.0, h * 1000.0),
        Section::Guadua { d, t } => format!("{:.1};{:.1}", d * 1000.0, t * 1000.0),
    }
}

fn fmtf(v: f64) -> String {
    // SAF coordinates are in metres, 6 decimal places is sub-μm precision.
    format!("{v:.6}")
}

// ============================================================================
// Public export entry point
// ============================================================================

/// Export the document as a SAF 2.2.0 `.xlsx` workbook.
///
/// The returned `Vec<u8>` is a valid OOXML spreadsheet (ZIP container).
/// Sheet names and column headers match the SAF 2.2.0 spec exactly, so
/// RFEM / SCIA / AxisVM / FEM-Design (and openpyxl scripts feeding
/// ETABS / SAP2000 / Robot importers) can read the workbook directly.
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
    for sup in &doc.supports {
        let _ = nodes.intern(sup.position);
    }
    for load in &doc.loads {
        if let LoadGeometry::Point { position } = load.geometry {
            let _ = nodes.intern(position);
        }
    }

    // -----------------------------------------------------------------------
    // 2. Build the SAF sheets
    // -----------------------------------------------------------------------

    // -- StructuralPointConnection (nodes) -----------------------------------
    let mut node_sheet = Sheet::new(
        "StructuralPointConnection",
        &["Name", "Coordinate X [m]", "Coordinate Y [m]", "Coordinate Z [m]"],
    );
    for (p, name) in nodes.iter() {
        node_sheet.push(&[name, &fmtf(p.x), &fmtf(p.y), &fmtf(p.z)]);
    }
    let node_count = nodes.len();

    // -- StructuralMaterial --------------------------------------------------
    let mut mat_sheet = Sheet::new(
        "StructuralMaterial",
        &[
            "Name",
            "Type",
            "Subtype",
            "Quality",
            "Unit mass [kg/m3]",
            "E modulus [MPa]",
            "G modulus [MPa]",
            "Poisson Coefficient",
            "Thermal expansion [1/K]",
        ],
    );
    for (name, mat) in &mat_set {
        let e_mpa = mat.elastic_modulus_e / 1e6;
        // G = E / (2(1+ν)); assume ν = 0.3 if not stored (structural default).
        let nu = 0.3_f64;
        let g_mpa = e_mpa / (2.0 * (1.0 + nu));
        mat_sheet.push(&[
            name,
            "Other",
            "",
            "",
            &format!("{:.3}", mat.density),
            &format!("{e_mpa:.3}"),
            &format!("{g_mpa:.3}"),
            &format!("{nu:.3}"),
            "0.000012",
        ]);
    }

    // -- StructuralCrossSection ----------------------------------------------
    let mut sec_sheet = Sheet::new(
        "StructuralCrossSection",
        &["Name", "Material", "Cross-section type", "Shape", "Parameters [mm]", "A [m2]"],
    );
    for (name, sec) in &sec_set {
        // Try to find an associated material name from any frame that uses
        // this section name (best-effort: first match wins).
        let mat_name = frame_rows
            .iter()
            .find(|f| &f.section_name == name)
            .and_then(|f| f.material.clone())
            .unwrap_or_default();
        sec_sheet.push(&[
            name,
            &mat_name,
            section_type(sec),
            section_shape(sec),
            &section_params(sec),
            &format!("{:.8}", sec.area()),
        ]);
    }

    // -- StructuralCurveMember (1D frame members) ----------------------------
    let mut curve_sheet = Sheet::new(
        "StructuralCurveMember",
        &[
            "Name",
            "Type",
            "Cross section",
            "Nodes",
            "LCS",
            "LCS Rotation [deg]",
            "System line",
            "Behaviour in analysis",
            "Layer",
        ],
    );
    let member_count = frame_rows.len();
    for fr in &frame_rows {
        let type_str = match fr.kind {
            FrameKind::Beam => "Beam",
            FrameKind::Column => "Column",
        };
        let nodes_str = format!("{};{}", fr.node_a, fr.node_b);
        curve_sheet.push(&[
            &fr.name,
            type_str,
            &fr.section_name,
            &nodes_str,
            "ZAxis",
            "0",
            "Centre",
            "Standard",
            "",
        ]);
    }

    // -- StructuralSurfaceMember (2D area members) ---------------------------
    let mut surf_sheet = Sheet::new(
        "StructuralSurfaceMember",
        &[
            "Name",
            "Type",
            "Thickness [m]",
            "Nodes",
            "System plane",
            "Behaviour in analysis",
            "Layer",
        ],
    );
    for ar in &area_rows {
        let type_str = match ar.kind {
            AreaKind::Slab => "Plate",
            AreaKind::Wall => "Wall",
        };
        let nodes_str = ar.nodes.join(";");
        surf_sheet.push(&[
            &ar.name,
            type_str,
            &fmtf(ar.thickness),
            &nodes_str,
            "Top",
            "Standard",
            "",
        ]);
    }

    // -- StructuralPointSupport ----------------------------------------------
    let mut supp_sheet = Sheet::new(
        "StructuralPointSupport",
        &["Name", "Node", "Type", "ux", "uy", "uz", "fix", "fiy", "fiz"],
    );
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
        supp_sheet.push(&[&sname, &node_name, "Nodal", ux, uy, uz, fix, fiy, fiz]);
    }

    // -- StructuralLoadCase + loads ------------------------------------------
    // SAF keeps load cases and loads in separate sheets.  We create one load
    // case per unique load name, then reference it from the action rows.
    let mut lc_names: BTreeMap<String, usize> = BTreeMap::new();
    for load in &doc.loads {
        let idx = lc_names.len() + 1;
        lc_names.entry(load.name.clone()).or_insert(idx);
    }

    let mut lc_sheet =
        Sheet::new("StructuralLoadCase", &["Name", "Description", "Action type", "Load type"]);
    for name in lc_names.keys() {
        // Default: Permanent (dead load) — the user can change in the solver.
        lc_sheet.push(&[name, name, "Permanent", "Self weight"]);
    }

    let mut pt_sheet = Sheet::new(
        "StructuralPointAction",
        &["Name", "Load case", "Node", "Direction", "Value [kN]"],
    );

    let mut ln_sheet = Sheet::new(
        "StructuralCurveAction",
        &[
            "Name",
            "Load case",
            "Node 1",
            "Node 2",
            "Direction",
            "Value [kN/m]",
            "Value 2 [kN/m]",
        ],
    );

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
                let nname = nodes.map.get(&k).cloned().unwrap_or_else(|| format!("N?{i}"));
                pt_sheet.push(&[&act_name, lc, &nname, &dir, &format!("{val_kn:.4}")]);
            }
            LoadGeometry::Line { a, b } => {
                ln_action_count += 1;
                let act_name = format!("LA{}", i + 1);
                let ka = NodeTable::key(*a);
                let kb = NodeTable::key(*b);
                let na = nodes.map.get(&ka).cloned().unwrap_or_else(|| format!("N?{i}a"));
                let nb = nodes.map.get(&kb).cloned().unwrap_or_else(|| format!("N?{i}b"));
                // Uniform line load: value1 == value2 == magnitude (N/m → kN/m).
                ln_sheet.push(&[
                    &act_name,
                    lc,
                    &na,
                    &nb,
                    &dir,
                    &format!("{val_kn:.4}"),
                    &format!("{val_kn:.4}"),
                ]);
            }
            LoadGeometry::Area { .. } => {
                // Surface loads would go in StructuralSurfaceAction; scope-cut:
                // area loads reference a 2D member ID which we don't track here.
                // Noted: area loads are omitted from the SAF export.
            }
        }
    }

    // -----------------------------------------------------------------------
    // 3. Pack into a genuine .xlsx workbook
    // -----------------------------------------------------------------------

    let sheets = [
        node_sheet, mat_sheet, sec_sheet, curve_sheet, surf_sheet, supp_sheet, lc_sheet,
        pt_sheet, ln_sheet,
    ];
    let bytes = write_xlsx(&sheets);

    let summary = format!(
        "SAF 2.2.0 xlsx (no analysis results — handoff only), {node_count} nodes, {member_count} members, {} sections, {} materials, {} supports, {} load cases, {} point loads, {} line loads",
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

    /// Minimal store-mode unzip: walks local file headers sequentially and
    /// returns entry name → bytes. Only supports what our ZipWriter emits
    /// (method 0, no data descriptors) — that is the point: it round-trips
    /// the writer.
    fn unzip(bytes: &[u8]) -> std::collections::BTreeMap<String, Vec<u8>> {
        let mut out = std::collections::BTreeMap::new();
        let mut off = 0usize;
        while off + 30 <= bytes.len() && bytes[off..off + 4] == [0x50, 0x4B, 0x03, 0x04] {
            let method = u16::from_le_bytes([bytes[off + 8], bytes[off + 9]]);
            assert_eq!(method, 0, "entries must be stored (method 0)");
            let size = u32::from_le_bytes([
                bytes[off + 18],
                bytes[off + 19],
                bytes[off + 20],
                bytes[off + 21],
            ]) as usize;
            let name_len =
                u16::from_le_bytes([bytes[off + 26], bytes[off + 27]]) as usize;
            let extra_len =
                u16::from_le_bytes([bytes[off + 28], bytes[off + 29]]) as usize;
            let name_start = off + 30;
            let name =
                String::from_utf8(bytes[name_start..name_start + name_len].to_vec()).unwrap();
            let data_start = name_start + name_len + extra_len;
            out.insert(name, bytes[data_start..data_start + size].to_vec());
            off = data_start + size;
        }
        out
    }

    /// Extract the worksheet XML for a SAF sheet name (resolves the sheet
    /// index through xl/workbook.xml, like a real xlsx reader would).
    fn sheet_by_name(parts: &std::collections::BTreeMap<String, Vec<u8>>, name: &str) -> String {
        let wb = String::from_utf8(parts["xl/workbook.xml"].clone()).unwrap();
        // <sheet name="X" sheetId="N" r:id="rIdN"/> — sheetId == part index.
        let tag = format!("<sheet name=\"{name}\" sheetId=\"");
        let pos = wb.find(&tag).unwrap_or_else(|| panic!("sheet '{name}' in workbook.xml"));
        let rest = &wb[pos + tag.len()..];
        let id: usize = rest[..rest.find('"').unwrap()].parse().unwrap();
        String::from_utf8(parts[&format!("xl/worksheets/sheet{id}.xml")].clone()).unwrap()
    }

    /// Build a minimal two-node frame: column at (0,0,0)→(0,0,3) and beam
    /// (0,0,3)→(5,0,3), plus a pinned support and a point load.
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
            material: None,
            lineweight_mm: None,
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
            material: None,
            lineweight_mm: None,
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
        // EOCD record present
        assert!(
            bytes.windows(4).any(|w| w == [0x50, 0x4B, 0x05, 0x06]),
            "ZIP end-of-central-directory record"
        );
        // Summary mentions tables
        assert!(summary.contains("nodes"), "summary has node count: {summary}");
        assert!(summary.contains("members"), "summary has member count: {summary}");
    }

    #[test]
    fn xlsx_package_parts_present() {
        let doc = frame_doc();
        let (bytes, _) = export(&doc).expect("export ok");
        let parts = unzip(&bytes);
        for name in &[
            "[Content_Types].xml",
            "_rels/.rels",
            "docProps/core.xml",
            "xl/workbook.xml",
            "xl/_rels/workbook.xml.rels",
            "xl/worksheets/sheet1.xml",
            "xl/worksheets/sheet9.xml",
        ] {
            assert!(parts.contains_key(*name), "xlsx missing part '{name}'");
        }
        // Content types must declare each worksheet part.
        let ct = String::from_utf8(parts["[Content_Types].xml"].clone()).unwrap();
        for i in 1..=9 {
            assert!(
                ct.contains(&format!("/xl/worksheets/sheet{i}.xml")),
                "content types missing sheet{i}"
            );
        }
    }

    #[test]
    fn workbook_lists_all_saf_sheets() {
        let doc = frame_doc();
        let (bytes, _) = export(&doc).expect("export ok");
        let parts = unzip(&bytes);
        let wb = String::from_utf8(parts["xl/workbook.xml"].clone()).unwrap();
        for name in &[
            "StructuralPointConnection",
            "StructuralMaterial",
            "StructuralCrossSection",
            "StructuralCurveMember",
            "StructuralSurfaceMember",
            "StructuralPointSupport",
            "StructuralLoadCase",
            "StructuralPointAction",
            "StructuralCurveAction",
        ] {
            assert!(wb.contains(&format!("name=\"{name}\"")), "workbook missing sheet '{name}'");
        }
    }

    #[test]
    fn node_sheet_has_three_numeric_nodes() {
        // Column: (0,0,0)→(0,0,3); Beam: (0,0,3)→(5,0,3); support at (0,0,0).
        // Unique nodes: (0,0,0), (0,0,3), (5,0,3) → 3.
        let doc = frame_doc();
        let (bytes, summary) = export(&doc).expect("export ok");
        assert!(summary.contains("3 nodes"), "expected 3 nodes in summary: {summary}");

        let parts = unzip(&bytes);
        let xml = sheet_by_name(&parts, "StructuralPointConnection");
        // Header row + 3 data rows
        assert_eq!(xml.matches("<row r=").count(), 4, "1 header + 3 node rows: {xml}");
        // Coordinates written as native number cells, not strings
        assert!(xml.contains("<v>3.000000</v>"), "Z=3.0 as numeric cell");
        assert!(xml.contains("<v>5.000000</v>"), "X=5.0 as numeric cell");
        // Node names as inline strings
        assert!(xml.contains("<is><t>N1</t></is>"), "node name N1 inline string");
    }

    #[test]
    fn member_sheet_has_two_rows_with_node_refs() {
        let doc = frame_doc();
        let (bytes, summary) = export(&doc).expect("export ok");
        assert!(summary.contains("2 members"), "expected 2 members: {summary}");
        let parts = unzip(&bytes);
        let xml = sheet_by_name(&parts, "StructuralCurveMember");
        assert_eq!(xml.matches("<row r=").count(), 3, "1 header + 2 member rows");
        assert!(xml.contains("<is><t>COL1</t></is>"), "column name");
        assert!(xml.contains("<is><t>BEAM1</t></is>"), "beam name");
        assert!(xml.contains("<is><t>Column</t></is>"), "SAF type Column");
        assert!(xml.contains("<is><t>Beam</t></is>"), "SAF type Beam");
        // Node references "Na;Nb"
        assert!(xml.contains(";N"), "member row references nodes: {xml}");
    }

    #[test]
    fn section_sheet_has_correct_entry() {
        let doc = frame_doc();
        let (bytes, _) = export(&doc).expect("export ok");
        let parts = unzip(&bytes);
        let xml = sheet_by_name(&parts, "StructuralCrossSection");
        // The section name RECT_300x300 must appear (300mm x 300mm).
        assert!(xml.contains("RECT_300x300"), "section name in sheet");
        assert!(xml.contains("Rectangle"), "shape label in sheet");
        assert!(xml.contains("300.0;300.0"), "parameters b;h in mm");
    }

    #[test]
    fn material_sheet_has_numeric_properties() {
        let doc = frame_doc();
        let (bytes, _) = export(&doc).expect("export ok");
        let parts = unzip(&bytes);
        let xml = sheet_by_name(&parts, "StructuralMaterial");
        assert!(xml.contains("<is><t>C30</t></is>"), "material name");
        // E = 30e9 Pa = 30000 MPa, numeric cell
        assert!(xml.contains("<v>30000.000</v>"), "E modulus in MPa as number: {xml}");
        assert!(xml.contains("<v>2400.000</v>"), "density as number");
    }

    #[test]
    fn support_sheet_pinned_dofs() {
        let doc = frame_doc();
        let (bytes, _) = export(&doc).expect("export ok");
        let parts = unzip(&bytes);
        let xml = sheet_by_name(&parts, "StructuralPointSupport");
        // Pinned: ux=Fixed, uy=Fixed, uz=Fixed, fix=Free, fiy=Free, fiz=Free
        let fixed = xml.matches("<is><t>Fixed</t></is>").count();
        let free = xml.matches("<is><t>Free</t></is>").count();
        assert_eq!((fixed, free), (3, 3), "pinned DOF pattern: {xml}");
    }

    #[test]
    fn load_case_and_point_action_exported() {
        let doc = frame_doc();
        let (bytes, summary) = export(&doc).expect("export ok");
        let parts = unzip(&bytes);
        let lc = sheet_by_name(&parts, "StructuralLoadCase");
        assert!(lc.contains("<is><t>DL</t></is>"), "load case name");
        let pa = sheet_by_name(&parts, "StructuralPointAction");
        // 50 kN = 50.0000, numeric; direction -Z as string
        assert!(pa.contains("<v>50.0000</v>"), "50 kN point load value: {pa}");
        assert!(pa.contains("<is><t>-Z</t></is>"), "direction -Z");
        assert!(summary.contains("1 load cases"), "load case count: {summary}");
        assert!(summary.contains("1 point loads"), "point load count: {summary}");
    }

    #[test]
    fn disclaimer_no_analysis_results() {
        let doc = frame_doc();
        let (bytes, summary) = export(&doc).expect("export ok");
        let parts = unzip(&bytes);
        let core = String::from_utf8(parts["docProps/core.xml"].clone()).unwrap();
        assert!(core.contains("no analysis results"), "disclaimer in docProps: {core}");
        assert!(core.contains("ItsJustCAD"), "creator in docProps");
        assert!(summary.contains("no analysis results"), "disclaimer in summary: {summary}");
    }

    #[test]
    fn crc_matches_per_entry() {
        // The unzip helper trusts sizes; verify the recorded CRC of each part
        // matches a recompute (catches writer offset bugs).
        let doc = frame_doc();
        let (bytes, _) = export(&doc).expect("export ok");
        let parts = unzip(&bytes);
        let mut off = 0usize;
        while off + 30 <= bytes.len() && bytes[off..off + 4] == [0x50, 0x4B, 0x03, 0x04] {
            let crc = u32::from_le_bytes([
                bytes[off + 14],
                bytes[off + 15],
                bytes[off + 16],
                bytes[off + 17],
            ]);
            let size = u32::from_le_bytes([
                bytes[off + 18],
                bytes[off + 19],
                bytes[off + 20],
                bytes[off + 21],
            ]) as usize;
            let name_len = u16::from_le_bytes([bytes[off + 26], bytes[off + 27]]) as usize;
            let extra_len = u16::from_le_bytes([bytes[off + 28], bytes[off + 29]]) as usize;
            let data_start = off + 30 + name_len + extra_len;
            assert_eq!(crc, crc32(&bytes[data_start..data_start + size]), "entry CRC");
            off = data_start + size;
        }
        assert_eq!(parts.len(), 14, "5 package parts + 9 worksheets");
    }

    #[test]
    fn col_letters_progression() {
        assert_eq!(col_letters(0), "A");
        assert_eq!(col_letters(8), "I");
        assert_eq!(col_letters(25), "Z");
        assert_eq!(col_letters(26), "AA");
    }

    #[test]
    fn xml_escaping_in_names() {
        let mut doc = frame_doc();
        // Rename the beam to something needing escaping.
        for obj in doc.objects_mut() {
            if let Geometry::Frame { kind: FrameKind::Beam, .. } = obj.geometry {
                obj.name = Some("B<1> & \"main\"".to_string());
            }
        }
        let (bytes, _) = export(&doc).expect("export ok");
        let parts = unzip(&bytes);
        let xml = sheet_by_name(&parts, "StructuralCurveMember");
        assert!(
            xml.contains("B&lt;1&gt; &amp; &quot;main&quot;"),
            "special chars escaped: {xml}"
        );
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
