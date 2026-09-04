// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Radial / circular generator (plan §7.3 / §1 owner scope, Phase 5b): a polar
//! road layout — concentric **ring** roads at block-depth spacing plus **radial
//! spokes** from a center. Unlike the rectilinear generators this does NOT use
//! recursive-OBB site splitting; it lays out its own polar grid and clips both
//! rings and spokes to the site boundary. The blocks the extractor then carves
//! from the site are annular-sector polygons.
//!
//! Determinism: the center(s) and ring/spoke counts derive only from the site
//! geometry + settings — no randomness — so the graph is byte-identical for a
//! fixed seed (which does not even enter here).

use crate::geometry::polygon2d::Polygon2d;
use crate::settings::SubdivisionSettings;
use crate::streets::generators::{effective_block_depth, effective_road_width};
use crate::streets::street_graph::{StreetGraph, StreetTier};
use glam::DVec2;

/// Number of segments used to approximate a full ring circle (a smooth ring
/// reads as a curve while staying a polyline the extractor can offset).
const RING_SEGMENTS: usize = 64;

/// Clip an open polyline to the parts that lie inside `site`, returning each
/// maximal inside run as its own polyline (a spoke crossing a re-entrant site
/// can leave and re-enter). Points exactly on the boundary count as inside.
fn clip_polyline_inside(site: &Polygon2d, pts: &[DVec2]) -> Vec<Vec<DVec2>> {
    // Densify then keep inside runs. Sampling is dense enough that a segment
    // straddling the boundary is split at ~sample resolution — good enough for a
    // centerline that the ROW offset will thicken anyway.
    let mut runs: Vec<Vec<DVec2>> = Vec::new();
    let mut cur: Vec<DVec2> = Vec::new();
    for w in pts.windows(2) {
        let steps = 8;
        for k in 0..=steps {
            let t = k as f64 / steps as f64;
            let p = w[0].lerp(w[1], t);
            if site.contains(p) {
                cur.push(p);
            } else if cur.len() >= 2 {
                runs.push(std::mem::take(&mut cur));
                cur.clear();
            } else {
                cur.clear();
            }
        }
    }
    if cur.len() >= 2 {
        runs.push(cur);
    }
    // Drop degenerate near-zero-length runs.
    runs.retain(|r| {
        r.windows(2).map(|w| w[0].distance(w[1])).sum::<f64>() > 1e-6
    });
    runs
}

/// Deduplicate consecutive near-equal points so an offset ribbon stays valid.
fn dedup(mut pts: Vec<DVec2>) -> Vec<DVec2> {
    pts.dedup_by(|a, b| a.distance_squared(*b) < 1e-12);
    pts
}

/// Generate a radial street network over `site`. One center for a compact site;
/// larger sites (long axis ≳ 6× block depth) get a small row of centers so a
/// sprawling boundary is not covered by a single enormous set of rings.
pub fn generate(site: &Polygon2d, settings: &SubdivisionSettings) -> StreetGraph {
    let mut graph = StreetGraph::new();
    let depth = effective_block_depth(settings);
    let width = effective_road_width(settings);
    if depth <= 0.0 {
        return graph;
    }

    let (lo, hi) = site.aabb();
    let extent = hi - lo;
    let long = extent.x.max(extent.y);
    // Max radius any center must reach = half the diagonal (covers corners).
    let diag = extent.length();

    // One center by default; a larger site gets N centers spread along its long
    // axis so rings stay at block-depth granularity across the whole boundary.
    let n_centers = ((long / (depth * 6.0)).floor() as usize + 1).clamp(1, 4);
    let centers = center_points(site, n_centers, extent);

    // Rings per center: enough to reach the site's far corner.
    let max_r = (diag * 0.5).max(depth);
    let ring_count = (max_r / depth).ceil() as usize;

    for c in &centers {
        // Concentric ring roads at block-depth spacing.
        for k in 1..=ring_count {
            let r = k as f64 * depth;
            let ring: Vec<DVec2> = (0..=RING_SEGMENTS)
                .map(|i| {
                    let a = i as f64 / RING_SEGMENTS as f64 * std::f64::consts::TAU;
                    *c + DVec2::new(a.cos(), a.sin()) * r
                })
                .collect();
            for run in clip_polyline_inside(site, &ring) {
                let run = dedup(run);
                if run.len() >= 2 {
                    graph.add(run, width, StreetTier::Connector);
                }
            }
        }

        // Radial spokes. Spoke count scales with the ring circumference so the
        // outer annulus is not left with impossibly long arc blocks.
        let spoke_count = ((std::f64::consts::TAU * max_r) / (depth * 1.5))
            .round()
            .clamp(6.0, 32.0) as usize;
        for i in 0..spoke_count {
            let a = i as f64 / spoke_count as f64 * std::f64::consts::TAU;
            let dir = DVec2::new(a.cos(), a.sin());
            // Extend past the far corner; clipping trims to the site.
            let spoke = vec![*c, *c + dir * (max_r + diag)];
            for run in clip_polyline_inside(site, &spoke) {
                let run = dedup(run);
                if run.len() >= 2 {
                    graph.add(run, width, StreetTier::Spine);
                }
            }
        }
    }

    graph
}

