// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Curve analysis: discrete curvature sampling for curvature-comb display.

use glam::DVec3;

use crate::Curve;

/// One curvature sample along a curve: the on-curve foot point, the unit
/// normal pointing toward the local center of curvature, and the unsigned
/// curvature `kappa` (1/radius; 0 on straight runs).
#[derive(Clone, Debug, PartialEq)]
pub struct CurvatureSample {
    pub point: DVec3,
    pub normal: DVec3,
    pub kappa: f64,
}

/// Sample the curvature of `curve` at up to `max_samples` points.
///
/// Method: tessellate the curve at `tol` (all sample points lie ON the curve
/// for every curve type), decimate to at most `max_samples` feet, and take the
/// Menger curvature of each foot with its neighbors on the dense polyline
/// (`kappa = 4·Area / (|ab|·|bc|·|ca|)` — exact for circular arcs regardless
/// of sampling). Open endpoints copy their neighbor's curvature so the comb
/// reaches the ends. Returns an empty vec for degenerate curves.
pub fn curvature_profile(curve: &Curve, max_samples: usize, tol: f64) -> Vec<CurvatureSample> {
    let dense = curve.tessellate(tol);
    if dense.len() < 3 || max_samples < 2 {
        return Vec::new();
    }
    let closed = curve.is_closed();
    let n = dense.len();
    // Decimate: pick evenly-strided indices, always keeping both open ends.
    let count = max_samples.min(n);
    let idx: Vec<usize> = if closed {
        (0..count).map(|i| i * n / count).collect()
    } else {
        (0..count).map(|i| i * (n - 1) / (count - 1)).collect()
    };

    let sample_at = |i: usize| -> Option<CurvatureSample> {
        let (a, b, c) = if closed {
            (dense[(i + n - 1) % n], dense[i], dense[(i + 1) % n])
        } else if i == 0 || i == n - 1 {
            return None; // endpoints patched below
        } else {
            (dense[i - 1], dense[i], dense[i + 1])
        };
        let (ab, bc, ca) = (b - a, c - b, a - c);
        let (lab, lbc, lca) = (ab.length(), bc.length(), ca.length());
        if lab < 1e-12 || lbc < 1e-12 || lca < 1e-12 {
            return Some(CurvatureSample { point: b, normal: DVec3::ZERO, kappa: 0.0 });
        }
        let area2 = ab.cross(bc).length(); // 2·triangle area
        let kappa = 2.0 * area2 / (lab * lbc * lca);
        // Normal: from the foot toward the circumcenter — the midchord
        // direction with the tangent component removed.
        let tangent = (c - a).normalize_or_zero();
        let v = (a + c) * 0.5 - b;
        let normal = (v - tangent * v.dot(tangent)).normalize_or_zero();
        Some(CurvatureSample { point: b, normal, kappa })
    };

    let mut out: Vec<CurvatureSample> = Vec::with_capacity(count);
    for &i in &idx {
        match sample_at(i) {
            Some(s) => out.push(s),
            None => out.push(CurvatureSample {
                point: dense[i],
                normal: DVec3::ZERO,
                kappa: 0.0,
            }),
        }
    }
    // Open endpoints: copy the neighboring interior curvature/normal so the
    // comb doesn't collapse to zero at the ends of arcs.
    if !closed && out.len() >= 3 {
        let (k1, n1) = (out[1].kappa, out[1].normal);
        out[0].kappa = k1;
        out[0].normal = n1;
        let m = out.len();
        let (k2, n2) = (out[m - 2].kappa, out[m - 2].normal);
        out[m - 1].kappa = k2;
        out[m - 1].normal = n2;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn circle_curvature_is_inverse_radius() {
        let c = Curve::Arc {
            center: DVec3::new(2.0, 1.0, 0.0),
            radius: 5.0,
            start: 0.0,
            end: std::f64::consts::TAU,
        };
        let prof = curvature_profile(&c, 24, 0.001);
        assert_eq!(prof.len(), 24);
        for s in &prof {
            assert!((s.kappa - 0.2).abs() < 1e-6, "kappa {} != 1/5", s.kappa);
            // Normal points from the rim toward the center.
            let to_center = (DVec3::new(2.0, 1.0, 0.0) - s.point).normalize();
            assert!(s.normal.dot(to_center) > 0.999, "normal off-center");
        }
    }

    #[test]
    fn line_curvature_is_zero() {
        let l = Curve::Polyline {
            points: vec![DVec3::ZERO, DVec3::new(3.0, 0.0, 0.0), DVec3::new(9.0, 0.0, 0.0)],
            closed: false,
        };
        for s in curvature_profile(&l, 10, 0.001) {
            assert!(s.kappa < 1e-9);
        }
    }

    #[test]
    fn open_arc_endpoints_carry_neighbor_curvature() {
        let c = Curve::Arc {
            center: DVec3::ZERO,
            radius: 2.0,
            start: 0.0,
            end: std::f64::consts::PI,
        };
        let prof = curvature_profile(&c, 16, 0.001);
        assert_eq!(prof.len(), 16);
        assert!((prof[0].kappa - 0.5).abs() < 1e-6);
        assert!((prof[15].kappa - 0.5).abs() < 1e-6);
        // Feet stay on the arc.
        for s in &prof {
            assert!((s.point.length() - 2.0).abs() < 1e-6);
        }
    }

    #[test]
    fn ellipse_curvature_extremes_at_axes() {
        // rx=4, ry=2: kappa max = rx/ry^2 = 1.0 at (±rx,0), min = ry/rx^2 = 0.125.
        let e = Curve::Ellipse { center: DVec3::ZERO, rx: 4.0, ry: 2.0 };
        let prof = curvature_profile(&e, 64, 1e-4);
        let kmax = prof.iter().map(|s| s.kappa).fold(0.0, f64::max);
        let kmin = prof.iter().map(|s| s.kappa).fold(f64::MAX, f64::min);
        assert!((kmax - 1.0).abs() < 0.05, "kmax {kmax}");
        assert!((kmin - 0.125).abs() < 0.02, "kmin {kmin}");
    }

    #[test]
    fn degenerate_returns_empty() {
        let l = Curve::Line { a: DVec3::ZERO, b: DVec3::X };
        assert!(curvature_profile(&l, 1, 0.01).is_empty());
    }
}
