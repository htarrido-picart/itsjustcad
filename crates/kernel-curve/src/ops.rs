//! Curve editing operations in the XY plane: closest point, curve-curve
//! intersection, split, extend, join and fillet.
//!
//! Line/Polyline/Arc are handled analytically; Ellipse/NURBS fall back to a
//! tessellated polyline where a fallback makes sense and are rejected where
//! exactness matters (split).

use glam::{DVec2, DVec3};
use std::f64::consts::TAU;

use crate::Curve;

const EPS: f64 = 1e-9;

/// Endpoint-matching tolerance for `join_curves` (and callers' cut dedup).
pub const JOIN_TOL: f64 = 1e-6;

// ---------------------------------------------------------------- closest

fn closest_on_segment(a: DVec3, b: DVec3, p: DVec3) -> DVec3 {
    let ab = b - a;
    let len2 = ab.length_squared();
    if len2 < EPS * EPS {
        return a;
    }
    a + ab * ((p - a).dot(ab) / len2).clamp(0.0, 1.0)
}

fn closest_on_path(pts: &[DVec3], closed: bool, p: DVec3) -> DVec3 {
    let n = pts.len();
    let segs = if closed { n } else { n.saturating_sub(1) };
    (0..segs.max(1).min(if n == 1 { 1 } else { segs })).fold(pts[0], |best, i| {
        let c = closest_on_segment(pts[i], pts[(i + 1) % n], p);
        if c.distance_squared(p) < best.distance_squared(p) { c } else { best }
    })
}

fn arc_point(center: DVec3, radius: f64, ang: f64) -> DVec3 {
    center + DVec3::new(radius * ang.cos(), radius * ang.sin(), 0.0)
}

/// Closest point on the curve to `p`. Ellipse/NURBS use a tessellation at
/// `tol` chord deviation.
pub fn closest_point(c: &Curve, p: DVec3, tol: f64) -> DVec3 {
    match c {
        Curve::Line { a, b } => closest_on_segment(*a, *b, p),
        Curve::Polyline { points, .. } => closest_on_path(points, c.is_closed(), p),
        Curve::Arc { center, radius, start, end } => {
            let sweep = end - start;
            let v = (p - *center).truncate();
            if v.length_squared() < EPS * EPS {
                return arc_point(*center, *radius, *start);
            }
            let off = (v.y.atan2(v.x) - start).rem_euclid(TAU);
            if off <= sweep + EPS {
                arc_point(*center, *radius, start + off.min(sweep))
            } else {
                let s = arc_point(*center, *radius, *start);
                let e = arc_point(*center, *radius, *end);
                if s.distance_squared(p) <= e.distance_squared(p) { s } else { e }
            }
        }
        _ => closest_on_path(&c.tessellate(tol), c.is_closed(), p),
    }
}

// ----------------------------------------------------------- intersections

/// Analytic form used by the intersector: a segment path or a circular arc.
enum Prim {
    Path(Vec<DVec3>, bool),
    Circ { c: DVec3, r: f64, start: f64, sweep: f64 },
}

fn prim(c: &Curve, tol: f64) -> Prim {
    match c {
        Curve::Line { a, b } => Prim::Path(vec![*a, *b], false),
        Curve::Polyline { points, .. } => Prim::Path(points.clone(), c.is_closed()),
        Curve::Arc { center, radius, start, end } => Prim::Circ {
            c: *center,
            r: *radius,
            start: *start,
            sweep: end - start,
        },
        _ => Prim::Path(c.tessellate(tol), c.is_closed()),
    }
}

fn angle_in(start: f64, sweep: f64, ang: f64) -> bool {
    sweep >= TAU - EPS || (ang - start).rem_euclid(TAU) <= sweep + 1e-7
}

fn seg_seg_xy(a1: DVec3, a2: DVec3, b1: DVec3, b2: DVec3) -> Option<DVec3> {
    let d1 = (a2 - a1).truncate();
    let d2 = (b2 - b1).truncate();
    let denom = d1.perp_dot(d2);
    if denom.abs() < 1e-12 {
        return None; // parallel (colinear overlap intentionally yields nothing)
    }
    let w = (b1 - a1).truncate();
    let t = w.perp_dot(d2) / denom;
    let u = w.perp_dot(d1) / denom;
    let e = 1e-9;
    if !(-e..=1.0 + e).contains(&t) || !(-e..=1.0 + e).contains(&u) {
        return None;
    }
    Some(a1 + (a2 - a1) * t.clamp(0.0, 1.0))
}

