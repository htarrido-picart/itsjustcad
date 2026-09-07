// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! `CornerLots` (plan §7.4, Phase 6). Where a block's interior angle is below
//! `corner_angle_max` (euro_latam 45°), the lot at that corner is WIDENED by
//! `corner_lot_width_bonus` (euro_latam +15 %, placeholder).
//!
//! Widening is a **true area transfer**: the shared boundary between the corner
//! lot and its adjacent (down-`dir`) neighbour is displaced along `dir` so the
//! corner lot gains exactly what the neighbour loses. Both pieces are re-derived
//! by clipping against that one displaced line, so Σ area is conserved and no
//! overlap is created. The displacement is clamped so the moved boundary stays
//! strictly inside the neighbour (never past its far edge) — the corner lot can
//! only grow into space that actually exists.

use crate::geometry::clip_bridge;
use crate::geometry::polygon2d::Polygon2d;
use crate::geometry::split::{split_by_line, Line2d};
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

/// The extent (lo, hi) of `poly`'s vertices projected onto unit `dir`.
fn extent(poly: &Polygon2d, dir: DVec2) -> (f64, f64) {
    let projs = poly.verts().iter().map(|v| v.dot(dir));
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for p in projs {
        lo = lo.min(p);
        hi = hi.max(p);
    }
    (lo, hi)
}

/// Result of a corner-lot area transfer: the grown corner lot and the shrunk
/// neighbour it took the area from.
pub struct CornerTransfer {
    pub corner: Polygon2d,
    pub neighbour: Polygon2d,
}

/// Move `extra` metres of frontage from `neighbour` to `corner` by displacing the
/// shared boundary between them along unit `dir` (from the corner toward the
/// neighbour). `corner` sits on the low-`dir` side, `neighbour` on the high side;
/// they meet at a line ⊥ `dir` at the corner's hi extent.
///
/// The displacement is clamped so the new boundary never passes the neighbour's
/// far edge (leaving it at least `min_keep` of its own width), so the transfer
/// takes only space that exists. Area is conserved exactly (the strip leaves the
/// neighbour and joins the corner via the SAME clip line) and no overlap results.
/// Returns `None` if the widen would be degenerate (extra ≤ 0, no room, or a clip
/// collapses a piece).
pub fn transfer_corner_widen(
    corner: &Polygon2d,
    neighbour: &Polygon2d,
    dir: DVec2,
    bonus: f64,
    min_keep: f64,
) -> Option<CornerTransfer> {
    if bonus <= 0.0 || dir.length_squared() < 1e-18 {
        return None;
    }
    let dir = dir.normalize();
    let (c_lo, c_hi) = extent(corner, dir);
    let (_n_lo, n_hi) = extent(neighbour, dir);
    // The corner's own frontage width, and the room available in the neighbour.
    let corner_width = c_hi - c_lo;
    if corner_width < 1e-9 {
        return None;
    }
    // Desired extra, clamped to the room the neighbour can actually give up while
    // keeping `min_keep` of its own frontage on the far side.
    let room = (n_hi - c_hi - min_keep.max(0.0)).max(0.0);
    let extra = (corner_width * bonus).min(room);
    if extra < 1e-6 {
        return None;
    }
    // The displaced shared boundary: a line ⊥ dir at projection c_hi + extra.
    // `Line2d::normal()` = (-dir.y, dir.x); dir·normal = 0, so a line whose
    // direction is `normal` has signed(q) = dir·(q - point). Put the point on the
    // boundary so signed(q) = dir·q - (c_hi + extra).
    let boundary = c_hi + extra;
    let perp = DVec2::new(-dir.y, dir.x);
    let line = Line2d::new(dir * boundary, perp);
    // signed(q) = normal·(q - point). normal = (-perp.y, perp.x) = (-dir.x, -dir.y)
    // = -dir. So signed(q) = -dir·q + dir·point = -(dir·q - boundary).
    // Positive side => dir·q < boundary (the corner side); negative => neighbour.
    // Take the UNION of the two lots implicitly: clip each against the line and
    // reassemble. Corner keeps its low side plus the strip taken from neighbour;
    // neighbour keeps only its far side.
    // The strip taken from the neighbour: neighbour ∩ {dir·q < boundary}; the far
    // piece the neighbour keeps: neighbour ∩ {dir·q > boundary}. The split is an
    // exact, area-conserving bisection of the neighbour.
    let (strip, neigh_keep) = split_by_line(neighbour, &line);
    let (neigh_new, strip) = match (neigh_keep, strip) {
        (Some(nk), Some(st)) => (nk, st),
        // Nothing to take, or the whole neighbour would go — bail (no transfer).
        _ => return None,
    };
    // The grown corner = corner ∪ strip, computed by the robust polygon boolean
    // (the same union the sliver merger uses). Both share the corner's old hi
    // edge, so the union is a single simple ring; area is exactly corner + strip.
    let corner_new = clip_bridge::union(corner, &strip)
        .into_iter()
        .max_by(|a, b| a.area().partial_cmp(&b.area()).unwrap_or(std::cmp::Ordering::Equal))?;
    Some(CornerTransfer {
        corner: corner_new,
        neighbour: neigh_new,
    })
}

