// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Smart Guides construction guides (Rhino habit, internally "SmartTrack"):
//! after the cursor DWELLS on an object snap the point is "acquired", and
//! construction guide lines are projected THROUGH each acquired point. Besides
//! the horizontal / vertical ortho guides, an ANGLE-TRACKING fan is emitted at
//! multiples of a base angle (default 45° → 0/45/90/135°), a DIRECTIONAL guide
//! is projected along the line connecting each pair of acquired points (extended
//! both ways), and PARALLELS of each established edge are projected through the
//! current point — matching Rhino. The app may also feed EXTRA guides
//! (perpendicular/tangent onto nearby curves, derived from osnap). The cursor
//! then snaps onto the nearest guide, or to the INTERSECTION of two non-parallel
//! guides, giving align-to-object precision without extra clicks.
//!
//! This module is the PURE geometry core: acquisition storage (dedup / cap /
//! FIFO), guide construction, and the cursor→guide projection + intersection
//! snap. All the timing (`Instant` dwell) and the egui rendering live in the
//! app layer; nothing here touches egui, a clock, or the document, so every
//! rule below is unit-tested. Work is done in the ground plane (world XY); the
//! primary drafting view is top-ortho where world XY maps linearly to screen,
//! so a world-space tolerance derived from the pixel snap radius is faithful.

use glam::DVec3;

/// A construction guide line: an origin it passes through plus a UNIT direction
/// (in the ground plane XY) it runs along. The app extends `origin ± dir * big`
/// across the viewport to draw it. Horizontal guides run along +X, vertical along
/// +Y, and directional guides along the connecting line of two acquired points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GuideLine {
    /// A point the line passes through (an acquired point, or the last-picked
    /// point for an align-to-last guide).
    pub origin: DVec3,
    /// Unit direction the line runs along, in the ground plane (z = 0).
    dir: DVec3,
}

impl GuideLine {
    /// Horizontal (constant Y) guide through `origin`: runs along world +X.
    /// Retained as a convenience constructor; the live guide set now builds H/V
    /// via the angle-tracking fan, so these are exercised only by tests.
    #[allow(dead_code)]
    pub fn axis_h(origin: DVec3) -> GuideLine {
        GuideLine { origin, dir: DVec3::X }
    }

    /// Vertical (constant X) guide through `origin`: runs along world +Y.
    #[allow(dead_code)]
    pub fn axis_v(origin: DVec3) -> GuideLine {
        GuideLine { origin, dir: DVec3::Y }
    }

    /// Directional guide through `origin` along `dir` (projected + normalized in
    /// the ground plane). Returns `None` for a near-zero direction.
    pub fn through(origin: DVec3, dir: DVec3) -> Option<GuideLine> {
        let flat = DVec3::new(dir.x, dir.y, 0.0);
        let len = flat.length();
        if len < 1e-9 {
            return None;
        }
        Some(GuideLine { origin, dir: flat / len })
    }

    /// Unit direction the guide runs along, in the ground plane.
    pub fn dir(self) -> DVec3 {
        self.dir
    }

    /// Perpendicular distance (in the ground plane) from `p` to this infinite
    /// line: |dir.x·(p.y−o.y) − dir.y·(p.x−o.x)| (exact for a unit `dir`).
    fn distance_xy(self, p: DVec3) -> f64 {
        (self.dir.x * (p.y - self.origin.y) - self.dir.y * (p.x - self.origin.x)).abs()
    }

    /// Project `p` orthogonally onto this line (ground plane): the closest point
    /// o + ((p−o)·dir) dir, keeping the cursor's z.
    fn project_xy(self, p: DVec3) -> DVec3 {
        let o = self.origin;
        let t = (p.x - o.x) * self.dir.x + (p.y - o.y) * self.dir.y;
        DVec3::new(o.x + t * self.dir.x, o.y + t * self.dir.y, p.z)
    }

    /// Are two guides near-parallel (directions collinear within `eps`)? Uses the
    /// 2D cross of unit directions.
    fn near_parallel(self, other: GuideLine, eps: f64) -> bool {
        (self.dir.x * other.dir.y - self.dir.y * other.dir.x).abs() <= eps
    }
}

