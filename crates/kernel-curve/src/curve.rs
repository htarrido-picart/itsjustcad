// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

use glam::DVec3;
use serde::{Deserialize, Serialize};

use crate::nurbs::nurbs_point;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Curve {
    Line {
        a: DVec3,
        b: DVec3,
    },
    Polyline {
        points: Vec<DVec3>,
        closed: bool,
    },
    /// Circular arc in the XY plane at `center.z`, CCW, angles in radians.
    /// A full circle is `start = 0, end = TAU`.
    Arc {
        center: DVec3,
        radius: f64,
        start: f64,
        end: f64,
    },
    /// Full ellipse in the XY plane at `center.z`.
    Ellipse {
        center: DVec3,
        rx: f64,
        ry: f64,
    },
    /// NURBS curve; clamped knot vector, `knots.len() == control.len() + degree + 1`.
    Nurbs {
        control: Vec<DVec3>,
        weights: Vec<f64>,
        knots: Vec<f64>,
        degree: usize,
    },
}

impl Curve {
    pub fn is_closed(&self) -> bool {
        match self {
            Curve::Line { .. } => false,
            Curve::Polyline { closed, points } => *closed && points.len() >= 3,
            Curve::Arc { start, end, .. } => (end - start).abs() >= std::f64::consts::TAU - 1e-9,
            Curve::Ellipse { .. } => true,
            Curve::Nurbs { control, .. } => control
                .first()
                .zip(control.last())
                .is_some_and(|(a, b)| a.distance(*b) < 1e-9),
        }
    }

    pub fn translate(&mut self, d: DVec3) {
        match self {
            Curve::Line { a, b } => {
                *a += d;
                *b += d;
            }
            Curve::Polyline { points, .. } => points.iter_mut().for_each(|p| *p += d),
            Curve::Arc { center, .. } | Curve::Ellipse { center, .. } => *center += d,
            Curve::Nurbs { control, .. } => control.iter_mut().for_each(|p| *p += d),
        }
    }

    /// Apply an affine transform. Returns `true` when the curve type survived
    /// exactly; `false` when an Arc/Ellipse could not represent the result and
    /// was tessellated to a closed polyline at `tol`.
    ///
    /// Point-based curves (Line, Polyline, Nurbs) are always exact. Arc
    /// survives translation + Z-rotation + uniform XY scale; Ellipse survives
    /// translation + axis-aligned scale (its type cannot store a rotation).
    pub fn transform(&mut self, m: &glam::DMat4, tol: f64) -> bool {
        const EPS: f64 = 1e-9;
        let lx = m.transform_vector3(DVec3::X);
        let ly = m.transform_vector3(DVec3::Y);
        match self {
            Curve::Line { a, b } => {
                *a = m.transform_point3(*a);
                *b = m.transform_point3(*b);
                true
            }
            Curve::Polyline { points, .. } | Curve::Nurbs { control: points, .. } => {
                points.iter_mut().for_each(|p| *p = m.transform_point3(*p));
                true
            }
            Curve::Arc { center, radius, start, end } => {
                // Preserved when XY maps to XY as rotation × uniform scale
                // without flipping orientation.
                let planar = lx.z.abs() < EPS && ly.z.abs() < EPS;
                let uniform = (lx.length() - ly.length()).abs() < EPS;
                let orthogonal = lx.dot(ly).abs() < EPS * lx.length_squared().max(1.0);
                let keeps_ccw = lx.cross(ly).z > EPS;
                if planar && uniform && orthogonal && keeps_ccw {
                    let angle = lx.y.atan2(lx.x);
                    *center = m.transform_point3(*center);
                    *radius *= lx.length();
                    *start += angle;
                    *end += angle;
                    true
                } else {
                    *self = tessellated_polyline(self, m, tol);
                    false
                }
            }
            Curve::Ellipse { center, rx, ry } => {
                // Preserved only under axis-aligned scaling (the type cannot
                // store a rotation); axis flips are fine by symmetry.
                let diagonal = lx.y.abs() < EPS && lx.z.abs() < EPS
                    && ly.x.abs() < EPS && ly.z.abs() < EPS;
                if diagonal {
                    *center = m.transform_point3(*center);
                    *rx *= lx.x.abs();
                    *ry *= ly.y.abs();
                    true
                } else {
                    *self = tessellated_polyline(self, m, tol);
                    false
                }
            }
        }
    }

