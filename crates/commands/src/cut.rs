// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Cut-ready DXF profile (`export <path>.dxf profile=cut units=mm|in`).
//!
//! The drafting DXF writer ([`crate::dxf`]) dumps the whole document in meters
//! with no `$INSUNITS`, tessellates curves at 5 mm and includes text,
//! dimensions and mesh edges. Fabrication services (laser, waterjet, router —
//! SendCutSend and friends) need the opposite: 1:1 coordinates in mm or inches
//! with `$INSUNITS` set, only cuttable curves, true arcs and circles, and each
//! outline as one closed contour. This module builds that contour set
//! ([`CutGeometry`]) and writes it as DXF R12. `cutcheck` reuses the same
//! contour set so the check and the export can never disagree.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use glam::{DVec2, DVec3};
use itsjustcad_doc::{Annotation, BlockGeometry, Document, Geometry, ObjectId, SceneObject};
use kernel_curve::{Curve, Seg2, fit_biarcs};
use serde::{Deserialize, Serialize};

use crate::command::Selector;

/// Output unit of a cut file. Fabricators accept only inches or millimeters;
/// there is deliberately no meter variant (a 50 mm part written as 0.05 units
/// quotes as a speck).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CutUnit {
    #[default]
    Mm,
    In,
}

impl CutUnit {
    pub fn parse(s: &str) -> Option<CutUnit> {
        match s.to_ascii_lowercase().as_str() {
            "mm" | "millimeter" | "millimeters" | "millimetre" | "millimetres" => Some(CutUnit::Mm),
            "in" | "inch" | "inches" | "\"" => Some(CutUnit::In),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            CutUnit::Mm => "mm",
            CutUnit::In => "in",
        }
    }

    /// File units per document meter.
    pub fn per_meter(self) -> f64 {
        match self {
            CutUnit::Mm => 1000.0,
            CutUnit::In => 1.0 / itsjustcad_doc::METERS_PER_INCH,
        }
    }

    /// DXF `$INSUNITS` code (1 = inches, 4 = millimeters).
    pub fn insunits(self) -> i32 {
        match self {
            CutUnit::Mm => 4,
            CutUnit::In => 1,
        }
    }

    /// Max deviation of fitted arcs from ellipses/splines, in file units
    /// (0.01 mm; 0.0004 in ≈ 0.01 mm).
    pub fn fit_tol(self) -> f64 {
        match self {
            CutUnit::Mm => 0.01,
            CutUnit::In => 0.0004,
        }
    }

    /// Endpoint gap at or below which two segments count as touching, in
    /// file units. Gaps above this are open contours.
    pub fn join_tol(self) -> f64 {
        self.fit_tol()
    }
}

/// Options of the cut profile, shared by `export … profile=cut` and `cutcheck`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CutOpts {
    pub units: CutUnit,
    /// Only objects on these layers (case-insensitive). Empty = every layer
    /// except hidden ones and the `compliance` marker layer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layers: Vec<String>,
    /// Only these objects (`sel`, a name, …). `None` = the layer scope above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub targets: Option<Selector>,
}

/// Why an object in scope is left out of the cut file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipKind {
    /// Text/field: single-stroke Hershey glyphs, not closed outlines.
    Text,
    Dimension,
    Hatch,
    /// Meshes, solids, structural members: no 2D profile.
    Mesh,
    /// Point clouds and other non-curve geometry.
    Other,
}

impl SkipKind {
    pub fn label(self) -> &'static str {
        match self {
            SkipKind::Text => "text",
            SkipKind::Dimension => "dimension",
            SkipKind::Hatch => "hatch",
            SkipKind::Mesh => "mesh",
            SkipKind::Other => "other",
        }
    }
}

/// One cut contour, in file units.
#[derive(Clone, Debug, PartialEq)]
pub enum Contour {
    Circle {
        c: DVec2,
        r: f64,
        src: usize,
    },
    /// `src[i]` is the source index of `segs[i]`.
    Path {
        segs: Vec<Seg2>,
        closed: bool,
        src: Vec<usize>,
    },
}

impl Contour {
    pub fn is_closed(&self) -> bool {
        match self {
            Contour::Circle { .. } => true,
            Contour::Path { closed, .. } => *closed,
        }
    }

    /// Distinct source indices (into [`CutGeometry::sources`]) this contour
    /// came from, in first-use order.
    pub fn srcs(&self) -> Vec<usize> {
        match self {
            Contour::Circle { src, .. } => vec![*src],
            Contour::Path { src, .. } => {
                let mut out: Vec<usize> = Vec::new();
                for s in src {
                    if !out.contains(s) {
                        out.push(*s);
                    }
                }
                out
            }
        }
    }

    pub fn bbox(&self) -> (DVec2, DVec2) {
        match self {
            Contour::Circle { c, r, .. } => (*c - DVec2::splat(*r), *c + DVec2::splat(*r)),
            Contour::Path { segs, .. } => segs.iter().map(Seg2::bbox).fold(
                (DVec2::splat(f64::INFINITY), DVec2::splat(f64::NEG_INFINITY)),
                |(lo, hi), (a, b)| (lo.min(a), hi.max(b)),
            ),
        }
    }

    /// Polyline approximation (chord error ≤ `tol`); closed contours repeat
    /// no point (the last-to-first edge is implied).
    pub fn flatten(&self, tol: f64) -> Vec<DVec2> {
        match self {
            Contour::Circle { c, r, .. } => {
                let full = Seg2::Arc {
                    c: *c,
                    r: *r,
                    a0: 0.0,
                    sweep: std::f64::consts::TAU,
                };
                let mut p = full.flatten(tol);
                p.pop();
                p
            }
            Contour::Path { segs, closed, .. } => {
                let mut out: Vec<DVec2> = Vec::new();
                for s in segs {
                    let pts = s.flatten(tol);
                    let skip = usize::from(!out.is_empty());
                    out.extend_from_slice(&pts[skip..]);
                }
                if *closed && out.len() > 1 {
                    out.pop();
                }
                out
            }
        }
    }
}

