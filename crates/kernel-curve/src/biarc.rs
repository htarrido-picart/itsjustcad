// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Planar line/arc segments and biarc fitting.
//!
//! Fabrication formats (laser, waterjet, router) want lines and true arcs, not
//! splines or dense chord soup. [`Seg2`] is an oriented 2D line or circular arc;
//! [`fit_biarcs`] approximates a smooth parametric curve (ellipse, NURBS) with
//! tangent-continuous arc pairs to a distance tolerance, falling back to short
//! chords only on spans where an arc pair cannot meet the tolerance (cusps,
//! degenerate tangents).

use glam::DVec2;
use std::f64::consts::PI;

/// An oriented planar segment: a line, or a circular arc from angle `a0`
/// sweeping `sweep` radians (positive = counter-clockwise).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Seg2 {
    Line {
        a: DVec2,
        b: DVec2,
    },
    Arc {
        c: DVec2,
        r: f64,
        a0: f64,
        sweep: f64,
    },
}

impl Seg2 {
    pub fn start(&self) -> DVec2 {
        match *self {
            Seg2::Line { a, .. } => a,
            Seg2::Arc { c, r, a0, .. } => c + r * DVec2::new(a0.cos(), a0.sin()),
        }
    }

    pub fn end(&self) -> DVec2 {
        match *self {
            Seg2::Line { b, .. } => b,
            Seg2::Arc { c, r, a0, sweep } => {
                let a = a0 + sweep;
                c + r * DVec2::new(a.cos(), a.sin())
            }
        }
    }

    /// The same segment traversed the other way.
    pub fn reversed(&self) -> Seg2 {
        match *self {
            Seg2::Line { a, b } => Seg2::Line { a: b, b: a },
            Seg2::Arc { c, r, a0, sweep } => Seg2::Arc {
                c,
                r,
                a0: a0 + sweep,
                sweep: -sweep,
            },
        }
    }

    /// DXF polyline bulge: `tan(sweep / 4)`, 0 for a line.
    pub fn bulge(&self) -> f64 {
        match *self {
            Seg2::Line { .. } => 0.0,
            Seg2::Arc { sweep, .. } => (sweep / 4.0).tan(),
        }
    }

    pub fn length(&self) -> f64 {
        match *self {
            Seg2::Line { a, b } => a.distance(b),
            Seg2::Arc { r, sweep, .. } => r * sweep.abs(),
        }
    }

    /// Point at normalized parameter `u` in `[0, 1]`.
    pub fn point_at(&self, u: f64) -> DVec2 {
        match *self {
            Seg2::Line { a, b } => a.lerp(b, u),
            Seg2::Arc { c, r, a0, sweep } => {
                let a = a0 + sweep * u;
                c + r * DVec2::new(a.cos(), a.sin())
            }
        }
    }

    /// Unit tangent at normalized parameter `u`, in travel direction.
    pub fn tangent_at(&self, u: f64) -> DVec2 {
        match *self {
            Seg2::Line { a, b } => (b - a).normalize_or_zero(),
            Seg2::Arc { a0, sweep, .. } => {
                let a = a0 + sweep * u;
                DVec2::new(-a.sin(), a.cos()) * sweep.signum()
            }
        }
    }