    /// Tessellate to a polyline with roughly `tol` max chord deviation.
    /// For closed curves the first point is NOT repeated at the end.
    pub fn tessellate(&self, tol: f64) -> Vec<DVec3> {
        match self {
            Curve::Line { a, b } => vec![*a, *b],
            Curve::Polyline { points, .. } => points.clone(),
            Curve::Arc {
                center,
                radius,
                start,
                end,
            } => {
                let sweep = (end - start).abs();
                let n = segments_for_arc(*radius, sweep, tol);
                let closed = self.is_closed();
                let count = if closed { n } else { n + 1 };
                (0..count)
                    .map(|i| {
                        let t = start + sweep * (i as f64) / (n as f64);
                        *center + DVec3::new(radius * t.cos(), radius * t.sin(), 0.0)
                    })
                    .collect()
            }
            Curve::Ellipse { center, rx, ry } => {
                let n = segments_for_arc(rx.max(*ry), std::f64::consts::TAU, tol);
                (0..n)
                    .map(|i| {
                        let t = std::f64::consts::TAU * (i as f64) / (n as f64);
                        *center + DVec3::new(rx * t.cos(), ry * t.sin(), 0.0)
                    })
                    .collect()
            }
            Curve::Nurbs {
                control,
                weights,
                knots,
                degree,
            } => {
                // Fixed sampling proportional to control count; adaptive later.
                let n = (control.len() * 8).max(32);
                let (t0, t1) = (knots[*degree], knots[knots.len() - degree - 1]);
                (0..=n)
                    .map(|i| {
                        let t = t0 + (t1 - t0) * (i as f64) / (n as f64);
                        nurbs_point(control, weights, knots, *degree, t)
                    })
                    .collect()
            }
        }
    }

    /// Offset the curve in the XY plane by `dist`, returning a new curve.
    ///
    /// Closed curves: positive `dist` offsets outward, negative inward.
    /// Open curves: positive `dist` offsets to the left of travel direction.
    /// Arcs stay exact (radius change); ellipses and NURBS tessellate at `tol`.
    /// Returns `None` when the offset collapses the curve (inward past its
    /// radius, or a degenerate result).
    pub fn offset(&self, dist: f64, tol: f64) -> Option<Curve> {
        match self {
            Curve::Line { a, b } => {
                let d = (*b - *a).truncate().normalize_or_zero();
                if d == glam::DVec2::ZERO {
                    return None;
                }
                let n = DVec3::new(-d.y, d.x, 0.0) * dist; // left of travel
                Some(Curve::Line { a: *a + n, b: *b + n })
            }
            Curve::Arc { center, radius, start, end } => {
                // Positive = outward = larger radius.
                let r = radius + dist;
                (r > 1e-9).then_some(Curve::Arc {
                    center: *center,
                    radius: r,
                    start: *start,
                    end: *end,
                })
            }
            Curve::Polyline { points, closed } => {
                offset_polyline(points, *closed, dist).map(|points| Curve::Polyline {
                    points,
                    closed: *closed,
                })
            }
            Curve::Ellipse { .. } | Curve::Nurbs { .. } => {
                let pts = self.tessellate(tol);
                offset_polyline(&pts, self.is_closed(), dist).map(|points| Curve::Polyline {
                    points,
                    closed: self.is_closed(),
                })
            }
        }
    }

    pub fn points_bound(&self) -> Vec<DVec3> {
        // Cheap bound-defining points (control points bound NURBS by convex hull).
        match self {
            Curve::Line { a, b } => vec![*a, *b],
            Curve::Polyline { points, .. } => points.clone(),
            Curve::Arc { center, radius, .. } => vec![
                *center + DVec3::new(-radius, -radius, 0.0),
                *center + DVec3::new(*radius, *radius, 0.0),
            ],
            Curve::Ellipse { center, rx, ry } => vec![
                *center + DVec3::new(-rx, -ry, 0.0),
                *center + DVec3::new(*rx, *ry, 0.0),
            ],
            Curve::Nurbs { control, .. } => control.clone(),
        }
    }
}