/// An exact duplicate dropped while building the contour set.
#[derive(Clone, Debug, PartialEq)]
pub struct Duplicate {
    pub src: usize,
    /// Midpoint of the dropped segment, file units.
    pub at: DVec2,
}

/// The cuttable geometry of a scope, in file units, plus what was left out.
#[derive(Clone, Debug, Default)]
pub struct CutGeometry {
    pub units: CutUnit,
    pub contours: Vec<Contour>,
    /// `(object id, layer)` per source index.
    pub sources: Vec<(ObjectId, String)>,
    /// Raw segments before dedupe/join (cutcheck's overlap/crossing input).
    pub raw: Vec<(Seg2, usize)>,
    pub duplicates: Vec<Duplicate>,
    /// Objects in scope that are not cut, with a world-space (meters) anchor.
    pub skipped: Vec<(ObjectId, SkipKind, DVec3)>,
    /// Curve spans where biarc fitting fell back to chords.
    pub fallback_spans: usize,
    /// Curves that were not flat in XY (projected to z = 0).
    pub nonplanar: usize,
}

impl CutGeometry {
    /// File-unit point → document meters (z = 0), for viewport markers.
    pub fn to_world(&self, p: DVec2) -> DVec3 {
        let s = self.units.per_meter();
        DVec3::new(p.x / s, p.y / s, 0.0)
    }
}

/// Similarity transform document meters → file units (uniform scale, Z
/// rotation, translation), so arcs stay arcs and ellipses fit exactly.
#[derive(Clone, Copy)]
struct Sim {
    s: f64,
    rot: f64,
    t: DVec2,
}

impl Sim {
    fn apply(&self, p: DVec3) -> DVec2 {
        let (sin, cos) = self.rot.sin_cos();
        DVec2::new(p.x * cos - p.y * sin, p.x * sin + p.y * cos) * self.s + self.t
    }
}

/// Objects in the cut scope, in creation order. `ids` (already resolved from
/// `targets`) wins over the default visibility filter; `layers` narrows both.
pub fn scope<'a>(
    doc: &'a Document,
    layers: &[String],
    ids: Option<&[ObjectId]>,
) -> Vec<&'a SceneObject> {
    let wanted: Option<HashSet<ObjectId>> = ids.map(|v| v.iter().copied().collect());
    let layer_ok = |o: &SceneObject| {
        if !layers.is_empty() {
            return layers.iter().any(|l| l.eq_ignore_ascii_case(&o.layer));
        }
        if wanted.is_some() {
            return true;
        }
        o.visible
            && o.layer != crate::checkengine::COMPLIANCE_LAYER
            && doc.layers.get(&o.layer).is_none_or(|st| st.visible)
    };
    doc.objects()
        .filter(|o| wanted.as_ref().is_none_or(|w| w.contains(&o.id)))
        .filter(|o| layer_ok(o))
        .collect()
}

fn anchor(doc: &Document, g: &Geometry) -> DVec3 {
    let b = doc.geometry_aabb(g);
    if b.min.is_finite() && b.max.is_finite() {
        (b.min + b.max) * 0.5
    } else {
        DVec3::ZERO
    }
}

/// Raw pieces of one curve: open/closed segment runs and whole circles.
fn curve_pieces(
    c: &Curve,
    sim: Sim,
    tol: f64,
    segs: &mut Vec<Seg2>,
    circles: &mut Vec<(DVec2, f64)>,
    fallback: &mut usize,
) {
    let push = |segs: &mut Vec<Seg2>, s: Seg2| {
        if s.length() > 0.0 {
            segs.push(s);
        }
    };
    match c {
        Curve::Line { a, b } => push(
            segs,
            Seg2::Line {
                a: sim.apply(*a),
                b: sim.apply(*b),
            },
        ),
        Curve::Polyline { points, .. } => {
            let pts: Vec<DVec2> = points.iter().map(|p| sim.apply(*p)).collect();
            for w in pts.windows(2) {
                push(segs, Seg2::Line { a: w[0], b: w[1] });
            }
            if c.is_closed()
                && let (Some(&f), Some(&l)) = (pts.first(), pts.last())
            {
                push(segs, Seg2::Line { a: l, b: f });
            }
        }
        Curve::Arc {
            center,
            radius,
            start,
            end,
        } => {
            let (cc, r) = (sim.apply(*center), radius * sim.s);
            if c.is_closed() {
                circles.push((cc, r));
            } else {
                // Same convention as `Curve::tessellate`: CCW from `start`.
                push(
                    segs,
                    Seg2::Arc {
                        c: cc,
                        r,
                        a0: start + sim.rot,
                        sweep: (end - start).abs(),
                    },
                );
            }
        }
        Curve::Ellipse { center, rx, ry } => {
            if (rx - ry).abs() <= 1e-12 * rx.abs().max(1.0) {
                circles.push((sim.apply(*center), rx * sim.s));
                return;
            }
            let (center, rx, ry) = (*center, *rx, *ry);
            let f = |t: f64| sim.apply(center + DVec3::new(rx * t.cos(), ry * t.sin(), 0.0));
            let df = |t: f64| {
                let d = DVec2::new(-rx * t.sin(), ry * t.cos());
                let (sin, cos) = sim.rot.sin_cos();
                DVec2::new(d.x * cos - d.y * sin, d.x * sin + d.y * cos)
            };
            let fit = fit_biarcs(&f, &df, 0.0, std::f64::consts::TAU, tol);
            *fallback += fit.fallback_spans;
            segs.extend(fit.segs);
        }
        Curve::Nurbs {
            control,
            weights,
            knots,
            degree,
        } => {
            if control.len() < 2 || knots.len() != control.len() + degree + 1 {
                return;
            }
            let (t0, t1) = (knots[*degree], knots[knots.len() - degree - 1]);
            let eval = |t: f64| kernel_curve::nurbs_point(control, weights, knots, *degree, t);
            let f = |t: f64| sim.apply(eval(t));
            let h = (t1 - t0) * 1e-6;
            let df = |t: f64| {
                let (a, b) = ((t - h).max(t0), (t + h).min(t1));
                (f(b) - f(a)) / (b - a)
            };
            let fit = fit_biarcs(&f, &df, t0, t1, tol);
            *fallback += fit.fallback_spans;
            segs.extend(fit.segs);
        }
    }
}

