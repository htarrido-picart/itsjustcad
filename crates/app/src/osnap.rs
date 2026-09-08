// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Object snapping: click-to-draw sticks to significant points of existing
//! geometry (screen-space radius), falling back to the 10cm grid.
//!
//! Snap kinds (Rhino-parity subset), in priority order when several hit within
//! the pick radius:
//!   end > int(ersection) > mid > cen(ter) > qua(drant) > perp(endicular) >
//!   tan(gent) > nod(e) > vtx (vertex) > near(est)
//!
//! Endpoint/Midpoint/Center are the classic three. Intersection, Perpendicular,
//! Tangent, Quadrant, Nearest, Node and Vertex extend toward Rhino's set. Each
//! candidate generator is analytic and pure so the geometry can be unit-tested
//! without a GPU or a live pick.

use glam::DVec3;
use kernel_curve::Curve;
use itsjustcad_doc::{Document, Geometry};

/// Screen-space pick radius in logical pixels.
pub const SNAP_RADIUS_PX: f32 = 10.0;

/// Mesh-vertex snap cap: meshes with more vertices than this do not contribute
/// per-vertex snap candidates (they would swamp the list and hurt the pick).
/// Big massing solids / imported meshes are still pickable via other kinds.
pub const VERTEX_CAP: usize = 5_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SnapKind {
    End,
    /// Intersection of two curves.
    Intersection,
    Mid,
    Center,
    /// 0/90/180/270° point of a circle/arc.
    Quadrant,
    /// Foot of the perpendicular from the last placed point onto a curve.
    Perpendicular,
    /// Tangent point from the last placed point to a circle/arc.
    Tangent,
    /// A point object or block insertion point.
    Node,
    /// A mesh vertex.
    Vertex,
    /// Closest point on a curve to the cursor (catch-all, lowest priority).
    Nearest,
}

impl SnapKind {
    /// Short readout label (`end`, `int`, `mid`, `cen`, `qua`, `perp`, `tan`,
    /// `nod`, `vtx`, `near`).
    pub fn label(self) -> &'static str {
        match self {
            SnapKind::End => "end",
            SnapKind::Intersection => "int",
            SnapKind::Mid => "mid",
            SnapKind::Center => "cen",
            SnapKind::Quadrant => "qua",
            SnapKind::Perpendicular => "perp",
            SnapKind::Tangent => "tan",
            SnapKind::Node => "nod",
            SnapKind::Vertex => "vtx",
            SnapKind::Nearest => "near",
        }
    }

    /// Priority when multiple candidates land within the radius — lower value
    /// wins the tie-break in [`resolve`]. Distance is the primary sort; priority
    /// breaks exact ties and keeps a clear precedence (endpoint beats nearest).
    pub fn priority(self) -> u8 {
        match self {
            SnapKind::End => 0,
            SnapKind::Intersection => 1,
            SnapKind::Mid => 2,
            SnapKind::Center => 3,
            SnapKind::Quadrant => 4,
            SnapKind::Perpendicular => 5,
            SnapKind::Tangent => 6,
            SnapKind::Node => 7,
            SnapKind::Vertex => 8,
            SnapKind::Nearest => 9,
        }
    }

    /// All snap kinds, in priority order. Drives the popup checkbox list and the
    /// settings serde.
    pub const ALL: [SnapKind; 10] = [
        SnapKind::End,
        SnapKind::Intersection,
        SnapKind::Mid,
        SnapKind::Center,
        SnapKind::Quadrant,
        SnapKind::Perpendicular,
        SnapKind::Tangent,
        SnapKind::Node,
        SnapKind::Vertex,
        SnapKind::Nearest,
    ];

    /// Stable key used in `ui.json` persistence (never localized).
    pub fn key(self) -> &'static str {
        self.label()
    }

    /// Parse a persisted / verb-supplied key back into a kind.
    pub fn from_key(s: &str) -> Option<SnapKind> {
        SnapKind::ALL.into_iter().find(|k| k.key() == s)
    }
}

/// Per-snap enable state + master toggle. Persisted to `ui.json`. The default
/// set matches a sensible working baseline: master on, End/Mid/Center/
/// Intersection on, the rest off (they can be noisy / are opt-in).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapSettings {
    /// Master object-snap switch. When off, [`candidates_filtered`] yields
    /// nothing (osnap disabled entirely; only the grid fallback applies).
    pub master: bool,
    /// Grid snap fallback toggle (empty-space clicks round to the 10cm grid).
    pub grid: bool,
    /// Per-kind enable bits, indexed by [`SnapKind::priority`].
    enabled: [bool; 10],
}

impl Default for SnapSettings {
    fn default() -> Self {
        let mut enabled = [false; 10];
        for k in [
            SnapKind::End,
            SnapKind::Mid,
            SnapKind::Center,
            SnapKind::Intersection,
        ] {
            enabled[k.priority() as usize] = true;
        }
        Self { master: true, grid: true, enabled }
    }
}

impl SnapSettings {
    /// Is `kind` currently enabled (ignoring the master switch)?
    pub fn is_on(&self, kind: SnapKind) -> bool {
        self.enabled[kind.priority() as usize]
    }

