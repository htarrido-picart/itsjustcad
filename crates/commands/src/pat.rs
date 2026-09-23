// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! AutoCAD `.pat` hatch-pattern parser.
//!
//! The `.pat` grammar is line-oriented text:
//!
//! ```text
//! ;  a comment line (also blank lines) — ignored
//! *NAME, optional description
//! angle, x-origin, y-origin, delta-x, delta-y [, dash1, dash2, …]
//! angle, x-origin, y-origin, delta-x, delta-y [, dash1, dash2, …]
//! *NEXTNAME, …
//! …
//! ```
//!
//! Each `*NAME` header starts a new pattern; the family-definition lines that
//! follow (until the next header or EOF) are its parallel dashed line-families.
//! A file may hold many patterns. We parse into `(name, HatchPatternDef)` pairs
//! and let the caller register them. Geometry is emitted by
//! [`itsjustcad_doc::hatch::hatch_pat`]; this module is purely the reader.

use glam::DVec2;
use itsjustcad_doc::{HatchPatternDef, PatLine};

/// Parse the text of a `.pat` file into named patterns, in file order.
///
/// Tolerant of the real-world quirks: leading whitespace, `;` comments, blank
/// lines, and family lines with only the 5 required numbers (dashes optional).
/// A `*header` with no following family lines still yields an (empty) pattern —
/// AutoCAD does the same. Returns an error string only on a malformed family
/// line (fewer than 5 numeric fields, or a non-numeric field).
pub fn parse_pat(text: &str) -> Result<Vec<(String, HatchPatternDef)>, String> {
    let mut patterns: Vec<(String, HatchPatternDef)> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with(';') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('*') {
            // `*NAME` or `*NAME, description`. The name runs to the first comma.
            let (name, desc) = match rest.split_once(',') {
                Some((n, d)) => (n.trim(), d.trim()),
                None => (rest.trim(), ""),
            };
            if name.is_empty() {
                return Err("pattern header '*' has no name".into());
            }
            patterns.push((
                name.to_string(),
                HatchPatternDef { description: desc.to_string(), lines: Vec::new() },
            ));
            continue;
        }
        // A family-definition line belongs to the current (last) pattern.
        let Some((_, def)) = patterns.last_mut() else {
            return Err(format!("family line before any '*name' header: {line}"));
        };
        def.lines.push(parse_family(line)?);
    }
    Ok(patterns)
}

/// Parse one `angle, x, y, dx, dy [, d1, d2, …]` family line.
fn parse_family(line: &str) -> Result<PatLine, String> {
    let nums: Vec<f64> = line
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<f64>().map_err(|_| format!("non-numeric field '{s}' in: {line}")))
        .collect::<Result<_, _>>()?;
    if nums.len() < 5 {
        return Err(format!("family line needs angle,x,y,dx,dy (got {} fields): {line}", nums.len()));
    }
    Ok(PatLine {
        angle_deg: nums[0],
        origin: DVec2::new(nums[1], nums[2]),
        delta: DVec2::new(nums[3], nums[4]),
        dashes: nums[5..].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // A small but real-shaped snippet: two patterns, comments, and a family
    // with a dash pen-pattern.
    const SAMPLE: &str = "\
; sample.pat — test fixture
*ANSI31, ANSI Iron / general use
45, 0,0, 0,3.175

*DASHED, dashed 45deg with gaps
45, 0,0, 0,4, 6.35,-6.35
90, 0,0, 0,4
";

    #[test]
    fn parses_two_patterns_with_families() {
        let pats = parse_pat(SAMPLE).unwrap();
        assert_eq!(pats.len(), 2, "two headers → two patterns");

        let (n0, d0) = &pats[0];
        assert_eq!(n0, "ANSI31");
        assert_eq!(d0.description, "ANSI Iron / general use");
        assert_eq!(d0.lines.len(), 1);
        let fam = &d0.lines[0];
        assert_eq!(fam.angle_deg, 45.0);
        assert_eq!(fam.delta.y, 3.175, "perpendicular spacing");
        assert!(fam.dashes.is_empty(), "solid line has no dashes");

        let (n1, d1) = &pats[1];
        assert_eq!(n1, "DASHED");
        assert_eq!(d1.lines.len(), 2, "two family lines");
        assert_eq!(d1.lines[0].dashes, vec![6.35, -6.35], "dash then gap");
        assert_eq!(d1.lines[1].angle_deg, 90.0);
    }

    #[test]
    fn comments_and_blank_lines_ignored() {
        let pats = parse_pat("\n; only comments\n\n; here\n").unwrap();
        assert!(pats.is_empty());
    }

    #[test]
    fn header_without_description_ok() {
        let pats = parse_pat("*SOLIDISH\n0,0,0,0,1\n").unwrap();
        assert_eq!(pats.len(), 1);
        assert_eq!(pats[0].0, "SOLIDISH");
        assert_eq!(pats[0].1.description, "");
    }

    #[test]
    fn family_before_header_errors() {
        let err = parse_pat("45,0,0,0,1\n").unwrap_err();
        assert!(err.contains("before any"), "{err}");
    }

    #[test]
    fn short_family_line_errors() {
        let err = parse_pat("*P\n45,0,0\n").unwrap_err();
        assert!(err.contains("needs angle"), "{err}");
    }
}