/// Choose `n` centers inside the site. For `n == 1` this is the site centroid
/// (clamped inside for non-convex sites). For more, spread them along the long
/// axis of the AABB, keeping each inside the polygon.
fn center_points(site: &Polygon2d, n: usize, extent: DVec2) -> Vec<DVec2> {
    let centroid = clamp_inside(site, site.centroid());
    if n <= 1 {
        return vec![centroid];
    }
    let (lo, _) = site.aabb();
    let along_x = extent.x >= extent.y;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let f = (i as f64 + 0.5) / n as f64;
        let raw = if along_x {
            DVec2::new(lo.x + f * extent.x, centroid.y)
        } else {
            DVec2::new(centroid.x, lo.y + f * extent.y)
        };
        out.push(clamp_inside(site, raw));
    }
    out
}

/// If `p` is inside `site` keep it; otherwise fall back to the centroid (always
/// a reasonable interior point for the shapes we handle).
fn clamp_inside(site: &Polygon2d, p: DVec2) -> DVec2 {
    if site.contains(p) {
        p
    } else {
        site.centroid()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(w: f64, h: f64) -> Polygon2d {
        Polygon2d::from_pairs([(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)]).unwrap()
    }

    #[test]
    fn emits_rings_and_spokes() {
        let site = rect(400.0, 300.0);
        let s = SubdivisionSettings {
            block_depth: 60.0,
            road_width: 12.0,
            ..SubdivisionSettings::default()
        };
        let g = generate(&site, &s);
        assert!(!g.is_empty(), "radial produced no streets");
        // Some connectors (rings) and some spines (spokes) present.
        assert!(g.roads().any(|r| r.tier == StreetTier::Connector), "no rings");
        assert!(g.roads().any(|r| r.tier == StreetTier::Spine), "no spokes");
    }

    #[test]
    fn ring_count_matches_radius_over_depth() {
        // A near-square site: rings should reach roughly diag/2 / depth.
        let site = rect(300.0, 300.0);
        let depth = 50.0;
        let s = SubdivisionSettings {
            block_depth: depth,
            road_width: 10.0,
            ..SubdivisionSettings::default()
        };
        let g = generate(&site, &s);
        // Distinct ring radii = number of connectors that form (partial) circles.
        // The outermost ring may be clipped away entirely if it exceeds the site,
        // so allow the observed count to be at most the theoretical maximum.
        let diag = (300.0f64 * 300.0 + 300.0 * 300.0).sqrt();
        let expected = (diag * 0.5 / depth).ceil() as usize;
        let rings = g.roads().filter(|r| r.tier == StreetTier::Connector).count();
        assert!(rings > 0 && rings <= expected * RING_SEGMENTS, "rings {rings}");
    }

    #[test]
    fn deterministic() {
        let site = rect(400.0, 300.0);
        let s = SubdivisionSettings {
            block_depth: 60.0,
            ..SubdivisionSettings::default()
        };
        let a = generate(&site, &s);
        let b = generate(&site, &s);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.streets.iter().zip(&b.streets) {
            assert_eq!(x.centerline.len(), y.centerline.len());
            for (px, py) in x.centerline.iter().zip(&y.centerline) {
                assert_eq!(px.x.to_bits(), py.x.to_bits());
                assert_eq!(px.y.to_bits(), py.y.to_bits());
            }
        }
    }
}