    /// Would a candidate of `kind` be produced right now (master AND per-kind)?
    pub fn active(&self, kind: SnapKind) -> bool {
        self.master && self.is_on(kind)
    }

    /// Set a kind's enable bit.
    pub fn set(&mut self, kind: SnapKind, on: bool) {
        self.enabled[kind.priority() as usize] = on;
    }

    /// Toggle a kind's enable bit; returns the new state. Pure — the popup UI
    /// calls this so the click→state transition is unit-testable.
    pub fn toggle(&mut self, kind: SnapKind) -> bool {
        let n = !self.is_on(kind);
        self.set(kind, n);
        n
    }

    /// Serialize to a `ui.json` object: `{ master, grid, kinds: ["end","mid",…] }`.
    pub fn to_json(self) -> serde_json::Value {
        let kinds: Vec<&str> = SnapKind::ALL
            .into_iter()
            .filter(|k| self.is_on(*k))
            .map(|k| k.key())
            .collect();
        serde_json::json!({
            "master": self.master,
            "grid": self.grid,
            "kinds": kinds,
        })
    }

    /// Restore from a `ui.json` object. Missing/garbage fields fall back to the
    /// default (so an older ui.json or a partial object still loads cleanly).
    pub fn from_json(v: &serde_json::Value) -> SnapSettings {
        let def = SnapSettings::default();
        if !v.is_object() {
            return def;
        }
        let master = v["master"].as_bool().unwrap_or(def.master);
        let grid = v["grid"].as_bool().unwrap_or(def.grid);
        let mut enabled = def.enabled;
        if let Some(arr) = v["kinds"].as_array() {
            enabled = [false; 10];
            for item in arr {
                if let Some(k) = item.as_str().and_then(SnapKind::from_key) {
                    enabled[k.priority() as usize] = true;
                }
            }
        }
        SnapSettings { master, grid, enabled }
    }
}

/// Fall-back grid snap (10cm) for clicks in empty space.
pub fn grid_snap(p: DVec3) -> DVec3 {
    (p * 10.0).round() / 10.0
}

/// Collect all snap candidates from the whole document (no culling, no filter).
/// The live app uses [`candidates_filtered`]; this unfiltered form is the
/// reference used by tests. Uses default settings + no last-point context.
#[cfg_attr(not(test), allow(dead_code))]
pub fn candidates(doc: &Document) -> Vec<(DVec3, SnapKind)> {
    candidates_filtered(doc, &SnapSettings::default(), None, |_| true)
}

/// Screen-proximity culled candidate generation, honouring `settings`.
///
/// Only objects for which `keep(aabb)` returns true contribute (the app passes a
/// projected-AABB proximity predicate). `last` is the in-progress draw's last
/// placed point — required for Perpendicular/Tangent (dropped when `None`).
///
/// When `settings.master` is off, the returned list is empty (osnap disabled).
/// Each per-kind generator is skipped unless its bit is on.
pub fn candidates_filtered(
    doc: &Document,
    settings: &SnapSettings,
    last: Option<DVec3>,
    keep: impl Fn(kernel_mesh::Aabb) -> bool,
) -> Vec<(DVec3, SnapKind)> {
    let mut out = Vec::new();
    if !settings.master {
        return out; // master off ⇒ no object snapping at all
    }

    // First pass: per-object candidates + gather nearby curves for the pairwise
    // intersection pass.
    let mut curves: Vec<Curve> = Vec::new();
    for obj in doc.objects() {
        if !obj.visible || !doc.layer_visible(&obj.layer) {
            continue; // invisible geometry must not attract the cursor
        }
        if !keep(obj.geometry.aabb()) {
            continue; // object nowhere near the cursor — skip its points
        }
        match &obj.geometry {
            Geometry::Curve(c) => {
                curve_candidates(c, settings, last, &mut out);
                curves.push(c.clone());
            }
            Geometry::Mesh(m)
            | Geometry::Frame { mesh: m, .. }
            | Geometry::Area { mesh: m, .. } => {
                // Mesh vertices are the natural corners of massing solids and
                // structural members. Capped so a huge imported mesh does not
                // flood the candidate list.
                if settings.is_on(SnapKind::Vertex) && m.positions().len() <= VERTEX_CAP {
                    out.extend(m.positions().iter().map(|p| (*p, SnapKind::Vertex)));
                }
            }
            // Annotation anchors (dim points, text position, hatch boundary).
            Geometry::Annotation(a) => {
                if settings.is_on(SnapKind::Node) {
                    out.extend(a.points().into_iter().map(|p| (p, SnapKind::Node)));
                }
            }
            // Block instance: snap to insertion point.
            Geometry::Instance { position, .. } => {
                if settings.is_on(SnapKind::Node) {
                    out.push((*position, SnapKind::Node));
                }
            }
            // Point clouds / point objects: only when small (cap as before).
            Geometry::Points { positions } if positions.len() <= VERTEX_CAP => {
                if settings.is_on(SnapKind::Node) {
                    out.extend(positions.iter().map(|p| (*p, SnapKind::Node)));
                }
            }
            Geometry::Points { .. } => {}
        }
    }

    // Second pass: intersections between nearby curves (line×line / segment
    // crossings). O(k²) over the culled set — k is tiny after the proximity cull.
    if settings.is_on(SnapKind::Intersection) {
        for i in 0..curves.len() {
            for j in (i + 1)..curves.len() {
                curve_intersections(&curves[i], &curves[j], &mut out);
            }
        }
    }

    out
}

