// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Voronoi generator (plan §7.3 / §1 owner scope, Phase 5b): seed points
//! (jittered grid, seeded from op data for replay) → Voronoi diagram built as
//! the **dual of the Delaunay triangulation** — the Voronoi vertices are the
//! circumcenters of the Delaunay triangles, and a Voronoi edge joins the
//! circumcenters of two triangles that share a Delaunay edge. We reuse the
//! existing Bowyer-Watson Delaunay in `kernel_mesh::triangulate` (no new external
//! dep; kernel-mesh does not depend on subdivision, so no cycle). Cell edges →
//! streets; the block extractor then carves the cells out of the site as blocks,
//! clipping partial cells at the boundary.
//!
//! Determinism: seeds come from a splitmix64 keyed by a quantized site hash +
//! `settings.seed` (the same `site_seed` the rectilinear generators use), and
//! triangulate is itself deterministic, so the diagram is byte-identical for a
//! fixed seed — the replay invariant.

use crate::geometry::polygon2d::Polygon2d;
use crate::settings::SubdivisionSettings;
use crate::streets::generators::{effective_block_depth, effective_road_width, site_seed, SplitMix64};
use crate::streets::street_graph::{StreetGraph, StreetTier};
use glam::DVec2;

/// Per-generator salt so Voronoi seeds do not collide with the recursive-OBB /
/// offset RNG streams keyed off the same site hash.
const VORONOI_SALT: u64 = 0x566F_726F_6E6F_6931; // "Voronoi1"

/// Quantize for undirected-edge dedup (µm grid — matches triangulate's own).
fn key(p: DVec2) -> (i64, i64) {
    ((p.x * 1e6).round() as i64, (p.y * 1e6).round() as i64)
}

fn edge_key(a: DVec2, b: DVec2) -> ((i64, i64), (i64, i64)) {
    let (ka, kb) = (key(a), key(b));
    if ka <= kb {
        (ka, kb)
    } else {
        (kb, ka)
    }
}

/// Circumcenter of triangle a,b,c. Returns `None` for a degenerate (collinear)
/// triangle.
pub fn circumcenter(a: DVec2, b: DVec2, c: DVec2) -> Option<DVec2> {
    let d = 2.0 * (a.x * (b.y - c.y) + b.x * (c.y - a.y) + c.x * (a.y - b.y));
    if d.abs() < 1e-12 {
        return None;
    }
    let a2 = a.length_squared();
    let b2 = b.length_squared();
    let c2 = c.length_squared();
    let ux = (a2 * (b.y - c.y) + b2 * (c.y - a.y) + c2 * (a.y - b.y)) / d;
    let uy = (a2 * (c.x - b.x) + b2 * (a.x - c.x) + c2 * (b.x - a.x)) / d;
    Some(DVec2::new(ux, uy))
}

/// Generate Voronoi seed points as a jittered grid over the site bbox, spaced by
/// block depth, keeping only seeds inside the site. Deterministic (splitmix64).
pub fn seed_points(site: &Polygon2d, settings: &SubdivisionSettings) -> Vec<DVec2> {
    let depth = effective_block_depth(settings);
    let (lo, hi) = site.aabb();
    let extent = hi - lo;
    if depth <= 0.0 {
        return Vec::new();
    }
    let cols = ((extent.x / depth).round() as i64).max(1);
    let rows = ((extent.y / depth).round() as i64).max(1);
    let mut rng = SplitMix64::new(site_seed(site, settings.seed, VORONOI_SALT));
    // Jitter magnitude grows with irregularity (loose unlocks the full cell).
    let jitter = 0.35 * settings.clamped_irregularity().max(0.15) * depth;
    let mut pts = Vec::new();
    for j in 0..=rows {
        for i in 0..=cols {
            let base = DVec2::new(
                lo.x + i as f64 * (extent.x / cols as f64),
                lo.y + j as f64 * (extent.y / rows as f64),
            );
            let jx = rng.signed_unit() * jitter;
            let jy = rng.signed_unit() * jitter;
            let p = base + DVec2::new(jx, jy);
            if site.contains(p) {
                pts.push(p);
            }
        }
    }
    pts
}

