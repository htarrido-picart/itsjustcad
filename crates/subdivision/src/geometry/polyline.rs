// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! `PolylineTools` — resample, simplify (Douglas–Peucker), perpendicular-at-
//! param, and arc-length for open 2D polylines. Used by offset/skeleton
//! subdivision (later phases) and by contour snapping in Phase 3.

use glam::DVec2;

/// A collection of stateless polyline utilities. Points are an ordered open
/// polyline (not implicitly closed).
pub struct PolylineTools;

impl PolylineTools {
    /// Total arc length of the polyline.
    pub fn arc_length(pts: &[DVec2]) -> f64 {
        pts.windows(2).map(|w| w[0].distance(w[1])).sum()
    }

    /// Cumulative arc length at each vertex (`out[0] == 0`).
    pub fn cumulative(pts: &[DVec2]) -> Vec<f64> {
        let mut acc = 0.0;
        let mut out = Vec::with_capacity(pts.len());
        for (i, p) in pts.iter().enumerate() {
            if i > 0 {
                acc += pts[i - 1].distance(*p);
            }
            out.push(acc);
        }
        out
    }

    /// Point at normalized parameter `t` in `[0,1]` along the polyline by arc
    /// length.
    pub fn point_at(pts: &[DVec2], t: f64) -> DVec2 {
        if pts.is_empty() {
            return DVec2::ZERO;
        }
        if pts.len() == 1 {
            return pts[0];
        }
        let total = Self::arc_length(pts);
        if total < 1e-12 {
            return pts[0];
        }
        let target = t.clamp(0.0, 1.0) * total;
        let mut acc = 0.0;
        for w in pts.windows(2) {
            let seg = w[0].distance(w[1]);
            if acc + seg >= target {
                let local = if seg < 1e-15 { 0.0 } else { (target - acc) / seg };
                return w[0].lerp(w[1], local);
            }
            acc += seg;
        }
        *pts.last().unwrap()
    }

    /// Unit tangent at normalized parameter `t`.
    pub fn tangent_at(pts: &[DVec2], t: f64) -> DVec2 {
        if pts.len() < 2 {
            return DVec2::X;
        }
        let total = Self::arc_length(pts);
        if total < 1e-12 {
            return DVec2::X;
        }
        let target = t.clamp(0.0, 1.0) * total;
        let mut acc = 0.0;
        for w in pts.windows(2) {
            let seg = w[0].distance(w[1]);
            if acc + seg >= target || acc + seg >= total - 1e-12 {
                let d = w[1] - w[0];
                if d.length_squared() > 1e-18 {
                    return d.normalize();
                }
            }
            acc += seg;
        }
        DVec2::X
    }

    /// Left-hand perpendicular (unit) at normalized parameter `t`.
    pub fn perpendicular_at(pts: &[DVec2], t: f64) -> DVec2 {
        let tan = Self::tangent_at(pts, t);
        DVec2::new(-tan.y, tan.x)
    }

    /// Resample the polyline to `n` points evenly spaced by arc length
    /// (endpoints preserved). `n >= 2`.
    pub fn resample(pts: &[DVec2], n: usize) -> Vec<DVec2> {
        if pts.is_empty() || n == 0 {
            return Vec::new();
        }
        if n == 1 {
            return vec![pts[0]];
        }
        (0..n)
            .map(|i| Self::point_at(pts, i as f64 / (n - 1) as f64))
            .collect()
    }

    /// Douglas–Peucker simplification with perpendicular-distance tolerance
    /// `eps`. Endpoints always kept.
    pub fn simplify(pts: &[DVec2], eps: f64) -> Vec<DVec2> {
        if pts.len() <= 2 || eps <= 0.0 {
            return pts.to_vec();
        }
        let mut keep = vec![false; pts.len()];
        keep[0] = true;
        keep[pts.len() - 1] = true;
        dp(pts, 0, pts.len() - 1, eps, &mut keep);
        pts.iter()
            .zip(keep)
            .filter_map(|(p, k)| if k { Some(*p) } else { None })
            .collect()
    }
}

fn dp(pts: &[DVec2], lo: usize, hi: usize, eps: f64, keep: &mut [bool]) {
    if hi <= lo + 1 {
        return;
    }
    let a = pts[lo];
    let b = pts[hi];
    let mut best_i = lo;
    let mut best_d = 0.0;
    for (i, &p) in pts.iter().enumerate().take(hi).skip(lo + 1) {
        let d = perp_dist(p, a, b);
        if d > best_d {
            best_d = d;
            best_i = i;
        }
    }
    if best_d > eps {
        keep[best_i] = true;
        dp(pts, lo, best_i, eps, keep);
        dp(pts, best_i, hi, eps, keep);
    }
}

/// Perpendicular distance of `p` from the segment `a→b`.
fn perp_dist(p: DVec2, a: DVec2, b: DVec2) -> f64 {
    let ab = b - a;
    let len = ab.length();
    if len < 1e-15 {
        return p.distance(a);
    }
    (ab.perp_dot(p - a)).abs() / len
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arc_length_of_l() {
        let pts = vec![DVec2::new(0.0, 0.0), DVec2::new(3.0, 0.0), DVec2::new(3.0, 4.0)];
        assert!((PolylineTools::arc_length(&pts) - 7.0).abs() < 1e-12);
    }

    #[test]
    fn point_at_midpoint() {
        let pts = vec![DVec2::new(0.0, 0.0), DVec2::new(10.0, 0.0)];
        let m = PolylineTools::point_at(&pts, 0.5);
        assert!((m - DVec2::new(5.0, 0.0)).length() < 1e-12);
    }

    #[test]
    fn resample_count_and_endpoints() {
        let pts = vec![DVec2::new(0.0, 0.0), DVec2::new(6.0, 0.0), DVec2::new(6.0, 6.0)];
        let r = PolylineTools::resample(&pts, 5);
        assert_eq!(r.len(), 5);
        assert!((r[0] - pts[0]).length() < 1e-12);
        assert!((r[4] - *pts.last().unwrap()).length() < 1e-12);
    }

    #[test]
    fn simplify_drops_collinear() {
        let pts = vec![
            DVec2::new(0.0, 0.0),
            DVec2::new(1.0, 0.0),
            DVec2::new(2.0, 0.0),
            DVec2::new(3.0, 0.0),
            DVec2::new(3.0, 2.0),
        ];
        let s = PolylineTools::simplify(&pts, 0.01);
        // Collinear middle points removed → corner + endpoints.
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn perpendicular_is_unit_and_orthogonal() {
        let pts = vec![DVec2::new(0.0, 0.0), DVec2::new(10.0, 0.0)];
        let perp = PolylineTools::perpendicular_at(&pts, 0.5);
        assert!((perp.length() - 1.0).abs() < 1e-12);
        let tan = PolylineTools::tangent_at(&pts, 0.5);
        assert!(perp.dot(tan).abs() < 1e-12);
    }
}