fn seg_arc_xy(a: DVec3, b: DVec3, c: DVec3, r: f64, start: f64, sweep: f64, out: &mut Vec<DVec3>) {
    // |a + t(b-a) - c|² = r² in XY → quadratic in t.
    let d = (b - a).truncate();
    let f = (a - c).truncate();
    let qa = d.length_squared();
    if qa < EPS * EPS {
        return;
    }
    let qb = 2.0 * f.dot(d);
    let qc = f.length_squared() - r * r;
    let disc = qb * qb - 4.0 * qa * qc;
    if disc < 0.0 {
        return;
    }
    let sq = disc.sqrt();
    for t in [(-qb - sq) / (2.0 * qa), (-qb + sq) / (2.0 * qa)] {
        if !(-1e-9..=1.0 + 1e-9).contains(&t) {
            continue;
        }
        let p = a + (b - a) * t.clamp(0.0, 1.0);
        let v = (p - c).truncate();
        if angle_in(start, sweep, v.y.atan2(v.x)) {
            out.push(DVec3::new(p.x, p.y, c.z));
        }
    }
}

type Circ = (DVec3, f64, f64, f64); // center, radius, start, sweep

fn circ_circ_xy((c1, r1, s1, w1): Circ, (c2, r2, s2, w2): Circ, out: &mut Vec<DVec3>) {
    let d = (c2 - c1).truncate();
    let dist = d.length();
    if dist < EPS || dist > r1 + r2 + EPS || dist < (r1 - r2).abs() - EPS {
        return; // concentric, too far apart, or one inside the other
    }
    let a = (r1 * r1 - r2 * r2 + dist * dist) / (2.0 * dist);
    let h2 = r1 * r1 - a * a;
    let h = h2.max(0.0).sqrt();
    let base = c1.truncate() + d * (a / dist);
    let perp = DVec2::new(-d.y, d.x) * (h / dist);
    let mut push = |p: DVec2| {
        let v1 = p - c1.truncate();
        let v2 = p - c2.truncate();
        if angle_in(s1, w1, v1.y.atan2(v1.x)) && angle_in(s2, w2, v2.y.atan2(v2.x)) {
            out.push(DVec3::new(p.x, p.y, c1.z));
        }
    };
    push(base + perp);
    if h > EPS {
        push(base - perp);
    }
}

/// All XY intersection points between two curves. Line/polyline/arc pairs are
/// analytic; ellipse/NURBS are tessellated at `tol`. Points within `JOIN_TOL`
/// of each other are deduplicated. Tangencies count once.
pub fn intersections(a: &Curve, b: &Curve, tol: f64) -> Vec<DVec3> {
    let mut pts = Vec::new();
    match (prim(a, tol), prim(b, tol)) {
        (Prim::Path(pa, ca), Prim::Path(pb, cb)) => {
            let (na, nb) = (pa.len(), pb.len());
            let (sa, sb) = (
                if ca { na } else { na.saturating_sub(1) },
                if cb { nb } else { nb.saturating_sub(1) },
            );
            for i in 0..sa {
                for j in 0..sb {
                    if let Some(p) =
                        seg_seg_xy(pa[i], pa[(i + 1) % na], pb[j], pb[(j + 1) % nb])
                    {
                        pts.push(p);
                    }
                }
            }
        }
        (Prim::Path(pa, ca), Prim::Circ { c, r, start, sweep })
        | (Prim::Circ { c, r, start, sweep }, Prim::Path(pa, ca)) => {
            let n = pa.len();
            let segs = if ca { n } else { n.saturating_sub(1) };
            for i in 0..segs {
                seg_arc_xy(pa[i], pa[(i + 1) % n], c, r, start, sweep, &mut pts);
            }
        }
        (
            Prim::Circ { c: c1, r: r1, start: s1, sweep: w1 },
            Prim::Circ { c: c2, r: r2, start: s2, sweep: w2 },
        ) => circ_circ_xy((c1, r1, s1, w1), (c2, r2, s2, w2), &mut pts),
    }
    // Dedup near-coincident hits (shared polyline vertices, tangencies).
    let mut unique: Vec<DVec3> = Vec::with_capacity(pts.len());
    for p in pts {
        if !unique.iter().any(|q| q.truncate().distance(p.truncate()) < JOIN_TOL) {
            unique.push(p);
        }
    }
    unique
}

// ------------------------------------------------------------------- split