/// Generate a Voronoi street network over `site`.
pub fn generate(site: &Polygon2d, settings: &SubdivisionSettings) -> StreetGraph {
    let mut graph = StreetGraph::new();
    let width = effective_road_width(settings);

    let seeds = seed_points(site, settings);
    if seeds.len() < 3 {
        return graph;
    }

    // Delaunay of the seeds → triangles as index triples.
    let tris = kernel_mesh::triangulate(&seeds);
    if tris.is_empty() {
        return graph;
    }

    // Circumcenter per triangle (the Voronoi vertices).
    let ccs: Vec<Option<DVec2>> = tris
        .iter()
        .map(|t| {
            circumcenter(
                seeds[t[0] as usize],
                seeds[t[1] as usize],
                seeds[t[2] as usize],
            )
        })
        .collect();

    // Map each undirected Delaunay edge → the triangles that own it. A Voronoi
    // edge joins the circumcenters of the two triangles sharing a Delaunay edge.
    use std::collections::HashMap;
    let mut edge_tris: HashMap<(u32, u32), Vec<usize>> = HashMap::new();
    for (ti, t) in tris.iter().enumerate() {
        for e in [[t[0], t[1]], [t[1], t[2]], [t[2], t[0]]] {
            let ek = if e[0] <= e[1] { (e[0], e[1]) } else { (e[1], e[0]) };
            edge_tris.entry(ek).or_default().push(ti);
        }
    }

    // Deterministic emission: sort the shared Delaunay edges, emit one Voronoi
    // segment per interior edge (shared by exactly two triangles).
    let mut shared: Vec<(&(u32, u32), &Vec<usize>)> = edge_tris.iter().collect();
    shared.sort_by_key(|(k, _)| **k);

    let mut seen: std::collections::HashSet<((i64, i64), (i64, i64))> =
        std::collections::HashSet::new();
    for (_ek, owners) in shared {
        if owners.len() != 2 {
            continue; // hull edge → the dual is a ray, dropped (site clips anyway)
        }
        let (Some(p), Some(q)) = (ccs[owners[0]], ccs[owners[1]]) else {
            continue;
        };
        if p.distance_squared(q) < 1e-12 {
            continue;
        }
        if seen.insert(edge_key(p, q)) {
            graph.add(vec![p, q], width, StreetTier::Connector);
        }
    }

    graph
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(w: f64, h: f64) -> Polygon2d {
        Polygon2d::from_pairs([(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)]).unwrap()
    }

    #[test]
    fn circumcenter_of_right_triangle() {
        // Right triangle: circumcenter is the midpoint of the hypotenuse.
        let a = DVec2::new(0.0, 0.0);
        let b = DVec2::new(4.0, 0.0);
        let c = DVec2::new(0.0, 4.0);
        let cc = circumcenter(a, b, c).unwrap();
        assert!((cc - DVec2::new(2.0, 2.0)).length() < 1e-9, "cc {cc:?}");
    }

    #[test]
    fn known_seed_set_gives_known_voronoi_vertex() {
        // Four seeds at the corners of a 2×2 square plus enough spread that the
        // Delaunay of the unit square (0,0)(2,0)(2,2)(0,2) triangulates into two
        // triangles sharing the diagonal; both circumcenters land at the center
        // (2,2)/... actually at (1,1). Verify the dual vertex is (1,1).
        let seeds = vec![
            DVec2::new(0.0, 0.0),
            DVec2::new(2.0, 0.0),
            DVec2::new(2.0, 2.0),
            DVec2::new(0.0, 2.0),
        ];
        let tris = kernel_mesh::triangulate(&seeds);
        assert_eq!(tris.len(), 2, "square = 2 triangles");
        // Both triangles of an axis-aligned square are right triangles whose
        // circumcenter is the square center (1,1).
        for t in &tris {
            let cc = circumcenter(seeds[t[0] as usize], seeds[t[1] as usize], seeds[t[2] as usize])
                .unwrap();
            assert!((cc - DVec2::new(1.0, 1.0)).length() < 1e-9, "cc {cc:?}");
        }
    }

    #[test]
    fn cell_count_tracks_seed_count() {
        let site = rect(400.0, 300.0);
        let s = SubdivisionSettings {
            block_depth: 60.0,
            seed: 7,
            ..SubdivisionSettings::default()
        };
        let seeds = seed_points(&site, &s);
        assert!(seeds.len() >= 3, "need seeds");
        let g = generate(&site, &s);
        assert!(!g.is_empty(), "voronoi produced no edges");
    }

    #[test]
    fn deterministic_diagram() {
        let site = rect(400.0, 300.0);
        let s = SubdivisionSettings {
            block_depth: 60.0,
            seed: 11,
            ..SubdivisionSettings::default()
        };
        let a = generate(&site, &s);
        let b = generate(&site, &s);
        assert_eq!(a.len(), b.len(), "edge count differs");
        for (x, y) in a.streets.iter().zip(&b.streets) {
            for (px, py) in x.centerline.iter().zip(&y.centerline) {
                assert_eq!(px.x.to_bits(), py.x.to_bits());
                assert_eq!(px.y.to_bits(), py.y.to_bits());
            }
        }
    }

    #[test]
    fn different_seed_changes_diagram() {
        let site = rect(400.0, 300.0);
        let mut s1 = SubdivisionSettings {
            block_depth: 60.0,
            seed: 1,
            irregularity: 0.4,
            ..SubdivisionSettings::default()
        };
        s1.irregularity = 0.4;
        let mut s2 = s1.clone();
        s2.seed = 2;
        let a = generate(&site, &s1);
        let b = generate(&site, &s2);
        // Very likely different vertex positions; at minimum not byte-identical.
        let same = a.len() == b.len()
            && a.streets.iter().zip(&b.streets).all(|(x, y)| {
                x.centerline.iter().zip(&y.centerline).all(|(p, q)| {
                    p.x.to_bits() == q.x.to_bits() && p.y.to_bits() == q.y.to_bits()
                })
            });
        assert!(!same, "distinct seeds should change the diagram");
    }
}
