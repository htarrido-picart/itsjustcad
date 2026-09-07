// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! §9 Phase-6 validation for the lot rules (width mix, depth, corner, flag,
//! loading, sliver merge). Run against the §8 sample blocks + a synthetic
//! alley-loaded block + a Phase-5 generated block.
//!
//! Assertions (plan §9 Phase 6):
//! - width mix hits requested proportions within 5 % on a 500 m frontage;
//! - alley-loaded blocks have correct two-sided (half) depth;
//! - corner lots widened + clamped (no self-intersection) on the 15° block #10;
//! - flag lots only when enabled, pole excluded from area;
//! - no slivers remain after merge;
//! - Σ area conserved through the rule passes;
//! - deterministic same-seed byte-identical;
//! - euro_latam placeholder note appears when defaults used.

use glam::DVec2;
use std::path::PathBuf;
use subdivision::{
    apply_lot_rules, generate_streets, extract_blocks, subdivide, Block, LotWidthMix, Polygon2d,
    RegionProfile, SampleBlock, StreetPattern, SubdivisionSettings,
};
use subdivision::subdivision::lot_rules::{
    flag, loading, subdivide_width_mix, WidthMixSolver,
};

fn blocks_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("samples/blocks")
}

fn load(name: &str) -> Polygon2d {
    let p = blocks_dir().join(name);
    let txt = std::fs::read_to_string(&p).unwrap();
    SampleBlock::from_json(&txt).unwrap().polygon().unwrap()
}

fn rect(w: f64, h: f64) -> Polygon2d {
    Polygon2d::from_pairs([(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)]).unwrap()
}

// ── Width mix ───────────────────────────────────────────────────────────────

#[test]
fn width_mix_within_5pct_on_500m_metric_frontage() {
    let block = Block::untagged(rect(500.0, 50.0));
    let s = SubdivisionSettings {
        region: RegionProfile::EuroLatam,
        width_mix: Some(LotWidthMix::euro_latam_default()),
        lot_depth_target: 25.0,
        lot_depth_tolerance: 5.0,
        ..SubdivisionSettings::default()
    };
    let (lots, err, placeholder) = subdivide_width_mix(&block, &s).expect("width-mix runs");
    assert!(err < 0.05, "proportion error {err} > 5%");
    assert!(!placeholder, "explicit mix must not flag placeholder");
    // Σ area conserved.
    let sum: f64 = lots.iter().map(|l| l.polygon.area()).sum();
    assert!((sum - block.area()).abs() / block.area() < 1e-3);
}

#[test]
fn width_mix_solver_isolated_proportions() {
    let s = WidthMixSolver::new(&[(6.0, 0.25), (8.0, 0.50), (10.0, 0.25)]).unwrap();
    let r = s.solve(500.0);
    assert!(r.max_proportion_error(s.targets()) < 0.05);
}

// ── Alley-loaded two-sided depth ──────────────────────────────────────────────

#[test]
fn alley_loaded_block_has_two_sided_half_depth() {
    // Build a block with an is_alley edge via the extractor + AlleyLoaded.
    let site = rect(200.0, 120.0);
    let mut s = SubdivisionSettings {
        street_pattern: StreetPattern::Orthogonal,
        road_width: 12.0,
        block_depth: 60.0,
        loading: subdivision::LoadingType::AlleyLoaded,
        alley_width: 5.0,
        seed: 7,
        ..SubdivisionSettings::default()
    };
    let graph = generate_streets(&site, &s);
    let blocks = extract_blocks(&site, &graph, &s);
    // At least one block must carry an alley edge.
    let alley_block = blocks.iter().find(|b| b.alley_edge_count() > 0);
    assert!(alley_block.is_some(), "AlleyLoaded must produce an alley edge");
    let b = alley_block.unwrap();
    // The loading plan halves the depth (street → central alley).
    let block_depth = 40.0;
    let plan = loading::plan(b, s.loading, block_depth);
    assert!(plan.is_two_frontage(), "alley block must be two-frontage");
    assert!((plan.lot_depth() - block_depth * 0.5).abs() < 1e-6, "two-sided depth wrong");

    // Front-loaded on the same block would use the full depth (sanity contrast).
    s.loading = subdivision::LoadingType::FrontLoaded;
    let front = loading::plan(b, s.loading, block_depth);
    assert!((front.lot_depth() - block_depth).abs() < 1e-6);
}