fn dedup_sorted(mut vals: Vec<f64>, tol: f64) -> Vec<f64> {
    vals.sort_by(|a, b| a.partial_cmp(b).expect("finite params"));
    vals.dedup_by(|a, b| (*a - *b).abs() < tol);
    vals
}

fn poly_point_at(pts: &[DVec3], s: f64) -> DVec3 {
    let n = pts.len();
    let i = (s.floor() as usize) % n;
    let t = s - s.floor();
    pts[i] + (pts[(i + 1) % n] - pts[i]) * t
}

/// Polyline piece from param `s0` to `s1` (`s1 > s0`; may wrap past the last
/// segment on closed loops via indices mod n).
fn poly_piece(pts: &[DVec3], s0: f64, s1: f64) -> Vec<DVec3> {
    let n = pts.len();
    let mut out = vec![poly_point_at(pts, s0)];
    let mut k = s0.floor() as i64 + 1;
    while (k as f64) < s1 - 1e-9 {
        let v = pts[(k as usize) % n];
        if out.last().is_none_or(|l| l.distance(v) > EPS) {
            out.push(v);
        }
        k += 1;
    }
    let end = poly_point_at(pts, s1);
    if out.last().is_none_or(|l| l.distance(end) > EPS) {
        out.push(end);
    }
    out
}

/// Param of `p` along a polyline: closest segment index + fraction.
fn poly_param(pts: &[DVec3], closed: bool, p: DVec3) -> f64 {
    let n = pts.len();
    let segs = if closed { n } else { n - 1 };
    let mut best = (f64::MAX, 0.0);
    for i in 0..segs {
        let (a, b) = (pts[i], pts[(i + 1) % n]);
        let ab = b - a;
        let len2 = ab.length_squared();
        let t = if len2 < EPS * EPS { 0.0 } else { ((p - a).dot(ab) / len2).clamp(0.0, 1.0) };
        let d = (a + ab * t).distance_squared(p);
        if d < best.0 {
            best = (d, i as f64 + t);
        }
    }
    best.1
}

/// Split a curve at points that lie on it. Open curves with k interior cuts
/// yield k+1 pieces; closed Polyline/Arc need 2+ distinct cuts and yield one
/// open piece per cut. Returns `None` for Ellipse/NURBS (unsupported) and for
/// closed curves with fewer than 2 distinct cut points.
pub fn split_at_points(c: &Curve, pts: &[DVec3], tol: f64) -> Option<Vec<Curve>> {
    match c {
        Curve::Line { a, b } => {
            let ab = *b - *a;
            let len = ab.length();
            if len < tol {
                return None;
            }
            let tol_t = tol / len;
            let ts = dedup_sorted(
                pts.iter()
                    .map(|p| (*p - *a).dot(ab) / (len * len))
                    .filter(|t| (tol_t..=1.0 - tol_t).contains(t))
                    .collect(),
                tol_t,
            );
            let mut cuts = vec![0.0];
            cuts.extend(ts);
            cuts.push(1.0);
            Some(
                cuts.windows(2)
                    .map(|w| Curve::Line { a: *a + ab * w[0], b: *a + ab * w[1] })
                    .collect(),
            )
        }
        Curve::Polyline { points, .. } => {
            let closed = c.is_closed();
            let n = points.len();
            let segs = if closed { n } else { n - 1 } as f64;
            let params: Vec<f64> =
                pts.iter().map(|p| poly_param(points, closed, *p)).collect();
            if closed {
                let params = dedup_sorted(params, 1e-9);
                if params.len() < 2 {
                    return None;
                }
                let pieces = params
                    .iter()
                    .zip(params.iter().cycle().skip(1))
                    .take(params.len())
                    .map(|(&s0, &s1)| {
                        let s1 = if s1 <= s0 { s1 + segs } else { s1 };
                        Curve::Polyline { points: poly_piece(points, s0, s1), closed: false }
                    })
                    .collect();
                Some(pieces)
            } else {
                let interior = dedup_sorted(
                    params.into_iter().filter(|&s| s > 1e-9 && s < segs - 1e-9).collect(),
                    1e-9,
                );
                let mut cuts = vec![0.0];
                cuts.extend(interior);
                cuts.push(segs);
                Some(
                    cuts.windows(2)
                        .map(|w| Curve::Polyline {
                            points: poly_piece(points, w[0], w[1]),
                            closed: false,
                        })
                        .collect(),
                )
            }
        }
        Curve::Arc { center, radius, start, end } => {
            let sweep = end - start;
            if sweep <= EPS || *radius < tol {
                return None;
            }
            let tol_a = tol / radius;
            let offs: Vec<f64> = pts
                .iter()
                .map(|p| {
                    let v = (*p - *center).truncate();
                    (v.y.atan2(v.x) - start).rem_euclid(TAU)
                })
                .collect();
            let arc = |o0: f64, o1: f64| Curve::Arc {
                center: *center,
                radius: *radius,
                start: start + o0,
                end: start + o1,
            };
            if c.is_closed() {
                let offs = dedup_sorted(offs, tol_a);
                if offs.len() < 2 {
                    return None;
                }
                Some(
                    offs.iter()
                        .zip(offs.iter().cycle().skip(1))
                        .take(offs.len())
                        .map(|(&o0, &o1)| arc(o0, if o1 <= o0 { o1 + TAU } else { o1 }))
                        .collect(),
                )
            } else {
                let interior = dedup_sorted(
                    offs.into_iter().filter(|&o| o > tol_a && o < sweep - tol_a).collect(),
                    tol_a,
                );
                let mut cuts = vec![0.0];
                cuts.extend(interior);
                cuts.push(sweep);
                Some(cuts.windows(2).map(|w| arc(w[0], w[1])).collect())
            }
        }
        Curve::Ellipse { .. } | Curve::Nurbs { .. } => None,
    }
}

