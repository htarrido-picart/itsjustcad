// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! CSV export: one row per scene object, RFC 4180-compliant quoting.
//!
//! Header: name,id,layer,type,area,volume
//! Area is in m², volume in m³ (raw SI; no unit conversion).

use itsjustcad_doc::Document;

use crate::exec::build_schedule_rows;

/// RFC 4180: quote a field if it contains commas, double-quotes, or newlines;
/// double internal double-quotes.
fn quote(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        let escaped = s.replace('"', "\"\"");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

/// Build the CSV bytes for all objects in the document.
/// Returns the CSV bytes and a short summary for the command echo.
pub fn export_csv(doc: &Document) -> (Vec<u8>, String) {
    let rows = build_schedule_rows(doc, None);
    let mut out = String::new();

    // Header row.
    out.push_str("name,id,layer,type,area,volume\r\n");

    for r in &rows {
        let cols: [String; 6] = [
            quote(&r.name),
            quote(&r.id),
            quote(&r.layer),
            quote(&r.kind),
            format!("{:.6}", r.area_m2),
            format!("{:.6}", r.volume_m3),
        ];
        out.push_str(&cols.join(","));
        out.push_str("\r\n");
    }

    let summary = format!("{} rows", rows.len());
    (out.into_bytes(), summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse, Session};

    #[test]
    fn csv_header_and_row_count() {
        let mut s = Session::default();
        s.run(parse("box 0,0,0 1,1,1").unwrap()).unwrap();
        s.run(parse("line 0,0,0 5,0,0").unwrap()).unwrap();

        let (bytes, summary) = export_csv(&s.doc);
        let csv = String::from_utf8(bytes).unwrap();

        // Header must be first line.
        let first = csv.lines().next().unwrap();
        assert_eq!(first, "name,id,layer,type,area,volume");
        // Two data rows for two objects.
        assert_eq!(csv.lines().count(), 3, "header + 2 data rows");
        assert!(summary.contains("2 rows"), "{summary}");
    }

    #[test]
    fn csv_uses_crlf_line_endings() {
        let mut s = Session::default();
        s.run(parse("box 0,0,0 1,1,1").unwrap()).unwrap();
        let (bytes, _) = export_csv(&s.doc);
        assert!(bytes.contains(&b'\r'), "should use CRLF per RFC 4180");
    }

    #[test]
    fn csv_quoting_for_commas_and_quotes() {
        assert_eq!(quote("hello"), "hello");
        assert_eq!(quote("a,b"), "\"a,b\"");
        assert_eq!(quote("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(quote("line\nnew"), "\"line\nnew\"");
    }

    #[test]
    fn csv_empty_doc_has_only_header() {
        let doc = Document::default();
        let (bytes, summary) = export_csv(&doc);
        let csv = String::from_utf8(bytes).unwrap();
        assert!(csv.starts_with("name,id,layer,type,area,volume"), "header present");
        assert_eq!(csv.lines().count(), 1, "header only");
        assert!(summary.contains("0 rows"), "{summary}");
    }

    #[test]
    fn csv_known_doc_exact_output() {
        // Fixed object ids are not predictable, so we check structure/values,
        // not verbatim output.
        let mut s = Session::default();
        s.run(parse("box 0,0,0 2,3,4").unwrap()).unwrap();
        let (bytes, _) = export_csv(&s.doc);
        let csv = String::from_utf8(bytes).unwrap();
        let mut lines = csv.lines();
        let _hdr = lines.next().unwrap();
        let data = lines.next().unwrap();
        let cols: Vec<&str> = data.split(',').collect();
        assert_eq!(cols.len(), 6, "6 columns");
        // type = mesh
        assert_eq!(cols[3], "mesh");
        // volume ≈ 24 m³
        let vol: f64 = cols[5].parse().unwrap();
        assert!((vol.abs() - 24.0).abs() < 0.01, "volume ≈ 24, got {vol}");
    }
}
