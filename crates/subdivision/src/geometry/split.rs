// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Split a convex-or-concave simple polygon by an **infinite line** into the two
//! half-plane pieces, via Sutherland–Hodgman clipping against each half-plane.
//!
//! This is all the recursive OBB subdivider needs — it never requires general
//! polygon boolean ops or offset for Phase 3. For strongly non-convex polygons a
//! single line can in principle produce multiple disjoint pieces per side;
//! Sutherland–Hodgman returns one (possibly zero-area-bridged) ring per side.
//! Blocks fed to Phase-3 subdivision are convex or mildly concave, where this is
//! exact.

use crate::geometry::polygon2d::Polygon2d;
use glam::DVec2;

/// An infinite line defined by a point `p` and a unit direction `dir`.
/// The signed side of a query point `q` is `normal · (q - p)` where
/// `normal = (-dir.y, dir.x)`.
#[derive(Debug, Clone, Copy)]
pub struct Line2d {
    pub point: DVec2,
    pub dir: DVec2,
}

impl Line2d {
    pub fn new(point: DVec2, dir: DVec2) -> Line2d {
        Line2d {
            point,
            dir: dir.normalize(),
        }
    }

    /// Left-hand normal (points to the "positive" half-plane).
    pub fn normal(&self) -> DVec2 {
        DVec2::new(-self.dir.y, self.dir.x)
    }

    /// Signed distance of `q` from the line along `normal`.
    pub fn signed(&self, q: DVec2) -> f64 {
        self.normal().dot(q - self.point)
    }
}

/// Clip `verts` (a closed CCW ring) to the half-plane where `signed >= 0`
/// (`keep_positive = true`) or `signed <= 0` (`false`). Returns the clipped ring
/// vertices, or an empty vec if nothing remains.
fn clip_halfplane(verts: &[DVec2], line: &Line2d, keep_positive: bool) -> Vec<DVec2> {
    let n = verts.len();
    if n == 0 {
        return Vec::new();
    }
    let sign = if keep_positive { 1.0 } else { -1.0 };
    // "Inside" = on the kept side (with a tiny tolerance so points on the line
    // are kept by both sides, preserving shared edges exactly).
    let inside = |q: DVec2| line.signed(q) * sign >= -1e-9;
    let mut out: Vec<DVec2> = Vec::with_capacity(n + 2);
    for i in 0..n {
        let cur = verts[i];
        let prev = verts[(i + n - 1) % n];
        let cur_in = inside(cur);
        let prev_in = inside(prev);
        if cur_in {
            if !prev_in && let Some(x) = intersect(prev, cur, line) {
                out.push(x);
            }
            out.push(cur);
        } else if prev_in && let Some(x) = intersect(prev, cur, line) {
            out.push(x);
        }
    }
    out
}

/// Intersection of segment `a→b` with the infinite `line`, if the segment
/// crosses it.
fn intersect(a: DVec2, b: DVec2, line: &Line2d) -> Option<DVec2> {
    let da = line.signed(a);
    let db = line.signed(b);
    let denom = da - db;
    if denom.abs() < 1e-15 {
        return None;
    }
    let t = da / denom;
    Some(a + (b - a) * t)
}

/// Drop consecutive duplicate vertices (within tolerance) that clipping can
/// introduce, then return a valid `Polygon2d` if ≥3 vertices with non-trivial
/// area remain.
fn cleanup(verts: Vec<DVec2>) -> Option<Polygon2d> {
    if verts.len() < 3 {
        return None;
    }
    let mut dedup: Vec<DVec2> = Vec::with_capacity(verts.len());
    for v in verts {
        if dedup.last().map(|l| l.distance_squared(v) > 1e-18).unwrap_or(true) {
            dedup.push(v);
        }
    }
    // Remove wrap-around duplicate.
    if dedup.len() >= 2 && dedup[0].distance_squared(*dedup.last().unwrap()) < 1e-18 {
        dedup.pop();
    }
    let poly = Polygon2d::new(dedup)?;
    if poly.area() < 1e-12 {
        return None;
    }
    Some(poly)
}

/// Split `poly` by an infinite `line` into `(positive_side, negative_side)`.
/// Either side may be `None` if the line does not actually divide the polygon
/// (i.e. the whole polygon lies on one side).
pub fn split_by_line(poly: &Polygon2d, line: &Line2d) -> (Option<Polygon2d>, Option<Polygon2d>) {
    let pos = cleanup(clip_halfplane(poly.verts(), line, true));
    let neg = cleanup(clip_halfplane(poly.verts(), line, false));
    (pos, neg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(s: f64) -> Polygon2d {
        Polygon2d::from_pairs([(0.0, 0.0), (s, 0.0), (s, s), (0.0, s)]).unwrap()
    }

    #[test]
    fn vertical_split_halves_area() {
        let sq = square(10.0);
        let line = Line2d::new(DVec2::new(5.0, 0.0), DVec2::new(0.0, 1.0));
        let (a, b) = split_by_line(&sq, &line);
        let a = a.unwrap();
        let b = b.unwrap();
        assert!((a.area() - 50.0).abs() < 1e-9, "a area {}", a.area());
        assert!((b.area() - 50.0).abs() < 1e-9, "b area {}", b.area());
        // Areas sum to the whole.
        assert!((a.area() + b.area() - 100.0).abs() < 1e-9);
    }

    #[test]
    fn diagonal_split_conserves_area() {
        let sq = square(10.0);
        let line = Line2d::new(DVec2::new(5.0, 5.0), DVec2::new(1.0, 1.0));
        let (a, b) = split_by_line(&sq, &line);
        let a = a.unwrap();
        let b = b.unwrap();
        assert!((a.area() + b.area() - 100.0).abs() < 1e-8);
        // A diagonal through the center of a square gives two equal triangles.
        assert!((a.area() - 50.0).abs() < 1e-6);
        assert!((b.area() - 50.0).abs() < 1e-6);
    }

    #[test]
    fn line_outside_leaves_one_side_empty() {
        let sq = square(10.0);
        let line = Line2d::new(DVec2::new(20.0, 0.0), DVec2::new(0.0, 1.0));
        let (a, b) = split_by_line(&sq, &line);
        // Line at x=20 with dir (0,1): normal (-1,0), so signed = -(q.x-20) > 0
        // for the whole square (x ≤ 10). Positive side keeps everything; the
        // negative side is empty.
        assert!(b.is_none());
        assert!(a.is_some());
        assert!((a.unwrap().area() - 100.0).abs() < 1e-9);
    }

    #[test]
    fn offset_split_conserves_area() {
        let sq = square(10.0);
        let line = Line2d::new(DVec2::new(3.0, 0.0), DVec2::new(0.0, 1.0));
        let (a, b) = split_by_line(&sq, &line);
        let a = a.unwrap().area();
        let b = b.unwrap().area();
        assert!((a + b - 100.0).abs() < 1e-9);
        // one side 3×10=30, other 70
        let (lo, hi) = if a < b { (a, b) } else { (b, a) };
        assert!((lo - 30.0).abs() < 1e-6 && (hi - 70.0).abs() < 1e-6);
    }
}