// ------------------------------------------------------------------ extend

/// Extend both open ends of a curve by `dist`: lines and open polylines
/// extend tangentially along their end segments; open arcs follow their
/// circle (clamped to a full circle). Closed curves and NURBS/ellipses
/// return `None`.
pub fn extend(c: &Curve, dist: f64) -> Option<Curve> {
    if c.is_closed() {
        return None;
    }
    match c {
        Curve::Line { a, b } => {
            let dir = (*b - *a).normalize_or_zero();
            (dir != DVec3::ZERO)
                .then_some(Curve::Line { a: *a - dir * dist, b: *b + dir * dist })
        }
        Curve::Polyline { points, .. } if points.len() >= 2 => {
            let mut points = points.clone();
            let d0 = (points[1] - points[0]).normalize_or_zero();
            let d1 = (points[points.len() - 1] - points[points.len() - 2]).normalize_or_zero();
            if d0 == DVec3::ZERO || d1 == DVec3::ZERO {
                return None;
            }
            points[0] -= d0 * dist;
            let last = points.len() - 1;
            points[last] += d1 * dist;
            Some(Curve::Polyline { points, closed: false })
        }
        Curve::Arc { center, radius, start, end } => {
            if *radius < EPS {
                return None;
            }
            let dang = dist / radius;
            let (mut s, mut e) = (start - dang, end + dang);
            if e - s >= TAU {
                // Clamp to a full circle, centered on the original sweep.
                let mid = (start + end) / 2.0;
                (s, e) = (mid - TAU / 2.0, mid + TAU / 2.0);
            }
            Some(Curve::Arc { center: *center, radius: *radius, start: s, end: e })
        }
        _ => None,
    }
}

// -------------------------------------------------------------------- join

fn chain_of(c: &Curve, chord_tol: f64) -> Option<Vec<DVec3>> {
    if c.is_closed() {
        return None;
    }
    match c {
        Curve::Line { a, b } => Some(vec![*a, *b]),
        Curve::Polyline { points, .. } => Some(points.clone()),
        _ => Some(c.tessellate(chord_tol)), // open arc / nurbs sample
    }
}

/// Chain end-touching open curves (endpoint gap ≤ `tol`) into one polyline;
/// arcs and NURBS are tessellated at `chord_tol`. The result closes when the
/// free ends meet. Returns `None` when any curve is closed/degenerate or the
/// set cannot be chained into a single run.
pub fn join_curves(curves: &[Curve], tol: f64, chord_tol: f64) -> Option<Curve> {
    let mut pool: Vec<Vec<DVec3>> = curves
        .iter()
        .map(|c| chain_of(c, chord_tol).filter(|p| p.len() >= 2))
        .collect::<Option<_>>()?;
    let mut chain = pool.swap_remove(0);
    while !pool.is_empty() {
        let (head, tail) = (chain[0], *chain.last().expect("non-empty"));
        let found = pool.iter().position(|c| {
            let (s, e) = (c[0], *c.last().expect("non-empty"));
            tail.distance(s) <= tol
                || tail.distance(e) <= tol
                || head.distance(s) <= tol
                || head.distance(e) <= tol
        })?;
        let mut next = pool.swap_remove(found);
        let (s, e) = (next[0], *next.last().expect("non-empty"));
        if tail.distance(s) <= tol {
            chain.extend_from_slice(&next[1..]);
        } else if tail.distance(e) <= tol {
            next.reverse();
            chain.extend_from_slice(&next[1..]);
        } else if head.distance(e) <= tol {
            next.extend_from_slice(&chain[1..]);
            chain = next;
        } else {
            next.reverse();
            next.extend_from_slice(&chain[1..]);
            chain = next;
        }
    }
    let closed = chain.len() >= 4 && chain[0].distance(*chain.last().expect("non-empty")) <= tol;
    if closed {
        chain.pop();
    }
    Some(Curve::Polyline { points: chain, closed })
}

