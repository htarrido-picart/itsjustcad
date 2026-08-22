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
    fn clamped_knots_shape() {
        let k = clamped_uniform_knots(6, 3);
        assert_eq!(k.len(), 6 + 3 + 1);
        assert_eq!(&k[..4], &[0.0; 4]);
        assert_eq!(&k[k.len() - 4..], &[1.0; 4]);
    }
}
