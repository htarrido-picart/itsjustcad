// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

use glam::DVec3;

/// Evaluate a NURBS curve point via de Boor on homogeneous coordinates.
///
/// `knots.len()` must equal `control.len() + degree + 1`; `t` is clamped to the
/// valid domain `[knots[degree], knots[len-degree-1]]`.
pub fn nurbs_point(
    control: &[DVec3],
    weights: &[f64],
    knots: &[f64],
    degree: usize,
    t: f64,
) -> DVec3 {
    let n = control.len();
    debug_assert_eq!(weights.len(), n);
    debug_assert_eq!(knots.len(), n + degree + 1);

    let t_min = knots[degree];
    let t_max = knots[n]; // == knots[len - degree - 1]
    let t = t.clamp(t_min, t_max);

    // Find knot span k with knots[k] <= t < knots[k+1] (or the last span at t_max)
    let mut k = degree;
    while k < n - 1 && t >= knots[k + 1] {
        k += 1;
    }

    // Homogeneous control points for the affected span
    let mut d: Vec<(DVec3, f64)> = (0..=degree)
        .map(|j| {
            let i = j + k - degree;
            (control[i] * weights[i], weights[i])
        })
        .collect();

    for r in 1..=degree {
        for j in (r..=degree).rev() {
            let i = j + k - degree;
            let denom = knots[i + degree - r + 1] - knots[i];
            let alpha = if denom.abs() < 1e-12 {
                0.0
            } else {
                (t - knots[i]) / denom
            };
            d[j] = (
                d[j - 1].0 * (1.0 - alpha) + d[j].0 * alpha,
                d[j - 1].1 * (1.0 - alpha) + d[j].1 * alpha,
            );
        }
    }

    d[degree].0 / d[degree].1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curve::clamped_uniform_knots;

    #[test]
    fn clamped_curve_interpolates_endpoints() {
        let control = vec![
            DVec3::new(0.0, 0.0, 0.0),
            DVec3::new(1.0, 2.0, 0.0),
            DVec3::new(3.0, 2.0, 0.0),
            DVec3::new(4.0, 0.0, 0.0),
        ];
        let weights = vec![1.0; 4];
        let knots = clamped_uniform_knots(4, 3);
        let p0 = nurbs_point(&control, &weights, &knots, 3, 0.0);
        let p1 = nurbs_point(&control, &weights, &knots, 3, 1.0);
        assert!(p0.distance(control[0]) < 1e-9);
        assert!(p1.distance(control[3]) < 1e-9);
    }

    #[test]
    fn degree_one_is_polyline() {
        let control = vec![
            DVec3::new(0.0, 0.0, 0.0),
            DVec3::new(2.0, 0.0, 0.0),
            DVec3::new(2.0, 2.0, 0.0),
        ];
        let weights = vec![1.0; 3];
        let knots = clamped_uniform_knots(3, 1);
        let mid = nurbs_point(&control, &weights, &knots, 1, 0.25);
        assert!(mid.distance(DVec3::new(1.0, 0.0, 0.0)) < 1e-9);
    }

    #[test]
    fn rational_quarter_circle() {
        // Standard rational Bezier quarter circle, degree 2
        let w = std::f64::consts::FRAC_1_SQRT_2;
        let control = vec![
            DVec3::new(1.0, 0.0, 0.0),
            DVec3::new(1.0, 1.0, 0.0),
            DVec3::new(0.0, 1.0, 0.0),
        ];
        let weights = vec![1.0, w, 1.0];
        let knots = clamped_uniform_knots(3, 2);
        for i in 0..=10 {
            let p = nurbs_point(&control, &weights, &knots, 2, i as f64 / 10.0);
            assert!((p.length() - 1.0).abs() < 1e-9, "not on unit circle: {p}");
        }
    }
}
