// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! `Polygon2d` — a simple (non-self-intersecting) 2D polygon.
//!
//! Convention: vertices are stored **CCW** (positive signed area). The closing
//! edge is implicit (last → first); do NOT repeat the first vertex. Constructors
//! normalise winding to CCW so downstream OBB / split code can assume it.

use glam::DVec2;

/// A simple polygon in the XY plane, vertices in CCW order, no repeated closing
/// point.
#[derive(Debug, Clone, PartialEq)]
pub struct Polygon2d {
    verts: Vec<DVec2>,
}

impl Polygon2d {
    /// Build from vertices, dropping an explicit closing duplicate if present and
    /// forcing CCW winding. Returns `None` if fewer than 3 distinct vertices
    /// remain.
    pub fn new(mut verts: Vec<DVec2>) -> Option<Self> {
        // Drop a trailing duplicate of the first vertex (explicit closure).
        if verts.len() >= 2 {
            let first = verts[0];
            let last = *verts.last().unwrap();
            if first.distance_squared(last) < 1e-18 {
                verts.pop();
            }
        }
        if verts.len() < 3 {
            return None;
        }
        let mut poly = Polygon2d { verts };
        if poly.signed_area() < 0.0 {
            poly.verts.reverse();
        }
        Some(poly)
    }

    /// Build from `(f64, f64)` pairs.
    pub fn from_pairs<I: IntoIterator<Item = (f64, f64)>>(pairs: I) -> Option<Self> {
        Self::new(pairs.into_iter().map(|(x, y)| DVec2::new(x, y)).collect())
    }

    /// Vertices in CCW order (no repeated closing point).
    pub fn verts(&self) -> &[DVec2] {
        &self.verts
    }

    pub fn len(&self) -> usize {
        self.verts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.verts.is_empty()
    }

    /// Edges as `(a, b)` vertex pairs, including the implicit closing edge.
    pub fn edges(&self) -> impl Iterator<Item = (DVec2, DVec2)> + '_ {
        let n = self.verts.len();
        (0..n).map(move |i| (self.verts[i], self.verts[(i + 1) % n]))
    }

    /// Signed area by the shoelace formula. Positive = CCW.
    pub fn signed_area(&self) -> f64 {
        let n = self.verts.len();
        if n < 3 {
            return 0.0;
        }
        let mut sum = 0.0;
        for i in 0..n {
            let a = self.verts[i];
            let b = self.verts[(i + 1) % n];
            sum += a.x * b.y - b.x * a.y;
        }
        sum * 0.5
    }

    /// Unsigned enclosed area.
    pub fn area(&self) -> f64 {
        self.signed_area().abs()
    }

    /// Perimeter length (closed).
    pub fn perimeter(&self) -> f64 {
        self.edges().map(|(a, b)| a.distance(b)).sum()
    }

    /// Centroid (area-weighted). Falls back to the vertex mean for degenerate
    /// (zero-area) polygons.
    pub fn centroid(&self) -> DVec2 {
        let a = self.signed_area();
        if a.abs() < 1e-12 {
            let s: DVec2 = self.verts.iter().copied().sum();
            return s / self.verts.len() as f64;
        }
        let n = self.verts.len();
        let mut c = DVec2::ZERO;
        for i in 0..n {
            let p = self.verts[i];
            let q = self.verts[(i + 1) % n];
            let cross = p.x * q.y - q.x * p.y;
            c += (p + q) * cross;
        }
        c / (6.0 * a)
    }

    /// Point-in-polygon by the even-odd ray-cast rule. Points exactly on an edge
    /// are reported inclusively-ish (boundary behaviour is not guaranteed exact —
    /// used for interior tests where the query point is well inside).
    pub fn contains(&self, p: DVec2) -> bool {
        let n = self.verts.len();
        let mut inside = false;
        let mut j = n - 1;
        for i in 0..n {
            let vi = self.verts[i];
            let vj = self.verts[j];
            let crosses = (vi.y > p.y) != (vj.y > p.y);
            if crosses {
                let t = (p.y - vi.y) / (vj.y - vi.y);
                let x_cross = vi.x + t * (vj.x - vi.x);
                if p.x < x_cross {
                    inside = !inside;
                }
            }
            j = i;
        }
        inside
    }

    /// Axis-aligned bounding box `(min, max)`.
    pub fn aabb(&self) -> (DVec2, DVec2) {
        let mut lo = DVec2::splat(f64::INFINITY);
        let mut hi = DVec2::splat(f64::NEG_INFINITY);
        for &v in &self.verts {
            lo = lo.min(v);
            hi = hi.max(v);
        }
        (lo, hi)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_square() -> Polygon2d {
        Polygon2d::from_pairs([(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)]).unwrap()
    }

    #[test]
    fn area_of_unit_square() {
        assert!((unit_square().area() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn winding_normalised_to_ccw() {
        // Supplied CW; constructor must flip to CCW (positive signed area).
        let cw = Polygon2d::from_pairs([(0.0, 0.0), (0.0, 1.0), (1.0, 1.0), (1.0, 0.0)]).unwrap();
        assert!(cw.signed_area() > 0.0);
    }

    #[test]
    fn explicit_closing_point_dropped() {
        let p = Polygon2d::from_pairs([
            (0.0, 0.0),
            (2.0, 0.0),
            (2.0, 2.0),
            (0.0, 2.0),
            (0.0, 0.0),
        ])
        .unwrap();
        assert_eq!(p.len(), 4);
        assert!((p.area() - 4.0).abs() < 1e-12);
    }

    #[test]
    fn point_in_poly() {
        let s = unit_square();
        assert!(s.contains(DVec2::new(0.5, 0.5)));
        assert!(!s.contains(DVec2::new(1.5, 0.5)));
        assert!(!s.contains(DVec2::new(-0.5, 0.5)));
    }

    #[test]
    fn centroid_of_square() {
        let c = unit_square().centroid();
        assert!((c.x - 0.5).abs() < 1e-12 && (c.y - 0.5).abs() < 1e-12);
    }

    #[test]
    fn triangle_area() {
        let t = Polygon2d::from_pairs([(0.0, 0.0), (4.0, 0.0), (0.0, 3.0)]).unwrap();
        assert!((t.area() - 6.0).abs() < 1e-12);
    }

    #[test]
    fn too_few_vertices_rejected() {
        assert!(Polygon2d::from_pairs([(0.0, 0.0), (1.0, 1.0)]).is_none());
    }
}