fn is_nonplanar(c: &Curve, tol_m: f64) -> bool {
    let pts = c.points_bound();
    let (lo, hi) = pts
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(l, h), p| {
            (l.min(p.z), h.max(p.z))
        });
    pts.len() > 1 && hi - lo > tol_m
}

/// Quantized key for exact-duplicate detection (direction-independent). The
/// mid-segment point tells a line from an arc and the two arcs sharing a chord
/// apart.
fn dup_key(s: &Seg2) -> [i64; 6] {
    let q = |v: f64| (v * 1e6).round() as i64;
    let (a, b, m) = (s.start(), s.end(), s.point_at(0.5));
    let (p, r) = if (q(a.x), q(a.y)) <= (q(b.x), q(b.y)) {
        (a, b)
    } else {
        (b, a)
    };
    [q(p.x), q(p.y), q(r.x), q(r.y), q(m.x), q(m.y)]
}

/// Build the cut contour set for `objs` in `units`.
pub fn build(doc: &Document, objs: &[&SceneObject], units: CutUnit) -> CutGeometry {
    let s = units.per_meter();
    let tol = units.fit_tol();
    let tol_m = tol / s;
    let mut g = CutGeometry {
        units,
        ..Default::default()
    };
    let mut circles: Vec<(DVec2, f64, usize)> = Vec::new();
    let top = Sim {
        s,
        rot: 0.0,
        t: DVec2::ZERO,
    };
    for obj in objs {
        let src = g.sources.len();
        g.sources.push((obj.id, obj.layer.clone()));
        let mut segs = Vec::new();
        let mut circ = Vec::new();
        let mut curves: Vec<(&Curve, Sim)> = Vec::new();
        match &obj.geometry {
            Geometry::Curve(c) => curves.push((c, top)),
            Geometry::Instance {
                block,
                position,
                rotation_deg,
                scale,
                ..
            } => {
                let sim = Sim {
                    s: s * scale,
                    rot: rotation_deg.to_radians(),
                    t: DVec2::new(position.x, position.y) * s,
                };
                let mut skipped_any = false;
                for def in doc.blocks.get(block).into_iter().flatten() {
                    match def {
                        BlockGeometry::Curve(c) => curves.push((c, sim)),
                        _ => skipped_any = true,
                    }
                }
                if skipped_any {
                    g.skipped
                        .push((obj.id, SkipKind::Other, anchor(doc, &obj.geometry)));
                }
            }
            Geometry::Annotation(a) => {
                let kind = match a {
                    Annotation::Text { .. } | Annotation::Field { .. } => SkipKind::Text,
                    Annotation::LinearDim { .. } | Annotation::AngularDim { .. } => {
                        SkipKind::Dimension
                    }
                    Annotation::Hatch { .. } => SkipKind::Hatch,
                };
                let at = match a {
                    Annotation::Text { pos, .. } | Annotation::Field { pos, .. } => *pos,
                    _ => anchor(doc, &obj.geometry),
                };
                g.skipped.push((obj.id, kind, at));
            }
            Geometry::Mesh(_)
            | Geometry::Frame { .. }
            | Geometry::Area { .. }
            | Geometry::Parametric { .. } => {
                g.skipped
                    .push((obj.id, SkipKind::Mesh, anchor(doc, &obj.geometry)))
            }
            Geometry::Points { .. } => {
                g.skipped
                    .push((obj.id, SkipKind::Other, anchor(doc, &obj.geometry)))
            }
        }
        for (c, sim) in curves {
            if is_nonplanar(c, tol_m) {
                g.nonplanar += 1;
            }
            curve_pieces(c, sim, tol, &mut segs, &mut circ, &mut g.fallback_spans);
        }
        g.raw.extend(segs.into_iter().map(|sg| (sg, src)));
        circles.extend(circ.into_iter().map(|(c, r)| (c, r, src)));
    }

    // Drop exact duplicates (first occurrence wins).
    let mut seen: HashSet<[i64; 6]> = HashSet::new();
    let mut pieces: Vec<(Seg2, usize)> = Vec::new();
    for &(sg, src) in &g.raw {
        if seen.insert(dup_key(&sg)) {
            pieces.push((sg, src));
        } else {
            g.duplicates.push(Duplicate {
                src,
                at: sg.point_at(0.5),
            });
        }
    }
    let q = |v: f64| (v * 1e6).round() as i64;
    let mut seen_c: HashSet<(i64, i64, i64)> = HashSet::new();
    // Contours tagged with their first source index for creation-order output.
    let mut tagged: Vec<(usize, usize, Contour)> = Vec::new();
    for (i, &(c, r, src)) in circles.iter().enumerate() {
        if seen_c.insert((q(c.x), q(c.y), q(r))) {
            tagged.push((src, i, Contour::Circle { c, r, src }));
        } else {
            g.duplicates.push(Duplicate {
                src,
                at: c + DVec2::new(r, 0.0),
            });
        }
    }
    for (k, c) in join(&pieces, units.join_tol()).into_iter().enumerate() {
        let first = c.srcs().first().copied().unwrap_or(0);
        tagged.push((first, circles.len() + k, c));
    }
    tagged.sort_by_key(|(src, k, _)| (*src, *k));
    g.contours = tagged.into_iter().map(|(_, _, c)| c).collect();
    g
}

