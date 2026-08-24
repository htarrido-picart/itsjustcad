//! Curve construction helpers: global cubic interpolation, helix, and
//! resampling (rebuild).

use glam::DVec3;
use std::f64::consts::TAU;

use crate::Curve;

/// Chord-length parameter values in `[0, 1]` for `pts` (closed adds the
/// wrap-around segment). Returns `None` if all points coincide.
fn chord_params(pts: &[DVec3], closed: bool) -> Option<Vec<f64>> {
    let n = pts.len();
    let mut d = vec![0.0f64; n];
    for i in 1..n {
        d[i] = d[i - 1] + pts[i].distance(pts[i - 1]);
    }
    let total = if closed {
        d[n - 1] + pts[n - 1].distance(pts[0])
    } else {
        d[n - 1]
    };
    if total < 1e-12 {
        return None;
    }
    Some(d.into_iter().map(|x| x / total).collect())
}

/// Solve a tridiagonal system `A x = rhs` (Thomas algorithm). `sub`, `diag`,
/// `sup` are the three bands (length n; `sub[0]` and `sup[n-1]` unused).
fn solve_tridiagonal(sub: &[f64], diag: &[f64], sup: &[f64], rhs: &[DVec3]) -> Vec<DVec3> {
    let n = diag.len();
    let mut c = vec![0.0f64; n];
    let mut d = vec![DVec3::ZERO; n];
    c[0] = sup[0] / diag[0];
    d[0] = rhs[0] / diag[0];
    for i in 1..n {
        let m = diag[i] - sub[i] * c[i - 1];
        c[i] = sup[i] / m;
        d[i] = (rhs[i] - d[i - 1] * sub[i]) / m;
    }
    let mut x = vec![DVec3::ZERO; n];
    x[n - 1] = d[n - 1];
    for i in (0..n - 1).rev() {
        x[i] = d[i] - x[i + 1] * c[i];
    }
    x
}

/// C2 cubic spline interpolating `pts` exactly (global interpolation).
///
/// Method: build a natural cubic spline (Piegl & Tiller, "The NURBS Book"
/// §9.2 global interpolation; equivalently the classic tridiagonal
/// cubic-spline setup, de Boor "A Practical Guide to Splines" ch. IV). We
/// solve a tridiagonal system for the per-point tangents `m_i` (Thomas
/// algorithm), then convert each Hermite segment `(p_i, p_{i+1}, m_i,
/// m_{i+1})` to its four Bézier control points and assemble one clamped
/// degree-3 NURBS. Because every segment is a Bézier that interpolates its
/// endpoints, the curve passes through every input point exactly, and shared
/// tangents make it C1 (C2 for the natural end conditions used here).
///
/// Chord-length spacing enters through the tangent RHS. Returns `None` if
/// fewer than 3 points or all points coincide.
pub fn interpolate_curve(pts: &[DVec3], closed: bool) -> Option<Curve> {
    let n = pts.len();
    if n < 3 {
        return None;
    }
    // Reject fully-degenerate input (all coincident).
    chord_params(pts, closed)?;

    if closed {
        return interpolate_closed(pts);
    }

    // Open natural cubic: tangents m_i solve the tridiagonal system
    //   m_{i-1} + 4 m_i + m_{i+1} = 3 (p_{i+1} - p_{i-1})   (interior)
    // with natural ends 2 m_0 + m_1 = 3(p_1 - p_0) and
    //                    m_{n-2} + 2 m_{n-1} = 3(p_{n-1} - p_{n-2}).
    let mut sub = vec![1.0f64; n];
    let mut diag = vec![4.0f64; n];
    let mut sup = vec![1.0f64; n];
    let mut rhs = vec![DVec3::ZERO; n];
    diag[0] = 2.0;
    sup[0] = 1.0;
    rhs[0] = (pts[1] - pts[0]) * 3.0;
    diag[n - 1] = 2.0;
    sub[n - 1] = 1.0;
    rhs[n - 1] = (pts[n - 1] - pts[n - 2]) * 3.0;
    for i in 1..n - 1 {
        rhs[i] = (pts[i + 1] - pts[i - 1]) * 3.0;
    }
    let m = solve_tridiagonal(&sub, &diag, &sup, &rhs);
    Some(hermite_to_nurbs(pts, &m, false))
}