/// Per-curve significant points (end/mid/center/quadrant/perp/tan/nearest).
fn curve_candidates(
    c: &Curve,
    settings: &SnapSettings,
    last: Option<DVec3>,
    out: &mut Vec<(DVec3, SnapKind)>,
) {
    match c {
        Curve::Line { a, b } => {
            if settings.is_on(SnapKind::End) {
                out.push((*a, SnapKind::End));
                out.push((*b, SnapKind::End));
            }
            if settings.is_on(SnapKind::Mid) {
                out.push(((*a + *b) / 2.0, SnapKind::Mid));
            }
            if settings.is_on(SnapKind::Perpendicular)
                && let Some(p) = last
            {
                out.push((perp_foot_on_segment(p, *a, *b), SnapKind::Perpendicular));
            }
        }
        Curve::Polyline { points, closed } => {
            if settings.is_on(SnapKind::End) {
                out.extend(points.iter().map(|p| (*p, SnapKind::End)));
            }
            let segs = if *closed { points.len() } else { points.len().saturating_sub(1) };
            for i in 0..segs {
                let a = points[i];
                let b = points[(i + 1) % points.len()];
                if settings.is_on(SnapKind::Mid) {
                    out.push(((a + b) / 2.0, SnapKind::Mid));
                }
                if settings.is_on(SnapKind::Perpendicular)
                    && let Some(p) = last
                {
                    out.push((perp_foot_on_segment(p, a, b), SnapKind::Perpendicular));
                }
            }
        }
        Curve::Arc { center, radius, start, end } => {
            if settings.is_on(SnapKind::Center) {
                out.push((*center, SnapKind::Center));
            }
            if !c.is_closed() {
                if settings.is_on(SnapKind::End) {
                    for t in [*start, *end] {
                        out.push((
                            *center + DVec3::new(radius * t.cos(), radius * t.sin(), 0.0),
                            SnapKind::End,
                        ));
                    }
                }
                if settings.is_on(SnapKind::Mid) {
                    let tm = (*start + *end) / 2.0;
                    out.push((
                        *center + DVec3::new(radius * tm.cos(), radius * tm.sin(), 0.0),
                        SnapKind::Mid,
                    ));
                }
            }
            // Quadrants: 0/90/180/270° points (distinct SnapKind). For an arc,
            // only those quadrant angles that fall within the sweep.
            if settings.is_on(SnapKind::Quadrant) {
                for q in quadrant_points(*center, *radius, *start, *end, c.is_closed()) {
                    out.push((q, SnapKind::Quadrant));
                }
            }
            // Tangent from the last point to this circle/arc.
            if settings.is_on(SnapKind::Tangent)
                && let Some(p) = last
            {
                for t in tangent_points(p, *center, *radius) {
                    out.push((t, SnapKind::Tangent));
                }
            }
        }
        Curve::Ellipse { center, .. } => {
            if settings.is_on(SnapKind::Center) {
                out.push((*center, SnapKind::Center));
            }
        }
        Curve::Nurbs { control, .. } => {
            if settings.is_on(SnapKind::End)
                && let (Some(a), Some(b)) = (control.first(), control.last())
            {
                out.push((*a, SnapKind::End));
                out.push((*b, SnapKind::End));
            }
        }
    }

    // Nearest-point-on-curve (lowest priority catch-all). Uses the last point as
    // the query when drawing; otherwise there is no meaningful "cursor" here
    // (the screen-space resolve does the final proximity test), so we only add it
    // when a query point exists.
    if settings.is_on(SnapKind::Nearest)
        && let Some(p) = last
        && let Some(n) = nearest_point_on_curve(c, p)
    {
        out.push((n, SnapKind::Nearest));
    }
}

/// Foot of the perpendicular from `p` onto the infinite line through `a`,`b`,
/// clamped to the `[a,b]` segment.
pub fn perp_foot_on_segment(p: DVec3, a: DVec3, b: DVec3) -> DVec3 {
    let ab = b - a;
    let len2 = ab.length_squared();
    if len2 < 1e-18 {
        return a;
    }
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    a + ab * t
}

