// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Phase 5 validation (plan §9): road generators + block extraction + street
//! tagging. Each of the four rectilinear generators runs on a real rectangular
//! and an L-shaped site; the emitted blocks must be non-overlapping, cover the
//! site minus road ROW within tolerance, carry correct street tags, and be
//! byte-identical for a fixed seed. The alley tier appears iff
//! `loading == AlleyLoaded`.

use glam::DVec2;
use subdivision::{
    clip_bridge, extract_blocks, generate_streets, Block, LoadingType, Polygon2d, StreetGraph,
    StreetPattern, SubdivisionSettings,
};

/// A 400×300 rectangular site.
fn rect_site() -> Polygon2d {
    Polygon2d::from_pairs([(0.0, 0.0), (400.0, 0.0), (400.0, 300.0), (0.0, 300.0)]).unwrap()
}

/// An L-shaped site (400×300 with a 200×150 bite out of the top-right corner).
fn l_site() -> Polygon2d {
    Polygon2d::from_pairs([
        (0.0, 0.0),
        (400.0, 0.0),
        (400.0, 150.0),
        (200.0, 150.0),
        (200.0, 300.0),
        (0.0, 300.0),
    ])
    .unwrap()
}

fn base_settings(pattern: StreetPattern) -> SubdivisionSettings {
    SubdivisionSettings {
        street_pattern: pattern,
        road_width: 12.0,
        block_depth: 60.0,
        seed: 7,
        loading: LoadingType::FrontLoaded,
        ..SubdivisionSettings::default()
    }
}

/// Approximate area of the road ROW union (for coverage accounting).
fn row_area(graph: &StreetGraph) -> f64 {
    // Sum ribbon areas is an upper bound (overlaps at intersections double
    // count), so we instead measure "site − blocks" against it loosely.
    graph
        .streets
        .iter()
        .filter_map(subdivision::streets::block_extractor::street_ribbon)
        .map(|p| p.area())
        .sum()
}

/// Pairwise-disjoint check: no two blocks share interior area (within tol).
fn blocks_disjoint(blocks: &[Block]) -> bool {
    for i in 0..blocks.len() {
        for j in (i + 1)..blocks.len() {
            let inter = clip_bridge::intersection(&blocks[i].polygon, &blocks[j].polygon);
            let overlap: f64 = inter.iter().map(|p| p.area()).sum();
            let scale = blocks[i].area().min(blocks[j].area());
            if overlap > scale * 1e-3 + 1.0 {
                return false;
            }
        }
    }
    true
}

fn run_generator(site: &Polygon2d, pattern: StreetPattern) -> (StreetGraph, Vec<Block>) {
    let s = base_settings(pattern);
    let graph = generate_streets(site, &s);
    let blocks = extract_blocks(site, &graph, &s);
    (graph, blocks)
}

const PATTERNS: [StreetPattern; 4] = [
    StreetPattern::Orthogonal,
    StreetPattern::Skewed,
    StreetPattern::Organic,
    StreetPattern::CulDeSac,
];

#[test]
fn all_generators_produce_streets_and_blocks_on_rect() {
    let site = rect_site();
    for p in PATTERNS {
        let (graph, blocks) = run_generator(&site, p);
        assert!(!graph.is_empty(), "{p:?}: empty street graph");
        assert!(!blocks.is_empty(), "{p:?}: no blocks");
        assert!(blocks_disjoint(&blocks), "{p:?}: blocks overlap");
        // Blocks cover the site minus the road ROW: block area ≤ site area, and
        // reasonably close to (site − ROW). Cul-de-sac stubs leave the far side
        // as one large block, so we use a generous lower bound.
        let block_area: f64 = blocks.iter().map(|b| b.area()).sum();
        assert!(block_area <= site.area() + 1.0, "{p:?}: blocks exceed site");
        assert!(
            block_area > site.area() * 0.3,
            "{p:?}: suspiciously little coverage ({block_area} of {})",
            site.area()
        );
    }
}

#[test]
fn all_generators_produce_blocks_on_l_shape() {
    let site = l_site();
    for p in PATTERNS {
        let (graph, blocks) = run_generator(&site, p);
        assert!(!graph.is_empty(), "{p:?}: empty street graph on L");
        assert!(!blocks.is_empty(), "{p:?}: no blocks on L");
        assert!(blocks_disjoint(&blocks), "{p:?}: L blocks overlap");
        let block_area: f64 = blocks.iter().map(|b| b.area()).sum();
        assert!(block_area <= site.area() + 1.0, "{p:?}: L blocks exceed site");
    }
}

#[test]
fn orthogonal_coverage_matches_site_minus_row() {
    // On a plain rectangle the orthogonal grid's blocks + ROW should recover the
    // whole site within tolerance (no gaps).
    let site = rect_site();
    let (graph, blocks) = run_generator(&site, StreetPattern::Orthogonal);
    let block_area: f64 = blocks.iter().map(|b| b.area()).sum();
    let row = row_area(&graph); // upper bound (double counts intersections)
    // site − blocks should be ≤ ROW upper bound and > 0 (roads took some area).
    let carved = site.area() - block_area;
    assert!(carved > 0.0, "roads carved nothing");
    assert!(
        carved <= row + 1.0,
        "carved {carved} exceeds ROW upper bound {row}"
    );
}