/// Build a clamped degree-3 NURBS from Hermite data (points + tangents).
/// Each segment contributes cubic Bézier control points; endpoints of
/// adjacent segments coincide, so control count is `3*segs + 1`.
fn hermite_to_nurbs(pts: &[DVec3], tangents: &[DVec3], closed: bool) -> Curve {
    let n = pts.len();
    let segs = if closed { n } else { n - 1 };
    let mut control = Vec::with_capacity(3 * segs + 1);
    for i in 0..segs {
        let p0 = pts[i];
        let p1 = pts[(i + 1) % n];
        let m0 = tangents[i];
        let m1 = tangents[(i + 1) % n];
        if i == 0 {
            control.push(p0);
        }
        control.push(p0 + m0 / 3.0);
        control.push(p1 - m1 / 3.0);
        control.push(p1);
    }
    // Piecewise-Bézier knot vector: clamped ends plus each interior segment
    // boundary repeated with multiplicity 3 (degree). This makes every group
    // of four control points an independent cubic Bézier, so each segment
    // interpolates its endpoints exactly.
    let mut knots = vec![0.0; 4];
    for s in 1..segs {
        let v = s as f64 / segs as f64;
        knots.extend([v, v, v]);
    }
    knots.extend([1.0; 4]);
    let m = control.len();
    debug_assert_eq!(knots.len(), m + 3 + 1);
    Curve::Nurbs { control, weights: vec![1.0; m], knots, degree: 3 }
}

/// Closed interpolation: periodic natural cubic through the points. Solves the
/// cyclic tridiagonal system (Sherman–Morrison) for the periodic tangents,
/// then wraps the Hermite segments into a closed NURBS.
fn interpolate_closed(pts: &[DVec3]) -> Option<Curve> {
    let n = pts.len();
    // Cyclic tridiagonal (Sherman–Morrison): diag 4, off 1, rhs 3*(p_{i+1}-p_{i-1}).
    let mut diag = vec![4.0f64; n];
    let sub = vec![1.0f64; n];
    let sup = vec![1.0f64; n];
    let mut rhs = vec![DVec3::ZERO; n];
    for i in 0..n {
        let prev = pts[(i + n - 1) % n];
        let next = pts[(i + 1) % n];
        rhs[i] = (next - prev) * 3.0;
    }
    // Sherman–Morrison for the cyclic corners (corner entries = 1).
    let gamma = -diag[0];
    diag[0] -= gamma;
    diag[n - 1] -= sup[n - 1] * sub[0] / gamma;
    let d = solve_tridiagonal(&sub, &diag, &sup, &rhs);
    let mut uu = vec![DVec3::ZERO; n];
    uu[0] = DVec3::splat(gamma);
    uu[n - 1] = DVec3::splat(sup[n - 1]);
    let z = solve_tridiagonal(&sub, &diag, &sup, &uu);
    let fact = (d[0] + d[n - 1] * (sub[0] / gamma))
        / (1.0 + z[0] + z[n - 1] * (sub[0] / gamma));
    let deriv: Vec<DVec3> = (0..n).map(|i| d[i] - z[i] * fact).collect();
    Some(hermite_to_nurbs(pts, &deriv, true))
}