/// Tangent point(s) from external point `p` to a circle centred at `c` radius
/// `r` (XY plane). Returns 0 points when `p` is inside the circle, 1 when on it,
/// 2 when outside. The classic construction: the tangent points lie on the
/// circle of diameter `p—c`, i.e. at angle ±acos(r/d) off the p→c direction.
pub fn tangent_points(p: DVec3, c: DVec3, r: f64) -> Vec<DVec3> {
    let d = (c - p).length();
    if d < r - 1e-9 || r <= 0.0 {
        return Vec::new(); // inside: no tangent
    }
    if (d - r).abs() < 1e-9 {
        return vec![p]; // on the circle: the point itself is the tangent point
    }
    // Angle of p as seen from c.
    let base = (p.y - c.y).atan2(p.x - c.x);
    let a = (r / d).acos();
    [base + a, base - a]
        .into_iter()
        .map(|t| c + DVec3::new(r * t.cos(), r * t.sin(), 0.0))
        .collect()
}

/// The quadrant points (0/90/180/270°) of a circle/arc that lie on the drawn
/// portion. For a closed circle all four are returned.
pub fn quadrant_points(
    center: DVec3,
    radius: f64,
    start: f64,
    end: f64,
    closed: bool,
) -> Vec<DVec3> {
    use std::f64::consts::FRAC_PI_2;
    let mut out = Vec::new();
    for i in 0..4 {
        let t = FRAC_PI_2 * i as f64;
        if closed || angle_in_sweep(t, start, end) {
            out.push(center + DVec3::new(radius * t.cos(), radius * t.sin(), 0.0));
        }
    }
    out
}

/// Is angle `t` within the CCW sweep `[start, end]` (mod TAU)?
fn angle_in_sweep(t: f64, start: f64, end: f64) -> bool {
    use std::f64::consts::TAU;
    let norm = |x: f64| x.rem_euclid(TAU);
    let sweep = norm(end - start);
    let rel = norm(t - start);
    rel <= sweep + 1e-9
}

/// Closest point on a curve to `q`. Analytic for line/arc/circle/ellipse
/// (ellipse approximated by its bounding box axes), polyline per-segment; NURBS
/// approximated by its control polygon.
pub fn nearest_point_on_curve(c: &Curve, q: DVec3) -> Option<DVec3> {
    match c {
        Curve::Line { a, b } => Some(perp_foot_on_segment(q, *a, *b)),
        Curve::Polyline { points, closed } => {
            let n = points.len();
            if n < 2 {
                return points.first().copied();
            }
            let segs = if *closed { n } else { n - 1 };
            let mut best: Option<(f64, DVec3)> = None;
            for i in 0..segs {
                let f = perp_foot_on_segment(q, points[i], points[(i + 1) % n]);
                let d = f.distance_squared(q);
                if best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, f));
                }
            }
            best.map(|(_, p)| p)
        }
        Curve::Arc { center, radius, start, end } => {
            // Project onto the circle; clamp to the arc sweep if not closed.
            let v = q - *center;
            let ang = v.y.atan2(v.x);
            let closed = c.is_closed();
            let t = if closed || angle_in_sweep(ang, *start, *end) {
                ang
            } else if ((ang - start).rem_euclid(std::f64::consts::TAU))
                < ((end - ang).rem_euclid(std::f64::consts::TAU))
            {
                // outside the sweep — snap to the nearer endpoint angle
                *start
            } else {
                *end
            };
            Some(*center + DVec3::new(radius * t.cos(), radius * t.sin(), 0.0))
        }
        Curve::Ellipse { center, rx, ry } => {
            let v = q - *center;
            let ang = v.y.atan2(v.x);
            Some(*center + DVec3::new(rx * ang.cos(), ry * ang.sin(), 0.0))
        }
        Curve::Nurbs { control, .. } => {
            if control.len() < 2 {
                return control.first().copied();
            }
            let mut best: Option<(f64, DVec3)> = None;
            for seg in control.windows(2) {
                let f = perp_foot_on_segment(q, seg[0], seg[1]);
                let d = f.distance_squared(q);
                if best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, f));
                }
            }
            best.map(|(_, p)| p)
        }
    }
}

/// Push intersection points between two curves. Currently handles the analytic
/// cases that matter for drafting: line/line, line/polyline-segment and
/// polyline/polyline segment crossings (all reduced to segment×segment).
fn curve_intersections(a: &Curve, b: &Curve, out: &mut Vec<(DVec3, SnapKind)>) {
    let sa = curve_segments(a);
    let sb = curve_segments(b);
    for (p0, p1) in &sa {
        for (q0, q1) in &sb {
            if let Some(x) = segment_intersection(*p0, *p1, *q0, *q1) {
                out.push((x, SnapKind::Intersection));
            }
        }
    }
}

/// Flatten a curve into its straight segments (for intersection tests). Curved
/// primitives yield no segments here (arc/arc intersection is out of scope).
fn curve_segments(c: &Curve) -> Vec<(DVec3, DVec3)> {
    match c {
        Curve::Line { a, b } => vec![(*a, *b)],
        Curve::Polyline { points, closed } => {
            let n = points.len();
            if n < 2 {
                return Vec::new();
            }
            let segs = if *closed { n } else { n - 1 };
            (0..segs).map(|i| (points[i], points[(i + 1) % n])).collect()
        }
        _ => Vec::new(),
    }
}

