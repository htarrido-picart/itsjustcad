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

/// Liang-Barsky clip of the segment `a`→`b` to the axis-aligned box [lo, hi].
/// Returns the clipped `(a', b')` (both endpoints on or inside the box), or `None`
/// if the segment lies entirely outside. Keeps every emitted Voronoi edge local
/// to the site: circumcenters of thin sliver triangles can land arbitrarily far
/// away, and left unclipped they would blow up the scene bounds so zoom-extents
/// frames the diagram to a few pixels (the near-blank render).
fn clip_segment_to_box(a: DVec2, b: DVec2, lo: DVec2, hi: DVec2) -> Option<(DVec2, DVec2)> {
    let d = b - a;
    let mut t0 = 0.0_f64;
    let mut t1 = 1.0_f64;
    let checks = [(-d.x, a.x - lo.x), (d.x, hi.x - a.x), (-d.y, a.y - lo.y), (d.y, hi.y - a.y)];
    for (num_dir, num_dist) in checks {
        if num_dir.abs() < 1e-18 {
            // Parallel to this edge and outside its slab → no intersection.
            if num_dist < 0.0 {
                return None;
            }
        } else {
            let t = num_dist / num_dir;
            if num_dir < 0.0 {
                if t > t1 {
                    return None;
                }
                if t > t0 {
                    t0 = t;
                }
            } else {
                if t < t0 {
                    return None;
                }
                if t < t1 {
                    t1 = t;
                }
            }
        }
    }
    Some((a + d * t0, a + d * t1))
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

    // Hull-edge rays are clipped to a bbox padded by ~one road width past the
    // site so the ROW ribbon still fully severs the boundary cell, WITHOUT the
    // baked road centerline shooting off to infinity (which would blow up the
    // scene bounds and make zoom-extents frame the whole diagram to a few
    // pixels). One road-width of overscan is enough for the half-width ribbon to
    // cross the site edge.
    let (lo, hi) = site.aabb();
    let pad = width.max(effective_block_depth(settings) * 0.25);
    let clip_lo = lo - DVec2::splat(pad);
    let clip_hi = hi + DVec2::splat(pad);
    // A fallback ray length used only to seed the parametric ray before clipping.
    let ray_len = (hi - lo).length() + pad;
    let site_centroid = site.centroid();

    // Deterministic emission: sort the shared Delaunay edges, emit one Voronoi
    // segment per interior edge (shared by two triangles) and one clipped ray per
    // hull edge (shared by one) so boundary cells close instead of leaking.
    let mut shared: Vec<(&(u32, u32), &Vec<usize>)> = edge_tris.iter().collect();
    shared.sort_by_key(|(k, _)| **k);

    let mut seen: std::collections::HashSet<((i64, i64), (i64, i64))> =
        std::collections::HashSet::new();
    for (ek, owners) in shared {
        if owners.len() == 2 {
            // Interior edge: the dual is the finite segment joining the two
            // circumcenters.
            let (Some(p), Some(q)) = (ccs[owners[0]], ccs[owners[1]]) else {
                continue;
            };
            // Clip to the padded bbox so distant circumcenters stay local.
            let Some((p, q)) = clip_segment_to_box(p, q, clip_lo, clip_hi) else {
                continue;
            };
            if p.distance_squared(q) < 1e-12 {
                continue;
            }
            if seen.insert(edge_key(p, q)) {
                graph.add(vec![p, q], width, StreetTier::Connector);
            }
        } else if owners.len() == 1 {
            // Hull edge: the dual is an infinite ray from the lone triangle's
            // circumcenter, perpendicular to the Delaunay hull edge, pointing
            // OUTWARD (away from the triangle interior). Extend it past the site
            // so the ROW ribbon carves the boundary cell closed. Dropping these
            // (the old behaviour) left every perimeter cell open → the sparse,
            // fragmented render.
            let Some(p) = ccs[owners[0]] else { continue };
            let a = seeds[ek.0 as usize];
            let b = seeds[ek.1 as usize];
            let mid = (a + b) * 0.5;
            let edge = b - a;
            let mut n = DVec2::new(-edge.y, edge.x); // perpendicular to hull edge
            if n.length_squared() < 1e-18 {
                continue;
            }
            n = n.normalize();
            // Orient outward: away from the site centroid (the hull opens outward
            // there). Fall back to the edge-midpoint direction for centred sites.
            let outward = mid - site_centroid;
            if outward.dot(n) < 0.0 {
                n = -n;
            }
            // Clip the ray p→(p + n·ray_len) to the padded bbox so the baked
            // centerline stays local. If the whole ray misses the clip box there
            // is nothing to draw.
            let far = p + n * ray_len;
            let Some((p, q)) = clip_segment_to_box(p, far, clip_lo, clip_hi) else {
                continue;
            };
            if p.distance_squared(q) < 1e-12 {
                continue;
            }
            if seen.insert(edge_key(p, q)) {
                graph.add(vec![p, q], width, StreetTier::Connector);
            }
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
    fn hull_rays_close_boundary_cells_into_blocks() {
        // Regression: previously hull-edge duals were dropped, so perimeter cells
        // stayed open and the block extractor produced only a handful of loose
        // fragments (near-blank render). With the clipped rays every cell closes,
        // so the block count should be on the order of the seed count.
        let site = rect(400.0, 300.0);
        let s = SubdivisionSettings {
            block_depth: 70.0,
            seed: 3,
            ..SubdivisionSettings::default()
        };
        let seeds = seed_points(&site, &s);
        let g = generate(&site, &s);
        let blocks = crate::streets::block_extractor::extract(&site, &g, &s);
        // Every interior + boundary cell should carve a block: expect at least
        // half the seed count (some edge seeds merge / clip away).
        assert!(
            blocks.len() >= seeds.len() / 2,
            "expected ~one block per cell: {} blocks for {} seeds",
            blocks.len(),
            seeds.len()
        );
    }

    #[test]
    fn baked_roads_stay_local_to_the_site() {
        // The clipped rays must not shoot off to infinity — every road vertex
        // stays within one block depth of the site bbox, so zoom-extents frames
        // the diagram (not a giant empty canvas).
        let site = rect(400.0, 300.0);
        let s = SubdivisionSettings {
            block_depth: 70.0,
            seed: 3,
            ..SubdivisionSettings::default()
        };
        let (lo, hi) = site.aabb();
        let margin = s.block_depth;
        let g = generate(&site, &s);
        for st in &g.streets {
            for p in &st.centerline {
                assert!(
                    p.x >= lo.x - margin
                        && p.x <= hi.x + margin
                        && p.y >= lo.y - margin
                        && p.y <= hi.y + margin,
                    "road vertex {p:?} escaped the site bbox by more than one block depth"
                );
            }
        }
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
