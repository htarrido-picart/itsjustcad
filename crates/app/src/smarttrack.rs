// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! SmartTrack construction guides (Rhino habit): after the cursor DWELLS on an
//! object snap the point is "acquired", and horizontal / vertical guide lines
//! are projected THROUGH each acquired point. The cursor then snaps onto the
//! nearest guide, or to the INTERSECTION of two guides, giving align-to-object
//! precision without extra clicks.
//!
//! This module is the PURE geometry core: acquisition storage (dedup / cap /
//! FIFO), guide construction, and the cursor→guide projection + intersection
//! snap. All the timing (`Instant` dwell) and the egui rendering live in the
//! app layer; nothing here touches egui, a clock, or the document, so every
//! rule below is unit-tested. Work is done in the ground plane (world XY); the
//! primary drafting view is top-ortho where world XY maps linearly to screen,
//! so a world-space tolerance derived from the pixel snap radius is faithful.

use glam::DVec3;

/// Which world axis a guide line runs along. A horizontal guide holds Y
/// constant (runs along X); a vertical guide holds X constant (runs along Y).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuideAxis {
    /// Constant Y, extends along world X.
    Horizontal,
    /// Constant X, extends along world Y.
    Vertical,
}

/// A construction guide line: an origin it passes through plus the axis it runs
/// along. The app extends `origin ± axis * big` across the viewport to draw it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GuideLine {
    /// A point the line passes through (an acquired point, or the last-picked
    /// point for an align-to-last guide).
    pub origin: DVec3,
    /// The direction the line runs along.
    pub axis: GuideAxis,
}

impl GuideLine {
    /// Unit direction the guide runs along, in the ground plane.
    pub fn dir(self) -> DVec3 {
        match self.axis {
            GuideAxis::Horizontal => DVec3::X,
            GuideAxis::Vertical => DVec3::Y,
        }
    }

    /// Perpendicular distance (in the ground plane) from `p` to this line.
    fn distance_xy(self, p: DVec3) -> f64 {
        match self.axis {
            GuideAxis::Horizontal => (p.y - self.origin.y).abs(),
            GuideAxis::Vertical => (p.x - self.origin.x).abs(),
        }
    }

