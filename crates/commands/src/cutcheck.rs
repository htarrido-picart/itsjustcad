// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! `cutcheck` — fabrication preflight for the cut DXF profile.
//!
//! Runs over the exact contour set the cut export writes ([`crate::cut`]) and
//! flags what a laser/waterjet/router shop (SendCutSend) would reject or
//! misread: open contours, duplicate/overlapping paths, crossings, holes and
//! bridges below the minimum, holes too close to an edge, part size outside
//! the material's limits, uncut text/annotations, and islands that fall out.
//!
//! The checks are plain geometry, not the `checkengine` rule DSL, but the
//! result is a [`ComplianceReport`] plus severity markers — the same shape
//! `codecheck` produces — so findings land in the viewport and in `report`.
//! Limits come from SendCutSend's specs feed for `material=<sku>` (see
//! [`crate::scs`]) or the 50 %-of-thickness rule of thumb; the report always
//! names the source.

use std::collections::HashMap;
use std::f64::consts::PI;
use std::path::Path;

use glam::DVec2;
use itsjustcad_doc::{ComplianceReport, ObjectId, RuleOutcome};
use kernel_curve::Seg2;
use serde::{Deserialize, Serialize};

use crate::checkengine::{Marker, Severity};
use crate::cut::{Contour, CutGeometry, CutUnit, SkipKind};
use crate::cut_preset::CutPreset;
use crate::scs;

/// Report key in `Document::compliance_reports` (and `report cutcheck`).
pub const PACK: &str = "cutcheck";

/// Resolved fabrication limits, embedded in the logged op so replay never
/// reads the SendCutSend cache. Lengths in meters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CutLimits {
    pub thickness: f64,
    pub min_hole: f64,
    pub min_bridge: f64,
    pub min_hole_to_edge: f64,
    /// `(long, short)` side.
    pub min_part: [f64; 2],
    pub max_part: [f64; 2],
    /// Where the numbers came from (SKU + data date, or the rule of thumb).
    pub source: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

const IN: f64 = itsjustcad_doc::METERS_PER_INCH;
const RULE_OF_THUMB: &str = "rule of thumb (holes, bridges and hole-to-edge ≥ 50% of thickness; part 0.5\"x0.5\" to 44\"x24\")";

impl CutLimits {
    /// The fallback: features ≥ 50 % of thickness, parts 0.5"×0.5" to 44"×24".
    pub fn rule_of_thumb(thickness: f64) -> CutLimits {
        CutLimits {
            thickness,
            min_hole: 0.5 * thickness,
            min_bridge: 0.5 * thickness,
            min_hole_to_edge: 0.5 * thickness,
            min_part: [0.5 * IN, 0.5 * IN],
            max_part: [44.0 * IN, 24.0 * IN],
            source: RULE_OF_THUMB.to_string(),
            notes: Vec::new(),
        }
    }
}

/// Resolve limits for `cutcheck`. `thickness` is in file units; `dir` is the
/// SendCutSend cache (consulted only with `material`, never by the generic
/// preset). Never passes silently: every fallback is recorded in `notes`.
pub fn resolve_limits(
    preset: CutPreset,
    units: CutUnit,
    thickness: Option<f64>,
    material: Option<&str>,
    dir: Option<&Path>,
) -> Result<CutLimits, String> {
    let t_m = thickness.map(|t| t / units.per_meter());
    let need_t = |why: &str| -> Result<f64, String> {
        t_m.ok_or_else(|| format!("cutcheck needs thickness=<t> ({}){why}", units.label()))
    };
    if preset == CutPreset::Generic {
        if material.is_some_and(|m| !m.trim().is_empty()) {
            return Err("material= is SendCutSend data — preset=generic takes thickness= only".into());
        }
        return Ok(CutLimits::rule_of_thumb(need_t("")?));
    }
    let Some(query) = material.map(str::trim).filter(|q| !q.is_empty()) else {
        return Ok(CutLimits::rule_of_thumb(need_t(" or material=<sku>")?));
    };
    let data = match dir
        .ok_or_else(|| "no config directory".to_string())
        .and_then(scs::load)
    {
        Ok(d) => d,
        Err(e) => {
            let mut l = CutLimits::rule_of_thumb(need_t(&format!(" — {e}"))?);
            l.source = format!("{RULE_OF_THUMB} — SendCutSend data unavailable");
            l.notes.push(e);
            return Ok(l);
        }
    };
    let m = data.resolve(query, t_m.map(|t| t / IN))?;
    let mut notes: Vec<String> = data.warnings.clone();
    let t = match (m.thickness, t_m) {
        (Some(mt), Some(t)) => {
            if ((mt * IN) - t).abs() > 0.05 * t {
                notes.push(format!(
                    "thickness={} {u} differs from {} ({} in); using the material's",
                    fmt(t * units.per_meter()),
                    m.sku,
                    fmt(mt),
                    u = units.label()
                ));
            }
            mt * IN
        }
        (Some(mt), None) => mt * IN,
        (None, Some(t)) => {
            notes.push(format!(
                "{} has no thickness in the data; using thickness=",
                m.sku
            ));
            t
        }
        (None, None) => need_t(&format!(" ({} has no thickness in the data)", m.sku))?,
    };
    let mut l = CutLimits::rule_of_thumb(t);
    let mut fallbacks: Vec<&str> = Vec::new();
    let mut take = |v: Option<f64>, slot: &mut f64, name: &'static str| match v {
        Some(x) => *slot = x * IN,
        None => fallbacks.push(name),
    };
    take(m.min_hole, &mut l.min_hole, "min_hole_size");
    take(m.min_bridge, &mut l.min_bridge, "min_bridge_size");
    take(
        m.min_hole_to_edge,
        &mut l.min_hole_to_edge,
        "min_hole_to_edge",
    );
    let (min_p, max_p) = m.part_limits();
    match min_p {
        Some((a, b)) => l.min_part = [a * IN, b * IN],
        None => fallbacks.push("min_part_size"),
    }
    match max_p {
        Some((a, b)) => l.max_part = [a * IN, b * IN],
        None => fallbacks.push("max_part_size"),
    }
    if !fallbacks.is_empty() {
        notes.push(format!(
            "{} has no usable {} in the SendCutSend data — rule of thumb used for those",
            m.sku,
            fallbacks.join(", ")
        ));
    }
    if m.out_of_stock {
        notes.push(format!("{} is marked out of stock", m.sku));
    }
    l.source = format!(
        "SendCutSend {} ({}{}, {} in{}) — data {} from {}",
        m.sku,
        m.name,
        if m.category.is_empty() || m.name.contains(&m.category) {
            String::new()
        } else {
            format!(" {}", m.category)
        },
        fmt(t / IN),
        m.process
            .as_deref()
            .map(|p| format!(", {p}"))
            .unwrap_or_default(),
        data.date.as_deref().unwrap_or("of unknown date"),
        data.dir.display()
    );
    l.notes = notes;
    Ok(l)
}

