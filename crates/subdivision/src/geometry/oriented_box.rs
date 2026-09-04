// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! `OrientedBox` — the minimum-area oriented bounding box of a point set.
//!
//! Computed by the rotating-calipers theorem: the minimum-area enclosing
//! rectangle has one side collinear with an edge of the convex hull. We build
//! the hull (Andrew's monotone chain, deterministic) and test every hull edge
//! as a candidate axis, keeping the smallest-area rectangle.
//!
//! Exposes the long axis, short axis, center, and half-extents so the recursive
//! OBB subdivider can split along the short direction pivoted on the long axis.

use crate::geometry::polygon2d::Polygon2d;
use glam::DVec2;

/// A minimum-area oriented rectangle. `long_axis` and `short_axis` are unit
/// vectors; `half_long`/`half_short` are half-extents along them.
#[derive(Debug, Clone, Copy)]
pub struct OrientedBox {
    pub center: DVec2,
    /// Unit direction of the longer side.
    pub long_axis: DVec2,
    /// Unit direction of the shorter side (perpendicular to `long_axis`).
    pub short_axis: DVec2,
    /// Half-extent along `long_axis` (so the full long side = `2 * half_long`).
    pub half_long: f64,
    /// Half-extent along `short_axis`.
    pub half_short: f64,
}

impl OrientedBox {
    /// Minimum-area OBB of a polygon's vertices.
    pub fn of_polygon(poly: &Polygon2d) -> Option<OrientedBox> {
        Self::of_points(poly.verts())
    }

    /// Minimum-area OBB enclosing `points`. Returns `None` for < 3 non-collinear
    /// points.
    pub fn of_points(points: &[DVec2]) -> Option<OrientedBox> {
        let hull = convex_hull(points);
        if hull.len() < 3 {
            return None;
        }
        let n = hull.len();
        let mut best: Option<OrientedBox> = None;
        for i in 0..n {
            let a = hull[i];
            let b = hull[(i + 1) % n];
            let edge = b - a;
            let len = edge.length();
            if len < 1e-12 {
                continue;
            }
            let ux = edge / len; // candidate x axis (along this hull edge)
            let uy = DVec2::new(-ux.y, ux.x); // perpendicular
            // Project all hull points onto (ux, uy).
            let mut min_x = f64::INFINITY;
            let mut max_x = f64::NEG_INFINITY;
            let mut min_y = f64::INFINITY;
            let mut max_y = f64::NEG_INFINITY;
            for &p in &hull {
                let d = p - a;
                let px = d.dot(ux);
                let py = d.dot(uy);
                min_x = min_x.min(px);
                max_x = max_x.max(px);
                min_y = min_y.min(py);
                max_y = max_y.max(py);
            }
            let w = max_x - min_x;
            let h = max_y - min_y;
            let area = w * h;
            let better = match &best {
                None => true,
                Some(bx) => area < bx.half_long * bx.half_short * 4.0 - 1e-9,
            };
            if better {
                let cx = (min_x + max_x) * 0.5;
                let cy = (min_y + max_y) * 0.5;
                let center = a + ux * cx + uy * cy;
                // Assign long/short by extent.
                let (long_axis, short_axis, half_long, half_short) = if w >= h {
                    (ux, uy, w * 0.5, h * 0.5)
                } else {
                    (uy, ux, h * 0.5, w * 0.5)
                };
                best = Some(OrientedBox {
                    center,
                    long_axis,
                    short_axis,
                    half_long,
                    half_short,
                });
            }
        }
        best
    }

    /// The four corner vertices, CCW.
    pub fn corners(&self) -> [DVec2; 4] {
        let l = self.long_axis * self.half_long;
        let s = self.short_axis * self.half_short;
        [
            self.center - l - s,
            self.center + l - s,
            self.center + l + s,
            self.center - l + s,
        ]
    }

    /// Full length of the long side.
    pub fn long_len(&self) -> f64 {
        self.half_long * 2.0
    }

    /// Full length of the short side.
    pub fn short_len(&self) -> f64 {
        self.half_short * 2.0
    }
}