    /// Project `p` orthogonally onto this line (ground plane). The free
    /// coordinate keeps the cursor's value; the locked coordinate takes the
    /// origin's.
    fn project_xy(self, p: DVec3) -> DVec3 {
        match self.axis {
            GuideAxis::Horizontal => DVec3::new(p.x, self.origin.y, p.z),
            GuideAxis::Vertical => DVec3::new(self.origin.x, p.y, p.z),
        }
    }
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

/// Try to snap `cursor` onto the SmartTrack guides built from `acquired`.
///
/// Guides considered:
///   * horizontal (constant Y) and vertical (constant X) THROUGH each acquired
///     point;
///   * the ortho guides (H and V) through `last` (align-to-last), when a draw
///     is in progress, so the cursor can align its own segment to an acquired
///     point's row/column.
///
/// Snapping rule: any guide within `tol` (perpendicular, ground-plane) is a
/// candidate. When two candidates cross (one horizontal, one vertical) and the
/// cursor is near BOTH, the cursor snaps to their INTERSECTION. Otherwise it
/// snaps onto the single nearest guide. Returns `None` when nothing is within
/// tolerance (the caller then keeps the plain ground/grid point).
///
/// The caller must NOT call this when a direct osnap hit already won — a hit
/// takes precedence over any guide.
pub fn snap(acquired: &Acquired, cursor: DVec3, last: Option<DVec3>, tol: f64) -> Option<Snap> {
    if acquired.is_empty() {
        return None;
    }

    // Build the candidate guide set: H + V through each acquired point, plus
    // H + V through the last-picked point (align-to-last).
    let mut guides: Vec<GuideLine> = Vec::with_capacity(acquired.points().len() * 2 + 2);
    for &p in acquired.points() {
        guides.push(GuideLine { origin: p, axis: GuideAxis::Horizontal });
        guides.push(GuideLine { origin: p, axis: GuideAxis::Vertical });
    }
    if let Some(l) = last {
        guides.push(GuideLine { origin: l, axis: GuideAxis::Horizontal });
        guides.push(GuideLine { origin: l, axis: GuideAxis::Vertical });
    }

    // Nearest horizontal and nearest vertical guide within tolerance.
    let nearest = |axis: GuideAxis| -> Option<GuideLine> {
        guides
            .iter()
            .copied()
            .filter(|g| g.axis == axis && g.distance_xy(cursor) <= tol)
            .min_by(|a, b| {
                a.distance_xy(cursor)
                    .partial_cmp(&b.distance_xy(cursor))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    };
    let h = nearest(GuideAxis::Horizontal);
    let v = nearest(GuideAxis::Vertical);

    match (h, v) {
        // Both axes in range: snap to their intersection (constant Y from H,
        // constant X from V), and draw both.
        (Some(h), Some(v)) => Some(Snap {
            snapped: DVec3::new(v.origin.x, h.origin.y, cursor.z),
            active: vec![h, v],
        }),
        (Some(h), None) => Some(Snap {
            snapped: h.project_xy(cursor),
            active: vec![h],
        }),
        (None, Some(v)) => Some(Snap {
            snapped: v.project_xy(cursor),
            active: vec![v],
        }),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: f64, y: f64) -> DVec3 {
        DVec3::new(x, y, 0.0)
    }

    #[test]
    fn horizontal_projection_locks_y() {
        // Acquired at (5, 5); cursor drifts to (12, 5.05) with tol 0.2.
        // The horizontal guide (constant Y=5) is within tol; snap Y back to 5,
        // keep X.
        let mut acq = Acquired::new();
        acq.acquire(p(5.0, 5.0), 0.01);
        let s = snap(&acq, p(12.0, 5.05), None, 0.2).unwrap();
        assert_eq!(s.snapped, p(12.0, 5.0));
        assert_eq!(s.active, vec![GuideLine { origin: p(5.0, 5.0), axis: GuideAxis::Horizontal }]);
    }

    #[test]
    fn vertical_projection_locks_x() {
        let mut acq = Acquired::new();
        acq.acquire(p(5.0, 5.0), 0.01);
        // Cursor near the vertical (constant X=5) but far in Y.
        let s = snap(&acq, p(5.08, 20.0), None, 0.2).unwrap();
        assert_eq!(s.snapped, p(5.0, 20.0));
        assert_eq!(s.active, vec![GuideLine { origin: p(5.0, 5.0), axis: GuideAxis::Vertical }]);
    }

    #[test]
    fn two_guide_intersection_snap() {
        // A at (10, 2) → vertical X=10. B at (3, 8) → horizontal Y=8.
        // Cursor near both → snap to (10, 8) intersection, both guides active.
        let mut acq = Acquired::new();
        acq.acquire(p(10.0, 2.0), 0.01);
        acq.acquire(p(3.0, 8.0), 0.01);
        let s = snap(&acq, p(10.05, 7.95), None, 0.2).unwrap();
        assert_eq!(s.snapped, p(10.0, 8.0));
        assert_eq!(s.active.len(), 2);
        assert!(s.active.iter().any(|g| g.axis == GuideAxis::Vertical && g.origin.x == 10.0));
        assert!(s.active.iter().any(|g| g.axis == GuideAxis::Horizontal && g.origin.y == 8.0));
    }

    #[test]
    fn tolerance_boundary_just_in_and_just_out() {
        let mut acq = Acquired::new();
        acq.acquire(p(0.0, 0.0), 0.01);
        // Horizontal guide Y=0. Cursor at Y = tol - eps → in; Y = tol + eps → out.
        let tol = 0.5;
        let just_in = snap(&acq, p(4.0, tol - 1e-6), None, tol);
        assert!(just_in.is_some(), "distance just under tol must snap");
        let just_out = snap(&acq, p(4.0, tol + 1e-6), None, tol);
        // Y is out, but X=0 vertical guide is 4.0 away → also out → None.
        assert!(just_out.is_none(), "distance just over tol must not snap");
    }

    #[test]
    fn tolerance_boundary_exact_is_in() {
        let mut acq = Acquired::new();
        acq.acquire(p(0.0, 0.0), 0.01);
        // Exactly at tol counts as in range (<=).
        let s = snap(&acq, p(9.0, 0.5), None, 0.5);
        assert!(s.is_some());
    }

    #[test]
    fn align_to_last_uses_last_point_guides() {
        // No cursor-near acquired guide in X, but last point at (7, 0) gives a
        // vertical X=7 guide the cursor aligns to.
        let mut acq = Acquired::new();
        acq.acquire(p(0.0, 30.0), 0.01); // horizontal Y=30, vertical X=0 (both far)
        let s = snap(&acq, p(7.02, 4.0), Some(p(7.0, 0.0)), 0.2).unwrap();
        assert_eq!(s.snapped.x, 7.0);
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
        assert!(snap(&acq, p(5.0, 5.0), Some(p(0.0, 0.0)), 1.0).is_none());
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