/// One violation: primary object, location (file units), measured value.
#[derive(Clone, Debug)]
struct Finding {
    obj: ObjectId,
    at: DVec2,
    measured: Option<f64>,
}

/// The rule table: id, severity, message.
const RULES: &[(&str, Severity, &str)] = &[
    (
        "open-contour",
        Severity::Error,
        "open contour: its ends do not meet, so the part cannot be cut out",
    ),
    (
        "duplicate",
        Severity::Warn,
        "exact duplicate path (the cut export drops it; shops reject duplicate paths)",
    ),
    (
        "overlap",
        Severity::Error,
        "overlapping paths: two segments run along each other",
    ),
    (
        "crossing",
        Severity::Error,
        "self-intersecting or crossing contours",
    ),
    (
        "min-hole",
        Severity::Error,
        "hole or slot narrower than the minimum hole size",
    ),
    (
        "bridge",
        Severity::Error,
        "bridge or web narrower than the minimum bridge size",
    ),
    (
        "hole-to-edge",
        Severity::Error,
        "hole closer to the part edge than the minimum",
    ),
    (
        "part-size",
        Severity::Error,
        "part bounding box outside the material's min/max part size",
    ),
    (
        "text",
        Severity::Warn,
        "text is single-stroke, not outlined — it is left out of the cut file (outline it with `textoutline <sel>`, or remove it)",
    ),
    (
        "annotation",
        Severity::Warn,
        "dimension, hatch, mesh or other non-curve object in the export set — left out of the cut file",
    ),
    (
        "island",
        Severity::Warn,
        "island inside a hole — it falls out as a loose piece",
    ),
];

