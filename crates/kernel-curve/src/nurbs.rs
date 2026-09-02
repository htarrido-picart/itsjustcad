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

/// Insert a knot at parameter `t` (Boehm's algorithm), preserving the curve's
/// shape exactly while adding one control point.
///
/// Works on homogeneous coordinates so rational curves stay rational. Returns
/// the new `(control, weights, knots)`. Returns `None` when `t` lies outside
/// the open domain or when the knot already has full multiplicity `degree`
/// (inserting again would disconnect the curve).
pub fn insert_knot(
    control: &[DVec3],
    weights: &[f64],
    knots: &[f64],
    degree: usize,
    t: f64,
) -> Option<(Vec<DVec3>, Vec<f64>, Vec<f64>)> {
    let n = control.len();
    if weights.len() != n || knots.len() != n + degree + 1 || degree == 0 {
        return None;
    }
    let (t0, t1) = (knots[degree], knots[n]);
    if !(t > t0 && t < t1) {
        return None; // endpoint knots are already fully clamped
    }
    // Knot span k: knots[k] <= t < knots[k+1].
    let mut k = degree;
    while k < n - 1 && t >= knots[k + 1] {
        k += 1;
    }
    // Existing multiplicity of t; degree-multiplicity knots cannot take more.
    let s = knots.iter().filter(|&&u| (u - t).abs() < 1e-12).count();
    if s >= degree {
        return None;
    }

    // Homogeneous control points (P*w, w).
    let hom: Vec<(DVec3, f64)> =
        control.iter().zip(weights).map(|(p, w)| (*p * *w, *w)).collect();

    let mut out: Vec<(DVec3, f64)> = Vec::with_capacity(n + 1);
    out.extend_from_slice(&hom[..=k - degree]);
    for i in (k - degree + 1)..=(k - s) {
        let denom = knots[i + degree] - knots[i];
        let alpha = if denom.abs() < 1e-12 { 0.0 } else { (t - knots[i]) / denom };
        out.push((
            hom[i - 1].0 * (1.0 - alpha) + hom[i].0 * alpha,
            hom[i - 1].1 * (1.0 - alpha) + hom[i].1 * alpha,
        ));
    }
    out.extend_from_slice(&hom[k - s..]);
    debug_assert_eq!(out.len(), n + 1);

    let mut new_knots = Vec::with_capacity(knots.len() + 1);
    new_knots.extend_from_slice(&knots[..=k]);
    new_knots.push(t);
    new_knots.extend_from_slice(&knots[k + 1..]);

    let (new_control, new_weights): (Vec<DVec3>, Vec<f64>) =
        out.into_iter().map(|(pw, w)| (pw / w, w)).unzip();
    Some((new_control, new_weights, new_knots))
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
    fn insert_knot_preserves_shape_polynomial() {
        let control = vec![
            DVec3::new(0.0, 0.0, 0.0),
            DVec3::new(1.0, 2.0, 0.0),
            DVec3::new(3.0, 2.0, 1.0),
            DVec3::new(4.0, 0.0, 0.5),
            DVec3::new(6.0, -1.0, 0.0),
        ];
        let weights = vec![1.0; 5];
        let knots = clamped_uniform_knots(5, 3);
        let (c2, w2, k2) = insert_knot(&control, &weights, &knots, 3, 0.37).unwrap();
        assert_eq!(c2.len(), 6);
        assert_eq!(k2.len(), knots.len() + 1);
        for i in 0..=100 {
            let t = i as f64 / 100.0;
            let a = nurbs_point(&control, &weights, &knots, 3, t);
            let b = nurbs_point(&c2, &w2, &k2, 3, t);
            assert!(a.distance(b) < 1e-9, "shape changed at t={t}: {a} vs {b}");
        }
    }

    #[test]
    fn insert_knot_preserves_rational_circle() {
        let w = std::f64::consts::FRAC_1_SQRT_2;
        let control = vec![
            DVec3::new(1.0, 0.0, 0.0),
            DVec3::new(1.0, 1.0, 0.0),
            DVec3::new(0.0, 1.0, 0.0),
        ];
        let weights = vec![1.0, w, 1.0];
        let knots = clamped_uniform_knots(3, 2);
        let (c2, w2, k2) = insert_knot(&control, &weights, &knots, 2, 0.5).unwrap();
        assert_eq!(c2.len(), 4);
        for i in 0..=20 {
            let p = nurbs_point(&c2, &w2, &k2, 2, i as f64 / 20.0);
            assert!((p.length() - 1.0).abs() < 1e-9, "left unit circle: {p}");
        }
    }

    #[test]
    fn insert_knot_rejects_out_of_domain_and_full_multiplicity() {
        let control = vec![
            DVec3::new(0.0, 0.0, 0.0),
            DVec3::new(1.0, 1.0, 0.0),
            DVec3::new(2.0, 0.0, 0.0),
            DVec3::new(3.0, 1.0, 0.0),
        ];
        let weights = vec![1.0; 4];
        let knots = clamped_uniform_knots(4, 3);
        assert!(insert_knot(&control, &weights, &knots, 3, 0.0).is_none());
        assert!(insert_knot(&control, &weights, &knots, 3, 1.0).is_none());
        assert!(insert_knot(&control, &weights, &knots, 3, -0.5).is_none());
        // Saturate an interior knot to degree multiplicity, then reject.
        let (mut c, mut w, mut k) = insert_knot(&control, &weights, &knots, 3, 0.5).unwrap();
        for _ in 0..2 {
            (c, w, k) = insert_knot(&c, &w, &k, 3, 0.5).unwrap();
        }
        assert!(insert_knot(&c, &w, &k, 3, 0.5).is_none());
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