/// A 3D helix about `center`, axis +Z, of `radius`, total `height`, and
/// `turns` revolutions. Returned as a dense polyline (36 segments per turn):
/// the NURBS type cannot represent a rational helix exactly and a dense
/// polyline is visually and geometrically faithful for CAD use.
pub fn helix(center: DVec3, radius: f64, height: f64, turns: f64) -> Option<Curve> {
    if radius <= 0.0 || turns.abs() < 1e-9 {
        return None;
    }
    let per_turn = 36usize;
    let n = ((turns.abs() * per_turn as f64).ceil() as usize).max(2);
    let points = (0..=n)
        .map(|i| {
            let f = i as f64 / n as f64;
            let ang = TAU * turns * f;
            center + DVec3::new(radius * ang.cos(), radius * ang.sin(), height * f)
        })
        .collect();
    Some(Curve::Polyline { points, closed: false })
}

/// Resample any curve to exactly `n` points along its length, returned as an
/// open (or closed, matching the source) polyline. `n >= 2`.
pub fn rebuild(curve: &Curve, n: usize, tol: f64) -> Option<Curve> {
    if n < 2 {
        return None;
    }
    let closed = curve.is_closed();
    let dense = curve.tessellate(tol);
    if dense.len() < 2 {
        return None;
    }
    // Arc-length table over the dense samples.
    let mut path = dense.clone();
    if closed {
        path.push(dense[0]); // close for even arc-length spacing
    }
    let mut acc = vec![0.0f64];
    for w in path.windows(2) {
        acc.push(acc.last().unwrap() + w[0].distance(w[1]));
    }
    let total = *acc.last().unwrap();
    if total < 1e-12 {
        return None;
    }
    // Closed: n points spread around the loop (last != first). Open: n points
    // including both ends (n-1 gaps).
    let denom = if closed { n } else { n - 1 } as f64;
    let points = (0..n)
        .map(|i| {
            let target = total * i as f64 / denom;
            sample_at_arclen(&path, &acc, target)
        })
        .collect();
    Some(Curve::Polyline { points, closed })
}

