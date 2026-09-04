// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! §8 validation cases for OFFSET / perimeter subdivision (Phase 4), run against
//! the 10 sample blocks in `samples/blocks/*.json` (the same set Phase 3 uses).
//!
//! Assertions per the plan §8:
//! - Σ lot area == block area within tolerance (conservation) — even when the
//!   algorithm degenerately falls back to recursive OBB.
//! - No overlapping lots (pairwise-disjoint interiors).
//! - Every emitted lot has at least one boundary edge (a real ring, not a
//!   floating sliver) — checked as "the lot's OBB has positive extent".
//! - Deterministic for a fixed seed (byte-identical on re-run).
//!
//! Plus the Phase-4-specific degenerate cases the plan §9 requires verified:
//! - `offset_width = 0` → falls back to recursive OBB.
//! - `offset_width` huge (interior collapses) → falls back cleanly, no panic.
//! - thin rectangle (#1) where the inset self-collapses.
//! - `subdivide_core` on vs off → core lots vs one hollow ring.

use glam::DVec2;
use std::path::PathBuf;
use subdivision::{
    subdivide as subdivide_grid, subdivide_offset, OrientedBox, Polygon2d, SampleBlock,
    SubdivisionMethod, SubdivisionSettings,
};

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

/// Offset settings sized so most sample blocks form a strip + core; small blocks
/// legitimately collapse and fall back.
fn test_settings() -> SubdivisionSettings {
    SubdivisionSettings {
        method: SubdivisionMethod::Offset,
        offset_width: 20.0,
        subdivide_core: true,
        lot_area_min: 3000.0,
        lot_area_max: 9000.0,
        lot_width_min: 20.0,
        force_street_access: 0.0,
        irregularity: 0.0,
        seed: 42,
        ..SubdivisionSettings::default()
    }
}

/// Overlap area of two lots by interior grid sampling (same cheap disjoint test
/// as the Phase-3 suite).
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
        let lots = subdivide_offset(&block, &s);
        assert!(!lots.is_empty(), "{name}: produced no lots");
        let sum: f64 = lots.iter().map(|l| l.polygon.area()).sum();
        let rel = (sum - block.area()).abs() / block.area();
        assert!(
            rel < 1e-3,
            "{name}: Σ lot area {sum} != block area {} (rel {rel})",
            block.area()
        );
    }
}

#[test]
fn no_overlapping_lots() {
    let s = test_settings();
    for (name, block) in load_all() {
        let lots = subdivide_offset(&block, &s);
        for i in 0..lots.len() {
            for j in (i + 1)..lots.len() {
                let ov = overlap_area(&lots[i].polygon, &lots[j].polygon);
                // Grid-sampled overlap double-counts cells that straddle a shared
                // edge/corner; on small angled wedge lots (e.g. the hex/annular
                // cells) that can graze the relative tol, so keep a small absolute
                // floor of a few sample cells. A genuine overlap is hundreds+.
                let tol = 0.02 * lots[i].polygon.area().min(lots[j].polygon.area());
                assert!(ov <= tol.max(20.0), "{name}: lots {i} and {j} overlap by {ov}");
            }
        }
    }
}

#[test]
fn every_lot_is_a_real_ring() {
    let s = test_settings();
    for (name, block) in load_all() {
        let lots = subdivide_offset(&block, &s);
        for (k, lot) in lots.iter().enumerate() {
            assert!(lot.polygon.len() >= 3, "{name}: lot {k} has < 3 verts");
            let ob = OrientedBox::of_polygon(&lot.polygon);
            let short = ob.map(|o| o.short_len()).unwrap_or(0.0);
            assert!(short > 1e-6, "{name}: lot {k} has a degenerate (zero-width) ring");
            assert!(lot.polygon.area() > 1e-6, "{name}: lot {k} has ~zero area");
        }
    }
}

