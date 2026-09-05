// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! §8 / §9 validation for **skeleton (street-following) subdivision** (Phase 7,
//! `method=streetfollowing`). Run on the blocks that matter for it (plan §8/§9):
//!
//! - #1 long thin rectangle → one double-loaded row
//! - #4 cul-de-sac bulb (near-circular, single street edge)
//! - #5 curved-street block, varying radius
//! - #7 one short street edge + 3 long non-street edges
//!
//! Assertions (plan §8):
//! - Σ lot area == block area within tolerance (conservation).
//! - No overlapping lots (pairwise-disjoint interiors).
//! - No gaps (conservation + disjointness on a partition).
//! - Every lot has a street edge (`force_street_access == 1.0`).
//! - Lots roughly PERPENDICULAR to the street on the curved/bulb cases — the
//!   visual claim (§9): each lot's inward (non-street) side aligns with the local
//!   street normal within a tolerance.
//! - Deterministic same-seed → byte-identical (also the ItsJustCAD replay
//!   invariant).
//! - lot count in a sane range (§9 structural proxy for "matches the reference").

use glam::DVec2;
use std::path::PathBuf;
use subdivision::{
    subdivide_skeleton, OrientedBox, Polygon2d, SampleBlock, SubdivisionSettings,
};

fn blocks_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("samples/blocks")
}

fn load(name_frag: &str) -> Polygon2d {
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
        if sb.name.contains(name_frag) {
            return sb.polygon().unwrap();
        }
    }
    panic!("no sample block matching {name_frag:?}");
}

/// Skeleton settings sized so each block yields several lots. Rules that would
/// perturb the pure skeleton geometry (width-mix packing, corner bonus) are off
/// so the perpendicularity assertion tests the skeleton slicing itself.
fn skel_settings() -> SubdivisionSettings {
    SubdivisionSettings {
        method: subdivision::SubdivisionMethod::Skeleton,
        lot_area_min: 400.0,
        lot_width_min: 20.0,
        force_street_access: 1.0,
        merge_slivers: true,
        corner_lot_width_bonus: 0.0,
        width_mix: None,
        region: subdivision::RegionProfile::UsSuburban,
        seed: 42,
        ..SubdivisionSettings::default()
    }
}

/// Sampling overlap test (same as the recursive-OBB suite).
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

fn matters() -> Vec<(&'static str, Polygon2d)> {
    vec![
        ("long thin", load("long thin")),
        ("cul-de-sac", load("cul-de-sac")),
        ("curved-street", load("curved-street")),
        ("one short street", load("one very short street")),
    ]
}

#[test]
fn area_conserved() {
    let s = skel_settings();
    for (name, block) in matters() {
        let lots = subdivide_skeleton(&block, &s);
        assert!(!lots.is_empty(), "{name}: no lots");
        let sum: f64 = lots.iter().map(|l| l.polygon.area()).sum();
        let rel = (sum - block.area()).abs() / block.area();
        assert!(
            rel < 0.02,
            "{name}: Σ lot area {sum} != block area {} (rel {rel})",
            block.area()
        );
    }
}

#[test]
fn no_overlaps_and_no_gaps() {
    let s = skel_settings();
    for (name, block) in matters() {
        let lots = subdivide_skeleton(&block, &s);
        // Disjointness. The overlap is measured by grid sampling; along a shared
        // edge that is NOT axis-aligned (the radial seams of a bulb, the slanted
        // seams of a curved band) a coarse grid double-counts border cells, so the
        // tolerance scales with the sample cell size. A REAL overlap is a large
        // fraction of a lot (hundreds of units), far above this. Area conservation
        // (checked separately) is the hard no-overlap guarantee on a partition.
        let (blo, bhi) = block.aabb();
        let cell = ((bhi.x - blo.x) / 40.0) * ((bhi.y - blo.y) / 40.0);
        for i in 0..lots.len() {
            for j in (i + 1)..lots.len() {
                let ov = overlap_area(&lots[i].polygon, &lots[j].polygon);
                let seam_tol = 10.0 * cell; // ~10 border cells along a slanted seam
                let frac_tol = 0.05 * lots[i].polygon.area().min(lots[j].polygon.area());
                let tol = seam_tol.max(frac_tol);
                assert!(ov <= tol, "{name}: lots {i},{j} overlap by {ov} (tol {tol})");
            }
        }
        // No gaps: conservation already checked; here confirm the union covers
        // the block interior by sampling (every interior sample lands in a lot).
        let (lo, hi) = block.aabb();
        let n = 30;
        let mut inside_block = 0;
        let mut covered = 0;
        for i in 0..n {
            for j in 0..n {
                let p = DVec2::new(
                    lo.x + (i as f64 + 0.5) * (hi.x - lo.x) / n as f64,
                    lo.y + (j as f64 + 0.5) * (hi.y - lo.y) / n as f64,
                );
                if block.contains(p) {
                    inside_block += 1;
                    if lots.iter().any(|l| l.polygon.contains(p)) {
                        covered += 1;
                    }
                }
            }
        }
        let cover_frac = covered as f64 / inside_block.max(1) as f64;
        assert!(cover_frac > 0.95, "{name}: lots cover only {cover_frac} of block (gaps)");
    }
}

#[test]
fn every_lot_has_street() {
    let s = skel_settings();
    for (name, block) in matters() {
        let lots = subdivide_skeleton(&block, &s);
        for (k, lot) in lots.iter().enumerate() {
            assert!(lot.has_street, "{name}: lot {k} has no street edge");
        }
    }
}