/// Miter-join polyline offset in the XY plane (z carried from each vertex).
///
/// Closed loops are normalized so positive `dist` is outward regardless of
/// winding; open runs treat positive as left-of-travel. Returns `None` when
/// the offset collapses the loop (area sign flips or vanishes). Local
/// self-intersections of tight inward offsets are NOT cleaned up.
fn offset_polyline(points: &[DVec3], closed: bool, dist: f64) -> Option<Vec<DVec3>> {
    if points.len() < 2 {
        return None;
    }
    // Signed area (shoelace) decides winding; for closed loops flip the
    // left-normal offset so positive dist always grows the loop.
    let area: f64 = points
        .iter()
        .zip(points.iter().cycle().skip(1))
        .take(points.len())
        .map(|(p, q)| p.x * q.y - q.x * p.y)
        .sum::<f64>()
        / 2.0;
    let d = if closed && area > 0.0 { -dist } else { dist };

    let n = points.len();
    let seg_normal = |i: usize| -> glam::DVec2 {
        let dir = (points[(i + 1) % n] - points[i]).truncate().normalize_or_zero();
        glam::DVec2::new(-dir.y, dir.x) // left of travel
    };
    let segs = if closed { n } else { n - 1 };
    let mut out = Vec::with_capacity(n);
    #[allow(clippy::needless_range_loop)] // `i` used for modular arithmetic, not just indexing
    for i in 0..n {
        let (prev, next) = if closed {
            (seg_normal((i + n - 1) % n), seg_normal(i % n))
        } else {
            // Endpoints use their single adjacent segment.
            (
                seg_normal(i.saturating_sub(1).min(segs - 1)),
                seg_normal(i.min(segs - 1)),
            )
        };
        // Miter: average of adjacent normals, scaled to keep segment distance.
        let m = prev + next;
        let len_sq = m.length_squared();
        let shift = if len_sq < 1e-18 {
            prev // 180° spike: fall back to the incoming normal
        } else {
            m * (2.0 / len_sq) // = m_hat / cos(theta/2)
        };
        out.push(points[i] + (shift * d).extend(0.0));
    }
    // Collapse detection: an offset past the local core reverses at least one
    // segment's direction (area sign alone misses loops that re-emerge with
    // the original winding).
    for i in 0..segs {
        let old_dir = (points[(i + 1) % n] - points[i]).truncate();
        let new_dir = (out[(i + 1) % n] - out[i]).truncate();
        if old_dir.dot(new_dir) <= 0.0 {
            return None;
        }
    }
    Some(out)
}

/// Fallback for transforms an Arc/Ellipse cannot represent: sample the curve,
/// transform the samples, return a polyline (closed if the source was).
fn tessellated_polyline(curve: &Curve, m: &glam::DMat4, tol: f64) -> Curve {
    let points = curve
        .tessellate(tol)
        .into_iter()
        .map(|p| m.transform_point3(p))
        .collect();
    Curve::Polyline {
        points,
        closed: curve.is_closed(),
    }
}

fn segments_for_arc(radius: f64, sweep: f64, tol: f64) -> usize {
    if radius <= tol {
        return 8;
    }
    // Chord error e = r(1 - cos(dt/2))  =>  dt = 2 acos(1 - e/r)
    let dt = 2.0 * (1.0 - tol / radius).clamp(-1.0, 1.0).acos();
    ((sweep / dt.max(1e-4)).ceil() as usize).clamp(8, 512)
}