fn fmt(v: f64) -> String {
    let s = format!("{v:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.to_string() }
}

// ---- small planar geometry helpers (file units) ----

fn seg_seg_dist(a: DVec2, b: DVec2, c: DVec2, d: DVec2) -> (f64, DVec2, DVec2) {
    if let Some(p) = seg_intersection(a, b, c, d) {
        return (0.0, p, p);
    }
    let l1 = Seg2::Line { a, b };
    let l2 = Seg2::Line { a: c, b: d };
    let proj = |s: &Seg2, p: DVec2| {
        let (a, b) = (s.start(), s.end());
        let ab = b - a;
        let t = if ab.length_squared() > 0.0 {
            ((p - a).dot(ab) / ab.length_squared()).clamp(0.0, 1.0)
        } else {
            0.0
        };
        a + ab * t
    };
    [
        (c, proj(&l1, c)),
        (d, proj(&l1, d)),
        (a, proj(&l2, a)),
        (b, proj(&l2, b)),
    ]
    .into_iter()
    .map(|(p, q)| (p.distance(q), p, q))
    .min_by(|x, y| x.0.total_cmp(&y.0))
    .unwrap()
}

/// Intersection point of two segments (touching included); `None` when
/// disjoint or parallel.
fn seg_intersection(a: DVec2, b: DVec2, c: DVec2, d: DVec2) -> Option<DVec2> {
    let (r, s) = (b - a, d - c);
    let den = r.perp_dot(s);
    if den.abs() <= 1e-12 * r.length() * s.length() {
        return None;
    }
    let t = (c - a).perp_dot(s) / den;
    let u = (c - a).perp_dot(r) / den;
    (-1e-12..=1.0 + 1e-12).contains(&t).then_some(())?;
    (-1e-12..=1.0 + 1e-12).contains(&u).then_some(())?;
    Some(a + r * t)
}

fn point_in_poly(p: DVec2, poly: &[DVec2]) -> bool {
    let mut inside = false;
    let n = poly.len();
    for i in 0..n {
        let (a, b) = (poly[i], poly[(i + 1) % n]);
        if (a.y > p.y) != (b.y > p.y) && p.x < a.x + (p.y - a.y) / (b.y - a.y) * (b.x - a.x) {
            inside = !inside;
        }
    }
    inside
}

fn poly_area(poly: &[DVec2]) -> f64 {
    let n = poly.len();
    (0..n)
        .map(|i| poly[i].perp_dot(poly[(i + 1) % n]))
        .sum::<f64>()
        .abs()
        * 0.5
}

/// Closest pair between two closed polygons: `(distance, point on a, point on b)`.
fn poly_dist(a: &[DVec2], b: &[DVec2]) -> (f64, DVec2, DVec2) {
    let mut best = (f64::INFINITY, DVec2::ZERO, DVec2::ZERO);
    for i in 0..a.len() {
        let (p, q) = (a[i], a[(i + 1) % a.len()]);
        for j in 0..b.len() {
            let r = seg_seg_dist(p, q, b[j], b[(j + 1) % b.len()]);
            if r.0 < best.0 {
                best = r;
            }
        }
    }
    best
}

/// Minimum width of the convex hull (rotating-calipers style, O(h²)).
fn min_width(points: &[DVec2]) -> f64 {
    let mut pts: Vec<DVec2> = points.to_vec();
    pts.sort_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
    pts.dedup();
    if pts.len() < 3 {
        return 0.0;
    }
    let mut hull: Vec<DVec2> = Vec::new();
    for pass in 0..2 {
        let start = hull.len();
        let iter: Box<dyn Iterator<Item = &DVec2>> = if pass == 0 {
            Box::new(pts.iter())
        } else {
            Box::new(pts.iter().rev())
        };
        for &p in iter {
            while hull.len() >= start + 2
                && (hull[hull.len() - 1] - hull[hull.len() - 2]).perp_dot(p - hull[hull.len() - 2])
                    <= 0.0
            {
                hull.pop();
            }
            hull.push(p);
        }
        hull.pop();
    }
    let n = hull.len();
    (0..n)
        .map(|i| {
            let (a, b) = (hull[i], hull[(i + 1) % n]);
            let dir = (b - a).normalize_or_zero();
            hull.iter()
                .map(|&p| dir.perp_dot(p - a).abs())
                .fold(0.0, f64::max)
        })
        .fold(f64::INFINITY, f64::min)
}

/// Overlap length of two segments running along each other (collinear lines
/// or concentric equal-radius arcs), 0 when they do not overlap.
fn overlap_len(s: &Seg2, t: &Seg2, tol: f64) -> f64 {
    match (*s, *t) {
        (Seg2::Line { a, b }, Seg2::Line { a: c, b: d }) => {
            let dir = (b - a).normalize_or_zero();
            let off = |p: DVec2| dir.perp_dot(p - a).abs();
            if dir == DVec2::ZERO || off(c) > tol || off(d) > tol {
                return 0.0;
            }
            let (s0, s1) = (0.0_f64, (b - a).length());
            let (t0, t1) = {
                let (x, y) = (dir.dot(c - a), dir.dot(d - a));
                (x.min(y), x.max(y))
            };
            (s1.min(t1) - s0.max(t0)).max(0.0)
        }
        (Seg2::Arc { c, r, .. }, Seg2::Arc { c: c2, r: r2, .. }) => {
            if c.distance(c2) > tol || (r - r2).abs() > tol {
                return 0.0;
            }
            // Sample t's arc and measure the length lying on s.
            let n = 64;
            let on = (0..n)
                .filter(|&k| {
                    let p = t.point_at((k as f64 + 0.5) / n as f64);
                    s.contains_angle((p - c).to_angle())
                })
                .count();
            t.length() * on as f64 / n as f64
        }
        _ => 0.0,
    }
}

/// Evaluate every cut check. Returns the report (unit = file unit, locations
/// in meters) and the viewport markers.
pub fn evaluate(
    g: &CutGeometry,
    limits: &CutLimits,
    context: String,
) -> (ComplianceReport, Vec<Marker>) {
    let u = g.units;
    let s = u.per_meter();
    let tol = u.join_tol();
    let ftol = u.fit_tol() * 0.5;
    let lim = |m: f64| m * s;
    let mut found: HashMap<&'static str, Vec<Finding>> = HashMap::new();
    let mut add = |rule: &'static str, f: Finding| found.entry(rule).or_default().push(f);
    let obj = |src: usize| g.sources[src].0;

    // Flattened contours (closed ones drive nesting).
    let polys: Vec<Vec<DVec2>> = g.contours.iter().map(|c| c.flatten(ftol)).collect();
    let first_obj = |i: usize| obj(g.contours[i].srcs()[0]);

    // 1. Open contours.
    for (i, c) in g.contours.iter().enumerate() {
        if let Contour::Path {
            segs,
            closed: false,
            ..
        } = c
        {
            let (a, b) = (segs[0].start(), segs[segs.len() - 1].end());
            add(
                "open-contour",
                Finding {
                    obj: first_obj(i),
                    at: b,
                    measured: Some(a.distance(b)),
                },
            );
        }
    }
    // 2. Exact duplicates (dropped by the export, still reported).
    for d in &g.duplicates {
        add(
            "duplicate",
            Finding {
                obj: obj(d.src),
                at: d.at,
                measured: None,
            },
        );
    }

    // Every segment with its contour and source; circles as full arcs.
    let mut segs: Vec<(Seg2, usize, usize)> = Vec::new();
    for (i, c) in g.contours.iter().enumerate() {
        match c {
            Contour::Circle { c, r, src } => segs.push((
                Seg2::Arc {
                    c: *c,
                    r: *r,
                    a0: 0.0,
                    sweep: 2.0 * PI,
                },
                i,
                *src,
            )),
            Contour::Path { segs: ss, src, .. } => {
                segs.extend(ss.iter().zip(src).map(|(sg, sr)| (*sg, i, *sr)))
            }
        }
    }
    let bbox: Vec<(DVec2, DVec2)> = segs.iter().map(|(sg, ..)| sg.bbox()).collect();
    let near = |i: usize, j: usize| {
        let ((a0, a1), (b0, b1)) = (bbox[i], bbox[j]);
        a0.x <= b1.x + tol && b0.x <= a1.x + tol && a0.y <= b1.y + tol && b0.y <= a1.y + tol
    };
    // 3. Overlaps (segment level).
    let mut overlapping: std::collections::HashSet<(usize, usize)> =
        std::collections::HashSet::new();
    for i in 0..segs.len() {
        for j in i + 1..segs.len() {
            if near(i, j) && overlap_len(&segs[i].0, &segs[j].0, tol) > tol {
                overlapping.insert((i, j));
                add(
                    "overlap",
                    Finding {
                        obj: obj(segs[j].2),
                        at: segs[j].0.point_at(0.5),
                        measured: None,
                    },
                );
            }
        }
    }
    // 4. Crossings: fine chords of different segments that meet away from a
    //    shared joint. Sweep over x for speed.
    let mut chords: Vec<(DVec2, DVec2, usize)> = Vec::new();
    for (k, (sg, ..)) in segs.iter().enumerate() {
        let p = sg.flatten(ftol);
        chords.extend(p.windows(2).map(|w| (w[0], w[1], k)));
    }
    chords.sort_by(|x, y| x.0.x.min(x.1.x).total_cmp(&y.0.x.min(y.1.x)));
    let mut crossings: Vec<(DVec2, usize, usize)> = Vec::new();
    for i in 0..chords.len() {
        let (a, b, si) = chords[i];
        let max_x = a.x.max(b.x);
        for &(c, d, sj) in &chords[i + 1..] {
            if c.x.min(d.x) > max_x + tol {
                break;
            }
            if si == sj || overlapping.contains(&(si.min(sj), si.max(sj))) {
                continue;
            }
            let Some(p) = seg_intersection(a, b, c, d) else {
                continue;
            };
            let (s1, s2) = (&segs[si].0, &segs[sj].0);
            let joint = [s1.start(), s1.end()].into_iter().any(|e| {
                e.distance(p) <= 2.0 * tol
                    && [s2.start(), s2.end()].iter().any(|f| f.distance(e) <= tol)
            });
            if !joint && crossings.iter().all(|(q, ..)| q.distance(p) > tol) {
                crossings.push((p, si, sj));
            }
        }
    }
    for (p, _, sj) in crossings {
        add(
            "crossing",
            Finding {
                obj: obj(segs[sj].2),
                at: p,
                measured: None,
            },
        );
    }

    // 5. Nesting of closed contours: depth = number of closed contours around.
    let closed: Vec<usize> = (0..g.contours.len())
        .filter(|&i| g.contours[i].is_closed() && polys[i].len() >= 3)
        .collect();
    let cb: HashMap<usize, (DVec2, DVec2)> =
        closed.iter().map(|&i| (i, g.contours[i].bbox())).collect();
    let area: HashMap<usize, f64> = closed.iter().map(|&i| (i, poly_area(&polys[i]))).collect();
    let contains = |outer: usize, inner: usize| {
        let ((o0, o1), (i0, i1)) = (cb[&outer], cb[&inner]);
        outer != inner
            && o0.x <= i0.x + tol
            && o0.y <= i0.y + tol
            && o1.x >= i1.x - tol
            && o1.y >= i1.y - tol
            && area[&outer] > area[&inner]
            && point_in_poly(polys[inner][0], &polys[outer])
    };
    let mut depth: HashMap<usize, usize> = HashMap::new();
    let mut parent: HashMap<usize, usize> = HashMap::new();
    for &i in &closed {
        let around: Vec<usize> = closed.iter().copied().filter(|&o| contains(o, i)).collect();
        depth.insert(i, around.len());
        if let Some(&p) = around.iter().min_by(|a, b| area[a].total_cmp(&area[b])) {
            parent.insert(i, p);
        }
    }
    let is_hole = |i: usize| depth[&i] % 2 == 1;
    let material = |p: DVec2| {
        closed
            .iter()
            .filter(|&&o| point_in_poly(p, &polys[o]))
            .count()
            % 2
            == 1
    };

    for &i in &closed {
        let at = polys[i][0];
        if is_hole(i) {
            // 6. Hole size.
            let size = match g.contours[i] {
                Contour::Circle { r, .. } => 2.0 * r,
                _ => min_width(&polys[i]),
            };
            if size < lim(limits.min_hole) - 1e-9 {
                let (lo, hi) = cb[&i];
                add(
                    "min-hole",
                    Finding {
                        obj: first_obj(i),
                        at: (lo + hi) * 0.5,
                        measured: Some(size),
                    },
                );
            }
            // 7. Hole to edge (vs its part's outline).
            if let Some(&p) = parent.get(&i) {
                let (d, a, b) = poly_dist(&polys[i], &polys[p]);
                if d < lim(limits.min_hole_to_edge) - 1e-9 {
                    add(
                        "hole-to-edge",
                        Finding {
                            obj: first_obj(i),
                            at: (a + b) * 0.5,
                            measured: Some(d),
                        },
                    );
                }
            }
        } else {
            // 8. Part size (every part, islands included).
            let (lo, hi) = cb[&i];
            let (w, h) = (hi.x - lo.x, hi.y - lo.y);
            let (long, short) = (w.max(h), w.min(h));
            let [min_l, min_s] = limits.min_part.map(lim);
            let [max_l, max_s] = limits.max_part.map(lim);
            let bad = if short < min_s - 1e-9 || short > max_s + 1e-9 {
                Some(short)
            } else if long < min_l - 1e-9 || long > max_l + 1e-9 {
                Some(long)
            } else {
                None
            };
            if let Some(m) = bad {
                add(
                    "part-size",
                    Finding {
                        obj: first_obj(i),
                        at,
                        measured: Some(m),
                    },
                );
            }
            if depth[&i] >= 2 {
                add(
                    "island",
                    Finding {
                        obj: first_obj(i),
                        at,
                        measured: None,
                    },
                );
            }
        }
    }
    // 8b. Bridges between holes of the same part.
    for (n, &i) in closed.iter().enumerate() {
        for &j in &closed[n + 1..] {
            if is_hole(i)
                && is_hole(j)
                && parent.contains_key(&i)
                && parent.get(&i) == parent.get(&j)
            {
                let ((a0, a1), (b0, b1)) = (cb[&i], cb[&j]);
                let m = lim(limits.min_bridge);
                if a0.x > b1.x + m || b0.x > a1.x + m || a0.y > b1.y + m || b0.y > a1.y + m {
                    continue;
                }
                let (d, a, b) = poly_dist(&polys[i], &polys[j]);
                if d < m - 1e-9 {
                    add(
                        "bridge",
                        Finding {
                            obj: first_obj(j),
                            at: (a + b) * 0.5,
                            measured: Some(d),
                        },
                    );
                }
            }
        }
    }
    // 8c. Necks inside one contour: two boundary points closer than the
    //     minimum while far apart along the boundary. Material between them
    //     is a thin web; void is a narrow slot.
    for &i in &closed {
        let m_b = lim(limits.min_bridge);
        let m_h = lim(limits.min_hole);
        let m = m_b.max(m_h);
        if m <= 0.0 {
            continue;
        }
        let pts = resample(&polys[i], m / 4.0, 20_000);
        let total = pts.last().map_or(0.0, |p| p.1);
        let cell = |p: DVec2| ((p.x / m).floor() as i64, (p.y / m).floor() as i64);
        let mut grid: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
        for (k, (p, _)) in pts.iter().enumerate() {
            grid.entry(cell(*p)).or_default().push(k);
        }
        let mut cands: Vec<(f64, DVec2, bool)> = Vec::new();
        for (k, &(p, sk)) in pts.iter().enumerate() {
            let (cx, cy) = cell(p);
            for dx in -1..=1 {
                for dy in -1..=1 {
                    for &j in grid.get(&(cx + dx, cy + dy)).into_iter().flatten() {
                        if j <= k {
                            continue;
                        }
                        let (q, sj) = pts[j];
                        let along = (sj - sk).min(total - (sj - sk));
                        let d = p.distance(q);
                        if along > PI * d.max(ftol) * 1.5 && along > 2.0 * ftol {
                            let mid = (p + q) * 0.5;
                            let solid = material(mid);
                            let limit = if solid { m_b } else { m_h };
                            if d < limit - 1e-9 && d > tol {
                                cands.push((d, mid, solid));
                            }
                        }
                    }
                }
            }
        }
        cands.sort_by(|a, b| a.0.total_cmp(&b.0));
        // One finding per neighbourhood, webs and slots clustered apart.
        let mut kept: Vec<(DVec2, bool)> = Vec::new();
        for (d, mid, solid) in cands {
            if kept
                .iter()
                .all(|(k, ks)| *ks != solid || k.distance(mid) > 2.0 * m)
            {
                kept.push((mid, solid));
                let rule = if solid { "bridge" } else { "min-hole" };
                add(
                    rule,
                    Finding {
                        obj: first_obj(i),
                        at: mid,
                        measured: Some(d),
                    },
                );
            }
        }
    }

    // 9. Non-cuttable objects in the export set.
    for (id, kind, at) in &g.skipped {
        let rule = if *kind == SkipKind::Text {
            "text"
        } else {
            "annotation"
        };
        let p = DVec2::new(at.x, at.y) * s;
        add(
            rule,
            Finding {
                obj: *id,
                at: p,
                measured: None,
            },
        );
    }

    // Assemble the report in rule-table order.
    let unit = u.label().to_string();
    let required = |rule: &str| -> Option<f64> {
        Some(match rule {
            "open-contour" => tol,
            "min-hole" => lim(limits.min_hole),
            "bridge" => lim(limits.min_bridge),
            "hole-to-edge" => lim(limits.min_hole_to_edge),
            _ => return None,
        })
    };
    let mut markers: Vec<Marker> = Vec::new();
    let mut rules = Vec::new();
    for &(id, sev, msg) in RULES {
        let fs = found.remove(id).unwrap_or_default();
        let worst = if id == "open-contour" {
            fs.iter().filter_map(|f| f.measured).reduce(f64::max)
        } else {
            fs.iter().filter_map(|f| f.measured).reduce(f64::min)
        };
        let (measured, req) = if id == "part-size" {
            (worst, None)
        } else {
            (worst, required(id))
        };
        let checked = match id {
            "text" | "annotation" => g.skipped.len(),
            "duplicate" | "overlap" | "crossing" => segs.len() + g.duplicates.len(),
            _ => g.contours.len(),
        };
        for f in &fs {
            markers.push((g.to_world(f.at), sev));
        }
        rules.push(RuleOutcome {
            rule_id: id.to_string(),
            code_ref: if limits.source.starts_with("SendCutSend") {
                "SendCutSend".into()
            } else {
                String::new()
            },
            severity: sev.label().to_string(),
            verdict: if fs.is_empty() {
                "pass".into()
            } else {
                sev.verdict().to_string()
            },
            message: msg.to_string(),
            objects: fs.iter().map(|f| f.obj.short()).collect(),
            locations: fs.iter().map(|f| g.to_world(f.at).to_array()).collect(),
            measured,
            required: req,
            unit: unit.clone(),
            checked,
        });
    }
    (
        ComplianceReport {
            pack: PACK.to_string(),
            context,
            rules,
        },
        markers,
    )
}

/// Points along a closed polygon at most `step` apart, with cumulative arc
/// length; the step grows so the count stays ≤ `cap`.
fn resample(poly: &[DVec2], step: f64, cap: usize) -> Vec<(DVec2, f64)> {
    let n = poly.len();
    let total: f64 = (0..n).map(|i| poly[i].distance(poly[(i + 1) % n])).sum();
    let step = step.max(total / cap as f64).max(1e-12);
    let mut out = Vec::new();
    let mut s = 0.0;
    for i in 0..n {
        let (a, b) = (poly[i], poly[(i + 1) % n]);
        let len = a.distance(b);
        let k = ((len / step).ceil() as usize).max(1);
        for j in 0..k {
            let t = j as f64 / k as f64;
            out.push((a.lerp(b, t), s + len * t));
        }
        s += len;
    }
    out
}

/// Context line for the report header.
pub fn context(units: CutUnit, limits: &CutLimits) -> String {
    let s = units.per_meter();
    let u = units.label();
    let mut c = format!(
        "units {u}, thickness {t} {u}; min hole {h}, bridge {b}, hole-to-edge {e} {u}; part {mn0}x{mn1} to {mx0}x{mx1} {u}; limits: {src}",
        t = fmt(limits.thickness * s),
        h = fmt(limits.min_hole * s),
        b = fmt(limits.min_bridge * s),
        e = fmt(limits.min_hole_to_edge * s),
        mn0 = fmt(limits.min_part[0] * s),
        mn1 = fmt(limits.min_part[1] * s),
        mx0 = fmt(limits.max_part[0] * s),
        mx1 = fmt(limits.max_part[1] * s),
        src = limits.source,
    );
    for n in &limits.notes {
        c.push_str(&format!("; NOTE: {n}"));
    }
    c
}

/// Markdown table of a cutcheck report (locations shown in file units).
pub fn markdown(r: &ComplianceReport, units: CutUnit) -> String {
    let s = units.per_meter();
    let (fail, warn) = (
        r.rules.iter().filter(|o| o.verdict == "fail").count(),
        r.rules.iter().filter(|o| o.verdict == "warn").count(),
    );
    let mut out = format!(
        "cutcheck: {} — {}\n\n| check | result | worst | limit | where ({}) |\n|---|---|---|---|---|\n",
        if fail + warn == 0 {
            "clean".to_string()
        } else {
            format!("{fail} fail, {warn} warn")
        },
        r.context,
        units.label(),
    );
    for o in &r.rules {
        let num = |v: Option<f64>| {
            v.map(|x| format!("{} {}", fmt(x), o.unit))
                .unwrap_or_else(|| "—".into())
        };
        let mut wh: Vec<String> = o
            .objects
            .iter()
            .zip(&o.locations)
            .take(4)
            .map(|(id, l)| format!("{id} ({}, {})", fmt(l[0] * s), fmt(l[1] * s)))
            .collect();
        if o.objects.len() > 4 {
            wh.push(format!("+{} more", o.objects.len() - 4));
        }
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            o.rule_id,
            o.verdict.to_uppercase(),
            num(o.measured),
            num(o.required),
            if wh.is_empty() {
                "—".into()
            } else {
                wh.join("; ")
            }
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Session, parse};

    fn run(s: &mut Session, line: &str) -> String {
        s.run(parse(line).unwrap_or_else(|e| panic!("{line}: {e}")))
            .unwrap_or_else(|e| panic!("{line}: {e}"))
            .message
    }

    fn rule<'a>(s: &'a Session, id: &str) -> &'a RuleOutcome {
        s.doc.compliance_reports[PACK]
            .rules
            .iter()
            .find(|r| r.rule_id == id)
            .unwrap()
    }

    fn failing(s: &Session) -> Vec<String> {
        s.doc.compliance_reports[PACK]
            .rules
            .iter()
            .filter(|r| r.verdict != "pass")
            .map(|r| r.rule_id.clone())
            .collect()
    }

    fn short(s: &Session, nth: usize) -> String {
        s.doc.objects().nth(nth).unwrap().id.short()
    }

    /// #172: a 60 × 30 mm plate with an 8 mm "IJC 08" label. Before outlining
    /// the label is a "text" WARN; after `textoutline` it is cut as closed
    /// glyph contours, and the counters of 0 and 8 fall out as islands.
    #[test]
    fn outlined_text_cuts_as_closed_contours_with_island_counters() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 0.06 0.03");
        run(&mut s, "text 0.005,0.01,0 IJC 08 0.008");
        let msg = run(&mut s, "cutcheck thickness=1");
        assert_eq!(failing(&s), ["text"], "{msg}");

        let out = run(&mut s, "textoutline all");
        assert!(out.contains("1 text object(s)"), "{out}");
        let msg = run(&mut s, "cutcheck thickness=1");
        // No text, open, crossing or duplicate finding: the glyphs are closed
        // holes in the plate. The counters (one in 0, two in 8) are islands,
        // and as loose parts they are also below the minimum part size.
        assert_eq!(failing(&s), ["part-size", "island"], "{msg}");
        assert_eq!(rule(&s, "island").objects.len(), 3, "{msg}");
        assert_eq!(rule(&s, "island").verdict, "warn");
        assert_eq!(rule(&s, "part-size").objects, rule(&s, "island").objects);
    }

    #[test]
    fn clean_plate_passes() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 0.05 0.03");
        run(&mut s, "circle 0.01,0.015,0 0.0025");
        let msg = run(&mut s, "cutcheck thickness=3");
        assert!(failing(&s).is_empty(), "{msg}");
        assert!(msg.contains("clean") && msg.contains("| check |"), "{msg}");
        assert!(
            !s.doc
                .layers
                .contains_key(crate::checkengine::COMPLIANCE_LAYER),
            "no markers"
        );
    }

    #[test]
    fn acceptance_open_outline_duplicate_edge_and_small_hole() {
        let mut s = Session::default();
        // Open outline: rectangle drawn as three lines (one side missing).
        run(&mut s, "polyline 0,0,0 0.05,0,0 0.05,0.03,0 0,0.03,0");
        // A closed plate next to it with a duplicated edge and a 1 mm hole.
        run(&mut s, "rect 0.1,0,0 0.05 0.03");
        run(&mut s, "line 0.1,0,0 0.15,0,0");
        run(&mut s, "circle 0.12,0.015,0 0.0005");
        let msg = run(&mut s, "cutcheck thickness=3");
        assert_eq!(
            failing(&s),
            ["open-contour", "duplicate", "min-hole"],
            "{msg}"
        );
        assert_eq!(rule(&s, "open-contour").objects, [short(&s, 0)]);
        assert_eq!(rule(&s, "duplicate").objects, [short(&s, 2)]);
        assert_eq!(rule(&s, "min-hole").objects, [short(&s, 3)]);
        assert_eq!(rule(&s, "min-hole").measured, Some(1.0));
        assert_eq!(rule(&s, "min-hole").required, Some(1.5));
        assert!(msg.contains(&short(&s, 3)), "{msg}");
        // One marker per finding on the compliance layer.
        let markers = s
            .doc
            .objects()
            .filter(|o| o.layer == crate::checkengine::COMPLIANCE_LAYER)
            .count();
        assert_eq!(markers, 3);
        // Re-running ignores the marker layer.
        run(&mut s, "cutcheck thickness=3");
        assert_eq!(failing(&s), ["open-contour", "duplicate", "min-hole"]);
    }

    #[test]
    fn overlap_and_crossing() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 0.05 0.03");
        run(&mut s, "line 0.01,0,0 0.03,0,0"); // runs along the bottom edge
        run(&mut s, "rect 0.04,0.02,0 0.03 0.03"); // crosses the plate corner
        let msg = run(&mut s, "cutcheck thickness=1");
        let f = failing(&s);
        assert!(
            f.contains(&"overlap".to_string()) && f.contains(&"crossing".to_string()),
            "{msg}"
        );
        assert_eq!(rule(&s, "crossing").objects.len(), 2, "{msg}");
        // A figure-eight polyline crosses itself.
        let mut s = Session::default();
        run(
            &mut s,
            "polyline 0,0,0 0.02,0.02,0 0.02,0,0 0,0.02,0 closed",
        );
        run(&mut s, "cutcheck thickness=1");
        assert_eq!(rule(&s, "crossing").verdict, "fail");
    }

    #[test]
    fn bridge_between_holes_and_hole_to_edge() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 0.05 0.03");
        run(&mut s, "circle 0.01,0.015,0 0.003");
        run(&mut s, "circle 0.0168,0.015,0 0.003"); // 0.8 mm web
        run(&mut s, "circle 0.04,0.0039,0 0.003"); // 0.9 mm from the bottom edge
        let msg = run(&mut s, "cutcheck thickness=2");
        assert_eq!(failing(&s), ["bridge", "hole-to-edge"], "{msg}");
        assert!(
            (rule(&s, "bridge").measured.unwrap() - 0.8).abs() < 0.01,
            "{msg}"
        );
        assert!(
            (rule(&s, "hole-to-edge").measured.unwrap() - 0.9).abs() < 0.01,
            "{msg}"
        );
        assert_eq!(rule(&s, "bridge").required, Some(1.0));
    }

    #[test]
    fn thin_web_and_narrow_slot_inside_one_outline() {
        let mut s = Session::default();
        // Comb: 40 × 20 mm plate with a 0.5 mm wide, 15 mm deep slot cut from
        // the top, and a 0.6 mm wide finger between slot and right edge.
        run(
            &mut s,
            "polyline 0,0,0 0.04,0,0 0.04,0.02,0 0.0394,0.02,0 0.0394,0.005,0 0.0389,0.005,0 0.0389,0.02,0 0,0.02,0 closed",
        );
        let msg = run(&mut s, "cutcheck thickness=2");
        assert_eq!(failing(&s), ["min-hole", "bridge"], "{msg}");
        assert!(
            (rule(&s, "min-hole").measured.unwrap() - 0.5).abs() < 0.01,
            "{msg}"
        );
        assert!(
            (rule(&s, "bridge").measured.unwrap() - 0.6).abs() < 0.01,
            "{msg}"
        );
    }

    #[test]
    fn slotted_hole_uses_min_width() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 0.05 0.03");
        run(&mut s, "rect 0.01,0.01,0 0.02 0.0008"); // 20 × 0.8 mm slot
        let msg = run(&mut s, "cutcheck thickness=2");
        assert_eq!(failing(&s), ["min-hole"], "{msg}");
        assert!((rule(&s, "min-hole").measured.unwrap() - 0.8).abs() < 1e-6);
    }

    #[test]
    fn part_size_text_annotation_and_island() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 0.005 0.005"); // 5 mm part < 0.5 in
        run(&mut s, "rect 0.1,0,0 1.2 0.1"); // 1.2 m long > 44 in
        run(&mut s, "text 0.02,0.01,0 HELLO 0.002");
        run(&mut s, "rect 0.2,0.2,0 0.06 0.06");
        run(&mut s, "rect 0.21,0.21,0 0.04 0.04");
        run(&mut s, "rect 0.22,0.22,0 0.02 0.02"); // island in the hole
        run(&mut s, "dim 0.2,0.2,0 0.26,0.2,0 0.005");
        let msg = run(&mut s, "cutcheck thickness=1");
        let f = failing(&s);
        for want in ["part-size", "text", "annotation", "island"] {
            assert!(f.contains(&want.to_string()), "{want}: {msg}");
        }
        assert_eq!(rule(&s, "part-size").objects.len(), 2, "{msg}");
        assert_eq!(rule(&s, "text").objects, [short(&s, 2)]);
        assert_eq!(rule(&s, "island").objects, [short(&s, 5)]);
    }

    #[test]
    fn material_uses_specs_and_reports_date_and_fallbacks() {
        crate::scs::tests::install_fixtures();
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 0.05 0.03");
        run(&mut s, "circle 0.01,0.015,0 0.0006"); // 1.2 mm hole: < 0.050 in
        let msg = run(&mut s, "cutcheck material=FIX-AL-125");
        let ctx = &s.doc.compliance_reports[PACK].context;
        assert!(
            ctx.contains("SendCutSend FIX-AL-125") && ctx.contains("2026-01-02T03:04:05"),
            "{ctx}"
        );
        assert!(ctx.contains("thickness 3.175 mm"), "{ctx}");
        assert_eq!(failing(&s), ["min-hole"], "{msg}");
        assert!((rule(&s, "min-hole").required.unwrap() - 1.27).abs() < 1e-9);
        // Words + thickness pick the nearest sheet; missing fields fall back
        // to the rule of thumb and say so.
        run(&mut s, "cutcheck material=aluminum thickness=1.6");
        let ctx = &s.doc.compliance_reports[PACK].context;
        assert!(
            ctx.contains("FIX-AL-063")
                && ctx.contains("min_bridge_size, min_hole_to_edge, max_part_size"),
            "{ctx}"
        );
        assert!(ctx.contains("out of stock"), "{ctx}");
    }

    #[test]
    fn missing_cache_falls_back_loudly_and_needs_thickness() {
        let dir = crate::scs::default_dir().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 0.05 0.03");
        let e = s
            .run(parse("cutcheck material=ALU-125").unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("thickness=") && e.contains("sendcutsend-specs.json"),
            "{e}"
        );
        let msg = run(&mut s, "cutcheck material=ALU-125 thickness=3");
        assert!(
            msg.contains("SendCutSend data unavailable") && msg.contains("download"),
            "{msg}"
        );
        assert!(
            s.run(parse("cutcheck").unwrap()).is_err(),
            "no thickness, no material"
        );
    }

    #[test]
    fn inches_and_replay_are_stable() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 0.05 0.03");
        run(&mut s, "circle 0.01,0.015,0 0.0005");
        run(&mut s, "cutcheck units=in thickness=0.125");
        let r = rule(&s, "min-hole");
        assert_eq!(r.unit, "in");
        assert!((r.required.unwrap() - 0.0625).abs() < 1e-12);
        // Logged op carries limits + marker ids: replay reproduces the report.
        let replayed = Session::replay(s.save_log()).unwrap();
        assert_eq!(replayed.doc.compliance_reports, s.doc.compliance_reports);
        // Undo removes the markers.
        let before = s.doc.len();
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), before - 1);
    }

    #[test]
    fn selection_scope() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 0.05 0.03");
        run(&mut s, "line 0.1,0,0 0.2,0,0");
        let rect = s.doc.objects().next().unwrap().id;
        s.doc.selection.insert(rect);
        run(&mut s, "cutcheck sel thickness=3");
        assert!(failing(&s).is_empty());
        run(&mut s, "cutcheck thickness=3");
        assert_eq!(failing(&s), ["open-contour"]);
    }

    #[test]
    fn generic_preset_uses_the_rule_of_thumb_and_no_data() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 0.05 0.03");
        run(&mut s, "circle 0.01,0.015,0 0.0005"); // 1 mm hole < 1.5 mm
        run(&mut s, "cutcheck preset=generic thickness=3");
        let ctx = &s.doc.compliance_reports[PACK].context;
        assert!(ctx.contains("limits: rule of thumb"), "{ctx}");
        assert_eq!(failing(&s), ["min-hole"]);
        // Logged with its preset; the default (scs) stays out of the op.
        let ops = s.save_log();
        let json = serde_json::to_string(&ops).unwrap();
        assert!(json.contains(r#""preset":"generic""#), "{json}");
        run(&mut s, "cutcheck preset=scs thickness=3");
        let last = serde_json::to_string(s.save_log().last().unwrap()).unwrap();
        assert!(!last.contains("preset"), "{last}");
        // SendCutSend data is never read, so a material is refused, not
        // silently dropped (also when the request skips the parser).
        assert!(parse("cutcheck preset=generic material=AL5052-0.125").is_err());
        assert!(parse("cutcheck preset=plasma thickness=3").is_err());
        let e = resolve_limits(CutPreset::Generic, CutUnit::Mm, Some(3.0), Some("al"), None);
        assert!(e.unwrap_err().contains("preset=generic"));
        let e = resolve_limits(CutPreset::Generic, CutUnit::Mm, None, None, None);
        assert!(e.unwrap_err().contains("thickness"));
    }
}
