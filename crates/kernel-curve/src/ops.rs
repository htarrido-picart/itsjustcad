// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

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
    if !(1e-6..=std::f64::consts::PI - 1e-6).contains(&theta) {
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

/// A segment chosen for filleting on a source curve: its two endpoints plus,
/// for polylines, the index of the corner vertex to pull back to the tangency.
struct FilletSeg {
    /// The corner endpoint (near the other curve) — this one moves.
    corner: DVec3,
    /// The far endpoint — kept in place.
    far: DVec3,
    /// `Some(index)` when the source is a polyline: the corner vertex index.
    /// `None` for a plain line (rebuilt as a line).
    corner_idx: Option<usize>,
}

/// Pick the segment of `c` to fillet, given a reference point `toward` that
/// lies on the other curve. Lines use the whole line (the nearer endpoint is
/// the corner). Open polylines may only fillet their two end segments; closed
/// polylines consider every segment. Returns `None` for curve kinds we cannot
/// fillet on (Arc/Ellipse/NURBS) or degenerate inputs.
fn pick_fillet_segment(c: &Curve, toward: DVec3) -> Option<FilletSeg> {
    match c {
        Curve::Line { a, b } => {
            let (corner, far) = if a.distance_squared(toward) <= b.distance_squared(toward) {
                (*a, *b)
            } else {
                (*b, *a)
            };
            Some(FilletSeg { corner, far, corner_idx: None })
        }
        Curve::Polyline { points, closed } => {
            if points.len() < 2 {
                return None;
            }
            let n = points.len();
            // Candidate segments as (corner_idx, far_idx).
            let candidates: Vec<(usize, usize)> = if *closed && n >= 3 {
                (0..n)
                    .flat_map(|i| [(i, (i + 1) % n), ((i + 1) % n, i)])
                    .collect()
            } else {
                vec![(0, 1), (n - 1, n - 2)]
            };
            // Choose the candidate whose corner vertex is nearest `toward`.
            let (corner_idx, far_idx) = candidates.into_iter().min_by(|&(ci, _), &(cj, _)| {
                points[ci]
                    .distance_squared(toward)
                    .total_cmp(&points[cj].distance_squared(toward))
            })?;
            Some(FilletSeg {
                corner: points[corner_idx],
                far: points[far_idx],
                corner_idx: Some(corner_idx),
            })
        }
        _ => None,
    }
}

/// Rebuild `src` after its chosen segment was trimmed to tangency point `tan`.
/// A line becomes `Line { a: tan, b: far }`; a polyline keeps all its vertices
/// but its corner vertex is moved to `tan`.
fn apply_trim(src: &Curve, seg: &FilletSeg, tan: DVec3) -> Curve {
    match (src, seg.corner_idx) {
        (Curve::Polyline { points, closed }, Some(idx)) => {
            let mut points = points.clone();
            points[idx] = tan;
            Curve::Polyline { points, closed: *closed }
        }
        // Lines (and, defensively, anything without a polyline index) rebuild
        // as a trimmed line — matching the historical `fillet_lines` behavior.
        _ => Curve::Line { a: tan, b: seg.far },
    }
}

/// A representative interior point of a curve, used only to seed the
/// nearest-segment search in `fillet_curves`.
fn fillet_seed(c: &Curve) -> DVec3 {
    match c {
        Curve::Line { a, b } => (*a + *b) * 0.5,
        Curve::Polyline { points, .. } if !points.is_empty() => {
            points.iter().copied().sum::<DVec3>() / points.len() as f64
        }
        _ => closest_point(c, DVec3::ZERO, 0.01),
    }
}

/// Fillet two curves (lines and/or polylines) with a tangent arc of `radius`,
/// trimming the two segments that meet — or come closest to meeting — at a
/// shared corner. A line is trimmed to the tangency point; a polyline keeps its
/// shape but its corner vertex is pulled back to the tangency point. Returns
/// `(trimmed a, arc, trimmed b)`, or `None` when a source is neither a line nor
/// a polyline, the chosen segments are parallel, or the radius does not fit.
pub fn fillet_curves(a: &Curve, b: &Curve, radius: f64) -> Option<(Curve, Curve, Curve)> {
    // Reference each curve toward a point on the other so we pick the end
    // segments that approach a shared corner.
    let toward_a = closest_point(b, fillet_seed(a), 0.01);
    let toward_b = closest_point(a, fillet_seed(b), 0.01);
    let seg_a = pick_fillet_segment(a, toward_a)?;
    let seg_b = pick_fillet_segment(b, toward_b)?;
    let (la, arc, lb) = fillet_lines(
        (seg_a.corner, seg_a.far),
        (seg_b.corner, seg_b.far),
        radius,
    )?;
    let Curve::Line { a: tan_a, .. } = la else { return None };
    let Curve::Line { a: tan_b, .. } = lb else { return None };
    Some((apply_trim(a, &seg_a, tan_a), arc, apply_trim(b, &seg_b, tan_b)))
}

// ----------------------------------------------------------------- chamfer

/// Chamfer (bevel) two line segments in the XY plane: cut off the corner with a
/// straight line. From the corner where the two segments meet (or the point
/// where their supporting lines cross), each source is set back by `dist` along
/// its own direction; the bevel is a straight [`Curve::Line`] between the two
/// setback points. Each source line keeps the endpoint farther from the corner
/// and is rebuilt to its setback point. Returns `(trimmed a, bevel, trimmed b)`,
/// or `None` when the lines are parallel or `dist` does not fit on either line.
pub fn chamfer_lines(
    a: (DVec3, DVec3),
    b: (DVec3, DVec3),
    dist: f64,
) -> Option<(Curve, Curve, Curve)> {
    let d1 = (a.1 - a.0).truncate();
    let d2 = (b.1 - b.0).truncate();
    let denom = d1.perp_dot(d2);
    if denom.abs() < 1e-12 || dist <= 0.0 {
        return None;
    }
    let w = (b.0 - a.0).truncate();
    let t1 = w.perp_dot(d2) / denom;
    let z = a.0.z;
    // The corner: where the two supporting lines cross.
    let p = (a.0.truncate() + d1 * t1).extend(z);
    // Keep the endpoint of each line farther from the corner.
    let keep = |l: (DVec3, DVec3)| {
        if l.0.distance_squared(p) >= l.1.distance_squared(p) { l.0 } else { l.1 }
    };
    let (e1, e2) = (keep(a), keep(b));
    let u = (e1 - p).truncate().normalize_or_zero();
    let v = (e2 - p).truncate().normalize_or_zero();
    if u == DVec2::ZERO || v == DVec2::ZERO {
        return None;
    }
    // Colinear (no real corner) → nothing to bevel.
    let cos_theta = u.dot(v).clamp(-1.0, 1.0);
    if cos_theta.acos() < 1e-6 {
        return None;
    }
    // Set back `dist` along each segment toward its kept end.
    if dist > (e1 - p).truncate().length() + EPS || dist > (e2 - p).truncate().length() + EPS {
        return None; // distance too large for the available line length
    }
    let s1 = p + (u * dist).extend(0.0);
    let s2 = p + (v * dist).extend(0.0);
    Some((
        Curve::Line { a: s1, b: e1 },
        Curve::Line { a: s1, b: s2 },
        Curve::Line { a: s2, b: e2 },
    ))
}

/// Chamfer two curves (lines and/or polylines): bevel the two segments that meet
/// — or come closest to meeting — at a shared corner with a straight setback
/// line of `dist`. A line is trimmed to its setback point; a polyline keeps its
/// shape but its corner vertex is pulled back to the setback point. Returns
/// `(trimmed a, bevel line, trimmed b)`, or `None` when a source is neither a
/// line nor a polyline, the chosen segments are parallel, or `dist` does not fit.
pub fn chamfer_curves(a: &Curve, b: &Curve, dist: f64) -> Option<(Curve, Curve, Curve)> {
    // Same segment-selection strategy as `fillet_curves`.
    let toward_a = closest_point(b, fillet_seed(a), 0.01);
    let toward_b = closest_point(a, fillet_seed(b), 0.01);
    let seg_a = pick_fillet_segment(a, toward_a)?;
    let seg_b = pick_fillet_segment(b, toward_b)?;
    let (la, bevel, lb) = chamfer_lines(
        (seg_a.corner, seg_a.far),
        (seg_b.corner, seg_b.far),
        dist,
    )?;
    let Curve::Line { a: set_a, .. } = la else { return None };
    let Curve::Line { a: set_b, .. } = lb else { return None };
    Some((apply_trim(a, &seg_a, set_a), bevel, apply_trim(b, &seg_b, set_b)))
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

    fn polyline(pts: &[(f64, f64)], closed: bool) -> Curve {
        Curve::Polyline {
            points: pts.iter().map(|&(x, y)| DVec3::new(x, y, 0.0)).collect(),
            closed,
        }
    }

    #[test]
    fn fillet_curves_two_lines_matches_fillet_lines() {
        // Regression: line × line goes through the same math, same result.
        let a = line(-2.0, 0.0, 8.0, 0.0);
        let b = line(0.0, -2.0, 0.0, 8.0);
        let (ta, arc, tb) = fillet_curves(&a, &b, 2.0).unwrap();
        let Curve::Line { a: aa, b: ab } = ta else { panic!("a stays a line") };
        assert!(aa.distance(DVec3::new(2.0, 0.0, 0.0)) < EPS); // tangency
        assert!(ab.distance(DVec3::new(8.0, 0.0, 0.0)) < EPS); // far end kept
        let Curve::Line { a: ba, b: bb } = tb else { panic!("b stays a line") };
        assert!(ba.distance(DVec3::new(0.0, 2.0, 0.0)) < EPS);
        assert!(bb.distance(DVec3::new(0.0, 8.0, 0.0)) < EPS);
        let Curve::Arc { center, radius, .. } = arc else { panic!() };
        assert!(center.distance(DVec3::new(2.0, 2.0, 0.0)) < EPS);
        assert!((radius - 2.0).abs() < EPS);
    }

    #[test]
    fn fillet_curves_line_and_polyline() {
        // Vertical line up the y-axis; open polyline whose first vertex sits at
        // the origin corner and runs off along +x. They meet at (0,0).
        let a = line(0.0, -2.0, 0.0, 8.0);
        let b = polyline(&[(0.0, 0.0), (8.0, 0.0), (8.0, 4.0)], false);
        let (ta, arc, tb) = fillet_curves(&a, &b, 2.0).unwrap();

        // Line trimmed to tangency (0,2), far end (0,8) kept.
        let Curve::Line { a: la, b: lb } = ta else { panic!("line stays a line") };
        assert!(la.distance(DVec3::new(0.0, 2.0, 0.0)) < EPS);
        assert!(lb.distance(DVec3::new(0.0, 8.0, 0.0)) < EPS);

        // Polyline stays a polyline; corner vertex 0 pulled to (2,0); the other
        // two vertices untouched.
        let Curve::Polyline { points, closed } = tb else { panic!("polyline stays a polyline") };
        assert!(!closed);
        assert_eq!(points.len(), 3);
        assert!(points[0].distance(DVec3::new(2.0, 0.0, 0.0)) < EPS);
        assert!(points[1].distance(DVec3::new(8.0, 0.0, 0.0)) < EPS);
        assert!(points[2].distance(DVec3::new(8.0, 4.0, 0.0)) < EPS);

        // Arc tangent at both trim points.
        let Curve::Arc { center, radius, start, end } = arc else { panic!() };
        assert!(center.distance(DVec3::new(2.0, 2.0, 0.0)) < EPS);
        assert!((radius - 2.0).abs() < EPS);
        let sp = arc_point(center, radius, start);
        let ep = arc_point(center, radius, end);
        let hits = |p: DVec3| p.distance(DVec3::new(0.0, 2.0, 0.0)) < EPS
            || p.distance(DVec3::new(2.0, 0.0, 0.0)) < EPS;
        assert!(hits(sp) && hits(ep));
    }

    #[test]
    fn fillet_curves_two_polylines_share_corner() {
        // Two open polylines approaching the origin corner: one along -x→origin,
        // one along origin→+y (their end segments meet at (0,0)).
        let a = polyline(&[(-8.0, 0.0), (0.0, 0.0)], false); // last vertex = corner
        let b = polyline(&[(0.0, 0.0), (0.0, 8.0)], false); // first vertex = corner
        let (ta, arc, tb) = fillet_curves(&a, &b, 2.0).unwrap();

        // a: last vertex (index 1) moves to (-2,0); first vertex untouched.
        let Curve::Polyline { points: pa, .. } = ta else { panic!() };
        assert!(pa[0].distance(DVec3::new(-8.0, 0.0, 0.0)) < EPS);
        assert!(pa[1].distance(DVec3::new(-2.0, 0.0, 0.0)) < EPS);

        // b: first vertex (index 0) moves to (0,2); last untouched.
        let Curve::Polyline { points: pb, .. } = tb else { panic!() };
        assert!(pb[0].distance(DVec3::new(0.0, 2.0, 0.0)) < EPS);
        assert!(pb[1].distance(DVec3::new(0.0, 8.0, 0.0)) < EPS);

        let Curve::Arc { center, radius, .. } = arc else { panic!() };
        assert!(center.distance(DVec3::new(-2.0, 2.0, 0.0)) < EPS);
        assert!((radius - 2.0).abs() < EPS);
    }

    #[test]
    fn fillet_curves_rejects_parallel_and_bad_kinds() {
        // Parallel lines: no fillet.
        assert!(fillet_curves(
            &line(0.0, 0.0, 5.0, 0.0),
            &line(0.0, 1.0, 5.0, 1.0),
            1.0,
        )
        .is_none());
        // Arc as a source: unsupported kind → None.
        assert!(fillet_curves(&line(0.0, 0.0, 5.0, 0.0), &circle(0.0, 0.0, 2.0), 1.0).is_none());
    }

    #[test]
    fn fillet_curves_closed_polyline_picks_nearest_segment() {
        // Closed square with a top-right corner at (2,2); a horizontal line
        // running left into that corner along y=2. They form a right angle.
        let sq = polyline(&[(-2.0, -2.0), (2.0, -2.0), (2.0, 2.0), (-2.0, 2.0)], true);
        let l = line(8.0, 2.0, 2.0, 2.0); // corner endpoint (2,2), far (8,2)
        let (tsq, arc, _tl) = fillet_curves(&sq, &l, 1.0).unwrap();
        let Curve::Polyline { points, closed } = tsq else { panic!() };
        assert!(closed);
        assert_eq!(points.len(), 4);
        // The (2,2) vertex (index 2) is nearest; it should have moved, rest fixed.
        assert!(points[0].distance(DVec3::new(-2.0, -2.0, 0.0)) < EPS);
        assert!(points[1].distance(DVec3::new(2.0, -2.0, 0.0)) < EPS);
        assert!(points[2].distance(DVec3::new(2.0, 2.0, 0.0)) > EPS); // moved back
        assert!(points[3].distance(DVec3::new(-2.0, 2.0, 0.0)) < EPS);
        assert!(matches!(arc, Curve::Arc { .. }));
    }

    #[test]
    fn chamfer_perpendicular_lines() {
        // Two lines crossing at the origin corner; keep the far +x / +y ends.
        let (la, bevel, lb) = chamfer_lines(
            (DVec3::new(-2.0, 0.0, 0.0), DVec3::new(8.0, 0.0, 0.0)),
            (DVec3::new(0.0, -2.0, 0.0), DVec3::new(0.0, 8.0, 0.0)),
            2.0,
        )
        .unwrap();
        // a trimmed to setback (2,0), far end (8,0) kept.
        let Curve::Line { a, b } = la else { panic!() };
        assert!(a.distance(DVec3::new(2.0, 0.0, 0.0)) < EPS);
        assert!(b.distance(DVec3::new(8.0, 0.0, 0.0)) < EPS);
        // b trimmed to setback (0,2), far end (0,8) kept.
        let Curve::Line { a, b } = lb else { panic!() };
        assert!(a.distance(DVec3::new(0.0, 2.0, 0.0)) < EPS);
        assert!(b.distance(DVec3::new(0.0, 8.0, 0.0)) < EPS);
        // bevel: straight line between the two setback points.
        let Curve::Line { a, b } = bevel else { panic!() };
        assert!(a.distance(DVec3::new(2.0, 0.0, 0.0)) < EPS);
        assert!(b.distance(DVec3::new(0.0, 2.0, 0.0)) < EPS);

        // parallel lines: no chamfer
        assert!(chamfer_lines(
            (DVec3::ZERO, DVec3::new(5.0, 0.0, 0.0)),
            (DVec3::new(0.0, 1.0, 0.0), DVec3::new(5.0, 1.0, 0.0)),
            1.0,
        )
        .is_none());
        // distance larger than the lines: no chamfer
        assert!(chamfer_lines(
            (DVec3::ZERO, DVec3::new(1.0, 0.0, 0.0)),
            (DVec3::ZERO, DVec3::new(0.0, 1.0, 0.0)),
            5.0,
        )
        .is_none());
    }

    #[test]
    fn chamfer_curves_line_and_polyline() {
        // Vertical line up the y-axis; open polyline off the origin along +x.
        let a = line(0.0, -2.0, 0.0, 8.0);
        let b = polyline(&[(0.0, 0.0), (8.0, 0.0), (8.0, 4.0)], false);
        let (ta, bevel, tb) = chamfer_curves(&a, &b, 2.0).unwrap();

        // Line trimmed to setback (0,2), far end (0,8) kept.
        let Curve::Line { a: la, b: lb } = ta else { panic!("line stays a line") };
        assert!(la.distance(DVec3::new(0.0, 2.0, 0.0)) < EPS);
        assert!(lb.distance(DVec3::new(0.0, 8.0, 0.0)) < EPS);

        // Polyline stays a polyline; corner vertex 0 pulled to (2,0); rest fixed.
        let Curve::Polyline { points, closed } = tb else { panic!("polyline stays a polyline") };
        assert!(!closed);
        assert_eq!(points.len(), 3);
        assert!(points[0].distance(DVec3::new(2.0, 0.0, 0.0)) < EPS);
        assert!(points[1].distance(DVec3::new(8.0, 0.0, 0.0)) < EPS);
        assert!(points[2].distance(DVec3::new(8.0, 4.0, 0.0)) < EPS);

        // Bevel joins the two setback points.
        let Curve::Line { a: bva, b: bvb } = bevel else { panic!() };
        let hits = |p: DVec3| {
            p.distance(DVec3::new(0.0, 2.0, 0.0)) < EPS || p.distance(DVec3::new(2.0, 0.0, 0.0)) < EPS
        };
        assert!(hits(bva) && hits(bvb));
    }

    #[test]
    fn chamfer_curves_two_polylines_share_corner() {
        // Two open polylines approaching the origin corner.
        let a = polyline(&[(-8.0, 0.0), (0.0, 0.0)], false); // last vertex = corner
        let b = polyline(&[(0.0, 0.0), (0.0, 8.0)], false); // first vertex = corner
        let (ta, bevel, tb) = chamfer_curves(&a, &b, 2.0).unwrap();

        // a: last vertex moves to (-2,0); first vertex untouched.
        let Curve::Polyline { points: pa, .. } = ta else { panic!() };
        assert!(pa[0].distance(DVec3::new(-8.0, 0.0, 0.0)) < EPS);
        assert!(pa[1].distance(DVec3::new(-2.0, 0.0, 0.0)) < EPS);

        // b: first vertex moves to (0,2); last untouched.
        let Curve::Polyline { points: pb, .. } = tb else { panic!() };
        assert!(pb[0].distance(DVec3::new(0.0, 2.0, 0.0)) < EPS);
        assert!(pb[1].distance(DVec3::new(0.0, 8.0, 0.0)) < EPS);

        let Curve::Line { a: bva, b: bvb } = bevel else { panic!() };
        let hits = |p: DVec3| {
            p.distance(DVec3::new(-2.0, 0.0, 0.0)) < EPS
                || p.distance(DVec3::new(0.0, 2.0, 0.0)) < EPS
        };
        assert!(hits(bva) && hits(bvb));
    }

    #[test]
    fn chamfer_curves_rejects_parallel_and_bad_kinds() {
        assert!(chamfer_curves(
            &line(0.0, 0.0, 5.0, 0.0),
            &line(0.0, 1.0, 5.0, 1.0),
            1.0,
        )
        .is_none());
        assert!(chamfer_curves(&line(0.0, 0.0, 5.0, 0.0), &circle(0.0, 0.0, 2.0), 1.0).is_none());
    }
}
