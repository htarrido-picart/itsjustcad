// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! `SliverMerger` (plan §7.4, Phase 6) — the single biggest professional-vs-
//! generated difference. Repeatedly merge any lot below
//! `sliver_area_frac × lot_area_min` (euro_latam frac 0.5) into its neighbour
//! with the LARGEST shared edge, until no sliver remains. Deterministic: the
//! merge order is by ascending area then a stable index, so the same input
//! yields byte-identical output.

use crate::geometry::clip_bridge;
use crate::geometry::polygon2d::Polygon2d;
use crate::subdivision::recursive_obb::Lot;
use glam::DVec2;

/// Length of shared boundary between two polygons: the total length of `a`'s
/// edges that lie (mostly) on an edge of `b`. Symmetric enough for ranking.
pub fn shared_edge_length(a: &Polygon2d, b: &Polygon2d) -> f64 {
    let b_edges: Vec<(DVec2, DVec2)> = b.edges().collect();
    let mut total = 0.0;
    for (a0, a1) in a.edges() {
        // Sample points along a's edge; the covered fraction × edge length is the
        // shared length contribution. Robust to non-identical vertex splits.
        let n = 8;
        let mut covered = 0usize;
        for k in 0..n {
            let t = (k as f64 + 0.5) / n as f64;
            let p = a0 + (a1 - a0) * t;
            if b_edges.iter().any(|&(b0, b1)| point_on_segment(p, b0, b1, 1e-3)) {
                covered += 1;
            }
        }
        total += a0.distance(a1) * (covered as f64 / n as f64);
    }
    total
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

/// Merge slivers below `threshold` area into their largest-shared-edge
/// neighbour, repeating until none remain (or no admissible merge exists).
/// `has_street` of the merged lot is the OR of the two (frontage is retained).
pub fn merge_slivers(mut lots: Vec<Lot>, threshold: f64) -> Vec<Lot> {
    if threshold <= 0.0 || lots.len() < 2 {
        return lots;
    }
    // Bound the loop: each successful merge drops one lot.
    let max_iters = lots.len() * 2 + 4;
    for _ in 0..max_iters {
        // Find the smallest sliver (deterministic: min area, then lowest index).
        let mut sliver: Option<usize> = None;
        let mut sliver_area = f64::INFINITY;
        for (i, l) in lots.iter().enumerate() {
            let a = l.polygon.area();
            if a < threshold && a < sliver_area - 1e-12 {
                sliver_area = a;
                sliver = Some(i);
            }
        }
        let Some(si) = sliver else { break };

        // Pick the neighbour with the largest shared edge (ties → lowest index).
        let mut best: Option<usize> = None;
        let mut best_share = 0.0;
        for (j, l) in lots.iter().enumerate() {
            if j == si {
                continue;
            }
            let share = shared_edge_length(&lots[si].polygon, &l.polygon);
            if share > best_share + 1e-9 {
                best_share = share;
                best = Some(j);
            }
            let _ = l;
        }
        let Some(bj) = best else {
            // No neighbour shares an edge — cannot merge this sliver; stop to
            // avoid an infinite loop. (A truly isolated sliver is left as-is.)
            break;
        };

        // Union the sliver into its neighbour.
        let merged = clip_bridge::union(&lots[si].polygon, &lots[bj].polygon);
        let Some(poly) = merged
            .into_iter()
            .max_by(|a, b| a.area().partial_cmp(&b.area()).unwrap_or(std::cmp::Ordering::Equal))
        else {
            break;
        };
        let has_street = lots[si].has_street || lots[bj].has_street;
        // Remove the higher index first so the lower stays valid.
        let (lo, hi) = if si < bj { (si, bj) } else { (bj, si) };
        lots.remove(hi);
        lots.remove(lo);
        lots.push(Lot { polygon: poly, has_street });
    }
    lots
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lot(poly: Polygon2d) -> Lot {
        Lot { polygon: poly, has_street: false }
    }

    #[test]
    fn shared_edge_measured() {
        let a = Polygon2d::from_pairs([(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)]).unwrap();
        let b = Polygon2d::from_pairs([(10.0, 0.0), (20.0, 0.0), (20.0, 10.0), (10.0, 10.0)]).unwrap();
        // They share the x=10 edge, length 10.
        assert!((shared_edge_length(&a, &b) - 10.0).abs() < 1.0);
    }

    #[test]
    fn sliver_merged_into_larger_neighbour() {
        // A big 10×10 lot and a 10×1 sliver sharing the x=10 edge.
        let big = lot(Polygon2d::from_pairs([(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)]).unwrap());
        let sliver = lot(Polygon2d::from_pairs([(10.0, 0.0), (11.0, 0.0), (11.0, 10.0), (10.0, 10.0)]).unwrap());
        let out = merge_slivers(vec![big, sliver], 50.0); // sliver area = 10 < 50
        assert_eq!(out.len(), 1, "sliver should merge away");
        // Area conserved: 100 + 10 = 110.
        assert!((out[0].polygon.area() - 110.0).abs() < 1.0);
    }

    #[test]
    fn no_sliver_left_after_merge() {
        let a = lot(Polygon2d::from_pairs([(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)]).unwrap());
        let b = lot(Polygon2d::from_pairs([(10.0, 0.0), (20.0, 0.0), (20.0, 10.0), (10.0, 10.0)]).unwrap());
        let s = lot(Polygon2d::from_pairs([(20.0, 0.0), (20.5, 0.0), (20.5, 10.0), (20.0, 10.0)]).unwrap());
        let out = merge_slivers(vec![a, b, s], 30.0);
        for l in &out {
            assert!(l.polygon.area() >= 30.0 - 1e-6, "sliver remains: {}", l.polygon.area());
        }
    }

    #[test]
    fn threshold_zero_is_noop() {
        let a = lot(Polygon2d::from_pairs([(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)]).unwrap());
        let out = merge_slivers(vec![a], 0.0);
        assert_eq!(out.len(), 1);
    }
}
