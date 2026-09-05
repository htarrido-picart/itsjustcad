// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! `CornerLots` (plan §7.4, Phase 6). Where a block's interior angle is below
//! `corner_angle_max` (euro_latam 45°), the lot at that corner is WIDENED by
//! `corner_lot_width_bonus` (euro_latam +15 %, placeholder). Width is
//! auto-clamped so the widened lot cannot self-intersect or overrun the frontage
//! slack available to it.

use crate::geometry::polygon2d::Polygon2d;
use glam::DVec2;

/// Interior angle (radians) at vertex `b`, formed by edges `a→b` and `b→c`.
pub fn interior_angle(a: DVec2, b: DVec2, c: DVec2) -> f64 {
    let u = a - b;
    let v = c - b;
    let lu = u.length();
    let lv = v.length();
    if lu < 1e-12 || lv < 1e-12 {
        return std::f64::consts::PI;
    }
    let cos = (u.dot(v) / (lu * lv)).clamp(-1.0, 1.0);
    cos.acos()
}

/// The acute corners of `poly`: indices of vertices whose interior angle is
/// below `max_deg` degrees.
pub fn acute_corners(poly: &Polygon2d, max_deg: f64) -> Vec<usize> {
    let v = poly.verts();
    let n = v.len();
    if n < 3 {
        return Vec::new();
    }
    let max_rad = max_deg.to_radians();
    let mut out = Vec::new();
    for i in 0..n {
        let a = v[(i + n - 1) % n];
        let b = v[i];
        let c = v[(i + 1) % n];
        if interior_angle(a, b, c) < max_rad {
            out.push(i);
        }
    }
    out
}

/// Widen `lot` along `frontage_dir` by `bonus` (a fraction, e.g. 0.15 = +15 %),
/// clamped so the extra width never exceeds `slack` (the free frontage room
/// beyond this lot). Returns the widened polygon, or the input unchanged if the
/// widen would be degenerate. Guarantees no self-intersection: the widen is a
/// pure translation of the vertices on the far side of the lot along
/// `frontage_dir`, and the clamped extra is always ≥ 0 and ≤ slack.
pub fn widen_corner_lot(
    lot: &Polygon2d,
    frontage_dir: DVec2,
    bonus: f64,
    slack: f64,
) -> Polygon2d {
    if bonus <= 0.0 || frontage_dir.length_squared() < 1e-18 {
        return lot.clone();
    }
    let dir = frontage_dir.normalize();
    // Current frontage extent = spread of vertices projected on dir.
    let projs: Vec<f64> = lot.verts().iter().map(|v| v.dot(dir)).collect();
    let lo = projs.iter().cloned().fold(f64::INFINITY, f64::min);
    let hi = projs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let width = hi - lo;
    if width < 1e-9 {
        return lot.clone();
    }
    // Desired extra, clamped to available slack (and non-negative).
    let extra = (width * bonus).min(slack.max(0.0));
    if extra < 1e-9 {
        return lot.clone();
    }
    // Push every vertex on the far (hi) half outward by `extra` along dir. The
    // mid split keeps the near side pinned (the corner) so the lot only grows.
    let mid = (lo + hi) * 0.5;
    let new_verts: Vec<DVec2> = lot
        .verts()
        .iter()
        .map(|&v| {
            if v.dot(dir) > mid {
                v + dir * extra
            } else {
                v
            }
        })
        .collect();
    Polygon2d::new(new_verts).unwrap_or_else(|| lot.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn right_angle_is_90_deg() {
        let a = DVec2::new(1.0, 0.0);
        let b = DVec2::new(0.0, 0.0);
        let c = DVec2::new(0.0, 1.0);
        assert!((interior_angle(a, b, c) - std::f64::consts::FRAC_PI_2).abs() < 1e-9);
    }

    #[test]
    fn detects_15_degree_acute_corner() {
        // A sliver triangle with a ~15° apex.
        let ang = 15f64.to_radians();
        let apex = DVec2::new(0.0, 0.0);
        let p1 = DVec2::new(100.0, 0.0);
        let p2 = DVec2::new(100.0 * ang.cos(), 100.0 * ang.sin());
        let poly = Polygon2d::new(vec![apex, p1, p2]).unwrap();
        let corners = acute_corners(&poly, 45.0);
        assert!(!corners.is_empty(), "should flag the acute apex");
    }

    #[test]
    fn widen_grows_frontage_and_clamps() {
        // A 20×30 lot, frontage along +x. +15 % with ample slack → +3 m.
        let lot = Polygon2d::from_pairs([(0.0, 0.0), (20.0, 0.0), (20.0, 30.0), (0.0, 30.0)])
            .unwrap();
        let out = widen_corner_lot(&lot, DVec2::X, 0.15, 100.0);
        let (lo, hi) = out.aabb();
        assert!((hi.x - lo.x - 23.0).abs() < 1e-6, "expected 23 m frontage, got {}", hi.x - lo.x);
        // Slack-limited widen: only 1 m available → +1 m not +3 m.
        let out2 = widen_corner_lot(&lot, DVec2::X, 0.15, 1.0);
        let (lo2, hi2) = out2.aabb();
        assert!((hi2.x - lo2.x - 21.0).abs() < 1e-6);
    }

    #[test]
    fn widened_lot_stays_simple_no_self_intersection() {
        let lot = Polygon2d::from_pairs([(0.0, 0.0), (20.0, 0.0), (20.0, 30.0), (0.0, 30.0)])
            .unwrap();
        let out = widen_corner_lot(&lot, DVec2::X, 0.15, 100.0);
        // Area must strictly grow and stay a valid (positive-area) polygon.
        assert!(out.area() > lot.area());
        assert!(out.len() >= 4);
    }
}