    /// Shortest distance from `p` to the segment.
    pub fn distance(&self, p: DVec2) -> f64 {
        match *self {
            Seg2::Line { a, b } => {
                let ab = b - a;
                let len2 = ab.length_squared();
                let t = if len2 > 0.0 {
                    ((p - a).dot(ab) / len2).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                p.distance(a + ab * t)
            }
            Seg2::Arc { c, r, .. } => {
                if self.contains_angle((p - c).to_angle()) {
                    (p.distance(c) - r).abs()
                } else {
                    p.distance(self.start()).min(p.distance(self.end()))
                }
            }
        }
    }

    /// Is angle `ang` (radians) inside this arc's sweep? Lines: always false.
    pub fn contains_angle(&self, ang: f64) -> bool {
        match *self {
            Seg2::Line { .. } => false,
            Seg2::Arc { a0, sweep, .. } => {
                let rel = if sweep >= 0.0 { ang - a0 } else { a0 - ang };
                rel.rem_euclid(2.0 * PI) <= sweep.abs() + 1e-12
            }
        }
    }

    /// Axis-aligned bounds `(min, max)`, exact for arcs (quadrant extremes).
    pub fn bbox(&self) -> (DVec2, DVec2) {
        let (s, e) = (self.start(), self.end());
        let (mut lo, mut hi) = (s.min(e), s.max(e));
        if let Seg2::Arc { c, r, .. } = *self {
            for k in 0..4 {
                let ang = k as f64 * PI / 2.0;
                if self.contains_angle(ang) {
                    let p = c + r * DVec2::new(ang.cos(), ang.sin());
                    lo = lo.min(p);
                    hi = hi.max(p);
                }
            }
        }
        (lo, hi)
    }

    /// Chord points (start and end included) within `tol` of the segment.
    pub fn flatten(&self, tol: f64) -> Vec<DVec2> {
        match *self {
            Seg2::Line { a, b } => vec![a, b],
            Seg2::Arc { r, sweep, .. } => {
                let tol = tol.max(r * 1e-9);
                let max_step = if tol >= r {
                    PI / 2.0
                } else {
                    2.0 * (1.0 - tol / r).acos()
                };
                let n = ((sweep.abs() / max_step.max(1e-6)).ceil() as usize).clamp(1, 4096);
                (0..=n)
                    .map(|i| self.point_at(i as f64 / n as f64))
                    .collect()
            }
        }
    }

    /// Arc from `a` leaving along unit tangent `t` and ending at `b`; a line
    /// when the chord is (nearly) along `t`.
    pub fn arc_from_tangent(a: DVec2, t: DVec2, b: DVec2) -> Seg2 {
        let chord = b - a;
        let len = chord.length();
        let phi = t.perp_dot(chord).atan2(t.dot(chord));
        if len == 0.0 || phi.abs() < 1e-9 {
            return Seg2::Line { a, b };
        }
        let r = len / (2.0 * phi.sin().abs());
        let n = if phi > 0.0 { t.perp() } else { -t.perp() };
        let c = a + n * r;
        Seg2::Arc {
            c,
            r,
            a0: (a - c).to_angle(),
            sweep: 2.0 * phi,
        }
    }
}

/// Biarc between `p0` (unit tangent `t0`) and `p1` (unit tangent `t1`), using
/// the equal-distance construction. `None` when the construction degenerates.
fn biarc(p0: DVec2, t0: DVec2, p1: DVec2, t1: DVec2) -> Option<[Seg2; 2]> {
    let v = p1 - p0;
    let vv = v.length_squared();
    if vv == 0.0 {
        return None;
    }
    let t = t0 + t1;
    let denom = 2.0 * (1.0 - t0.dot(t1));
    let vt = v.dot(t);
    let d = if denom.abs() < 1e-12 {
        let vt1 = v.dot(t1);
        if vt1.abs() < 1e-12 {
            return None;
        }
        vv / (4.0 * vt1)
    } else {
        (-vt + (vt * vt + denom * vv).sqrt()) / denom
    };
    if !d.is_finite() || d <= 0.0 {
        return None;
    }
    let q0 = p0 + t0 * d;
    let q1 = p1 - t1 * d;
    let j = (q0 + q1) * 0.5;
    let s0 = Seg2::arc_from_tangent(p0, t0, j);
    let s1 = Seg2::arc_from_tangent(p1, -t1, j).reversed();
    // Reject looping solutions: each half of a well-behaved biarc turns less
    // than a half circle.
    for s in [s0, s1] {
        if let Seg2::Arc { sweep, .. } = s
            && sweep.abs() > PI
        {
            return None;
        }
    }
    Some([s0, s1])
}

/// Result of [`fit_biarcs`]: the fitted segments plus how many parameter spans
/// fell back to chords because no arc pair met the tolerance.
pub struct BiarcFit {
    pub segs: Vec<Seg2>,
    pub fallback_spans: usize,
}

/// Fit tangent-continuous arc pairs to the curve `f` over `[t0, t1]` so every
/// sampled curve point is within `tol` of the fit. `df` is the derivative
/// (any non-zero length). Spans are bisected up to a fixed depth; a span that
/// still misses the tolerance becomes chords at `tol`.
pub fn fit_biarcs(
    f: &dyn Fn(f64) -> DVec2,
    df: &dyn Fn(f64) -> DVec2,
    t0: f64,
    t1: f64,
    tol: f64,
) -> BiarcFit {
    const MAX_DEPTH: u32 = 14;
    const SAMPLES: usize = 16;
    let mut out = BiarcFit {
        segs: Vec::new(),
        fallback_spans: 0,
    };
    // Explicit stack, processed left-to-right so the output stays in order.
    let mut stack: Vec<(f64, f64, u32)> = vec![(t0, t1, 0)];
    while let Some((a, b, depth)) = stack.pop() {
        let (pa, pb) = (f(a), f(b));
        let (ta, tb) = (df(a).normalize_or_zero(), df(b).normalize_or_zero());
        let fit = (ta != DVec2::ZERO && tb != DVec2::ZERO)
            .then(|| biarc(pa, ta, pb, tb))
            .flatten()
            .filter(|pair| {
                (1..SAMPLES).all(|k| {
                    let p = f(a + (b - a) * k as f64 / SAMPLES as f64);
                    pair[0].distance(p).min(pair[1].distance(p)) <= tol
                })
            });
        match fit {
            Some(pair) => {
                for s in pair {
                    if s.length() > 0.0 {
                        out.segs.push(s);
                    }
                }
            }
            None if depth < MAX_DEPTH => {
                let m = 0.5 * (a + b);
                stack.push((m, b, depth + 1));
                stack.push((a, m, depth + 1));
            }
            None => {
                out.fallback_spans += 1;
                // Chord fallback: subdivide until every chord's midpoint
                // deviation is within tol (bounded).
                let mut n = 1usize;
                while n < 256 {
                    let ok = (0..n).all(|i| {
                        let (u, w) = (
                            a + (b - a) * i as f64 / n as f64,
                            a + (b - a) * (i + 1) as f64 / n as f64,
                        );
                        let chord = Seg2::Line { a: f(u), b: f(w) };
                        chord.distance(f(0.5 * (u + w))) <= tol
                    });
                    if ok {
                        break;
                    }
                    n *= 2;
                }
                for i in 0..n {
                    let (u, w) = (
                        a + (b - a) * i as f64 / n as f64,
                        a + (b - a) * (i + 1) as f64 / n as f64,
                    );
                    let (p, q) = (f(u), f(w));
                    if p != q {
                        out.segs.push(Seg2::Line { a: p, b: q });
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arc_endpoints_bulge_and_reverse() {
        let s = Seg2::Arc {
            c: DVec2::ZERO,
            r: 2.0,
            a0: 0.0,
            sweep: PI / 2.0,
        };
        assert!(s.start().distance(DVec2::new(2.0, 0.0)) < 1e-12);
        assert!(s.end().distance(DVec2::new(0.0, 2.0)) < 1e-12);
        assert!((s.bulge() - (PI / 8.0).tan()).abs() < 1e-12);
        let r = s.reversed();
        assert!(r.start().distance(s.end()) < 1e-12);
        assert!(r.bulge() < 0.0);
        let (lo, hi) = s.bbox();
        assert!(
            lo.distance(DVec2::new(0.0, 0.0)) < 1e-12 && hi.distance(DVec2::new(2.0, 2.0)) < 1e-12
        );
        assert!(
            (s.distance(DVec2::new(3.0, 3.0) * std::f64::consts::FRAC_1_SQRT_2) - 1.0).abs() < 1e-9
        );
    }

    #[test]
    fn arc_from_tangent_matches_quarter_circle() {
        let s = Seg2::arc_from_tangent(DVec2::new(1.0, 0.0), DVec2::Y, DVec2::new(0.0, 1.0));
        let Seg2::Arc { c, r, sweep, .. } = s else {
            panic!("expected arc")
        };
        assert!(c.length() < 1e-12 && (r - 1.0).abs() < 1e-12 && (sweep - PI / 2.0).abs() < 1e-12);
    }

    #[test]
    fn ellipse_fits_within_tolerance_with_arcs_only() {
        let (rx, ry, tol) = (30.0, 12.0, 0.01);
        let f = |t: f64| DVec2::new(rx * t.cos(), ry * t.sin());
        let df = |t: f64| DVec2::new(-rx * t.sin(), ry * t.cos());
        let fit = fit_biarcs(&f, &df, 0.0, 2.0 * PI, tol);
        assert_eq!(fit.fallback_spans, 0);
        assert!(fit.segs.iter().all(|s| matches!(s, Seg2::Arc { .. })));
        assert!(fit.segs.len() < 200, "{} segs", fit.segs.len());
        // Continuity and accuracy on dense samples.
        for w in fit.segs.windows(2) {
            assert!(w[0].end().distance(w[1].start()) < 1e-9);
        }
        for k in 0..2000 {
            let p = f(2.0 * PI * k as f64 / 2000.0);
            let d = fit
                .segs
                .iter()
                .map(|s| s.distance(p))
                .fold(f64::INFINITY, f64::min);
            assert!(d <= tol * 1.05, "deviation {d}");
        }
    }

    #[test]
    fn cusp_falls_back_to_chords() {
        // Astroid-like cusp at t = 0.5: zero derivative there.
        let f = |t: f64| {
            let u = t - 0.5;
            DVec2::new(u * u * u, u * u)
        };
        let df = |t: f64| {
            let u = t - 0.5;
            DVec2::new(3.0 * u * u, 2.0 * u)
        };
        let fit = fit_biarcs(&f, &df, 0.0, 1.0, 1e-4);
        assert!(!fit.segs.is_empty());
        assert!(fit.segs.first().unwrap().start().distance(f(0.0)) < 1e-12);
        assert!(fit.segs.last().unwrap().end().distance(f(1.0)) < 1e-12);
    }
}