// ── Corner clamping on the 15° acute block #10 ────────────────────────────────

#[test]
fn corner_lots_widened_and_clamped_on_acute_block() {
    let block_poly = load("10_acute_corner.json");
    let block = Block::untagged(block_poly.clone());
    // Subdivide first, then apply the rules (corner widening + sliver merge).
    let mut s = SubdivisionSettings {
        region: RegionProfile::EuroLatam,
        lot_area_min: 3000.0,
        lot_width_min: 20.0,
        corner_angle_max: 45.0,
        corner_lot_width_bonus: 0.15,
        merge_slivers: false,
        force_street_access: 1.0,
        seed: 42,
        ..SubdivisionSettings::default()
    };
    // Disable placeholder-only depth/width-mix to keep this test on corners.
    s.width_mix = None;
    s.lot_depth_target = 25.0;
    let lots = subdivide(&block_poly, &s);
    let before_area: f64 = lots.iter().map(|l| l.polygon.area()).sum();
    let (out, report) = apply_lot_rules(&block, lots, &s);
    // The acute corner was detected + a lot widened.
    assert!(report.corners_widened >= 1, "no corner widened on acute block");
    // No lot self-intersects: every lot stays a valid positive-area polygon and
    // its vertex count is sane (widen only translates far-side vertices).
    for l in &out {
        assert!(l.polygon.area() > 0.0);
        assert!(l.polygon.len() >= 3);
    }
    // Corner widening is a TRUE area transfer: Σ lot area is conserved exactly
    // (the neighbour gives up precisely what the corner lot gains) — NOT merely
    // "bounded growth". Assert against the BLOCK area, the invariant used in
    // blocks.rs / offset_blocks.rs.
    let block_area = block_poly.area();
    let after_area: f64 = out.iter().map(|l| l.polygon.area()).sum();
    assert!(
        (before_area - block_area).abs() / block_area < 5e-3,
        "pre-condition: subdivision should already tile the block ({before_area} vs {block_area})"
    );
    assert!(
        (after_area - block_area).abs() / block_area < 5e-3,
        "corner widening broke area conservation: Σ {after_area} != block {block_area}"
    );
    // No pairwise lot overlap after widening.
    for i in 0..out.len() {
        for j in (i + 1)..out.len() {
            let ov = overlap_area(&out[i].polygon, &out[j].polygon);
            let tol = 0.01 * out[i].polygon.area().min(out[j].polygon.area());
            assert!(
                ov <= tol.max(1.0),
                "lots {i} and {j} overlap by {ov} after corner widening"
            );
        }
    }
}

/// Overlap area of two lots by sampling (mirrors blocks.rs / offset_blocks.rs):
/// count interior sample points of `a` that also fall inside `b`, scaled by cell
/// area. Cheap disjoint test tolerant of shared boundaries.
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

// ── Flag lots: only when enabled, pole excluded ───────────────────────────────

#[test]
fn flag_lots_only_when_enabled_and_pole_excluded() {
    let body = Polygon2d::from_pairs([(0.0, 20.0), (30.0, 20.0), (30.0, 50.0), (0.0, 50.0)]).unwrap();
    let pole = flag::rectangular_pole(DVec2::new(15.0, 20.0), DVec2::new(15.0, 0.0), 3.0).unwrap();

    // Disabled → None.
    assert!(flag::make_flag_lot(&body, &pole, 3.0, false, 3.0).is_none());
    // Pole below min → None.
    assert!(flag::make_flag_lot(&body, &pole, 2.0, true, 3.0).is_none());
    // Enabled + valid → pole excluded from countable area.
    let f = flag::make_flag_lot(&body, &pole, 3.0, true, 3.0).unwrap();
    assert!(f.countable_area() < f.gross_area());
    assert!((f.gross_area() - f.countable_area() - f.pole.area()).abs() < 1.5);
}

// ── No slivers remain after merge (all §8 blocks) ─────────────────────────────