/// Greedy deterministic chaining of segments whose endpoints touch within
/// `tol`. Lowest index wins at branch points.
fn join(pieces: &[(Seg2, usize)], tol: f64) -> Vec<Contour> {
    let cell = |p: DVec2| ((p.x / tol).floor() as i64, (p.y / tol).floor() as i64);
    let mut grid: HashMap<(i64, i64), Vec<(usize, bool)>> = HashMap::new();
    for (i, (s, _)) in pieces.iter().enumerate() {
        grid.entry(cell(s.start())).or_default().push((i, false));
        grid.entry(cell(s.end())).or_default().push((i, true));
    }
    let mut used = vec![false; pieces.len()];
    // Nearest-by-index unused segment end touching `p`: (index, at_end).
    let find = |p: DVec2, used: &[bool]| -> Option<(usize, bool)> {
        let (cx, cy) = cell(p);
        let mut best: Option<(usize, bool)> = None;
        for dx in -1..=1 {
            for dy in -1..=1 {
                for &(i, at_end) in grid.get(&(cx + dx, cy + dy)).into_iter().flatten() {
                    if used[i] {
                        continue;
                    }
                    let q = if at_end {
                        pieces[i].0.end()
                    } else {
                        pieces[i].0.start()
                    };
                    if q.distance(p) <= tol && best.is_none_or(|b| (i, at_end) < b) {
                        best = Some((i, at_end));
                    }
                }
            }
        }
        best
    };
    let mut out = Vec::new();
    for i in 0..pieces.len() {
        if used[i] {
            continue;
        }
        used[i] = true;
        let mut chain: VecDeque<(Seg2, usize)> = VecDeque::from([pieces[i]]);
        let closes = |chain: &VecDeque<(Seg2, usize)>| {
            chain.len() >= 2
                && chain
                    .back()
                    .unwrap()
                    .0
                    .end()
                    .distance(chain.front().unwrap().0.start())
                    <= tol
        };
        while !closes(&chain) {
            let p = chain.back().unwrap().0.end();
            let Some((j, at_end)) = find(p, &used) else {
                break;
            };
            used[j] = true;
            let s = if at_end {
                pieces[j].0.reversed()
            } else {
                pieces[j].0
            };
            chain.push_back((s, pieces[j].1));
        }
        if !closes(&chain) {
            loop {
                let p = chain.front().unwrap().0.start();
                let Some((j, at_end)) = find(p, &used) else {
                    break;
                };
                used[j] = true;
                let s = if at_end {
                    pieces[j].0
                } else {
                    pieces[j].0.reversed()
                };
                chain.push_front((s, pieces[j].1));
                if closes(&chain) {
                    break;
                }
            }
        }
        let closed = closes(&chain);
        let (segs, src) = chain.into_iter().unzip();
        out.push(Contour::Path { segs, closed, src });
    }
    out
}

/// What [`write_dxf`] wrote, for the command echo.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CutSummary {
    pub closed: usize,
    pub open: usize,
    pub circles: usize,
    pub arcs: usize,
    pub duplicates: usize,
    pub skipped: BTreeMap<&'static str, usize>,
    pub fallback_spans: usize,
    pub nonplanar: usize,
    /// Overall extent `(width, height)` in file units.
    pub extent: (f64, f64),
}

impl CutSummary {
    pub fn describe(&self, units: CutUnit) -> String {
        let u = units.label();
        let mut s = format!(
            "cut DXF ({u}, $INSUNITS={}): {} closed contour(s) ({} circle(s)), {} open, {} arc(s); extent {} x {} {u}",
            units.insunits(),
            self.closed,
            self.circles,
            self.open,
            self.arcs,
            fmt_num(self.extent.0),
            fmt_num(self.extent.1),
        );
        if self.duplicates > 0 {
            s.push_str(&format!("; dropped {} exact duplicate(s)", self.duplicates));
        }
        if !self.skipped.is_empty() {
            let parts: Vec<String> = self
                .skipped
                .iter()
                .map(|(k, n)| format!("{n} {k}"))
                .collect();
            s.push_str(&format!("; left out {} (not cuttable)", parts.join(", ")));
            if self.skipped.contains_key(SkipKind::Text.label()) {
                s.push_str("; outline text with `textoutline <sel>` to cut it");
            }
        }
        if self.fallback_spans > 0 {
            s.push_str(&format!(
                "; {} curve span(s) fell back to chords",
                self.fallback_spans
            ));
        }
        if self.nonplanar > 0 {
            s.push_str(&format!(
                "; {} non-planar curve(s) projected to XY",
                self.nonplanar
            ));
        }
        if self.open > 0 || self.duplicates > 0 || !self.skipped.is_empty() {
            s.push_str(" — run `cutcheck` before ordering");
        }
        s
    }
}

/// Fixed 6-decimal output with trailing zeros trimmed and no `-0`.
fn fmt_num(v: f64) -> String {
    let v = if v.abs() < 5e-7 { 0.0 } else { v };
    let mut s = format!("{v:.6}");
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.push('0');
    }
    s
}

struct W(String);

impl W {
    fn tag(&mut self, code: i32, value: &str) {
        self.0.push_str(&format!("{code}\n{value}\n"));
    }
    fn num(&mut self, code: i32, v: f64) {
        self.tag(code, &fmt_num(v));
    }
    fn xy(&mut self, p: DVec2) {
        self.num(10, p.x);
        self.num(20, p.y);
        self.num(30, 0.0);
    }
}