#[test]
fn deterministic_byte_identical() {
    let s = skel_settings();
    for (name, block) in matters() {
        let a = subdivide_skeleton(&block, &s);
        let b = subdivide_skeleton(&block, &s);
        assert_eq!(a.len(), b.len(), "{name}: lot count differs");
        for (i, (la, lb)) in a.iter().zip(&b).enumerate() {
            assert_eq!(
                la.polygon.verts(),
                lb.polygon.verts(),
                "{name}: lot {i} geometry non-deterministic"
            );
            assert_eq!(la.has_street, lb.has_street);
        }
    }
}

#[test]
fn lot_count_in_sane_range() {
    // §9 structural proxy for "matches the reference": each block should yield a
    // handful-to-many lots, not 1 (undivided) and not thousands (runaway).
    let s = skel_settings();
    for (name, block) in matters() {
        let lots = subdivide_skeleton(&block, &s);
        assert!(
            (2..=500).contains(&lots.len()),
            "{name}: {} lots is out of the sane range",
            lots.len()
        );
    }
}

/// The visual claim (§9): on the curved-street and cul-de-sac-bulb blocks the lot
/// lines run PERPENDICULAR to the street. We verify the structural proxy: for
/// each lot, its longest "inward" side (the side running away from the street)
/// aligns with the local street NORMAL — i.e. is perpendicular to the nearest
/// street-boundary tangent — within a tolerance, for a good fraction of lots.
#[test]
fn lots_roughly_perpendicular_to_curved_street() {
    let s = skel_settings();
    for name in ["cul-de-sac", "curved-street"] {
        let block = load(name);
        let lots = subdivide_skeleton(&block, &s);
        let block_edges: Vec<(DVec2, DVec2)> = block.polygon_edges();

        let mut checked = 0;
        let mut aligned = 0;
        for lot in &lots {
            // Find the lot's street edge (on the block boundary) → its tangent.
            let Some((sa, sb)) = lot_street_edge(&lot.polygon, &block_edges) else {
                continue;
            };
            let street_tan = (sb - sa).normalize_or_zero();
            if street_tan.length_squared() < 0.5 {
                continue;
            }
            let street_normal = DVec2::new(-street_tan.y, street_tan.x);
            // The lot's inward side = a lot edge NOT on the block boundary whose
            // direction should align with the street normal.
            let Some(inward_dir) = lot_inward_side_dir(&lot.polygon, &block_edges) else {
                continue;
            };
            checked += 1;
            // Alignment: |inward_dir · street_normal| close to 1 (parallel to
            // normal == perpendicular to the street). 30° tolerance.
            let align = inward_dir.dot(street_normal).abs();
            if align > 30.0_f64.to_radians().cos() {
                aligned += 1;
            }
        }
        assert!(checked >= 3, "{name}: too few checkable lots ({checked})");
        let frac = aligned as f64 / checked as f64;
        assert!(
            frac >= 0.6,
            "{name}: only {frac} of lots are perpendicular to the street (want ≥ 0.6)"
        );
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

trait Edges {
    fn polygon_edges(&self) -> Vec<(DVec2, DVec2)>;
}
impl Edges for Polygon2d {
    fn polygon_edges(&self) -> Vec<(DVec2, DVec2)> {
        self.edges().collect()
    }
}

fn point_on_segment(p: DVec2, a: DVec2, b: DVec2, tol: f64) -> bool {
    let ab = b - a;
    let len2 = ab.length_squared();
    if len2 < 1e-18 {
        return p.distance(a) < tol;
    }
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    p.distance(a + ab * t) < tol
}

/// The lot edge that lies on the block boundary (the street side) — returns its
/// endpoints. Picks the longest such edge.
fn lot_street_edge(lot: &Polygon2d, block_edges: &[(DVec2, DVec2)]) -> Option<(DVec2, DVec2)> {
    let mut best: Option<((DVec2, DVec2), f64)> = None;
    for (la, lb) in lot.edges() {
        let mid = (la + lb) * 0.5;
        let on = block_edges
            .iter()
            .any(|&(ea, eb)| point_on_segment(mid, ea, eb, 1.0));
        if on {
            let len = la.distance(lb);
            if len > best.map(|(_, l)| l).unwrap_or(0.0) {
                best = Some(((la, lb), len));
            }
        }
    }
    best.map(|(e, _)| e)
}

/// The direction (unit) of the lot's longest edge NOT on the block boundary —
/// the "inward" side that should run along the street normal.
fn lot_inward_side_dir(lot: &Polygon2d, block_edges: &[(DVec2, DVec2)]) -> Option<DVec2> {
    let mut best: Option<(DVec2, f64)> = None;
    for (la, lb) in lot.edges() {
        let mid = (la + lb) * 0.5;
        let on = block_edges
            .iter()
            .any(|&(ea, eb)| point_on_segment(mid, ea, eb, 1.0));
        if on {
            continue;
        }
        let dir = (lb - la).normalize_or_zero();
        if dir.length_squared() < 0.5 {
            continue;
        }
        let len = la.distance(lb);
        if len > best.map(|(_, l)| l).unwrap_or(0.0) {
            best = Some((dir, len));
        }
    }
    best.map(|(d, _)| d)
}

#[allow(dead_code)]
fn obb_short(p: &Polygon2d) -> f64 {
    OrientedBox::of_polygon(p).map(|o| o.short_len()).unwrap_or(0.0)
}
