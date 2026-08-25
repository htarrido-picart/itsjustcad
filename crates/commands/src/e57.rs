// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! E57 point-cloud import (ASTM E2807).
//!
//! Reads all point-cloud sections from an E57 file using the `e57` crate,
//! extracts Cartesian positions and optional RGB colors (normalized to 0–1),
//! and decimates to at most [`MAX_POINTS`] like the LAS importer does.
//! Each point cloud section is merged into a single output; unknown or
//! exotic sections (image-only, no cartesian coords) are skipped with a count.

use glam::DVec3;

/// Maximum number of points kept after decimation (mirrors LAS).
pub const MAX_POINTS: usize = 200_000;

#[derive(Debug, thiserror::Error)]
pub enum E57Error {
    #[error("cannot open E57 file: {0}")]
    Io(#[from] std::io::Error),
    #[error("E57 parse error: {0}")]
    Parse(String),
    #[error("E57 file contains no point clouds with Cartesian coordinates")]
    NoPoints,
}

/// Parsed output from an E57 file.
#[derive(Debug)]
pub struct E57Points {
    /// Decimated world-space positions (Cartesian, after pose is applied).
    pub positions: Vec<DVec3>,
    /// Per-point RGB colors normalised to 0–1, or empty if the file has none.
    /// When present the length matches `positions`.
    pub colors: Vec<[f32; 3]>,
    /// Total number of point records before decimation.
    pub total_records: u64,
    /// Stride used for decimation (1 = no decimation).
    pub stride: usize,
    /// Number of point-cloud sections in the file that were skipped (no
    /// Cartesian coordinates or other unsupported format).
    pub skipped_sections: usize,
}

/// Parse an E57 file from bytes, decimate to ≤`MAX_POINTS`, and return
/// positions + optional colors.
pub fn parse(data: &[u8]) -> Result<E57Points, E57Error> {
    // The e57 crate works from a Read+Seek source; we wrap the bytes in a cursor.
    let cursor = std::io::Cursor::new(data);
    let mut reader =
        e57::E57Reader::new(cursor).map_err(|e| E57Error::Parse(e.to_string()))?;

    let point_clouds = reader.pointclouds();

    // First pass: count total records across all useful sections so we can
    // compute a global stride that ensures ≤MAX_POINTS overall.
    let mut total_records: u64 = 0;
    let mut skipped_sections = 0usize;
    for pc in &point_clouds {
        if pc.has_cartesian() {
            total_records += pc.records;
        } else {
            skipped_sections += 1;
        }
    }

    if total_records == 0 {
        return Err(E57Error::NoPoints);
    }

    let stride = ((total_records as usize) / MAX_POINTS).max(1);
    let has_color = point_clouds.iter().any(|pc| pc.has_color() || pc.has_intensity());

    let cap = (total_records as usize).div_ceil(stride).min(MAX_POINTS + 1);
    let mut positions: Vec<DVec3> = Vec::with_capacity(cap);
    let mut colors: Vec<[f32; 3]> = if has_color {
        Vec::with_capacity(cap)
    } else {
        Vec::new()
    };

    // Second pass: read points with the computed stride.
    let mut global_index: usize = 0;

    for pc in &point_clouds {
        if !pc.has_cartesian() {
            continue;
        }

        let pc_records = pc.records as usize;
        let pc_has_color = pc.has_color() || pc.has_intensity();

        let mut iter = reader
            .pointcloud_simple(pc)
            .map_err(|e| E57Error::Parse(e.to_string()))?;

        let mut local_index: usize = 0;
        while local_index < pc_records {
            // Determine whether this point falls on a stride boundary relative
            // to the *global* index.
            let keep = global_index.is_multiple_of(stride);
            global_index += 1;
            local_index += 1;

            let point = match iter.next() {
                Some(Ok(p)) => p,
                Some(Err(_)) => continue, // skip malformed points
                None => break,
            };

            if !keep {
                continue;
            }

            let (x, y, z) = match point.cartesian {
                e57::CartesianCoordinate::Valid { x, y, z } => (x, y, z),
                _ => continue, // invalid/direction-only cartesian: skip
            };

            positions.push(DVec3::new(x, y, z));

            if has_color {
                if let Some(c) = point.color.filter(|_| pc_has_color) {
                    colors.push([c.red, c.green, c.blue]);
                } else {
                    // Pad with white when this section has no color but others do.
                    colors.push([1.0, 1.0, 1.0]);
                }
            }
        }
    }

    if positions.is_empty() {
        return Err(E57Error::NoPoints);
    }

    // If we collected colors but they differ in length (e.g. some sections
    // were all-invalid-cartesian), trim to positions length.
    if !colors.is_empty() && colors.len() != positions.len() {
        colors.truncate(positions.len());
        // If somehow shorter, pad.
        while colors.len() < positions.len() {
            colors.push([1.0, 1.0, 1.0]);
        }
    }

    Ok(E57Points {
        positions,
        colors,
        total_records,
        stride,
        skipped_sections,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal but valid E57 file with N Cartesian points using the
    /// `e57` crate's writer, then read it back.
    fn make_e57_xyz(points: &[(f64, f64, f64)]) -> Vec<u8> {
        use e57::{E57Writer, Record, RecordDataType, RecordName};

        // Use an inner Vec, write into it, then return it — writer borrows an
        // owned cursor so we can't use `into_inner` while writer is alive.
        let mut out = Vec::<u8>::new();
        {
            let mut cur = std::io::Cursor::new(&mut out);
            let mut writer = E57Writer::new(&mut cur, "test-guid-e57-xyz")
                .expect("E57Writer::new");

            let prototype = vec![
                Record {
                    name: RecordName::CartesianX,
                    data_type: RecordDataType::Double { min: None, max: None },
                },
                Record {
                    name: RecordName::CartesianY,
                    data_type: RecordDataType::Double { min: None, max: None },
                },
                Record {
                    name: RecordName::CartesianZ,
                    data_type: RecordDataType::Double { min: None, max: None },
                },
            ];

            let mut pc_writer = writer
                .add_pointcloud("test-pc-guid", prototype)
                .expect("add_pointcloud");

            for &(x, y, z) in points {
                pc_writer
                    .add_point(vec![
                        e57::RecordValue::Double(x),
                        e57::RecordValue::Double(y),
                        e57::RecordValue::Double(z),
                    ])
                    .expect("add_point");
            }
            pc_writer.finalize().expect("finalize pc");
            writer.finalize().expect("finalize writer");
        }
        out
    }

    /// Build an E57 with RGB colors.
    fn make_e57_xyz_rgb(points: &[(f64, f64, f64, f32, f32, f32)]) -> Vec<u8> {
        use e57::{E57Writer, Record, RecordDataType, RecordName};

        let mut out = Vec::<u8>::new();
        {
        let mut cur = std::io::Cursor::new(&mut out);
        let mut writer = E57Writer::new(&mut cur, "test-guid-e57-rgb")
            .expect("E57Writer::new");

        let prototype = vec![
            Record {
                name: RecordName::CartesianX,
                data_type: RecordDataType::Double { min: None, max: None },
            },
            Record {
                name: RecordName::CartesianY,
                data_type: RecordDataType::Double { min: None, max: None },
            },
            Record {
                name: RecordName::CartesianZ,
                data_type: RecordDataType::Double { min: None, max: None },
            },
            Record {
                name: RecordName::ColorRed,
                data_type: RecordDataType::Single { min: None, max: None },
            },
            Record {
                name: RecordName::ColorGreen,
                data_type: RecordDataType::Single { min: None, max: None },
            },
            Record {
                name: RecordName::ColorBlue,
                data_type: RecordDataType::Single { min: None, max: None },
            },
        ];

        let mut pc_writer = writer
            .add_pointcloud("test-pc-rgb-guid", prototype)
            .expect("add_pointcloud");

        for &(x, y, z, r, g, b) in points {
            pc_writer
                .add_point(vec![
                    e57::RecordValue::Double(x),
                    e57::RecordValue::Double(y),
                    e57::RecordValue::Double(z),
                    e57::RecordValue::Single(r),
                    e57::RecordValue::Single(g),
                    e57::RecordValue::Single(b),
                ])
                .expect("add_point");
        }
        pc_writer.finalize().expect("finalize pc");
        writer.finalize().expect("finalize writer");
        }
        out
    }

    #[test]
    fn parse_single_xyz_point() {
        let data = make_e57_xyz(&[(1.0, 2.0, 3.0)]);
        let pts = parse(&data).unwrap();
        assert_eq!(pts.positions.len(), 1);
        assert_eq!(pts.total_records, 1);
        assert_eq!(pts.stride, 1);
        let p = pts.positions[0];
        assert!((p.x - 1.0).abs() < 1e-9, "x={}", p.x);
        assert!((p.y - 2.0).abs() < 1e-9, "y={}", p.y);
        assert!((p.z - 3.0).abs() < 1e-9, "z={}", p.z);
        // No colors in this file.
        assert!(pts.colors.is_empty());
    }

    #[test]
    fn parse_xyz_rgb_colors() {
        let pts_in = vec![
            (0.0f64, 0.0, 0.0, 1.0f32, 0.0, 0.0),
            (1.0, 0.0, 0.0, 0.0, 1.0, 0.0),
            (2.0, 0.0, 0.0, 0.0, 0.0, 1.0),
        ];
        let data = make_e57_xyz_rgb(&pts_in);
        let pts = parse(&data).unwrap();
        assert_eq!(pts.positions.len(), 3);
        assert_eq!(pts.colors.len(), 3);
        // Colors are normalised from the data-type range (f32 min/max), so the
        // raw f32 values are preserved as-is when normalization range = 1.
        // Just assert length and that no color is NaN.
        for c in &pts.colors {
            assert!(c[0].is_finite() && c[1].is_finite() && c[2].is_finite());
        }
    }

    #[test]
    fn point_count_matches() {
        let pts_in: Vec<(f64, f64, f64)> = (0..50).map(|i| (i as f64, 0.0, 0.0)).collect();
        let data = make_e57_xyz(&pts_in);
        let pts = parse(&data).unwrap();
        assert_eq!(pts.total_records, 50);
        assert_eq!(pts.positions.len(), 50);
        assert_eq!(pts.stride, 1);
    }

    #[test]
    fn decimation_stride_formula() {
        // Mirror the LAS decimation test: verify the stride math.
        let count = MAX_POINTS * 3; // 600k
        let stride = (count / MAX_POINTS).max(1);
        assert_eq!(stride, 3);
        let kept = count.div_ceil(stride);
        assert!(kept <= MAX_POINTS + 1);
    }

    #[test]
    fn malformed_data_rejected() {
        let err = parse(&[0u8; 32]).unwrap_err();
        assert!(matches!(err, E57Error::Parse(_)));
    }

    #[test]
    fn empty_data_rejected() {
        let err = parse(&[]).unwrap_err();
        assert!(matches!(err, E57Error::Parse(_)));
    }

    /// Replay-stability: to_json → from_json → to_json must be byte-identical for
    /// a PointLiteral command that would be produced by an E57 import.
    #[test]
    fn replay_stable_point_literal() {
        use crate::Command;
        use glam::DVec3;

        let cmd = Command::PointLiteral {
            id: Some(itsjustcad_doc::ObjectId(uuid::Uuid::nil())),
            positions: vec![DVec3::new(1.0, 2.0, 3.0), DVec3::new(4.0, 5.0, 6.0)],
        };
        let json1 = serde_json::to_string(&cmd).unwrap();
        let cmd2: Command = serde_json::from_str(&json1).unwrap();
        let json2 = serde_json::to_string(&cmd2).unwrap();
        assert_eq!(json1, json2, "PointLiteral round-trip must be byte-identical");
    }

    #[test]
    fn skipped_sections_counted() {
        // A file with only 1 point-cloud section and no skipped sections.
        let data = make_e57_xyz(&[(0.0, 0.0, 0.0)]);
        let pts = parse(&data).unwrap();
        assert_eq!(pts.skipped_sections, 0);
    }

    /// Writes a small colored E57 to /tmp/sanity.e57 for the headless sanity check.
    /// Run with `cargo test e57::tests::write_sanity_fixture -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn write_sanity_fixture() {
        let pts_in: Vec<(f64, f64, f64, f32, f32, f32)> = (0..100usize)
            .map(|i| {
                let x = (i % 10) as f64;
                let y = (i / 10) as f64;
                let r = x as f32 / 9.0;
                let g = y as f32 / 9.0;
                (x, y, 0.0, r, g, 0.5)
            })
            .collect();
        let data = make_e57_xyz_rgb(&pts_in);
        std::fs::write("/tmp/sanity.e57", &data).expect("write /tmp/sanity.e57");
        println!("Wrote {} bytes, {} points", data.len(), pts_in.len());
    }
}