/// Write the contour set as DXF R12 (AC1009) in file units.
pub fn write_dxf(g: &CutGeometry) -> (String, CutSummary) {
    let mut sum = CutSummary {
        duplicates: g.duplicates.len(),
        fallback_spans: g.fallback_spans,
        nonplanar: g.nonplanar,
        ..Default::default()
    };
    for (_, k, _) in &g.skipped {
        *sum.skipped.entry(k.label()).or_default() += 1;
    }
    let (mut lo, mut hi) = (DVec2::splat(f64::INFINITY), DVec2::splat(f64::NEG_INFINITY));
    for c in &g.contours {
        let (a, b) = c.bbox();
        lo = lo.min(a);
        hi = hi.max(b);
    }
    if g.contours.is_empty() {
        (lo, hi) = (DVec2::ZERO, DVec2::ZERO);
    }
    sum.extent = (hi.x - lo.x, hi.y - lo.y);
    let layer_of = |src: usize| crate::dxf::dxf_layer(&g.sources[src].1);
    let layers: std::collections::BTreeSet<String> = g
        .contours
        .iter()
        .filter_map(|c| c.srcs().first().map(|&s| layer_of(s)))
        .collect();

    let mut w = W(String::new());
    w.tag(0, "SECTION");
    w.tag(2, "HEADER");
    w.tag(9, "$ACADVER");
    w.tag(1, "AC1009");
    w.tag(9, "$INSUNITS");
    w.tag(70, &g.units.insunits().to_string());
    w.tag(9, "$MEASUREMENT");
    w.tag(70, if g.units == CutUnit::Mm { "1" } else { "0" });
    w.tag(9, "$EXTMIN");
    w.xy(lo);
    w.tag(9, "$EXTMAX");
    w.xy(hi);
    w.tag(0, "ENDSEC");

    w.tag(0, "SECTION");
    w.tag(2, "TABLES");
    w.tag(0, "TABLE");
    w.tag(2, "LTYPE");
    w.tag(70, "1");
    w.tag(0, "LTYPE");
    w.tag(2, "CONTINUOUS");
    w.tag(70, "0");
    w.tag(3, "Solid line");
    w.tag(72, "65");
    w.tag(73, "0");
    w.num(40, 0.0);
    w.tag(0, "ENDTAB");
    w.tag(0, "TABLE");
    w.tag(2, "LAYER");
    w.tag(70, &layers.len().to_string());
    for name in &layers {
        w.tag(0, "LAYER");
        w.tag(2, name);
        w.tag(70, "0");
        w.tag(62, "7");
        w.tag(6, "CONTINUOUS");
    }
    w.tag(0, "ENDTAB");
    w.tag(0, "ENDSEC");

    w.tag(0, "SECTION");
    w.tag(2, "ENTITIES");
    for c in &g.contours {
        let layer = c
            .srcs()
            .first()
            .map(|&s| layer_of(s))
            .unwrap_or_else(|| "0".into());
        match c {
            Contour::Circle { c, r, .. } => {
                sum.closed += 1;
                sum.circles += 1;
                w.tag(0, "CIRCLE");
                w.tag(8, &layer);
                w.xy(*c);
                w.num(40, *r);
            }
            Contour::Path { segs, closed, .. } => {
                if *closed {
                    sum.closed += 1;
                } else {
                    sum.open += 1;
                }
                sum.arcs += segs
                    .iter()
                    .filter(|s| matches!(s, Seg2::Arc { .. }))
                    .count();
                match (segs.as_slice(), closed) {
                    ([Seg2::Line { a, b }], false) => {
                        w.tag(0, "LINE");
                        w.tag(8, &layer);
                        w.xy(*a);
                        w.num(11, b.x);
                        w.num(21, b.y);
                        w.num(31, 0.0);
                    }
                    ([arc @ Seg2::Arc { c, r, .. }], false) => {
                        // DXF arcs run CCW from 50 to 51 (degrees).
                        let ccw = if arc.bulge() >= 0.0 {
                            *arc
                        } else {
                            arc.reversed()
                        };
                        let (s, e) = (ccw.start() - *c, ccw.end() - *c);
                        w.tag(0, "ARC");
                        w.tag(8, &layer);
                        w.xy(*c);
                        w.num(40, *r);
                        w.num(50, s.to_angle().to_degrees().rem_euclid(360.0));
                        w.num(51, e.to_angle().to_degrees().rem_euclid(360.0));
                    }
                    _ => {
                        // 2D polyline; bulge (42) keeps arcs exact inside it.
                        w.tag(0, "POLYLINE");
                        w.tag(8, &layer);
                        w.tag(66, "1");
                        w.xy(DVec2::ZERO);
                        w.tag(70, if *closed { "1" } else { "0" });
                        let mut vertex = |p: DVec2, bulge: f64| {
                            w.tag(0, "VERTEX");
                            w.tag(8, &layer);
                            w.xy(p);
                            if bulge != 0.0 {
                                w.num(42, bulge);
                            }
                        };
                        for s in segs {
                            vertex(s.start(), s.bulge());
                        }
                        if !*closed && let Some(last) = segs.last() {
                            vertex(last.end(), 0.0);
                        }
                        w.tag(0, "SEQEND");
                        w.tag(8, &layer);
                    }
                }
            }
        }
    }
    w.tag(0, "ENDSEC");
    w.tag(0, "EOF");
    (w.0, sum)
}

/// Scope + build + write: the whole cut export for already-resolved ids.
pub fn export(
    doc: &Document,
    opts: &CutOpts,
    ids: Option<&[ObjectId]>,
) -> Result<(String, CutSummary), String> {
    let objs = scope(doc, &opts.layers, ids);
    let g = build(doc, &objs, opts.units);
    if g.contours.is_empty() {
        let why = if g.skipped.is_empty() {
            "no curves in scope".to_string()
        } else {
            format!(
                "no curves in scope ({} non-cuttable object(s) left out)",
                g.skipped.len()
            )
        };
        return Err(format!(
            "cut export: {why}; draw closed outlines or pick layers=/sel"
        ));
    }
    Ok(write_dxf(&g))
}

