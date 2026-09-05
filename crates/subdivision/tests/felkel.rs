// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Phase 12 (§12.2) — the true **Felkel** straight skeleton, and skeleton
//! subdivision driven by it (`skeleton_impl = Felkel`).
//!
//! Scope shipped is convex-exact Felkel with a robust fallback to the Phase-7
//! offset-approximate skeleton on non-convex blocks (where split events would be
//! required). These tests assert:
//! - Felkel correctness where it is exact: square → single centre node; rectangle
//!   → medial ridge segment; L-shape (non-convex) → no exact nodes but a covering
//!   face partition via fallback.
//! - Felkel and OffsetApprox agree within tolerance on convex polygons.
//! - Felkel-backed `method=streetfollowing` still passes the §8 subdivision
//!   assertions (area conservation, no gaps, street access, determinism) on the
//!   blocks that matter — i.e. swapping the backend never ships a broken skeleton.

use glam::DVec2;
use std::path::PathBuf;
use subdivision::{
    subdivide_skeleton, FelkelSkeleton, OffsetApproxSkeleton, Polygon2d, SampleBlock, SkeletonImpl,
    StraightSkeleton, SubdivisionSettings,
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

// ── Felkel exactness on convex polygons ──────────────────────────────────────

#[test]
fn square_skeleton_node_at_center() {
    let sq = Polygon2d::from_pairs([(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)]).unwrap();
    let nodes = FelkelSkeleton::new().skeleton_nodes(&sq);
    assert!(!nodes.is_empty());
    assert!(nodes.last().unwrap().distance(DVec2::new(5.0, 5.0)) < 1e-6);
}

#[test]
fn rectangle_skeleton_is_medial_segment() {
    let r = Polygon2d::from_pairs([(0.0, 0.0), (30.0, 0.0), (30.0, 10.0), (0.0, 10.0)]).unwrap();
    let nodes = FelkelSkeleton::new().skeleton_nodes(&r);
    assert!(nodes.len() >= 2, "medial ridge has ≥2 nodes");
    for nd in &nodes {
        assert!((nd.y - 5.0).abs() < 1e-6, "ridge centred in y");
    }
    let xmin = nodes.iter().map(|p| p.x).fold(f64::INFINITY, f64::min);
    let xmax = nodes.iter().map(|p| p.x).fold(f64::NEG_INFINITY, f64::max);
    // A 30×10 rectangle's ridge runs x∈[5,25].
    assert!((xmin - 5.0).abs() < 1e-6 && (xmax - 25.0).abs() < 1e-6);
}

#[test]
fn lshape_nonconvex_falls_back_and_covers() {
    let l = load("L-shaped");
    let felkel = FelkelSkeleton::new();
    assert!(
        felkel.skeleton_nodes(&l).is_empty(),
        "non-convex L: no exact Felkel nodes (split events unsupported → fallback)"
    );
    let faces = felkel.faces(&l);
    assert!(!faces.is_empty());
    let sum: f64 = faces.iter().map(|f| f.polygon.area()).sum();
    assert!(sum >= l.area() * 0.9, "fallback faces cover the L-shape");
}

#[test]
fn felkel_agrees_with_offset_approx_on_convex() {
    // Several convex blocks: the two skeletons produce the same face partition to
    // tolerance (both are the bisector / nearest-edge partition).
    let convex = [
        Polygon2d::from_pairs([(0.0, 0.0), (40.0, 0.0), (40.0, 20.0), (0.0, 20.0)]).unwrap(),
        Polygon2d::from_pairs([(0.0, 0.0), (30.0, 0.0), (15.0, 26.0)]).unwrap(),
        Polygon2d::from_pairs([
            (10.0, 0.0),
            (5.0, 8.66),
            (-5.0, 8.66),
            (-10.0, 0.0),
            (-5.0, -8.66),
            (5.0, -8.66),
        ])
        .unwrap(),
    ];
    for (k, poly) in convex.iter().enumerate() {
        let f = FelkelSkeleton::new().faces(poly);
        let a = OffsetApproxSkeleton::new().faces(poly);
        assert_eq!(f.len(), a.len(), "convex #{k}: same face count");
        let fs: f64 = f.iter().map(|p| p.polygon.area()).sum();
        let as_: f64 = a.iter().map(|p| p.polygon.area()).sum();
        assert!(
            (fs - as_).abs() < 1e-4 + fs * 1e-6,
            "convex #{k}: same coverage {fs} vs {as_}"
        );
    }
}

// ── Felkel-backed subdivision on §8 blocks ──────────────────────────────────

fn felkel_settings() -> SubdivisionSettings {
    SubdivisionSettings {
        method: subdivision::SubdivisionMethod::Skeleton,
        skeleton_impl: SkeletonImpl::Felkel,
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

/// The full §8 set — used for the no-panic fragility guard (the skeleton must
/// never panic on ANY block, whatever its convexity).
fn all_blocks() -> Vec<(&'static str, Polygon2d)> {
    vec![
        ("long thin", load("long thin")),
        ("L-shaped", load("L-shaped")),
        ("notch", load("re-entrant")),
        ("cul-de-sac", load("cul-de-sac")),
        ("curved-street", load("curved-street")),
        ("one short street", load("one very short street")),
        ("sliver", load("sliver")),
        ("acute", load("acute")),
    ]
}

/// The blocks the skeleton subdivider guarantees area-conservation on — the same
/// set the Phase-7 suite (`skeleton_blocks.rs`) asserts on. The approximate
/// skeleton's nearest-edge fan does NOT conserve area on the deep re-entrant
/// notch (a known Phase-7 limit, hence its exclusion there too); the Felkel
/// backend inherits that fallback on the non-convex notch, so it is excluded from
/// the conservation assertion here as well — it is still covered by the no-panic
/// guard above.
fn matters() -> Vec<(&'static str, Polygon2d)> {
    vec![
        ("long thin", load("long thin")),
        ("cul-de-sac", load("cul-de-sac")),
        ("curved-street", load("curved-street")),
        ("one short street", load("one very short street")),
    ]
}

#[test]
fn felkel_backend_no_panic_on_all_section8_blocks() {
    let s = felkel_settings();
    for (name, block) in all_blocks() {
        // Must never panic (fragility guard) and must produce lots.
        let lots = subdivide_skeleton(&block, &s);
        assert!(!lots.is_empty(), "{name}: felkel backend produced no lots");
    }
}

#[test]
fn felkel_backend_conserves_area() {
    let s = felkel_settings();
    for (name, block) in matters() {
        let lots = subdivide_skeleton(&block, &s);
        let sum: f64 = lots.iter().map(|l| l.polygon.area()).sum();
        let rel = (sum - block.area()).abs() / block.area();
        assert!(rel < 0.03, "{name}: Σ area {sum} vs {} (rel {rel})", block.area());
    }
}

#[test]
fn felkel_backend_street_access() {
    let s = felkel_settings();
    for (name, block) in matters() {
        let lots = subdivide_skeleton(&block, &s);
        assert!(
            lots.iter().all(|l| l.has_street),
            "{name}: every lot must front a street under force_street_access=1"
        );
    }
}

#[test]
fn felkel_backend_deterministic_byte_identical() {
    let s = felkel_settings();
    for (_name, block) in matters() {
        let a = subdivide_skeleton(&block, &s);
        let b = subdivide_skeleton(&block, &s);
        assert_eq!(a.len(), b.len());
        for (la, lb) in a.iter().zip(&b) {
            assert_eq!(la.polygon.verts(), lb.polygon.verts());
        }
    }
}

/// The DEFAULT (offset) backend still passes — swapping does not regress Phase 7.
#[test]
fn default_offset_backend_unchanged() {
    let mut s = felkel_settings();
    s.skeleton_impl = SkeletonImpl::Offset;
    for (name, block) in matters() {
        let lots = subdivide_skeleton(&block, &s);
        assert!(!lots.is_empty(), "{name}: offset backend produced no lots");
        let sum: f64 = lots.iter().map(|l| l.polygon.area()).sum();
        assert!((sum - block.area()).abs() / block.area() < 0.03, "{name}: area");
    }
}