/// Intersection of segments `p0—p1` and `q0—q1` in the XY plane. Returns the
/// crossing point when the segments properly cross (parameters within `[0,1]`),
/// `None` for parallel / non-crossing.
pub fn segment_intersection(p0: DVec3, p1: DVec3, q0: DVec3, q1: DVec3) -> Option<DVec3> {
    let r = p1 - p0;
    let s = q1 - q0;
    let denom = r.x * s.y - r.y * s.x;
    if denom.abs() < 1e-12 {
        return None; // parallel or degenerate
    }
    let qp = q0 - p0;
    let t = (qp.x * s.y - qp.y * s.x) / denom;
    let u = (qp.x * r.y - qp.y * r.x) / denom;
    if (0.0..=1.0).contains(&t) && (0.0..=1.0).contains(&u) {
        Some(p0 + r * t)
    } else {
        None
    }
}

/// Nearest enabled candidate within `radius_px` of the cursor, in screen space.
/// Depth does not matter — what is visually closest wins (Rhino behaviour).
/// Ties in screen distance break by [`SnapKind::priority`] (endpoint beats
/// nearest). Candidates whose kind is not enabled in `settings` are ignored.
pub fn resolve(
    candidates: &[(DVec3, SnapKind)],
    cursor: egui::Pos2,
    radius_px: f32,
    settings: &SnapSettings,
    project: impl Fn(DVec3) -> Option<egui::Pos2>,
) -> Option<(DVec3, SnapKind)> {
    let mut best: Option<(f32, u8, DVec3, SnapKind)> = None;
    for (p, kind) in candidates {
        if !settings.active(*kind) {
            continue;
        }
        let Some(screen) = project(*p) else { continue };
        let d = screen.distance(cursor);
        if d > radius_px {
            continue;
        }
        let prio = kind.priority();
        let better = match best {
            None => true,
            // Prefer smaller distance; on a near-tie prefer higher priority.
            Some((bd, bp, _, _)) => d < bd - 0.5 || ((d - bd).abs() <= 0.5 && prio < bp),
        };
        if better {
            best = Some((d, prio, *p, *kind));
        }
    }
    best.map(|(_, _, p, k)| (p, k))
}

#[cfg(test)]
mod tests {
    use super::*;
    use itsjustcad_doc::{ObjectId, SceneObject};