// ------------------------------------------------------------------ fillet

/// Fillet two line segments in the XY plane with a tangent arc of `radius`,
/// trimming both to the tangency points. Each line keeps the endpoint
/// farther from the (extended) intersection. Returns
/// `(trimmed a, arc, trimmed b)`, or `None` when the lines are parallel or
/// the radius does not fit within either line.
pub fn fillet_lines(
    a: (DVec3, DVec3),
    b: (DVec3, DVec3),
    radius: f64,
) -> Option<(Curve, Curve, Curve)> {
    let d1 = (a.1 - a.0).truncate();
    let d2 = (b.1 - b.0).truncate();
    let denom = d1.perp_dot(d2);
    if denom.abs() < 1e-12 || radius <= 0.0 {
        return None;
    }
    let w = (b.0 - a.0).truncate();
    let t1 = w.perp_dot(d2) / denom;
    let z = a.0.z;
    let p = (a.0.truncate() + d1 * t1).extend(z);
    // Keep the endpoint of each line farther from the intersection.
    let keep = |l: (DVec3, DVec3)| if l.0.distance_squared(p) >= l.1.distance_squared(p) { l.0 } else { l.1 };
    let (e1, e2) = (keep(a), keep(b));
    let u = (e1 - p).truncate().normalize_or_zero();
    let v = (e2 - p).truncate().normalize_or_zero();
    if u == DVec2::ZERO || v == DVec2::ZERO {
        return None;
    }
    let cos_theta = u.dot(v).clamp(-1.0, 1.0);
    let theta = cos_theta.acos();
    if theta < 1e-6 || theta > std::f64::consts::PI - 1e-6 {
        return None; // colinear: no corner to round
    }
    let t = radius / (theta / 2.0).tan();
    if t > (e1 - p).truncate().length() + EPS || t > (e2 - p).truncate().length() + EPS {
        return None; // radius too large for the available line length
    }
    let t1p = p + (u * t).extend(0.0);
    let t2p = p + (v * t).extend(0.0);
    let bis = (u + v).normalize_or_zero();
    if bis == DVec2::ZERO {
        return None;
    }
    let center = p + (bis * (radius / (theta / 2.0).sin())).extend(0.0);
    let ang = |q: DVec3| {
        let d = (q - center).truncate();
        d.y.atan2(d.x)
    };
    let (mut s, mut e) = (ang(t1p), ang(t2p));
    if (t1p - center).truncate().perp_dot((t2p - center).truncate()) < 0.0 {
        std::mem::swap(&mut s, &mut e); // keep the arc CCW
    }
    if e < s {
        e += TAU;
    }
    Some((
        Curve::Line { a: t1p, b: e1 },
        Curve::Arc { center, radius, start: s, end: e },
        Curve::Line { a: t2p, b: e2 },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(ax: f64, ay: f64, bx: f64, by: f64) -> Curve {
        Curve::Line { a: DVec3::new(ax, ay, 0.0), b: DVec3::new(bx, by, 0.0) }
    }

    fn circle(cx: f64, cy: f64, r: f64) -> Curve {
        Curve::Arc { center: DVec3::new(cx, cy, 0.0), radius: r, start: 0.0, end: TAU }
    }

    #[test]
    fn closest_point_line_arc_polyline() {
        let l = line(0.0, 0.0, 10.0, 0.0);
        assert!(closest_point(&l, DVec3::new(3.0, 5.0, 0.0), 0.01)
            .distance(DVec3::new(3.0, 0.0, 0.0)) < EPS);
        // beyond the end clamps
        assert!(closest_point(&l, DVec3::new(20.0, 1.0, 0.0), 0.01)
            .distance(DVec3::new(10.0, 0.0, 0.0)) < EPS);

        let arc = Curve::Arc {
            center: DVec3::ZERO, radius: 2.0, start: 0.0, end: std::f64::consts::FRAC_PI_2,
        };
        // radially inside the sweep
        assert!(closest_point(&arc, DVec3::new(5.0, 5.0, 0.0), 0.01)
            .distance(DVec3::new(2.0 / 2f64.sqrt(), 2.0 / 2f64.sqrt(), 0.0)) < EPS);
        // outside the sweep snaps to the nearer endpoint
        assert!(closest_point(&arc, DVec3::new(3.0, -1.0, 0.0), 0.01)
            .distance(DVec3::new(2.0, 0.0, 0.0)) < EPS);

        let pl = Curve::Polyline {
            points: vec![DVec3::ZERO, DVec3::new(4.0, 0.0, 0.0), DVec3::new(4.0, 4.0, 0.0)],
            closed: false,
        };
        assert!(closest_point(&pl, DVec3::new(5.0, 2.0, 0.0), 0.01)
            .distance(DVec3::new(4.0, 2.0, 0.0)) < EPS);
    }

    #[test]
    fn intersect_crossing_lines() {
        let pts = intersections(&line(-2.0, 0.0, 8.0, 0.0), &line(0.0, -2.0, 0.0, 8.0), 0.01);
        assert_eq!(pts.len(), 1);
        assert!(pts[0].distance(DVec3::ZERO) < EPS);
        // parallel: none
        assert!(intersections(&line(0.0, 0.0, 5.0, 0.0), &line(0.0, 1.0, 5.0, 1.0), 0.01)
            .is_empty());
        // disjoint (segments would cross only if extended): none
        assert!(intersections(&line(0.0, 0.0, 1.0, 0.0), &line(5.0, -1.0, 5.0, 1.0), 0.01)
            .is_empty());
    }

    #[test]
    fn intersect_line_circle_and_tangent() {
        let pts = intersections(&line(-5.0, 0.0, 5.0, 0.0), &circle(0.0, 0.0, 2.0), 0.01);
        assert_eq!(pts.len(), 2);
        assert!(pts.iter().all(|p| (p.truncate().length() - 2.0).abs() < EPS));
        // tangent line touches once
        let pts = intersections(&line(-5.0, 2.0, 5.0, 2.0), &circle(0.0, 0.0, 2.0), 0.01);
        assert_eq!(pts.len(), 1);
        assert!(pts[0].distance(DVec3::new(0.0, 2.0, 0.0)) < 1e-6);
        // arc range filters: lower semicircle misses a line above
        let lower = Curve::Arc {
            center: DVec3::ZERO, radius: 2.0, start: std::f64::consts::PI, end: TAU,
        };
        assert!(intersections(&line(-5.0, 1.0, 5.0, 1.0), &lower, 0.01).is_empty());
    }

    #[test]
    fn intersect_circle_circle() {
        let pts = intersections(&circle(0.0, 0.0, 2.0), &circle(3.0, 0.0, 2.0), 0.01);
        assert_eq!(pts.len(), 2);
        for p in &pts {
            assert!((p.truncate().length() - 2.0).abs() < EPS);
            assert!(((p.truncate() - DVec2::new(3.0, 0.0)).length() - 2.0).abs() < EPS);
        }
        // disjoint
        assert!(intersections(&circle(0.0, 0.0, 1.0), &circle(5.0, 0.0, 1.0), 0.01).is_empty());
    }

    #[test]
    fn intersect_polyline_line() {
        let pl = Curve::Polyline {
            points: vec![DVec3::ZERO, DVec3::new(4.0, 0.0, 0.0), DVec3::new(4.0, 4.0, 0.0)],
            closed: false,
        };
        let pts = intersections(&pl, &line(2.0, -1.0, 2.0, 1.0), 0.01);
        assert_eq!(pts.len(), 1);
        assert!(pts[0].distance(DVec3::new(2.0, 0.0, 0.0)) < EPS);
        // through the corner: one dedup'd hit
        let pts = intersections(&pl, &line(3.0, -1.0, 5.0, 1.0), 0.01);
        assert_eq!(pts.len(), 1);
    }

    #[test]
    fn split_line_middle() {
        let pieces =
            split_at_points(&line(0.0, 0.0, 10.0, 0.0), &[DVec3::new(4.0, 0.0, 0.0)], 1e-6)
                .unwrap();
        assert_eq!(pieces.len(), 2);
        let Curve::Line { a, b } = pieces[0] else { panic!() };
        assert!(a.distance(DVec3::ZERO) < EPS && b.distance(DVec3::new(4.0, 0.0, 0.0)) < EPS);
        // cut at the very end: nothing to split
        let pieces =
            split_at_points(&line(0.0, 0.0, 10.0, 0.0), &[DVec3::new(10.0, 0.0, 0.0)], 1e-6)
                .unwrap();
        assert_eq!(pieces.len(), 1);
    }

    #[test]
    fn split_polyline_open_and_closed() {
        let pl = Curve::Polyline {
            points: vec![DVec3::ZERO, DVec3::new(4.0, 0.0, 0.0), DVec3::new(4.0, 4.0, 0.0)],
            closed: false,
        };
        let pieces = split_at_points(&pl, &[DVec3::new(4.0, 2.0, 0.0)], 1e-6).unwrap();
        assert_eq!(pieces.len(), 2);
        let Curve::Polyline { ref points, closed: false } = pieces[0] else { panic!() };
        assert_eq!(points.len(), 3); // 0,0 · 4,0 · 4,2

        // closed square split at two opposite edge midpoints → two open halves
        let sq = Curve::Polyline {
            points: vec![
                DVec3::ZERO,
                DVec3::new(4.0, 0.0, 0.0),
                DVec3::new(4.0, 4.0, 0.0),
                DVec3::new(0.0, 4.0, 0.0),
            ],
            closed: true,
        };
        let pieces = split_at_points(
            &sq,
            &[DVec3::new(2.0, 0.0, 0.0), DVec3::new(2.0, 4.0, 0.0)],
            1e-6,
        )
        .unwrap();
        assert_eq!(pieces.len(), 2);
        for p in &pieces {
            assert!(!p.is_closed());
        }
        // one cut on a closed loop cannot split
        assert!(split_at_points(&sq, &[DVec3::new(2.0, 0.0, 0.0)], 1e-6).is_none());
    }

    #[test]
    fn split_arc_and_circle() {
        let arc = Curve::Arc {
            center: DVec3::ZERO, radius: 2.0, start: 0.0, end: std::f64::consts::PI,
        };
        let pieces =
            split_at_points(&arc, &[DVec3::new(0.0, 2.0, 0.0)], 1e-6).unwrap();
        assert_eq!(pieces.len(), 2);
        let Curve::Arc { start, end, .. } = pieces[1] else { panic!() };
        assert!((start - std::f64::consts::FRAC_PI_2).abs() < 1e-9);
        assert!((end - std::f64::consts::PI).abs() < 1e-9);

        let pieces = split_at_points(
            &circle(0.0, 0.0, 2.0),
            &[DVec3::new(2.0, 0.0, 0.0), DVec3::new(-2.0, 0.0, 0.0)],
            1e-6,
        )
        .unwrap();
        assert_eq!(pieces.len(), 2);
        for p in &pieces {
            let Curve::Arc { start, end, .. } = p else { panic!() };
            assert!((end - start - std::f64::consts::PI).abs() < 1e-9);
        }
        assert!(split_at_points(&circle(0.0, 0.0, 2.0), &[DVec3::new(2.0, 0.0, 0.0)], 1e-6)
            .is_none());
    }

    #[test]
    fn extend_line_polyline_arc() {
        let Curve::Line { a, b } = extend(&line(0.0, 0.0, 10.0, 0.0), 2.0).unwrap() else {
            panic!()
        };
        assert!(a.distance(DVec3::new(-2.0, 0.0, 0.0)) < EPS);
        assert!(b.distance(DVec3::new(12.0, 0.0, 0.0)) < EPS);

        let pl = Curve::Polyline {
            points: vec![DVec3::ZERO, DVec3::new(4.0, 0.0, 0.0), DVec3::new(4.0, 4.0, 0.0)],
            closed: false,
        };
        let Curve::Polyline { points, .. } = extend(&pl, 1.0).unwrap() else { panic!() };
        assert!(points[0].distance(DVec3::new(-1.0, 0.0, 0.0)) < EPS);
        assert!(points[2].distance(DVec3::new(4.0, 5.0, 0.0)) < EPS);

        let arc = Curve::Arc {
            center: DVec3::ZERO, radius: 2.0, start: 0.0, end: std::f64::consts::FRAC_PI_2,
        };
        let Curve::Arc { start, end, .. } = extend(&arc, 1.0).unwrap() else { panic!() };
        assert!((start - (-0.5)).abs() < EPS && (end - (std::f64::consts::FRAC_PI_2 + 0.5)).abs() < EPS);

        // closed curves refuse
        assert!(extend(&circle(0.0, 0.0, 2.0), 1.0).is_none());
    }

    #[test]
    fn join_chains_and_closes() {
        // three sides of a square, one reversed, joined into one open polyline
        let curves = [
            line(0.0, 0.0, 4.0, 0.0),
            line(4.0, 4.0, 4.0, 0.0), // reversed
            line(4.0, 4.0, 0.0, 4.0),
        ];
        let Curve::Polyline { points, closed } = join_curves(&curves, JOIN_TOL, 0.01).unwrap()
        else {
            panic!()
        };
        assert!(!closed);
        assert_eq!(points.len(), 4);

        // fourth side closes the loop
        let curves = [
            line(0.0, 0.0, 4.0, 0.0),
            line(4.0, 0.0, 4.0, 4.0),
            line(4.0, 4.0, 0.0, 4.0),
            line(0.0, 4.0, 0.0, 0.0),
        ];
        let joined = join_curves(&curves, JOIN_TOL, 0.01).unwrap();
        assert!(joined.is_closed());

        // gap larger than tol: no join
        let curves = [line(0.0, 0.0, 4.0, 0.0), line(4.1, 0.0, 8.0, 0.0)];
        assert!(join_curves(&curves, JOIN_TOL, 0.01).is_none());
    }

    #[test]
    fn fillet_perpendicular_lines() {
        let (la, arc, lb) = fillet_lines(
            (DVec3::new(-2.0, 0.0, 0.0), DVec3::new(8.0, 0.0, 0.0)),
            (DVec3::new(0.0, -2.0, 0.0), DVec3::new(0.0, 8.0, 0.0)),
            2.0,
        )
        .unwrap();
        // lines trimmed to the tangency points, far ends kept
        let Curve::Line { a, b } = la else { panic!() };
        assert!(a.distance(DVec3::new(2.0, 0.0, 0.0)) < EPS);
        assert!(b.distance(DVec3::new(8.0, 0.0, 0.0)) < EPS);
        let Curve::Line { a, b } = lb else { panic!() };
        assert!(a.distance(DVec3::new(0.0, 2.0, 0.0)) < EPS);
        assert!(b.distance(DVec3::new(0.0, 8.0, 0.0)) < EPS);
        // arc: center 2,2 radius 2, quarter sweep, tangent at both trim points
        let Curve::Arc { center, radius, start, end } = arc else { panic!() };
        assert!(center.distance(DVec3::new(2.0, 2.0, 0.0)) < EPS);
        assert!((radius - 2.0).abs() < EPS);
        assert!((end - start - std::f64::consts::FRAC_PI_2).abs() < 1e-9);

        // parallel lines: no fillet
        assert!(fillet_lines(
            (DVec3::ZERO, DVec3::new(5.0, 0.0, 0.0)),
            (DVec3::new(0.0, 1.0, 0.0), DVec3::new(5.0, 1.0, 0.0)),
            1.0,
        )
        .is_none());
        // radius larger than the lines: no fillet
        assert!(fillet_lines(
            (DVec3::ZERO, DVec3::new(1.0, 0.0, 0.0)),
            (DVec3::ZERO, DVec3::new(0.0, 1.0, 0.0)),
            5.0,
        )
        .is_none());
    }

    #[test]
    fn fillet_arc_endpoints_touch_trimmed_lines() {
        // acute angle: arc endpoints coincide with the trimmed line starts
        let (la, arc, lb) = fillet_lines(
            (DVec3::ZERO, DVec3::new(10.0, 0.0, 0.0)),
            (DVec3::ZERO, DVec3::new(10.0, 5.0, 0.0)),
            1.0,
        )
        .unwrap();
        let Curve::Line { a: ta, .. } = la else { panic!() };
        let Curve::Line { a: tb, .. } = lb else { panic!() };
        let Curve::Arc { center, radius, start, end } = arc else { panic!() };
        let sp = center + DVec3::new(radius * start.cos(), radius * start.sin(), 0.0);
        let ep = center + DVec3::new(radius * end.cos(), radius * end.sin(), 0.0);
        let hits = |p: DVec3| p.distance(ta) < 1e-9 || p.distance(tb) < 1e-9;
        assert!(hits(sp) && hits(ep));
        assert!(end > start && end - start < std::f64::consts::PI);
    }
}