/// Intersection of the two infinite ground-plane lines (o1,d1) and (o2,d2).
/// Parametric solve; returns `None` when the directions are (near-)parallel.
/// z is taken from `keep_z`.
fn line_intersection_xy(
    o1: DVec3,
    d1: DVec3,
    o2: DVec3,
    d2: DVec3,
    keep_z: f64,
) -> Option<DVec3> {
    let cross = d1.x * d2.y - d1.y * d2.x;
    if cross.abs() < 1e-12 {
        return None;
    }
    // o1 + t·d1 = o2 + u·d2  ⇒ solve for t via Cramer's rule.
    let dx = o2.x - o1.x;
    let dy = o2.y - o1.y;
    let t = (dx * d2.y - dy * d2.x) / cross;
    Some(DVec3::new(o1.x + t * d1.x, o1.y + t * d1.y, keep_z))
}

/// Acquired-point store: sticky osnap points the guides are built from. Capped
/// FIFO with a small dedup tolerance so re-hovering the same corner does not
/// fill the ring with duplicates.
#[derive(Clone, Debug, Default)]
pub struct Acquired {
    points: Vec<DVec3>,
}

/// Maximum number of sticky acquired points (Rhino tracks a handful; three is
/// plenty for H/V + intersection align without visual clutter).
pub const MAX_POINTS: usize = 3;

impl Acquired {
    pub fn new() -> Self {
        Self { points: Vec::new() }
    }

    /// Currently acquired points, oldest first.
    pub fn points(&self) -> &[DVec3] {
        &self.points
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// Drop every acquired point (Esc / tool end).
    pub fn clear(&mut self) {
        self.points.clear();
    }

    /// Acquire `p`. A point already within `tol` (ground-plane XY) of an
    /// existing one is ignored (dedup). Otherwise it is pushed; when the ring
    /// is full the OLDEST point is evicted (FIFO). Returns true when a new
    /// point was actually stored.
    pub fn acquire(&mut self, p: DVec3, tol: f64) -> bool {
        if self
            .points
            .iter()
            .any(|q| dist_xy(*q, p) <= tol)
        {
            return false;
        }
        if self.points.len() >= MAX_POINTS {
            self.points.remove(0);
        }
        self.points.push(p);
        true
    }
}

/// Result of a guide snap: where the cursor should land plus the guides to
/// draw.
#[derive(Clone, Debug, PartialEq)]
pub struct Snap {
    /// The snapped cursor position (ground plane).
    pub snapped: DVec3,
    /// The guide lines that participated, for rendering.
    pub active: Vec<GuideLine>,
}

/// Ground-plane (XY) distance between two points.
fn dist_xy(a: DVec3, b: DVec3) -> f64 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    (dx * dx + dy * dy).sqrt()
}

/// Two guide directions this close (2D cross of unit dirs) count as parallel:
/// they can't form a stable intersection, and near-duplicates are deduped.
const PARALLEL_EPS: f64 = 1e-6;

/// Base angle increment (degrees) for angle tracking. Besides H (0°) and V
/// (90°), tracking lines are emitted at every multiple of this step, matching
/// Rhino's default SmartTrack angle of 45° → guides at 0/45/90/135°.
pub const TRACK_ANGLE_STEP: f64 = 45.0;

/// Unit directions for the angle-tracking guide fan, one per increment of
/// [`TRACK_ANGLE_STEP`] over a half-turn (a line and its 180° twin coincide, so
/// 0..180° covers every distinct guide). With the 45° default this yields
/// 0/45/90/135° (i.e. H, the two diagonals, and V).
fn angle_dirs() -> Vec<DVec3> {
    let mut dirs = Vec::new();
    let step = TRACK_ANGLE_STEP.max(1.0); // guard against a zero/negative step
    let mut a: f64 = 0.0;
    while a < 180.0 - 1e-9 {
        let r = a.to_radians();
        dirs.push(DVec3::new(r.cos(), r.sin(), 0.0));
        a += step;
    }
    dirs
}