    fn doc_with(geometry: Geometry) -> Document {
        let mut doc = Document::default();
        doc.insert(SceneObject {
            visible: true,
            id: ObjectId::new(),
            name: None,
            layer: itsjustcad_doc::DEFAULT_LAYER.to_string(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry,
        });
        doc
    }

    fn doc_many(geoms: Vec<Geometry>) -> Document {
        let mut doc = Document::default();
        for g in geoms {
            doc.insert(SceneObject {
                visible: true,
                id: ObjectId::new(),
                name: None,
                layer: itsjustcad_doc::DEFAULT_LAYER.to_string(),
                color: None,
                material: None,
                lineweight_mm: None,
                geometry: g,
            });
        }
        doc
    }

    /// Identity-ish projection: world xy -> screen xy (z ignored).
    fn flat(p: DVec3) -> Option<egui::Pos2> {
        Some(egui::pos2(p.x as f32, p.y as f32))
    }

    fn all_on() -> SnapSettings {
        let mut s = SnapSettings::default();
        s.master = true;
        for k in SnapKind::ALL {
            s.set(k, true);
        }
        s
    }

    fn approx(a: DVec3, b: DVec3) -> bool {
        a.distance(b) < 1e-6
    }

    #[test]
    fn line_candidates_ends_and_mid() {
        let doc = doc_with(Geometry::Curve(Curve::Line {
            a: DVec3::ZERO,
            b: DVec3::new(10.0, 0.0, 0.0),
        }));
        let c = candidates(&doc);
        assert!(c.contains(&(DVec3::ZERO, SnapKind::End)));
        assert!(c.contains(&(DVec3::new(10.0, 0.0, 0.0), SnapKind::End)));
        assert!(c.contains(&(DVec3::new(5.0, 0.0, 0.0), SnapKind::Mid)));
    }

    #[test]
    fn closed_polyline_wraps_midpoints() {
        let doc = doc_with(Geometry::Curve(Curve::Polyline {
            points: vec![
                DVec3::ZERO,
                DVec3::new(4.0, 0.0, 0.0),
                DVec3::new(4.0, 4.0, 0.0),
                DVec3::new(0.0, 4.0, 0.0),
            ],
            closed: true,
        }));
        let c = candidates(&doc);
        assert!(c.contains(&(DVec3::new(0.0, 2.0, 0.0), SnapKind::Mid)));
        assert_eq!(c.iter().filter(|(_, k)| *k == SnapKind::Mid).count(), 4);
    }

    #[test]
    fn circle_center_and_quadrants() {
        // Quadrants are a distinct kind now; default set has them OFF, so ask
        // with an all-on settings to see them.
        let doc = doc_with(Geometry::Curve(Curve::Arc {
            center: DVec3::new(2.0, 3.0, 0.0),
            radius: 1.0,
            start: 0.0,
            end: std::f64::consts::TAU,
        }));
        let c = candidates_filtered(&doc, &all_on(), None, |_| true);
        assert!(c.contains(&(DVec3::new(2.0, 3.0, 0.0), SnapKind::Center)));
        // Four quadrants at (3,3),(2,4),(1,3),(2,2).
        let quads: Vec<DVec3> = c
            .iter()
            .filter(|(_, k)| *k == SnapKind::Quadrant)
            .map(|(p, _)| *p)
            .collect();
        assert_eq!(quads.len(), 4);
        assert!(quads.iter().any(|p| approx(*p, DVec3::new(3.0, 3.0, 0.0))));
        assert!(quads.iter().any(|p| approx(*p, DVec3::new(2.0, 4.0, 0.0))));
        assert!(quads.iter().any(|p| approx(*p, DVec3::new(1.0, 3.0, 0.0))));
        assert!(quads.iter().any(|p| approx(*p, DVec3::new(2.0, 2.0, 0.0))));
        // Closed circle: no ends.
        assert!(!c.iter().any(|(_, k)| *k == SnapKind::End));
    }

    #[test]
    fn mesh_vertices_snap_as_vertex_kind() {
        let doc = doc_with(Geometry::Mesh(kernel_mesh::make_box(
            DVec3::ZERO,
            DVec3::splat(2.0),
        )));
        // Vertex snap is off by default; enable it.
        let c = candidates_filtered(&doc, &all_on(), None, |_| true);
        let verts: Vec<DVec3> = c
            .iter()
            .filter(|(_, k)| *k == SnapKind::Vertex)
            .map(|(p, _)| *p)
            .collect();
        assert_eq!(verts.len(), 8);
        assert!(verts.iter().any(|p| approx(*p, DVec3::splat(2.0))));
        // With default settings (vertex off) the box contributes nothing.
        let def = candidates(&doc);
        assert!(!def.iter().any(|(_, k)| *k == SnapKind::Vertex));
    }

    // ---- new analytic kinds ----

    #[test]
    fn perp_foot_from_point_to_line() {
        // Foot of perpendicular from (0,5) onto the x-axis segment is (0,0).
        let f = perp_foot_on_segment(
            DVec3::new(0.0, 5.0, 0.0),
            DVec3::new(-3.0, 0.0, 0.0),
            DVec3::new(3.0, 0.0, 0.0),
        );
        assert!(approx(f, DVec3::ZERO));
        // From (2,5): foot at (2,0) (inside the segment).
        let f = perp_foot_on_segment(
            DVec3::new(2.0, 5.0, 0.0),
            DVec3::new(-3.0, 0.0, 0.0),
            DVec3::new(3.0, 0.0, 0.0),
        );
        assert!(approx(f, DVec3::new(2.0, 0.0, 0.0)));
        // Beyond the end clamps to the endpoint.
        let f = perp_foot_on_segment(
            DVec3::new(10.0, 5.0, 0.0),
            DVec3::new(-3.0, 0.0, 0.0),
            DVec3::new(3.0, 0.0, 0.0),
        );
        assert!(approx(f, DVec3::new(3.0, 0.0, 0.0)));
    }

    #[test]
    fn perp_snap_candidate_generated_with_last_point() {
        let doc = doc_with(Geometry::Curve(Curve::Line {
            a: DVec3::new(-3.0, 0.0, 0.0),
            b: DVec3::new(3.0, 0.0, 0.0),
        }));
        let last = Some(DVec3::new(2.0, 5.0, 0.0));
        let c = candidates_filtered(&doc, &all_on(), last, |_| true);
        assert!(c
            .iter()
            .any(|(p, k)| *k == SnapKind::Perpendicular && approx(*p, DVec3::new(2.0, 0.0, 0.0))));
        // Without a last point, no perp candidate.
        let c = candidates_filtered(&doc, &all_on(), None, |_| true);
        assert!(!c.iter().any(|(_, k)| *k == SnapKind::Perpendicular));
    }

    #[test]
    fn tangent_from_point_to_circle() {
        // Unit circle at origin, external point (2,0). Tangent length = sqrt(3),
        // tangent points at (0.5, ±sqrt(3)/2).
        let ts = tangent_points(DVec3::new(2.0, 0.0, 0.0), DVec3::ZERO, 1.0);
        assert_eq!(ts.len(), 2);
        let s = 3f64.sqrt() / 2.0;
        assert!(ts.iter().any(|p| approx(*p, DVec3::new(0.5, s, 0.0))));
        assert!(ts.iter().any(|p| approx(*p, DVec3::new(0.5, -s, 0.0))));
        // Inside the circle: no tangents.
        assert!(tangent_points(DVec3::new(0.2, 0.0, 0.0), DVec3::ZERO, 1.0).is_empty());
    }

    #[test]
    fn quadrant_points_of_circle() {
        let q = quadrant_points(DVec3::ZERO, 2.0, 0.0, std::f64::consts::TAU, true);
        assert_eq!(q.len(), 4);
        assert!(q.iter().any(|p| approx(*p, DVec3::new(2.0, 0.0, 0.0))));
        assert!(q.iter().any(|p| approx(*p, DVec3::new(0.0, 2.0, 0.0))));
        assert!(q.iter().any(|p| approx(*p, DVec3::new(-2.0, 0.0, 0.0))));
        assert!(q.iter().any(|p| approx(*p, DVec3::new(0.0, -2.0, 0.0))));
        // A quarter arc from 0 to 90° includes exactly the 0° and 90° quadrants.
        let q = quadrant_points(DVec3::ZERO, 2.0, 0.0, std::f64::consts::FRAC_PI_2, false);
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn line_line_intersection_point() {
        // Two crossing lines meeting at (2,2).
        let x = segment_intersection(
            DVec3::new(0.0, 0.0, 0.0),
            DVec3::new(4.0, 4.0, 0.0),
            DVec3::new(0.0, 4.0, 0.0),
            DVec3::new(4.0, 0.0, 0.0),
        )
        .unwrap();
        assert!(approx(x, DVec3::new(2.0, 2.0, 0.0)));
        // Parallel: no intersection.
        assert!(segment_intersection(
            DVec3::new(0.0, 0.0, 0.0),
            DVec3::new(4.0, 0.0, 0.0),
            DVec3::new(0.0, 1.0, 0.0),
            DVec3::new(4.0, 1.0, 0.0),
        )
        .is_none());
        // Would-cross only if extended (not within segments).
        assert!(segment_intersection(
            DVec3::new(0.0, 0.0, 0.0),
            DVec3::new(1.0, 1.0, 0.0),
            DVec3::new(0.0, 4.0, 0.0),
            DVec3::new(4.0, 0.0, 0.0),
        )
        .is_none());
    }

    #[test]
    fn intersection_snap_generated_between_two_lines() {
        let doc = doc_many(vec![
            Geometry::Curve(Curve::Line {
                a: DVec3::new(0.0, 0.0, 0.0),
                b: DVec3::new(4.0, 4.0, 0.0),
            }),
            Geometry::Curve(Curve::Line {
                a: DVec3::new(0.0, 4.0, 0.0),
                b: DVec3::new(4.0, 0.0, 0.0),
            }),
        ]);
        // Intersection is ON in the default set.
        let c = candidates(&doc);
        assert!(c
            .iter()
            .any(|(p, k)| *k == SnapKind::Intersection && approx(*p, DVec3::new(2.0, 2.0, 0.0))));
    }

    #[test]
    fn nearest_point_on_line() {
        let c = Curve::Line {
            a: DVec3::new(-5.0, 0.0, 0.0),
            b: DVec3::new(5.0, 0.0, 0.0),
        };
        let n = nearest_point_on_curve(&c, DVec3::new(1.0, 3.0, 0.0)).unwrap();
        assert!(approx(n, DVec3::new(1.0, 0.0, 0.0)));
    }

    // ---- settings + resolve ----

    #[test]
    fn master_off_yields_no_candidates() {
        let doc = doc_with(Geometry::Curve(Curve::Line {
            a: DVec3::ZERO,
            b: DVec3::new(10.0, 0.0, 0.0),
        }));
        let mut s = SnapSettings::default();
        s.master = false;
        let c = candidates_filtered(&doc, &s, None, |_| true);
        assert!(c.is_empty());
    }

    #[test]
    fn disabled_kind_is_filtered_out() {
        let doc = doc_with(Geometry::Curve(Curve::Line {
            a: DVec3::ZERO,
            b: DVec3::new(10.0, 0.0, 0.0),
        }));
        let mut s = SnapSettings::default();
        s.set(SnapKind::Mid, false);
        let c = candidates_filtered(&doc, &s, None, |_| true);
        assert!(!c.iter().any(|(_, k)| *k == SnapKind::Mid));
        assert!(c.iter().any(|(_, k)| *k == SnapKind::End));
    }

    #[test]
    fn resolve_respects_enabled_set_and_priority() {
        // Endpoint and midpoint candidates at the same screen point; endpoint
        // wins by priority.
        let cands = vec![
            (DVec3::new(100.0, 100.0, 0.0), SnapKind::Mid),
            (DVec3::new(100.0, 100.0, 0.0), SnapKind::End),
        ];
        let hit = resolve(&cands, egui::pos2(100.0, 100.0), 10.0, &all_on(), flat).unwrap();
        assert_eq!(hit.1, SnapKind::End);
        // Nearer candidate wins over priority when clearly closer.
        let cands = vec![
            (DVec3::new(100.0, 100.0, 0.0), SnapKind::End),
            (DVec3::new(104.0, 100.0, 0.0), SnapKind::Nearest),
        ];
        let hit = resolve(&cands, egui::pos2(103.0, 100.0), 10.0, &all_on(), flat).unwrap();
        assert_eq!(hit.1, SnapKind::Nearest);
        // Disabling End means a lone End candidate does not resolve.
        let mut s = all_on();
        s.set(SnapKind::End, false);
        let cands = vec![(DVec3::new(100.0, 100.0, 0.0), SnapKind::End)];
        assert!(resolve(&cands, egui::pos2(100.0, 100.0), 10.0, &s, flat).is_none());
    }

    #[test]
    fn settings_serde_round_trip() {
        let mut s = SnapSettings::default();
        s.master = true;
        s.grid = false;
        s.set(SnapKind::Tangent, true);
        s.set(SnapKind::Mid, false);
        let json = s.to_json();
        let back = SnapSettings::from_json(&json);
        assert_eq!(back, s);
        // A missing object restores defaults.
        assert_eq!(
            SnapSettings::from_json(&serde_json::Value::Null),
            SnapSettings::default()
        );
    }

    #[test]
    fn toggle_flips_state() {
        let mut s = SnapSettings::default();
        assert!(s.is_on(SnapKind::End));
        assert!(!s.toggle(SnapKind::End)); // off now
        assert!(!s.is_on(SnapKind::End));
        assert!(s.toggle(SnapKind::End)); // back on
        // A default-off kind toggles on.
        assert!(!s.is_on(SnapKind::Tangent));
        assert!(s.toggle(SnapKind::Tangent));
    }

    #[test]
    fn default_set_is_sensible() {
        let s = SnapSettings::default();
        assert!(s.master && s.grid);
        assert!(s.is_on(SnapKind::End));
        assert!(s.is_on(SnapKind::Mid));
        assert!(s.is_on(SnapKind::Center));
        assert!(s.is_on(SnapKind::Intersection));
        assert!(!s.is_on(SnapKind::Perpendicular));
        assert!(!s.is_on(SnapKind::Tangent));
        assert!(!s.is_on(SnapKind::Quadrant));
        assert!(!s.is_on(SnapKind::Nearest));
        assert!(!s.is_on(SnapKind::Node));
        assert!(!s.is_on(SnapKind::Vertex));
    }

    #[test]
    fn kind_key_round_trip() {
        for k in SnapKind::ALL {
            assert_eq!(SnapKind::from_key(k.key()), Some(k));
        }
        assert_eq!(SnapKind::from_key("bogus"), None);
    }

    #[test]
    fn grid_snap_rounds_to_10cm() {
        assert_eq!(
            grid_snap(DVec3::new(1.234, 5.678, 0.0)),
            DVec3::new(1.2, 5.7, 0.0)
        );
    }

    // ---- stress harness: 10k-object pick + osnap under a loose time bound ----

    fn grid_doc(n: usize) -> Document {
        let mut doc = Document::default();
        let side = (n as f64).sqrt().ceil() as usize;
        for i in 0..n {
            let (gx, gy) = ((i % side) as f64, (i / side) as f64);
            let corner = DVec3::new(gx * 3.0, gy * 3.0, 0.0);
            doc.insert(SceneObject {
                visible: true,
                id: ObjectId::new(),
                name: None,
                layer: itsjustcad_doc::DEFAULT_LAYER.to_string(),
                color: None,
                material: None,
                lineweight_mm: None,
                geometry: Geometry::Mesh(kernel_mesh::make_box(corner, corner + DVec3::ONE)),
            });
        }
        doc
    }

    #[test]
    fn stress_pick_and_osnap_10k_objects() {
        let n = 10_000;
        let doc = grid_doc(n);

        let boxes: Vec<kernel_mesh::Aabb> =
            doc.objects().map(|o| o.geometry.aabb()).collect();
        let t0 = std::time::Instant::now();
        let bvh = kernel_mesh::Bvh::build(&boxes);
        let build_ms = t0.elapsed().as_secs_f64() * 1e3;

        let origin = DVec3::new(15.0, 15.0, 100.0);
        let dir = DVec3::new(0.0, 0.0, -1.0);
        let t1 = std::time::Instant::now();
        let mut picks = 0usize;
        for _ in 0..1000 {
            picks += bvh.ray_candidates(origin, dir).len();
        }
        let pick_ms = t1.elapsed().as_secs_f64() * 1e3 / 1000.0;
        assert!(picks > 0, "ray should cross at least one box");

        let win = kernel_mesh::Aabb::from_points([
            DVec3::new(14.0, 14.0, -1.0),
            DVec3::new(16.0, 16.0, 2.0),
        ]);
        let t2 = std::time::Instant::now();
        let cands = candidates_filtered(&doc, &all_on(), None, |bb| {
            bb.min.x <= win.max.x
                && bb.max.x >= win.min.x
                && bb.min.y <= win.max.y
                && bb.max.y >= win.min.y
        });
        let osnap_ms = t2.elapsed().as_secs_f64() * 1e3;

        eprintln!(
            "stress {n} objs: bvh build {build_ms:.2} ms, pick {pick_ms:.4} ms/ray, \
             osnap cull {osnap_ms:.2} ms -> {} candidates",
            cands.len()
        );
        assert!(build_ms < 1000.0, "bvh build too slow: {build_ms} ms");
        assert!(pick_ms < 50.0, "pick too slow: {pick_ms} ms/ray");
        assert!(osnap_ms < 1000.0, "osnap cull too slow: {osnap_ms} ms");
        assert!(cands.len() < n, "cull should drop most objects, got {}", cands.len());
    }
}