fn sample_at_arclen(path: &[DVec3], acc: &[f64], target: f64) -> DVec3 {
    // Binary search the cumulative table.
    let mut lo = 0usize;
    let mut hi = acc.len() - 1;
    while lo + 1 < hi {
        let mid = (lo + hi) / 2;
        if acc[mid] <= target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let seg = acc[hi] - acc[lo];
    let t = if seg < 1e-12 { 0.0 } else { (target - acc[lo]) / seg };
    path[lo] + (path[hi] - path[lo]) * t
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nurbs::nurbs_point;

    fn eval(c: &Curve, t: f64) -> DVec3 {
        match c {
            Curve::Nurbs { control, weights, knots, degree } => {
                let (t0, t1) = (knots[*degree], knots[knots.len() - degree - 1]);
                nurbs_point(control, weights, knots, *degree, t0 + (t1 - t0) * t)
            }
            _ => panic!("not nurbs"),
        }
    }

    #[test]
    fn interpolate_passes_through_points() {
        let pts = vec![
            DVec3::new(0.0, 0.0, 0.0),
            DVec3::new(1.0, 2.0, 0.0),
            DVec3::new(3.0, 1.0, 0.0),
            DVec3::new(5.0, 3.0, 0.0),
            DVec3::new(7.0, 0.0, 0.0),
        ];
        let c = interpolate_curve(&pts, false).unwrap();
        let Curve::Nurbs { ref control, ref weights, ref knots, degree } = c else { panic!() };
        // Each Hermite/Bézier segment interpolates its endpoints, so point i
        // sits at knot value i/(n-1) in the uniform clamped parameterization.
        let segs = (pts.len() - 1) as f64;
        for (i, p) in pts.iter().enumerate() {
            let got = nurbs_point(control, weights, knots, degree, i as f64 / segs);
            assert!(got.distance(*p) < 1e-6, "point {i}: {got} vs {p}");
        }
    }

    #[test]
    fn interpolate_endpoints_exact() {
        let pts = vec![
            DVec3::new(0.0, 0.0, 0.0),
            DVec3::new(2.0, 3.0, 1.0),
            DVec3::new(4.0, 0.0, 2.0),
            DVec3::new(6.0, -2.0, 0.0),
        ];
        let c = interpolate_curve(&pts, false).unwrap();
        assert!(eval(&c, 0.0).distance(pts[0]) < 1e-9);
        assert!(eval(&c, 1.0).distance(*pts.last().unwrap()) < 1e-9);
    }

    #[test]
    fn interpolate_closed_passes_through_points() {
        let pts = vec![
            DVec3::new(0.0, 0.0, 0.0),
            DVec3::new(4.0, 0.0, 0.0),
            DVec3::new(4.0, 4.0, 0.0),
            DVec3::new(0.0, 4.0, 0.0),
        ];
        let c = interpolate_curve(&pts, true).unwrap();
        assert!(c.is_closed());
        // Each input point should appear on the tessellated curve.
        let tess = c.tessellate(0.001);
        for p in &pts {
            let near = tess.iter().map(|q| q.distance(*p)).fold(f64::MAX, f64::min);
            assert!(near < 1e-4, "closed curve misses {p}: nearest {near}");
        }
    }

    #[test]
    fn interpolate_too_few_points() {
        assert!(interpolate_curve(&[DVec3::ZERO, DVec3::X], false).is_none());
    }

    #[test]
    fn helix_radius_and_height() {
        let c = helix(DVec3::new(1.0, 2.0, 0.0), 3.0, 12.0, 4.0).unwrap();
        let Curve::Polyline { points, closed } = c else { panic!() };
        assert!(!closed);
        // Radius: every point is `radius` from the axis in XY.
        for p in &points {
            let r = ((p.x - 1.0).powi(2) + (p.y - 2.0).powi(2)).sqrt();
            assert!((r - 3.0).abs() < 1e-9, "radius {r}");
        }
        // Height: z spans 0..12.
        let zmin = points.iter().map(|p| p.z).fold(f64::MAX, f64::min);
        let zmax = points.iter().map(|p| p.z).fold(f64::MIN, f64::max);
        assert!((zmin - 0.0).abs() < 1e-9 && (zmax - 12.0).abs() < 1e-9);
        // 4 turns * 36 segments/turn = 144 segments -> 145 points.
        assert_eq!(points.len(), 145);
    }

    #[test]
    fn helix_rejects_bad_input() {
        assert!(helix(DVec3::ZERO, 0.0, 5.0, 2.0).is_none());
        assert!(helix(DVec3::ZERO, 1.0, 5.0, 0.0).is_none());
    }

    #[test]
    fn rebuild_open_count_and_endpoints() {
        let line = Curve::Line { a: DVec3::ZERO, b: DVec3::new(10.0, 0.0, 0.0) };
        let Curve::Polyline { points, closed } = rebuild(&line, 6, 0.01).unwrap() else {
            panic!()
        };
        assert!(!closed);
        assert_eq!(points.len(), 6);
        assert!(points[0].distance(DVec3::ZERO) < 1e-9);
        assert!(points[5].distance(DVec3::new(10.0, 0.0, 0.0)) < 1e-9);
        // Even spacing: 2 units apart.
        assert!(points[1].distance(DVec3::new(2.0, 0.0, 0.0)) < 1e-9);
    }

    #[test]
    fn rebuild_closed_count() {
        let sq = Curve::Polyline {
            points: vec![
                DVec3::ZERO,
                DVec3::new(4.0, 0.0, 0.0),
                DVec3::new(4.0, 4.0, 0.0),
                DVec3::new(0.0, 4.0, 0.0),
            ],
            closed: true,
        };
        let Curve::Polyline { points, closed } = rebuild(&sq, 8, 0.01).unwrap() else { panic!() };
        assert!(closed);
        assert_eq!(points.len(), 8);
        // Perimeter 16, 8 points -> 2 units spacing; last->first also 2.
        assert!(points[0].distance(points[1]) - 2.0 < 1e-9);
    }

    #[test]
    fn rebuild_needs_two_points() {
        let line = Curve::Line { a: DVec3::ZERO, b: DVec3::X };
        assert!(rebuild(&line, 1, 0.01).is_none());
    }
}