/// Widen `lot` along `frontage_dir` by `bonus` (a fraction, e.g. 0.15 = +15 %),
/// clamped so the extra width never exceeds `slack`. Returns the widened polygon,
/// or the input unchanged if the widen would be degenerate. This is a pure
/// translation of the far-side vertices (kept for the isolated unit tests and as
/// a fallback); the area-conserving path uses [`transfer_corner_widen`].
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
    let (lo, hi) = extent(lot, dir);
    let width = hi - lo;
    if width < 1e-9 {
        return lot.clone();
    }
    let extra = (width * bonus).min(slack.max(0.0));
    if extra < 1e-9 {
        return lot.clone();
    }
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
    fn transfer_conserves_area_and_no_overlap() {
        // Two abutting 20×30 lots along +x: corner [0,20], neighbour [20,40].
        let corner = Polygon2d::from_pairs([(0.0, 0.0), (20.0, 0.0), (20.0, 30.0), (0.0, 30.0)])
            .unwrap();
        let neigh = Polygon2d::from_pairs([(20.0, 0.0), (40.0, 0.0), (40.0, 30.0), (20.0, 30.0)])
            .unwrap();
        let before = corner.area() + neigh.area();
        let t = transfer_corner_widen(&corner, &neigh, DVec2::X, 0.15, 1.0).expect("transfer");
        // Corner grew, neighbour shrank, total conserved.
        assert!(t.corner.area() > corner.area(), "corner should grow");
        assert!(t.neighbour.area() < neigh.area(), "neighbour should shrink");
        let after = t.corner.area() + t.neighbour.area();
        assert!((after - before).abs() < 1e-6, "area not conserved: {before} -> {after}");
        // The transfer moved exactly +3 m of frontage (15% of 20).
        let (clo, chi) = t.corner.aabb();
        assert!((chi.x - clo.x - 23.0).abs() < 1e-6, "corner frontage {}", chi.x - clo.x);
        // No overlap: corner's hi edge == neighbour's lo edge at x=23.
        let (nlo, _nhi) = t.neighbour.aabb();
        assert!((nlo.x - 23.0).abs() < 1e-6, "neighbour lo {}", nlo.x);
    }

    #[test]
    fn transfer_clamped_when_no_room() {
        // Neighbour is only 2 m wide and we must keep 1 m: room = 1 m, so a 15% of
        // 20 = 3 m request is clamped to 1 m.
        let corner = Polygon2d::from_pairs([(0.0, 0.0), (20.0, 0.0), (20.0, 30.0), (0.0, 30.0)])
            .unwrap();
        let neigh = Polygon2d::from_pairs([(20.0, 0.0), (22.0, 0.0), (22.0, 30.0), (20.0, 30.0)])
            .unwrap();
        let t = transfer_corner_widen(&corner, &neigh, DVec2::X, 0.15, 1.0).expect("transfer");
        let (clo, chi) = t.corner.aabb();
        assert!((chi.x - clo.x - 21.0).abs() < 1e-6, "corner clamped to +1 m, got {}", chi.x - clo.x);
    }
}