/// Try to snap `cursor` onto the Smart Guides built from `acquired`.
///
/// Guides considered:
///   * ANGLE-TRACKING lines THROUGH each acquired point at every multiple of
///     [`TRACK_ANGLE_STEP`] (default 45° → 0/45/90/135°, i.e. horizontal,
///     vertical, and both diagonals), matching Rhino's angle tracking;
///   * a DIRECTIONAL guide through each acquired point toward every OTHER
///     acquired point — the connecting-line direction, extended both ways — so
///     the cursor can draw along the extension of a line defined by two points;
///   * PARALLELS: when ≥2 points are acquired, a guide through `last` (the
///     current point) parallel to each established connecting direction, so the
///     cursor can align parallel to an existing edge;
///   * the angle-tracking fan through `last` (align-to-last), when a draw is in
///     progress, so the cursor can align its own segment to an acquired point's
///     row/column/diagonal;
///   * any EXTRA guides the app supplies (e.g. perpendicular/tangent directions
///     onto a nearby curve, derived from osnap) — treated exactly like the
///     built-in guides for projection and intersection.
///
/// Snapping rule: find the nearest guide within `tol` (perpendicular, ground
/// plane). Then find the nearest OTHER guide within `tol` that is NOT
/// near-parallel to it; if found, snap to their line–line INTERSECTION and draw
/// both. Otherwise snap onto the single nearest guide. Returns `None` when
/// nothing is within tolerance (the caller then keeps the plain ground/grid
/// point).
///
/// The caller must NOT call this when a direct osnap hit already won — a hit
/// takes precedence over any guide.
pub fn snap(
    acquired: &Acquired,
    cursor: DVec3,
    last: Option<DVec3>,
    tol: f64,
    extra: &[GuideLine],
) -> Option<Snap> {
    if acquired.is_empty() && extra.is_empty() {
        return None;
    }

    // Build the candidate guide set.
    let pts = acquired.points();
    let angle_dirs = angle_dirs();
    let mut guides: Vec<GuideLine> = Vec::new();
    // Established connecting directions (for the parallels pass through `last`).
    let mut conn_dirs: Vec<DVec3> = Vec::new();
    for (i, &p) in pts.iter().enumerate() {
        // Angle-tracking fan through each acquired point.
        for &d in &angle_dirs {
            if let Some(g) = GuideLine::through(p, d) {
                guides.push(g);
            }
        }
        // Directional guide toward every OTHER acquired point (connecting line).
        for (j, &q) in pts.iter().enumerate() {
            if i != j
                && let Some(g) = GuideLine::through(p, q - p)
            {
                guides.push(g);
                if i < j {
                    conn_dirs.push(g.dir());
                }
            }
        }
    }
    if let Some(l) = last {
        // Align-to-last: the full angle-tracking fan through the current point.
        for &d in &angle_dirs {
            if let Some(g) = GuideLine::through(l, d) {
                guides.push(g);
            }
        }
        // Parallels: through the current point, parallel to each established
        // connecting direction (align parallel to an existing edge).
        for &d in &conn_dirs {
            if let Some(g) = GuideLine::through(l, d) {
                guides.push(g);
            }
        }
    }
    // App-supplied extra guides (perp/tangent onto nearby curves, etc.).
    guides.extend_from_slice(extra);

    // Dedup guides that share an origin and a near-parallel direction.
    let mut deduped: Vec<GuideLine> = Vec::with_capacity(guides.len());
    for g in guides {
        if !deduped
            .iter()
            .any(|d| dist_xy(d.origin, g.origin) < 1e-9 && d.near_parallel(g, PARALLEL_EPS))
        {
            deduped.push(g);
        }
    }
    let guides = deduped;

    // Nearest guide within tolerance (index into `guides`).
    let nearest_within = |exclude: Option<usize>, forbid_parallel_to: Option<GuideLine>| {
        let mut best: Option<(usize, f64)> = None;
        for (i, g) in guides.iter().enumerate() {
            if Some(i) == exclude {
                continue;
            }
            if let Some(ref other) = forbid_parallel_to
                && g.near_parallel(*other, PARALLEL_EPS)
            {
                continue;
            }
            let d = g.distance_xy(cursor);
            if d <= tol && best.is_none_or(|(_, bd)| d < bd) {
                best = Some((i, d));
            }
        }
        best.map(|(i, _)| i)
    };

    let first = nearest_within(None, None)?;
    let g1 = guides[first];

    // Nearest OTHER, non-parallel guide within tolerance → intersection snap.
    if let Some(second) = nearest_within(Some(first), Some(g1)) {
        let g2 = guides[second];
        if let Some(x) = line_intersection_xy(g1.origin, g1.dir(), g2.origin, g2.dir(), cursor.z) {
            return Some(Snap { snapped: x, active: vec![g1, g2] });
        }
    }

    // Single-guide projection.
    Some(Snap { snapped: g1.project_xy(cursor), active: vec![g1] })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: f64, y: f64) -> DVec3 {
        DVec3::new(x, y, 0.0)
    }

    fn approx(a: DVec3, b: DVec3) -> bool {
        dist_xy(a, b) < 1e-9
    }

    /// Is `g` a horizontal (along +X) guide through `y`?
    fn is_h(g: &GuideLine, y: f64) -> bool {
        g.dir().y.abs() < 1e-9 && (g.origin.y - y).abs() < 1e-9
    }
    /// Is `g` a vertical (along +Y) guide through `x`?
    fn is_v(g: &GuideLine, x: f64) -> bool {
        g.dir().x.abs() < 1e-9 && (g.origin.x - x).abs() < 1e-9
    }

    #[test]
    fn horizontal_projection_locks_y() {
        // Acquired at (5, 5); cursor drifts to (12, 5.05) with tol 0.2.
        // The horizontal guide (constant Y=5) is within tol; snap Y back to 5,
        // keep X. Only one acquired point → no directional guide, single project.
        let mut acq = Acquired::new();
        acq.acquire(p(5.0, 5.0), 0.01);
        let s = snap(&acq, p(12.0, 5.05), None, 0.2, &[]).unwrap();
        assert!(approx(s.snapped, p(12.0, 5.0)));
        assert_eq!(s.active.len(), 1);
        assert!(is_h(&s.active[0], 5.0));
    }

    #[test]
    fn vertical_projection_locks_x() {
        let mut acq = Acquired::new();
        acq.acquire(p(5.0, 5.0), 0.01);
        // Cursor near the vertical (constant X=5) but far in Y.
        let s = snap(&acq, p(5.08, 20.0), None, 0.2, &[]).unwrap();
        assert!(approx(s.snapped, p(5.0, 20.0)));
        assert_eq!(s.active.len(), 1);
        assert!(is_v(&s.active[0], 5.0));
    }

    #[test]
    fn two_guide_intersection_snap() {
        // A at (10, 2) → vertical X=10. B at (3, 8) → horizontal Y=8.
        // Cursor near both → snap to (10, 8) intersection, two guides active.
        let mut acq = Acquired::new();
        acq.acquire(p(10.0, 2.0), 0.01);
        acq.acquire(p(3.0, 8.0), 0.01);
        let s = snap(&acq, p(10.05, 7.95), None, 0.2, &[]).unwrap();
        assert!(approx(s.snapped, p(10.0, 8.0)));
        assert_eq!(s.active.len(), 2);
        assert!(s.active.iter().any(|g| is_v(g, 10.0)));
        assert!(s.active.iter().any(|g| is_h(g, 8.0)));
    }

    #[test]
    fn diagonal_guide_projects_along_connecting_direction() {
        // Two acquired points define a 45° diagonal through the origin:
        // (0,0) and (2,2) → direction (1,1). A cursor near the EXTENSION of that
        // line (e.g. near (5,5)) snaps ONTO the diagonal, not to an ortho guide.
        let mut acq = Acquired::new();
        acq.acquire(p(0.0, 0.0), 0.01);
        acq.acquire(p(2.0, 2.0), 0.01);
        // Cursor at (5.05, 4.95): perpendicular distance to the y=x line is
        // |5.05-4.95|/√2 ≈ 0.0707 < tol; but 5 units from every ortho guide.
        let s = snap(&acq, p(5.05, 4.95), None, 0.2, &[]).unwrap();
        // Projects onto y=x at the midpoint of the cursor's coords → (5,5).
        assert!(approx(s.snapped, p(5.0, 5.0)), "got {:?}", s.snapped);
        // The active guide must be the diagonal (unit dir (1,1)/√2), single guide.
        assert_eq!(s.active.len(), 1);
        let d = s.active[0].dir();
        assert!((d.x - d.y).abs() < 1e-9 && d.x.abs() > 1e-3, "not diagonal: {d:?}");
    }

    #[test]
    fn tolerance_boundary_just_in_and_just_out() {
        let mut acq = Acquired::new();
        acq.acquire(p(0.0, 0.0), 0.01);
        // Horizontal guide Y=0. Cursor at Y = tol - eps → in; Y = tol + eps → out.
        let tol = 0.5;
        let just_in = snap(&acq, p(4.0, tol - 1e-6), None, tol, &[]);
        assert!(just_in.is_some(), "distance just under tol must snap");
        let just_out = snap(&acq, p(4.0, tol + 1e-6), None, tol, &[]);
        // Y is out, but X=0 vertical guide is 4.0 away → also out → None.
        assert!(just_out.is_none(), "distance just over tol must not snap");
    }

    #[test]
    fn tolerance_boundary_exact_is_in() {
        let mut acq = Acquired::new();
        acq.acquire(p(0.0, 0.0), 0.01);
        // Exactly at tol counts as in range (<=).
        let s = snap(&acq, p(9.0, 0.5), None, 0.5, &[]);
        assert!(s.is_some());
    }

    #[test]
    fn align_to_last_uses_last_point_guides() {
        // No cursor-near acquired guide in X, but last point at (7, 0) gives a
        // vertical X=7 guide the cursor aligns to.
        let mut acq = Acquired::new();
        acq.acquire(p(0.0, 30.0), 0.01); // horizontal Y=30, vertical X=0 (both far)
        let s = snap(&acq, p(7.02, 4.0), Some(p(7.0, 0.0)), 0.2, &[]).unwrap();
        assert!((s.snapped.x - 7.0).abs() < 1e-9);
    }

    #[test]
    fn dedup_within_tolerance() {
        let mut acq = Acquired::new();
        assert!(acq.acquire(p(1.0, 1.0), 0.1));
        // Within dedup tol → ignored.
        assert!(!acq.acquire(p(1.05, 1.0), 0.1));
        assert_eq!(acq.points().len(), 1);
        // Outside dedup tol → stored.
        assert!(acq.acquire(p(2.0, 2.0), 0.1));
        assert_eq!(acq.points().len(), 2);
    }

    #[test]
    fn cap_three_fifo_eviction() {
        let mut acq = Acquired::new();
        acq.acquire(p(1.0, 0.0), 0.01);
        acq.acquire(p(2.0, 0.0), 0.01);
        acq.acquire(p(3.0, 0.0), 0.01);
        assert_eq!(acq.points().len(), MAX_POINTS);
        // Fourth evicts the oldest (1,0) FIFO.
        acq.acquire(p(4.0, 0.0), 0.01);
        assert_eq!(acq.points().len(), MAX_POINTS);
        assert_eq!(acq.points()[0], p(2.0, 0.0));
        assert_eq!(acq.points()[2], p(4.0, 0.0));
    }

    #[test]
    fn no_guide_when_nothing_acquired() {
        // Direct-osnap case: the caller passes an empty acquired set (a hit
        // took precedence). No guide is produced.
        let acq = Acquired::new();
        assert!(snap(&acq, p(5.0, 5.0), Some(p(0.0, 0.0)), 1.0, &[]).is_none());
    }

    #[test]
    fn angle_track_diagonal_through_single_point() {
        // A single acquired point emits the 45° angle fan (0/45/90/135°), so a
        // cursor near the +45° diagonal through it snaps ONTO that diagonal even
        // with no second point defining a connecting direction.
        let mut acq = Acquired::new();
        acq.acquire(p(0.0, 0.0), 0.01);
        // Cursor at (5.05, 4.95): perp distance to y=x ≈ 0.07 < tol; far from H/V.
        let s = snap(&acq, p(5.05, 4.95), None, 0.2, &[]).unwrap();
        assert!(approx(s.snapped, p(5.0, 5.0)), "got {:?}", s.snapped);
        assert_eq!(s.active.len(), 1);
        let d = s.active[0].dir();
        assert!((d.x - d.y).abs() < 1e-9 && d.x.abs() > 1e-3, "not 45°: {d:?}");
    }

    #[test]
    fn angle_dirs_are_the_default_fan() {
        // Default 45° step over a half-turn → 0/45/90/135° (4 distinct guides).
        let dirs = angle_dirs();
        assert_eq!(dirs.len(), 4);
        assert!(dirs.iter().any(|d| d.y.abs() < 1e-9 && d.x > 0.0)); // H (0°)
        assert!(dirs.iter().any(|d| d.x.abs() < 1e-9 && d.y > 0.0)); // V (90°)
        assert!(dirs.iter().any(|d| (d.x - d.y).abs() < 1e-9 && d.x > 0.0)); // 45°
        assert!(dirs.iter().any(|d| (d.x + d.y).abs() < 1e-9 && d.y > 0.0)); // 135°
    }

    #[test]
    fn parallel_to_established_edge_through_last() {
        // Two acquired points (0,0)-(4,2) establish a direction (2,1)/√5. With
        // the current point `last` at (0,5), a guide parallel to that edge runs
        // through (0,5). A cursor near that parallel line snaps onto it.
        let mut acq = Acquired::new();
        acq.acquire(p(0.0, 0.0), 0.01);
        acq.acquire(p(4.0, 2.0), 0.01);
        let last = p(0.0, 5.0);
        // Point exactly on the parallel: (2,6) = (0,5) + (2,1). Nudge off slightly.
        let s = snap(&acq, p(2.0, 6.05), Some(last), 0.2, &[]).unwrap();
        // Snapped point lies on the line through (0,5) with dir (2,1)/√5.
        let g = GuideLine::through(last, p(4.0, 2.0) - p(0.0, 0.0)).unwrap();
        assert!(g.distance_xy(s.snapped) < 1e-9, "not on parallel: {:?}", s.snapped);
    }

    #[test]
    fn extra_guide_projection_and_intersection() {
        // No acquired points; a single app-supplied extra guide (e.g. a perp foot
        // direction onto a curve) still drives a projection snap.
        let acq = Acquired::new();
        let extra = GuideLine::through(p(0.0, 0.0), DVec3::new(1.0, 1.0, 0.0)).unwrap();
        let s = snap(&acq, p(5.05, 4.95), None, 0.2, &[extra]).unwrap();
        assert!(approx(s.snapped, p(5.0, 5.0)), "got {:?}", s.snapped);

        // With an acquired vertical guide (x=10) plus an extra horizontal guide
        // (y=8), the cursor near both snaps to their intersection (10,8).
        let mut acq2 = Acquired::new();
        acq2.acquire(p(10.0, 0.0), 0.01); // vertical x=10 via the angle fan
        let extra_h = GuideLine::axis_h(p(3.0, 8.0));
        let s2 = snap(&acq2, p(10.05, 7.95), None, 0.2, &[extra_h]).unwrap();
        assert!(approx(s2.snapped, p(10.0, 8.0)), "got {:?}", s2.snapped);
        assert_eq!(s2.active.len(), 2);
    }

    #[test]
    fn clear_empties_the_ring() {
        let mut acq = Acquired::new();
        acq.acquire(p(1.0, 1.0), 0.01);
        assert!(!acq.is_empty());
        acq.clear();
        assert!(acq.is_empty());
    }
}