/// Construct a clamped uniform knot vector for `n` control points of `degree`.
pub fn clamped_uniform_knots(n: usize, degree: usize) -> Vec<f64> {
    let inner = n - degree; // number of spans
    let mut knots = Vec::with_capacity(n + degree + 1);
    knots.extend(std::iter::repeat_n(0.0, degree + 1));
    for i in 1..inner {
        knots.push(i as f64 / inner as f64);
    }
    knots.extend(std::iter::repeat_n(1.0, degree + 1));
    knots
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arc_tessellation_closes_circle() {
        let c = Curve::Arc {
            center: DVec3::ZERO,
            radius: 5.0,
            start: 0.0,
            end: std::f64::consts::TAU,
        };
        assert!(c.is_closed());
        let pts = c.tessellate(0.01);
        // no duplicated seam point
        assert!(pts.first().unwrap().distance(*pts.last().unwrap()) > 1e-6);
        for p in &pts {
            assert!((p.length() - 5.0).abs() < 0.02);
        }
    }

    #[test]
    fn transform_arc_z_rotation_exact() {
        let mut c = Curve::Arc {
            center: DVec3::new(2.0, 0.0, 1.0),
            radius: 3.0,
            start: 0.0,
            end: 1.0,
        };
        let m = glam::DMat4::from_rotation_z(std::f64::consts::FRAC_PI_2);
        assert!(c.transform(&m, 0.01));
        let Curve::Arc { center, radius, start, .. } = c else { panic!() };
        assert!((center - DVec3::new(0.0, 2.0, 1.0)).length() < 1e-9);
        assert!((radius - 3.0).abs() < 1e-12);
        assert!((start - std::f64::consts::FRAC_PI_2).abs() < 1e-12);
    }

    #[test]
    fn transform_arc_uniform_scale_exact() {
        let mut c = Curve::Arc {
            center: DVec3::ZERO,
            radius: 2.0,
            start: 0.0,
            end: 1.0,
        };
        assert!(c.transform(&glam::DMat4::from_scale(DVec3::splat(2.5)), 0.01));
        let Curve::Arc { radius, .. } = c else { panic!() };
        assert!((radius - 5.0).abs() < 1e-12);
    }

    #[test]
    fn transform_arc_nonuniform_tessellates() {
        let mut c = Curve::Arc {
            center: DVec3::ZERO,
            radius: 2.0,
            start: 0.0,
            end: std::f64::consts::TAU,
        };
        let exact = c.transform(&glam::DMat4::from_scale(DVec3::new(2.0, 1.0, 1.0)), 0.01);
        assert!(!exact);
        let Curve::Polyline { closed, ref points } = c else { panic!("expected polyline") };
        assert!(closed);
        // stretched to an ellipse: x extent 4, y extent 2
        let max_x = points.iter().map(|p| p.x.abs()).fold(0.0, f64::max);
        let max_y = points.iter().map(|p| p.y.abs()).fold(0.0, f64::max);
        assert!((max_x - 4.0).abs() < 0.05 && (max_y - 2.0).abs() < 0.05);
    }

    #[test]
    fn transform_ellipse_axis_scale_exact_rotation_tessellates() {
        let mut e = Curve::Ellipse { center: DVec3::ZERO, rx: 4.0, ry: 2.0 };
        assert!(e.transform(&glam::DMat4::from_scale(DVec3::new(2.0, 3.0, 1.0)), 0.01));
        let Curve::Ellipse { rx, ry, .. } = e else { panic!() };
        assert_eq!((rx, ry), (8.0, 6.0));

        let mut e = Curve::Ellipse { center: DVec3::ZERO, rx: 4.0, ry: 2.0 };
        assert!(!e.transform(&glam::DMat4::from_rotation_z(0.5), 0.01));
        assert!(matches!(e, Curve::Polyline { closed: true, .. }));
    }

    #[test]
    fn transform_polyline_and_line_exact() {
        let m = glam::DMat4::from_rotation_z(0.7) * glam::DMat4::from_translation(DVec3::X);
        let mut l = Curve::Line { a: DVec3::ZERO, b: DVec3::X };
        assert!(l.transform(&m, 0.01));
        let mut p = Curve::Polyline { points: vec![DVec3::ZERO, DVec3::X, DVec3::Y], closed: true };
        assert!(p.transform(&m, 0.01));
        assert!(p.is_closed());
    }

    fn shoelace(points: &[DVec3]) -> f64 {
        points
            .iter()
            .zip(points.iter().cycle().skip(1))
            .take(points.len())
            .map(|(p, q)| p.x * q.y - q.x * p.y)
            .sum::<f64>()
            / 2.0
    }

    #[test]
    fn offset_square_outward_and_inward() {
        // 4x4 CCW square at z=1.
        let sq = |s: f64| Curve::Polyline {
            points: vec![
                DVec3::new(-s, -s, 1.0),
                DVec3::new(s, -s, 1.0),
                DVec3::new(s, s, 1.0),
                DVec3::new(-s, s, 1.0),
            ],
            closed: true,
        };
        let out = sq(2.0).offset(1.0, 0.01).unwrap();
        let Curve::Polyline { ref points, .. } = out else { panic!() };
        assert!((shoelace(points).abs() - 36.0).abs() < 1e-9); // grew to 6x6
        assert!(points.iter().all(|p| (p.z - 1.0).abs() < 1e-12)); // z kept

        let inw = sq(2.0).offset(-1.0, 0.01).unwrap();
        let Curve::Polyline { ref points, .. } = inw else { panic!() };
        assert!((shoelace(points).abs() - 4.0).abs() < 1e-9); // shrank to 2x2

        // CW winding: outward must still grow.
        let cw = Curve::Polyline {
            points: vec![
                DVec3::new(-2.0, -2.0, 0.0),
                DVec3::new(-2.0, 2.0, 0.0),
                DVec3::new(2.0, 2.0, 0.0),
                DVec3::new(2.0, -2.0, 0.0),
            ],
            closed: true,
        };
        let out = cw.offset(1.0, 0.01).unwrap();
        let Curve::Polyline { ref points, .. } = out else { panic!() };
        assert!((shoelace(points).abs() - 36.0).abs() < 1e-9);
    }

    #[test]
    fn offset_collapse_returns_none() {
        let sq = Curve::Polyline {
            points: vec![
                DVec3::new(-1.0, -1.0, 0.0),
                DVec3::new(1.0, -1.0, 0.0),
                DVec3::new(1.0, 1.0, 0.0),
                DVec3::new(-1.0, 1.0, 0.0),
            ],
            closed: true,
        };
        assert!(sq.offset(-1.5, 0.01).is_none());
    }

    #[test]
    fn offset_circle_exact_radius() {
        let c = Curve::Arc {
            center: DVec3::ZERO,
            radius: 3.0,
            start: 0.0,
            end: std::f64::consts::TAU,
        };
        let Some(Curve::Arc { radius, .. }) = c.offset(0.5, 0.01) else { panic!() };
        assert!((radius - 3.5).abs() < 1e-12);
        assert!(c.offset(-3.0, 0.01).is_none()); // collapses to a point
    }

    #[test]
    fn offset_line_left_of_travel() {
        let l = Curve::Line { a: DVec3::ZERO, b: DVec3::new(10.0, 0.0, 0.0) };
        let Some(Curve::Line { a, b }) = l.offset(2.0, 0.01) else { panic!() };
        assert!((a - DVec3::new(0.0, 2.0, 0.0)).length() < 1e-12);
        assert!((b - DVec3::new(10.0, 2.0, 0.0)).length() < 1e-12);
    }

    #[test]
    fn offset_open_polyline_l_shape() {
        // L along +x then +y; offset left of travel puts the copy inside the L.
        let l = Curve::Polyline {
            points: vec![DVec3::ZERO, DVec3::new(4.0, 0.0, 0.0), DVec3::new(4.0, 4.0, 0.0)],
            closed: false,
        };
        let Some(Curve::Polyline { points, .. }) = l.offset(1.0, 0.01) else { panic!() };
        assert!((points[0] - DVec3::new(0.0, 1.0, 0.0)).length() < 1e-9);
        assert!((points[1] - DVec3::new(3.0, 1.0, 0.0)).length() < 1e-9); // miter corner
        assert!((points[2] - DVec3::new(3.0, 4.0, 0.0)).length() < 1e-9);
    }

    #[test]
    fn offset_ellipse_tessellates() {
        let e = Curve::Ellipse { center: DVec3::ZERO, rx: 4.0, ry: 2.0 };
        let Some(Curve::Polyline { closed, .. }) = e.offset(0.5, 0.01) else {
            panic!("expected polyline")
        };
        assert!(closed);
    }

    #[test]
    fn clamped_knots_shape() {
        let k = clamped_uniform_knots(6, 3);
        assert_eq!(k.len(), 6 + 3 + 1);
        assert_eq!(&k[..4], &[0.0; 4]);
        assert_eq!(&k[k.len() - 4..], &[1.0; 4]);
    }
}