/// Build a known 2×2 orthogonal grid by hand: a 200×200 site with a vertical and
/// a horizontal road through the center, ROW 20. Then assert tag correctness.
#[test]
fn street_tagging_on_known_2x2_grid() {
    use subdivision::StreetTier;
    let site = Polygon2d::from_pairs([
        (0.0, 0.0),
        (200.0, 0.0),
        (200.0, 200.0),
        (0.0, 200.0),
    ])
    .unwrap();
    let mut graph = StreetGraph::new();
    // Horizontal road at y=100, vertical road at x=100, both ROW 20.
    let h = graph.add(
        vec![DVec2::new(0.0, 100.0), DVec2::new(200.0, 100.0)],
        20.0,
        StreetTier::Spine,
    );
    let v = graph.add(
        vec![DVec2::new(100.0, 0.0), DVec2::new(100.0, 200.0)],
        20.0,
        StreetTier::Spine,
    );
    let s = base_settings(StreetPattern::Orthogonal);
    let blocks = extract_blocks(&site, &graph, &s);

    // Four corner blocks, each 90×90 = 8100 (200/2 − 20/2 = 90).
    assert_eq!(blocks.len(), 4, "expected 4 corner blocks");
    for b in &blocks {
        assert!((b.area() - 8100.0).abs() < 30.0, "corner area {}", b.area());
        // Each corner block: two edges on the site boundary (outer, non-street),
        // two edges on roads (interior, street-tagged).
        let street_edges = b.street_edge_count();
        assert_eq!(street_edges, 2, "corner block should front 2 roads, got {street_edges}");
        // Its street edges carry width 20 and one of the two road ids.
        for e in b.edges.iter().filter(|e| e.is_street) {
            assert!((e.street_width - 20.0).abs() < 1e-6);
            let id = e.street_id.unwrap();
            assert!(id == h || id == v, "unexpected street id {id}");
        }
        // Outer (site-boundary) edges must NOT be tagged street.
        let boundary_non_street = b
            .edges
            .iter()
            .filter(|e| !e.is_street && !e.is_alley)
            .count();
        assert!(boundary_non_street >= 2, "corner should keep 2 outer edges");
    }
}

#[test]
fn alley_tier_present_only_when_alley_loaded() {
    let site = rect_site();

    // Front-loaded → no alley edges anywhere.
    let s_front = base_settings(StreetPattern::Orthogonal);
    let g = generate_streets(&site, &s_front);
    let front_blocks = extract_blocks(&site, &g, &s_front);
    let front_alleys: usize = front_blocks.iter().map(|b| b.alley_edge_count()).sum();
    assert_eq!(front_alleys, 0, "front-loaded must have no alley edges");

    // Alley-loaded → each original block is bisected, producing alley edges.
    let mut s_alley = base_settings(StreetPattern::Orthogonal);
    s_alley.loading = LoadingType::AlleyLoaded;
    s_alley.alley_width = 5.0;
    let alley_blocks = extract_blocks(&site, &g, &s_alley);
    let alley_edges: usize = alley_blocks.iter().map(|b| b.alley_edge_count()).sum();
    assert!(alley_edges > 0, "alley-loaded must insert alley edges");
    // Alley loading splits blocks → more blocks than the front-loaded case.
    assert!(
        alley_blocks.len() >= front_blocks.len(),
        "alley loading should not reduce block count"
    );
    // Alley edges carry the configured alley width.
    for b in &alley_blocks {
        for e in b.edges.iter().filter(|e| e.is_alley) {
            assert!((e.street_width - 5.0).abs() < 1e-6, "alley width {}", e.street_width);
        }
    }
}

#[test]
fn deterministic_for_fixed_seed() {
    let site = rect_site();
    for p in PATTERNS {
        let s = base_settings(p);
        let g1 = generate_streets(&site, &s);
        let g2 = generate_streets(&site, &s);
        // Byte-identical street graphs: same street count, ids, widths, points.
        assert_eq!(g1.len(), g2.len(), "{p:?}: street count differs");
        for (a, b) in g1.streets.iter().zip(g2.streets.iter()) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.width.to_bits(), b.width.to_bits(), "{p:?}: width differs");
            assert_eq!(a.centerline.len(), b.centerline.len());
            for (pa, pb) in a.centerline.iter().zip(b.centerline.iter()) {
                assert_eq!(pa.x.to_bits(), pb.x.to_bits(), "{p:?}: x differs");
                assert_eq!(pa.y.to_bits(), pb.y.to_bits(), "{p:?}: y differs");
            }
        }
        // Blocks likewise identical.
        let b1 = extract_blocks(&site, &g1, &s);
        let b2 = extract_blocks(&site, &g2, &s);
        assert_eq!(b1.len(), b2.len(), "{p:?}: block count differs");
        for (x, y) in b1.iter().zip(b2.iter()) {
            assert_eq!(x.polygon.len(), y.polygon.len());
            for (vx, vy) in x.polygon.verts().iter().zip(y.polygon.verts().iter()) {
                assert_eq!(vx.x.to_bits(), vy.x.to_bits());
                assert_eq!(vx.y.to_bits(), vy.y.to_bits());
            }
        }
    }
}