/// Parse the cut-profile options after `export <path>`: `profile=cut`,
/// `units=mm|in`, `layers=a,b`, and an optional selector (`sel`, `all`,
/// `name:<n>`, …). Returns `None` when no option is present (plain export).
pub fn parse_export_opts(args: &[&str]) -> Result<Option<CutOpts>, String> {
    if args.is_empty() {
        return Ok(None);
    }
    let mut profile = None;
    let mut opts = CutOpts::default();
    let mut rest: Vec<&str> = Vec::new();
    for a in args {
        match a.split_once('=') {
            Some(("profile", v)) => profile = Some(v.to_ascii_lowercase()),
            Some(("units", v)) => {
                opts.units = CutUnit::parse(v)
                    .ok_or_else(|| format!("units={v}: the cut profile writes only mm or in"))?
            }
            Some(("layers" | "layer", v)) => {
                opts.layers = v
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            }
            Some((k, _)) => return Err(format!("unknown export option '{k}='")),
            None => rest.push(a),
        }
    }
    match profile.as_deref() {
        Some("cut") => {}
        Some(p) => return Err(format!("unknown export profile '{p}' (only 'cut')")),
        None => return Err("export options need profile=cut".to_string()),
    }
    opts.targets = parse_selector(&rest)?;
    Ok(Some(opts))
}

/// A length in `units`, optionally suffixed `mm` or `in` (`3`, `3mm`,
/// `0.125in`); a suffix in the other unit is converted.
pub fn parse_len(v: &str, units: CutUnit) -> Result<f64, String> {
    let t = v.trim().to_ascii_lowercase();
    let (num, from) = if let Some(n) = t.strip_suffix("mm") {
        (n, CutUnit::Mm)
    } else if let Some(n) = t.strip_suffix("in").or_else(|| t.strip_suffix('"')) {
        (n, CutUnit::In)
    } else {
        (t.as_str(), units)
    };
    let x: f64 = num
        .trim()
        .parse()
        .map_err(|_| format!("'{v}' is not a length (e.g. 3, 3mm, 0.125in)"))?;
    if !(x.is_finite() && x > 0.0) {
        return Err(format!("'{v}' must be a positive length"));
    }
    Ok(x / from.per_meter() * units.per_meter())
}