#[test]
fn no_slivers_remain_after_merge_on_all_blocks() {
    let names = [
        "01_long_thin_rectangle.json",
        "02_l_shaped.json",
        "03_reentrant_notch.json",
        "07_one_short_street_edge.json",
        "09_width_forces_above_area_max.json",
        "10_acute_corner.json",
    ];
    for name in names {
        let poly = load(name);
        let block = Block::untagged(poly.clone());
        let s = SubdivisionSettings {
            lot_area_min: 4000.0,
            lot_width_min: 25.0,
            merge_slivers: true,
            sliver_area_frac: 0.5,
            force_street_access: 1.0,
            corner_lot_width_bonus: 0.0,
            lot_depth_target: 0.0,
            width_mix: None,
            region: RegionProfile::UsSuburban, // avoid placeholder fallbacks here
            seed: 42,
            ..SubdivisionSettings::default()
        };
        let lots = subdivide(&poly, &s);
        let block_area: f64 = poly.area();
        let (out, _r) = apply_lot_rules(&block, lots, &s);
        let threshold = s.sliver_area_frac * s.lot_area_min;
        for l in &out {
            assert!(
                l.polygon.area() >= threshold - 1e-3,
                "{name}: sliver remains ({} < {threshold})",
                l.polygon.area()
            );
        }
        // Σ area conserved through the rule passes.
        let sum: f64 = out.iter().map(|l| l.polygon.area()).sum();
        assert!(
            (sum - block_area).abs() / block_area < 5e-3,
            "{name}: area not conserved {sum} vs {block_area}"
        );
    }
}

// ── Determinism ───────────────────────────────────────────────────────────────

#[test]
fn lot_rules_deterministic_same_seed() {
    let poly = load("02_l_shaped.json");
    let block = Block::untagged(poly.clone());
    let s = SubdivisionSettings {
        lot_area_min: 4000.0,
        lot_width_min: 25.0,
        merge_slivers: true,
        force_street_access: 1.0,
        seed: 99,
        region: RegionProfile::UsSuburban,
        width_mix: None,
        lot_depth_target: 0.0,
        corner_lot_width_bonus: 0.0,
        ..SubdivisionSettings::default()
    };
    let a = apply_lot_rules(&block, subdivide(&poly, &s), &s).0;
    let b = apply_lot_rules(&block, subdivide(&poly, &s), &s).0;
    assert_eq!(a.len(), b.len());
    for (la, lb) in a.iter().zip(&b) {
        assert_eq!(la.polygon.verts(), lb.polygon.verts());
    }
}

// ── euro_latam placeholder note ───────────────────────────────────────────────

#[test]
fn placeholder_note_appears_when_defaults_used() {
    let block = Block::untagged(rect(200.0, 40.0));
    // EuroLatam + no explicit overrides → placeholders resolve.
    let s = SubdivisionSettings {
        region: RegionProfile::EuroLatam,
        width_mix: None,
        lot_depth_target: 0.0,
        corner_lot_width_bonus: 0.0,
        merge_slivers: false,
        ..SubdivisionSettings::default()
    };
    let lots = vec![subdivision::Lot {
        polygon: block.polygon.clone(),
        has_street: true,
    }];
    let (_out, report) = apply_lot_rules(&block, lots, &s);
    let banner = report.placeholder_banner().expect("placeholder banner expected");
    assert!(banner.contains("confirm with Manuel"), "banner: {banner}");
}

// ── Phase-5 generated block honours the rules ─────────────────────────────────

#[test]
fn generated_block_runs_through_lot_rules() {
    let site = rect(400.0, 300.0);
    let s = SubdivisionSettings {
        street_pattern: StreetPattern::Orthogonal,
        road_width: 12.0,
        block_depth: 80.0,
        seed: 7,
        ..SubdivisionSettings::default()
    };
    let graph = generate_streets(&site, &s);
    let blocks = extract_blocks(&site, &graph, &s);
    let block = blocks
        .into_iter()
        .max_by(|a, b| a.area().partial_cmp(&b.area()).unwrap())
        .unwrap();
    let ss = SubdivisionSettings {
        lot_area_min: 1500.0,
        lot_width_min: 15.0,
        merge_slivers: true,
        sliver_area_frac: 0.5,
        force_street_access: 1.0,
        region: RegionProfile::UsSuburban,
        width_mix: None,
        lot_depth_target: 0.0,
        corner_lot_width_bonus: 0.0,
        seed: 3,
        ..SubdivisionSettings::default()
    };
    let lots = subdivide(&block.polygon, &ss);
    let (out, _r) = apply_lot_rules(&block, lots, &ss);
    assert!(!out.is_empty());
    let threshold = ss.sliver_area_frac * ss.lot_area_min;
    for l in &out {
        assert!(l.polygon.area() >= threshold - 1e-3, "sliver in generated block");
    }
}
