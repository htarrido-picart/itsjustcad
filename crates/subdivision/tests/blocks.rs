// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! §8 validation cases for recursive OBB subdivision (Phase 3), run against the
//! first 10 sample blocks in `samples/blocks/*.json`. (#11 annular / #12 hex /
//! #13 Voronoi are Phase 5b and intentionally absent.)
//!
//! Assertions per the plan §8:
//! - Σ lot area == block area within tolerance (conservation).
//! - No overlapping lots (pairwise-disjoint interiors).
//! - No gaps (covered by conservation + disjointness on a partition).
//! - Every lot has a street edge when `force_street_access == 1.0`.
//! - Lot widths ≥ `lot_width_min` where the method guarantees it.
//! - Deterministic for a fixed seed (byte-identical on re-run).

use glam::DVec2;
use std::path::PathBuf;
use subdivision::{subdivide, OrientedBox, Polygon2d, SampleBlock, SubdivisionSettings};

fn blocks_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("samples/blocks")
}

fn load_all() -> Vec<(String, Polygon2d)> {
    let mut out = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(blocks_dir())
        .expect("samples/blocks dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    entries.sort();
    for p in entries {
        let txt = std::fs::read_to_string(&p).unwrap();
        let sb = SampleBlock::from_json(&txt).unwrap();
        let poly = sb.polygon().unwrap_or_else(|| panic!("bad polygon in {}", sb.name));
        out.push((sb.name, poly));
    }
    out
}

/// Settings sized so every block actually subdivides into several lots.
fn test_settings() -> SubdivisionSettings {
    SubdivisionSettings {
        lot_area_min: 4000.0,
        lot_area_max: 9000.0,
        lot_width_min: 30.0,
        force_street_access: 1.0,
        irregularity: 0.0,
        seed: 42,
        ..SubdivisionSettings::default()
    }
}

/// Overlap area of two convex-ish lots by sampling: count interior grid points
/// of `a` that fall inside `b`, scaled by the sample cell area. Cheap disjoint
/// test — flags any real overlap while tolerating shared boundaries.
fn overlap_area(a: &Polygon2d, b: &Polygon2d) -> f64 {
    let (lo_a, hi_a) = a.aabb();
    let (lo_b, hi_b) = b.aabb();
    let lo = lo_a.max(lo_b);
    let hi = hi_a.min(hi_b);
    if lo.x >= hi.x || lo.y >= hi.y {
        return 0.0;
    }
    let n = 40;
    let dx = (hi.x - lo.x) / n as f64;
    let dy = (hi.y - lo.y) / n as f64;
    let cell = dx * dy;
    let mut acc = 0.0;
    for i in 0..n {
        for j in 0..n {
            let p = DVec2::new(lo.x + (i as f64 + 0.5) * dx, lo.y + (j as f64 + 0.5) * dy);
            if a.contains(p) && b.contains(p) {
                acc += cell;
            }
        }
    }
    acc
}

#[test]
fn area_conserved_on_all_blocks() {
    let s = test_settings();
    for (name, block) in load_all() {
        let lots = subdivide(&block, &s);
        assert!(!lots.is_empty(), "{name}: produced no lots");
        let sum: f64 = lots.iter().map(|l| l.polygon.area()).sum();
        let rel = (sum - block.area()).abs() / block.area();
        assert!(
            rel < 1e-6,
            "{name}: Σ lot area {sum} != block area {} (rel {rel})",
            block.area()
        );
    }
}

#[test]
fn no_overlapping_lots() {
    let s = test_settings();
    for (name, block) in load_all() {
        let lots = subdivide(&block, &s);
        for i in 0..lots.len() {
            for j in (i + 1)..lots.len() {
                let ov = overlap_area(&lots[i].polygon, &lots[j].polygon);
                // Allow a tiny sampling epsilon; a real overlap is a large chunk.
                let tol = 0.01 * lots[i].polygon.area().min(lots[j].polygon.area());
                assert!(
                    ov <= tol.max(1.0),
                    "{name}: lots {i} and {j} overlap by {ov}"
                );
            }
        }
    }
}

#[test]
fn every_lot_has_street_when_forced() {
    let s = test_settings();
    for (name, block) in load_all() {
        let lots = subdivide(&block, &s);
        for (k, lot) in lots.iter().enumerate() {
            assert!(
                lot.has_street,
                "{name}: lot {k} has no street edge under force_street_access=1.0"
            );
        }
    }
}

#[test]
fn lot_widths_respect_min_where_guaranteed() {
    let s = test_settings();
    for (name, block) in load_all() {
        let lots = subdivide(&block, &s);
        for (k, lot) in lots.iter().enumerate() {
            let width = OrientedBox::of_polygon(&lot.polygon)
                .map(|ob| ob.short_len())
                .unwrap_or(0.0);
            // A block whose own OBB short side is already below lot_width_min
            // (the near-degenerate sliver, the acute-corner strip) cannot yield
            // wider lots — the method does not guarantee min width there, so
            // only assert when the parent block itself cleared it.
            let block_short = OrientedBox::of_polygon(&block)
                .map(|ob| ob.short_len())
                .unwrap_or(0.0);
            if block_short >= s.lot_width_min {
                assert!(
                    width >= s.lot_width_min - 1e-6,
                    "{name}: lot {k} width {width} < lot_width_min {}",
                    s.lot_width_min
                );
            }
        }
    }
}

#[test]
fn deterministic_for_fixed_seed() {
    let s = test_settings();
    for (name, block) in load_all() {
        let a = subdivide(&block, &s);
        let b = subdivide(&block, &s);
        assert_eq!(a.len(), b.len(), "{name}: lot count differs across runs");
        for (i, (la, lb)) in a.iter().zip(&b).enumerate() {
            assert_eq!(
                la.polygon.verts(),
                lb.polygon.verts(),
                "{name}: lot {i} geometry differs across runs (non-deterministic)"
            );
            assert_eq!(la.has_street, lb.has_street);
        }
    }
}

#[test]
fn different_seed_can_change_layout_but_conserves_area() {
    // Irregularity on so the seed actually influences pivots.
    let mut s = test_settings();
    s.irregularity = 0.3;
    for (name, block) in load_all() {
        let mut s1 = s.clone();
        s1.seed = 1;
        let mut s2 = s.clone();
        s2.seed = 2;
        let a: f64 = subdivide(&block, &s1).iter().map(|l| l.polygon.area()).sum();
        let b: f64 = subdivide(&block, &s2).iter().map(|l| l.polygon.area()).sum();
        assert!((a - block.area()).abs() / block.area() < 1e-6, "{name}: seed1 area drift");
        assert!((b - block.area()).abs() / block.area() < 1e-6, "{name}: seed2 area drift");
    }
}

#[test]
fn high_width_min_forces_large_lots_correctly() {
    // Case #9: a high lot_width_min forcing lots larger than lot_area_max is
    // CORRECT — the subdivider should just stop, not error or loop.
    let (_, block) = load_all()
        .into_iter()
        .find(|(n, _)| n.contains("forces lots above"))
        .expect("case 09 present");
    let s = SubdivisionSettings {
        lot_area_min: 2000.0,
        lot_area_max: 3000.0,
        lot_width_min: 200.0, // very wide: forces lots above area_max
        force_street_access: 1.0,
        ..SubdivisionSettings::default()
    };
    let lots = subdivide(&block, &s);
    assert!(!lots.is_empty());
    // At least one lot legitimately exceeds lot_area_max — that's expected.
    let sum: f64 = lots.iter().map(|l| l.polygon.area()).sum();
    assert!((sum - block.area()).abs() / block.area() < 1e-6);
}