/// Optional selector tokens (`sel`, `all`, `last [n]`, `name:<n>`, `id:<n>`).
pub(crate) fn parse_selector(rest: &[&str]) -> Result<Option<Selector>, String> {
    Ok(match rest {
        [] => None,
        ["sel" | "selected"] => Some(Selector::Selected),
        ["all"] => Some(Selector::All),
        ["last"] => Some(Selector::Last { n: 1 }),
        ["last", n] => Some(Selector::Last {
            n: n.parse().map_err(|_| format!("bad count '{n}'"))?,
        }),
        [tok] => {
            let name = tok
                .strip_prefix("name:")
                .or_else(|| tok.strip_prefix("id:"))
                .unwrap_or(tok.trim_start_matches('#'));
            if name.is_empty() {
                return Err(format!("bad selector '{tok}'"));
            }
            Some(Selector::Named {
                name: name.to_string(),
            })
        }
        _ => return Err(format!("unexpected arguments: {}", rest.join(" "))),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{Session, parse};

    /// Minimal DXF reader for round-trip tests: header vars and entities as
    /// `(kind, layer, fields)`; POLYLINE vertices are folded into their parent.
    pub(crate) type Fields = Vec<(i32, String)>;
    /// `(kind, fields, polyline vertices)`.
    pub(crate) type Entity = (String, Fields, Vec<Fields>);

    pub(crate) struct Parsed {
        pub header: BTreeMap<String, Vec<(i32, String)>>,
        pub entities: Vec<Entity>,
    }

    pub(crate) fn parse_dxf(text: &str) -> Parsed {
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.len().is_multiple_of(2), "pairs");
        let pairs: Vec<(i32, String)> = lines
            .chunks(2)
            .map(|c| (c[0].trim().parse().unwrap(), c[1].to_string()))
            .collect();
        let mut header = BTreeMap::new();
        let mut entities: Vec<Entity> = Vec::new();
        let mut section = String::new();
        let mut i = 0;
        let mut var: Option<String> = None;
        while i < pairs.len() {
            let (code, v) = &pairs[i];
            if *code == 0 && v == "SECTION" {
                section = pairs[i + 1].1.clone();
                i += 2;
                continue;
            }
            if *code == 0 && v == "ENDSEC" {
                section.clear();
            }
            match section.as_str() {
                "HEADER" => {
                    if *code == 9 {
                        var = Some(v.clone());
                        header.insert(v.clone(), Vec::new());
                    } else if *code != 0
                        && let Some(k) = &var
                    {
                        header.get_mut(k).unwrap().push((*code, v.clone()));
                    }
                }
                "ENTITIES" if *code == 0 => match v.as_str() {
                    "VERTEX" => entities.last_mut().unwrap().2.push(Vec::new()),
                    "SEQEND" => {}
                    _ => entities.push((v.clone(), Vec::new(), Vec::new())),
                },
                "ENTITIES" if *code != 0 => {
                    let e = entities.last_mut().unwrap();
                    match e.2.last_mut() {
                        Some(vx) if e.0 == "POLYLINE" => vx.push((*code, v.clone())),
                        _ => e.1.push((*code, v.clone())),
                    }
                }
                _ => {}
            }
            i += 1;
        }
        Parsed { header, entities }
    }

    pub(crate) fn f(fields: &[(i32, String)], code: i32) -> f64 {
        fields
            .iter()
            .find(|(c, _)| *c == code)
            .map(|(_, v)| v.parse().unwrap())
            .unwrap_or(0.0)
    }

    /// Bounds over every entity point (circles/arcs by radius) — re-import
    /// measurement for the acceptance check.
    fn measure(p: &Parsed) -> (f64, f64) {
        let (mut lo, mut hi) = (DVec2::splat(f64::INFINITY), DVec2::splat(f64::NEG_INFINITY));
        let mut add = |q: DVec2| {
            lo = lo.min(q);
            hi = hi.max(q);
        };
        for (kind, fields, verts) in &p.entities {
            match kind.as_str() {
                "CIRCLE" => {
                    let c = DVec2::new(f(fields, 10), f(fields, 20));
                    add(c - DVec2::splat(f(fields, 40)));
                    add(c + DVec2::splat(f(fields, 40)));
                }
                "POLYLINE" => verts
                    .iter()
                    .for_each(|v| add(DVec2::new(f(v, 10), f(v, 20)))),
                _ => add(DVec2::new(f(fields, 10), f(fields, 20))),
            }
        }
        (hi.x - lo.x, hi.y - lo.y)
    }

    fn run(s: &mut Session, line: &str) -> String {
        s.run(parse(line).unwrap_or_else(|e| panic!("{line}: {e}")))
            .unwrap_or_else(|e| panic!("{line}: {e}"))
            .message
    }

    /// The acceptance plate: 50 × 30 mm, 5 mm hole, 2 mm text label.
    pub(crate) fn plate() -> Session {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 0.05 0.03");
        run(&mut s, "circle 0.01,0.015,0 0.0025");
        run(&mut s, "text 0.02,0.01,0 PART-1 0.002");
        s
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = crate::config_dir().unwrap();
        std::fs::create_dir_all(&d).unwrap();
        d.join(name)
    }

    #[test]
    fn acceptance_plate_exports_mm_with_circle_and_no_text() {
        let mut s = plate();
        let path = tmp("plate.dxf");
        let msg = run(
            &mut s,
            &format!("export {} profile=cut units=mm", path.display()),
        );
        assert!(msg.contains("$INSUNITS=4"), "{msg}");
        assert!(msg.contains("1 text"), "{msg}");
        let text = std::fs::read_to_string(&path).unwrap();
        let p = parse_dxf(&text);
        assert_eq!(p.header["$INSUNITS"], vec![(70, "4".to_string())]);
        let kinds: Vec<&str> = p.entities.iter().map(|e| e.0.as_str()).collect();
        assert_eq!(kinds, ["POLYLINE", "CIRCLE"], "{text}");
        let (w, h) = measure(&p);
        assert!(
            (w - 50.0).abs() < 1e-9 && (h - 30.0).abs() < 1e-9,
            "{w} x {h}"
        );
        let circle = &p.entities[1].1;
        assert_eq!(
            (f(circle, 10), f(circle, 20), f(circle, 40)),
            (10.0, 15.0, 2.5)
        );
        // Outline is one closed polyline of four vertices.
        assert_eq!(f(&p.entities[0].1, 70), 1.0);
        assert_eq!(p.entities[0].2.len(), 4);
        assert!(!text.contains("TEXT") && !text.contains("DIMENSION"));
    }

    /// #172 acceptance: the 2 mm "PART-1" label, outlined, exports as closed
    /// glyph polylines and no TEXT; cutcheck keeps checking the glyphs (their
    /// strokes are thinner than 3 mm stock allows); undo brings the text back.
    #[test]
    fn outlined_label_exports_closed_glyphs_and_undo_restores_text() {
        let mut s = plate();
        let path = tmp("plate-outlined.dxf");
        let export = format!("export {} profile=cut units=mm", path.display());
        assert!(run(&mut s, &export).contains("textoutline"));

        let out = run(&mut s, "textoutline all");
        let contours = s.doc.len() - 2;
        assert!(
            out.contains(&format!("{contours} closed contour(s)")),
            "{out}"
        );
        // P, A, R, T, -, 1: P/A/R have a counter each.
        assert_eq!(contours, 9, "{out}");
        let msg = run(&mut s, &export);
        assert!(
            msg.contains(" 0 open") && !msg.contains("left out"),
            "{msg}"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("TEXT"), "{text}");
        let p = parse_dxf(&text);
        let polys: Vec<_> = p.entities.iter().filter(|e| e.0 == "POLYLINE").collect();
        assert_eq!(polys.len(), 1 + contours);
        assert!(polys.iter().all(|e| f(&e.1, 70) == 1.0), "all closed");
        assert_eq!(p.entities.len(), polys.len() + 1, "plus the circle");

        let check = run(&mut s, "cutcheck thickness=3");
        for ok in ["open-contour", "crossing", "duplicate", "overlap", "text"] {
            assert!(check.contains(&format!("| {ok} | PASS")), "{ok}: {check}");
        }
        assert!(check.contains("| min-hole | FAIL"), "{check}");

        run(&mut s, "undo"); // cutcheck
        run(&mut s, "undo"); // textoutline
        assert_eq!(s.doc.len(), 3);
        assert!(s.doc.objects().any(|o| matches!(
            &o.geometry,
            Geometry::Annotation(Annotation::Text { text, .. }) if text == "PART-1"
        )));
        run(&mut s, "redo");
        assert_eq!(s.doc.len(), 2 + contours);
    }

    #[test]
    fn inches_scale_and_insunits() {
        let mut s = plate();
        let path = tmp("plate_in.dxf");
        run(
            &mut s,
            &format!("export {} profile=cut units=in", path.display()),
        );
        let p = parse_dxf(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(p.header["$INSUNITS"], vec![(70, "1".to_string())]);
        let (w, h) = measure(&p);
        assert!(
            (w - 50.0 / 25.4).abs() < 1e-5 && (h - 30.0 / 25.4).abs() < 1e-5,
            "{w} x {h}"
        );
    }

    #[test]
    fn units_never_default_to_meters() {
        assert!(parse("export /tmp/x.dxf profile=cut units=m").is_err());
        let opts = parse_export_opts(&["profile=cut"]).unwrap().unwrap();
        assert_eq!(opts.units, CutUnit::Mm);
        assert!(
            parse_export_opts(&["units=mm"]).is_err(),
            "options need profile=cut"
        );
        assert!(parse_export_opts(&["profile=laser"]).is_err());
    }

    #[test]
    fn deterministic_bytes() {
        let s = plate();
        let opts = CutOpts::default();
        let a = export(&s.doc, &opts, None).unwrap().0;
        let b = export(&plate().doc, &opts, None).unwrap().0;
        assert_eq!(a, b);
    }

    #[test]
    fn joins_lines_and_arcs_into_closed_bulge_polyline_and_drops_duplicates() {
        let mut s = Session::default();
        // Slot outline from loose pieces: two lines + two half arcs, plus an
        // exact duplicate of the bottom line drawn the other way.
        run(&mut s, "line 0,0,0 0.04,0,0");
        run(&mut s, "arc 0.04,0.005,0 0.005 -90 90");
        run(&mut s, "line 0.04,0.01,0 0,0.01,0");
        run(&mut s, "arc 0,0.005,0 0.005 90 270");
        run(&mut s, "line 0.04,0,0 0,0,0");
        let (text, sum) = export(&s.doc, &CutOpts::default(), None).unwrap();
        assert_eq!(
            (sum.closed, sum.open, sum.duplicates, sum.arcs),
            (1, 0, 1, 2),
            "{sum:?}"
        );
        let p = parse_dxf(&text);
        assert_eq!(p.entities.len(), 1);
        let verts = &p.entities[0].2;
        assert_eq!(verts.len(), 4);
        let bulges: Vec<f64> = verts.iter().map(|v| f(v, 42)).collect();
        assert_eq!(
            bulges
                .iter()
                .filter(|b| (b.abs() - 1.0).abs() < 1e-6)
                .count(),
            2,
            "{bulges:?}"
        );
        let (w, h) = measure(&p);
        assert!(
            (w - 40.0).abs() < 1e-6 && (h - 10.0).abs() < 1e-6,
            "{w} x {h}"
        );
    }

    #[test]
    fn ellipse_and_spline_become_arcs_not_chords() {
        let mut s = Session::default();
        run(&mut s, "ellipse 0,0,0 0.02 0.01");
        let (text, sum) = export(&s.doc, &CutOpts::default(), None).unwrap();
        assert_eq!(sum.fallback_spans, 0);
        assert_eq!((sum.closed, sum.open), (1, 0));
        let p = parse_dxf(&text);
        let verts = &p.entities[0].2;
        assert!(
            verts.iter().all(|v| f(v, 42) != 0.0),
            "every ellipse span is an arc"
        );
        assert!(verts.len() < 120, "{} vertices", verts.len());
        let (w, h) = measure(&p);
        // Vertices lie on the ellipse; extremes within the fit tolerance.
        assert!(
            (w - 40.0).abs() < 0.02 && (h - 20.0).abs() < 0.02,
            "{w} x {h}"
        );
    }

    #[test]
    fn spline_fits_with_arcs_within_tolerance() {
        let mut s = Session::default();
        run(
            &mut s,
            "curve 0,0,0 0.02,0.03,0 0.05,-0.01,0 0.08,0.02,0 degree 3",
        );
        let Geometry::Curve(c) = &s.doc.objects().next().unwrap().geometry else {
            panic!()
        };
        let truth: Vec<DVec2> = c
            .tessellate(1e-6)
            .iter()
            .map(|p| DVec2::new(p.x, p.y) * 1000.0)
            .collect();
        let g = build(&s.doc, &scope(&s.doc, &[], None), CutUnit::Mm);
        assert_eq!(g.fallback_spans, 0);
        let Contour::Path { segs, closed, .. } = &g.contours[0] else {
            panic!()
        };
        assert!(!closed);
        assert!(segs.iter().any(|s| matches!(s, Seg2::Arc { .. })));
        for p in truth {
            let d = segs
                .iter()
                .map(|s| s.distance(p))
                .fold(f64::INFINITY, f64::min);
            assert!(d <= 0.0105, "deviation {d} mm");
        }
    }

    #[test]
    fn open_arc_stays_arc_entity_and_layers_scope() {
        let mut s = Session::default();
        run(&mut s, "layer cut");
        run(&mut s, "arc 0,0,0 0.01 0 90");
        run(&mut s, "layer notes");
        run(&mut s, "line 0,0,0 0.01,0,0");
        let opts = CutOpts {
            layers: vec!["CUT".into()],
            ..Default::default()
        };
        let (text, sum) = export(&s.doc, &opts, None).unwrap();
        assert_eq!(sum.open, 1);
        let p = parse_dxf(&text);
        assert_eq!(p.entities.len(), 1);
        let (kind, fields, _) = &p.entities[0];
        assert_eq!(kind, "ARC");
        assert_eq!(
            (f(fields, 40), f(fields, 50), f(fields, 51)),
            (10.0, 0.0, 90.0)
        );
    }

    #[test]
    fn selection_scope_and_empty_scope_error() {
        let mut s = plate();
        let circle = s.doc.objects().nth(1).unwrap().id;
        let (text, _) = export(&s.doc, &CutOpts::default(), Some(&[circle])).unwrap();
        assert_eq!(parse_dxf(&text).entities.len(), 1);
        let label = s.doc.objects().nth(2).unwrap().id;
        assert!(export(&s.doc, &CutOpts::default(), Some(&[label])).is_err());
        // `sel` resolves the live selection through the export command.
        s.doc.selection.insert(circle);
        let path = tmp("sel.dxf");
        run(
            &mut s,
            &format!("export {} profile=cut sel", path.display()),
        );
        let p = parse_dxf(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(p.entities.len(), 1);
    }

    #[test]
    fn block_instance_curves_are_placed() {
        let mut s = Session::default();
        run(&mut s, "circle 0,0,0 0.002");
        run(&mut s, "block all hole");
        run(&mut s, "insert hole 0.01,0.02,0");
        let inst = s.doc.objects().last().unwrap().id;
        let (text, sum) = export(&s.doc, &CutOpts::default(), Some(&[inst])).unwrap();
        assert_eq!(sum.circles, 1, "{text}");
        let p = parse_dxf(&text);
        let c = &p.entities[0].1;
        assert_eq!((f(c, 10), f(c, 20), f(c, 40)), (10.0, 20.0, 2.0));
    }
}