#[test]
fn deterministic_for_fixed_seed() {
    let mut s = test_settings();
    s.irregularity = 0.3;
    for (name, block) in load_all() {
        let a = subdivide_offset(&block, &s);
        let b = subdivide_offset(&block, &s);
        assert_eq!(a.len(), b.len(), "{name}: lot count differs across runs");
        for (i, (la, lb)) in a.iter().zip(&b).enumerate() {
            assert_eq!(
                la.polygon.verts(),
                lb.polygon.verts(),
                "{name}: lot {i} geometry differs across runs (non-deterministic)"
            );
        }
    }
}

// ── Degenerate cases the plan §9 Phase 4 requires verified ──────────────────

#[test]
fn zero_offset_falls_back_to_recursive_obb() {
    let mut s = test_settings();
    s.offset_width = 0.0;
    for (name, block) in load_all() {
        let via_offset = subdivide_offset(&block, &s);
        let mut rec = s.clone();
        rec.method = SubdivisionMethod::Recursive;
        let via_grid = subdivide_grid(&block, &rec);
        assert_eq!(
            via_offset.len(),
            via_grid.len(),
            "{name}: offset_width=0 should match recursive OBB exactly"
        );
        for (a, b) in via_offset.iter().zip(&via_grid) {
            assert_eq!(a.polygon.verts(), b.polygon.verts(), "{name}: fallback geometry differs");
        }
    }
}

#[test]
fn huge_offset_collapses_falls_back_no_panic() {
    let mut s = test_settings();
    s.offset_width = 5000.0; // dwarfs every sample block
    for (name, block) in load_all() {
        let lots = subdivide_offset(&block, &s);
        assert!(!lots.is_empty(), "{name}: huge offset produced no lots");
        let sum: f64 = lots.iter().map(|l| l.polygon.area()).sum();
        let rel = (sum - block.area()).abs() / block.area();
        assert!(rel < 1e-3, "{name}: huge-offset fallback lost area (rel {rel})");
    }
}

#[test]
fn thin_rectangle_inset_self_collapses() {
    // Case #1 is 400×120; a 70 m inward inset collapses its 120 m dimension
    // (60 m each side leaves 0), forcing the degenerate fallback. Must not panic
    // and must conserve area.
    let (_, block) = load_all()
        .into_iter()
        .find(|(n, _)| n.contains("thin rectangle"))
        .expect("case 01 present");
    let mut s = test_settings();
    s.offset_width = 70.0;
    let lots = subdivide_offset(&block, &s);
    assert!(!lots.is_empty());
    let sum: f64 = lots.iter().map(|l| l.polygon.area()).sum();
    assert!((sum - block.area()).abs() / block.area() < 1e-3);
}

#[test]
fn subdivide_core_on_yields_more_than_off() {
    // On a block big enough to keep a real core, core-on carves the interior and
    // core-off keeps one hollow ring — so core-on has strictly more lots. Uses a
    // large synthetic rectangle so the core survives on every platform.
    let block = Polygon2d::from_pairs([
        (0.0, 0.0),
        (600.0, 0.0),
        (600.0, 400.0),
        (0.0, 400.0),
    ])
    .unwrap();
    let mut on = test_settings();
    on.subdivide_core = true;
    let mut off = test_settings();
    off.subdivide_core = false;
    let lots_on = subdivide_offset(&block, &on);
    let lots_off = subdivide_offset(&block, &off);
    let sum_on: f64 = lots_on.iter().map(|l| l.polygon.area()).sum();
    let sum_off: f64 = lots_off.iter().map(|l| l.polygon.area()).sum();
    assert!((sum_on - block.area()).abs() / block.area() < 1e-3);
    assert!((sum_off - block.area()).abs() / block.area() < 1e-3);
    assert!(
        lots_on.len() > lots_off.len(),
        "core-on {} should exceed core-off {}",
        lots_on.len(),
        lots_off.len()
    );
}