/// Andrew's monotone-chain convex hull. Deterministic (sorts by (x, y)).
/// Returns hull vertices CCW, no repeated closing point.
pub fn convex_hull(points: &[DVec2]) -> Vec<DVec2> {
    let mut pts: Vec<DVec2> = points.to_vec();
    pts.sort_by(|a, b| {
        a.x.partial_cmp(&b.x)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.y.partial_cmp(&b.y).unwrap_or(std::cmp::Ordering::Equal))
    });
    pts.dedup_by(|a, b| a.distance_squared(*b) < 1e-20);
    let n = pts.len();
    if n < 3 {
        return pts;
    }
    let cross = |o: DVec2, a: DVec2, b: DVec2| (a - o).perp_dot(b - o);
    let mut lower: Vec<DVec2> = Vec::new();
    for &p in &pts {
        while lower.len() >= 2 && cross(lower[lower.len() - 2], lower[lower.len() - 1], p) <= 0.0 {
            lower.pop();
        }
        lower.push(p);
    }
    let mut upper: Vec<DVec2> = Vec::new();
    for &p in pts.iter().rev() {
        while upper.len() >= 2 && cross(upper[upper.len() - 2], upper[upper.len() - 1], p) <= 0.0 {
            upper.pop();
        }
        upper.push(p);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    /// Rotate points about the origin by `theta`.
    fn rotate(points: &[DVec2], theta: f64) -> Vec<DVec2> {
        let (s, c) = theta.sin_cos();
        points
            .iter()
            .map(|p| DVec2::new(p.x * c - p.y * s, p.x * s + p.y * c))
            .collect()
    }

    #[test]
    fn axis_aligned_rectangle() {
        // 10 x 2 rectangle → long axis ≈ x, short axis ≈ y.
        let r = vec![
            DVec2::new(0.0, 0.0),
            DVec2::new(10.0, 0.0),
            DVec2::new(10.0, 2.0),
            DVec2::new(0.0, 2.0),
        ];
        let ob = OrientedBox::of_points(&r).unwrap();
        assert!((ob.long_len() - 10.0).abs() < 1e-6, "long {}", ob.long_len());
        assert!((ob.short_len() - 2.0).abs() < 1e-6, "short {}", ob.short_len());
        // Long axis parallel to x.
        assert!(ob.long_axis.x.abs() > 0.999);
        // Center at (5, 1).
        assert!((ob.center - DVec2::new(5.0, 1.0)).length() < 1e-6);
    }

    #[test]
    fn rotated_rectangle_recovers_axes() {
        let base = vec![
            DVec2::new(0.0, 0.0),
            DVec2::new(12.0, 0.0),
            DVec2::new(12.0, 3.0),
            DVec2::new(0.0, 3.0),
        ];
        for &theta in &[0.1, 0.5, PI / 6.0, PI / 4.0, 1.0, 2.0] {
            let pts = rotate(&base, theta);
            let ob = OrientedBox::of_points(&pts).unwrap();
            assert!(
                (ob.long_len() - 12.0).abs() < 1e-5,
                "theta {theta}: long {}",
                ob.long_len()
            );
            assert!(
                (ob.short_len() - 3.0).abs() < 1e-5,
                "theta {theta}: short {}",
                ob.short_len()
            );
            // Long axis should align with the rotated x direction (± sign).
            let expected = DVec2::new(theta.cos(), theta.sin());
            let align = ob.long_axis.dot(expected).abs();
            assert!(align > 0.9999, "theta {theta}: align {align}");
            // Axes orthonormal.
            assert!(ob.long_axis.dot(ob.short_axis).abs() < 1e-9);
        }
    }

    #[test]
    fn area_never_exceeds_aabb() {
        // For a rotated shape the OBB area must be ≤ the axis-aligned box area.
        let base = vec![
            DVec2::new(0.0, 0.0),
            DVec2::new(8.0, 0.0),
            DVec2::new(8.0, 5.0),
            DVec2::new(0.0, 5.0),
        ];
        let pts = rotate(&base, 0.7);
        let ob = OrientedBox::of_points(&pts).unwrap();
        let obb_area = ob.long_len() * ob.short_len();
        assert!((obb_area - 40.0).abs() < 1e-5, "obb area {obb_area}");
    }

    #[test]
    fn hull_of_square_is_four_points() {
        let pts = vec![
            DVec2::new(0.0, 0.0),
            DVec2::new(1.0, 0.0),
            DVec2::new(1.0, 1.0),
            DVec2::new(0.0, 1.0),
            DVec2::new(0.5, 0.5), // interior point
        ];
        assert_eq!(convex_hull(&pts).len(), 4);
    }
}
