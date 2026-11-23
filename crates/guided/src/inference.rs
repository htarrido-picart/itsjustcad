// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Coloured axis inference and arrow-key axis lock for point picks (#372,
//! SketchUp red/green/blue). Pure geometry shared by the desktop draw tools
//! and the FFI guided point steps: a rubber band from the last picked point
//! that runs within [`INFER_TOL_DEG`] of a world axis *on screen* snaps onto
//! that axis; a lock ([`AxisLock`]) forces it. Axes are the letters `x`, `y`,
//! `z` used by [`crate::axis_vector`].
//!
//! #373 adds the edge inferences: On Edge ([`on_edge`]), Extend Edge
//! ([`extend_edge`]), and parallel / perpendicular to the last hovered edge
//! ([`infer_direction`], locked with ↓ through [`EdgeLock`]).

use glam::{DVec3, Vec2};

use crate::axis_vector;

/// Screen-space angle (degrees) within which the rubber band infers an axis.
pub const INFER_TOL_DEG: f32 = 5.0;

/// Shortest rubber band (pixels) that infers an axis; shorter bands are noise.
pub const INFER_MIN_PX: f32 = 12.0;

/// An axis whose screen projection is shorter than this fraction of the
/// longest axis projection is foreshortened (e.g. Z in a plan view) and is
/// never inferred: its on-screen direction is meaningless.
const MIN_AXIS_FRACTION: f32 = 0.15;

/// The axes in inference order (ties go to the earlier one).
pub const AXES: [char; 3] = ['x', 'y', 'z'];

/// Which axis the rubber band `band` (screen pixels, last point → cursor)
/// follows: the axis whose screen direction `axis_px(a)` (projection of a
/// short world step along `a` from the last point; `None` when it is behind
/// the camera) is within `tol_deg` of the band, either way along it. The
/// closest one wins.
pub fn infer_axis(band: Vec2, axis_px: impl Fn(char) -> Option<Vec2>, tol_deg: f32) -> Option<char> {
    let dirs: Vec<(char, Vec2)> = AXES
        .iter()
        .filter_map(|&a| axis_px(a).filter(|d| d.is_finite()).map(|d| (a, d)))
        .collect();
    closest_within(band, &dirs, tol_deg)
}

/// The candidate whose screen direction runs within `tol_deg` of `band`
/// (either way), the closest winning and ties going to the earlier one.
/// Directions shorter than [`MIN_AXIS_FRACTION`] of the longest are
/// foreshortened and skipped; a band under [`INFER_MIN_PX`] infers nothing.
fn closest_within<T: Copy>(band: Vec2, dirs: &[(T, Vec2)], tol_deg: f32) -> Option<T> {
    let len = band.length();
    if len.is_nan() || len < INFER_MIN_PX {
        return None;
    }
    let longest = dirs.iter().map(|(_, d)| d.length()).fold(0.0, f32::max);
    if longest <= f32::EPSILON {
        return None;
    }
    let b = band.normalize();
    let cos_tol = tol_deg.to_radians().cos();
    let mut best: Option<(T, f32)> = None;
    for &(a, d) in dirs {
        if d.length() < longest * MIN_AXIS_FRACTION {
            continue;
        }
        let c = b.dot(d.normalize()).abs();
        if c >= cos_tol && best.is_none_or(|(_, bc)| c > bc) {
            best = Some((a, c));
        }
    }
    best.map(|(a, _)| a)
}

/// How a rubber band relates to the reference edge (#373, SketchUp magenta).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeRel {
    Parallel,
    Perpendicular,
}

/// A straight edge of the model: a curve segment or a solid's feature edge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Edge {
    pub a: DVec3,
    pub b: DVec3,
}

impl Edge {
    /// Unit direction `a → b`, `None` for a degenerate edge.
    pub fn dir(&self) -> Option<DVec3> {
        let d = self.b - self.a;
        let len = d.length();
        (len.is_finite() && len > 1e-12).then(|| d / len)
    }

    /// The same edge (either way round) within `tol`.
    pub fn same_as(&self, o: &Edge, tol: f64) -> bool {
        (self.a.distance(o.a) <= tol && self.b.distance(o.b) <= tol)
            || (self.a.distance(o.b) <= tol && self.b.distance(o.a) <= tol)
    }

    /// The direction a band runs to be `rel` to this edge: along it, or
    /// square to it within the plane of normal `plane_n` (the drawing plane;
    /// the ground's is +Z). `None` for a degenerate edge, or a perpendicular
    /// to an edge that runs along `plane_n` (any in-plane direction is).
    pub fn rel_dir(&self, rel: EdgeRel, plane_n: DVec3) -> Option<DVec3> {
        let e = self.dir()?;
        match rel {
            EdgeRel::Parallel => Some(e),
            EdgeRel::Perpendicular => plane_n.cross(e).try_normalize(),
        }
    }
}

/// What a band infers: a world axis (#372) or a relation to the reference
/// edge (#373) along the unit direction it runs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Inferred {
    Axis(char),
    Edge(EdgeRel, DVec3),
}

impl Inferred {
    /// The unit world direction of the inferred line.
    pub fn dir(self) -> Option<DVec3> {
        match self {
            Inferred::Axis(a) => axis_vector(a),
            Inferred::Edge(_, d) => Some(d),
        }
    }
}

/// [`infer_axis`] widened to the reference edge (#373): the axes and the
/// directions parallel / perpendicular to `edge` (in the plane of normal
/// `plane_n`) compete, the band's closest within `tol_deg` winning — an
/// axis on a tie, so an edge along red still reads "On Red Axis".
/// `dir_px(u)` projects a short world step along unit `u` from the anchor
/// (`None` behind the camera).
pub fn infer_direction(
    band: Vec2,
    dir_px: impl Fn(DVec3) -> Option<Vec2>,
    edge: Option<(Edge, DVec3)>,
    tol_deg: f32,
) -> Option<Inferred> {
    let mut cands: Vec<Inferred> = AXES.iter().map(|&a| Inferred::Axis(a)).collect();
    if let Some((e, n)) = edge {
        for rel in [EdgeRel::Parallel, EdgeRel::Perpendicular] {
            if let Some(d) = e.rel_dir(rel, n) {
                cands.push(Inferred::Edge(rel, d));
            }
        }
    }
    let dirs: Vec<(Inferred, Vec2)> = cands
        .into_iter()
        .filter_map(|c| Some((c, dir_px(c.dir()?).filter(|d| d.is_finite())?)))
        .collect();
    closest_within(band, &dirs, tol_deg)
}

/// The point of segment `a`–`b` closest to the ray `ray_o + t·ray_d`.
pub fn closest_on_segment_to_ray(a: DVec3, b: DVec3, ray_o: DVec3, ray_d: DVec3) -> DVec3 {
    let u = b - a;
    let w = a - ray_o;
    let (uu, ud, dd) = (u.dot(u), u.dot(ray_d), ray_d.dot(ray_d));
    let denom = uu * dd - ud * ud;
    if uu < 1e-18 || denom.abs() < 1e-12 * uu * dd {
        return a;
    }
    let s = ((ud * ray_d.dot(w) - dd * u.dot(w)) / denom).clamp(0.0, 1.0);
    a + u * s
}

/// Screen distance from `p` to segment `a`–`b`, and where along it (0..1).
fn screen_seg(p: Vec2, a: Vec2, b: Vec2) -> (f32, f32) {
    let ab = b - a;
    let l2 = ab.length_squared();
    let t = if l2 > 0.0 { ((p - a).dot(ab) / l2).clamp(0.0, 1.0) } else { 0.0 };
    ((a + ab * t).distance(p), t)
}

/// Edges this much (relative) deeper along the cursor ray than the surface
/// it hits are behind that surface: hidden, not inferred.
const HIDDEN_REL: f64 = 0.02;

/// On Edge (#373): the edge of `edges` passing within `radius_px` of the
/// cursor on screen (the nearest; on a tie the nearer the eye, so a top
/// view picks the top edge of a box), and the point on it under the cursor
/// — where the cursor ray passes closest, so perspective does not skew it.
/// `project` maps world to screen (`None` behind the camera; such edges are
/// skipped). `front` is the ray parameter of the surface the ray hits
/// first: edges behind it are hidden and skipped.
pub fn on_edge(
    edges: &[Edge],
    cursor: Vec2,
    radius_px: f32,
    project: impl Fn(DVec3) -> Option<Vec2>,
    ray: Option<(DVec3, DVec3)>,
    front: Option<f64>,
) -> Option<(usize, DVec3)> {
    let mut best: Option<(f32, f64, usize, DVec3)> = None;
    for (i, e) in edges.iter().enumerate() {
        let (Some(a), Some(b)) = (project(e.a), project(e.b)) else {
            continue;
        };
        let (d, t) = screen_seg(cursor, a, b);
        if d > radius_px {
            continue;
        }
        let (p, depth) = match ray {
            Some((o, dir)) => {
                let p = closest_on_segment_to_ray(e.a, e.b, o, dir);
                (p, (p - o).dot(dir) / dir.length_squared())
            }
            None => (e.a.lerp(e.b, t as f64), 0.0),
        };
        if front.is_some_and(|f| depth > f + HIDDEN_REL * f.abs().max(1.0)) {
            continue;
        }
        let better = best.is_none_or(|(bd, bdepth, ..)| d < bd - 0.5 || (d <= bd + 0.5 && depth < bdepth));
        if better {
            best = Some((d, depth, i, p));
        }
    }
    best.map(|(_, _, i, p)| (i, p))
}

/// Extend Edge (#373): with the cursor within `radius_px` of `edge`'s line
/// on screen but past its ends, the point of that line under the cursor
/// ray. `None` on the edge itself (that is [`on_edge`]) or off the line.
pub fn extend_edge(
    edge: Edge,
    cursor: Vec2,
    radius_px: f32,
    project: impl Fn(DVec3) -> Option<Vec2>,
    ray: (DVec3, DVec3),
) -> Option<DVec3> {
    let u = edge.dir()?;
    let (a, b) = (project(edge.a)?, project(edge.b)?);
    let ab = b - a;
    if ab.length_squared() < 1.0 {
        return None; // seen end-on: its line is a dot
    }
    let off = (cursor - a).perp_dot(ab.normalize()).abs();
    if off > radius_px {
        return None;
    }
    let p = closest_on_line_to_ray(edge.a, u, ray.0, ray.1)?;
    let s = (p - edge.a).dot(u);
    let len = edge.a.distance(edge.b);
    (s < -1e-9 || s > len + 1e-9).then_some(p)
}

/// The point of the line `origin + s·dir` closest to the ray `ray_o + t·ray_d`
/// — where the cursor ray "touches" an axis line, so a blue (Z) lock can
/// follow the cursor up a vertical line that never meets the ground pick.
/// `None` when the ray runs parallel to the line.
pub fn closest_on_line_to_ray(origin: DVec3, dir: DVec3, ray_o: DVec3, ray_d: DVec3) -> Option<DVec3> {
    let w = origin - ray_o;
    let (uu, ud, dd) = (dir.dot(dir), dir.dot(ray_d), ray_d.dot(ray_d));
    let denom = uu * dd - ud * ud;
    if uu < 1e-18 || denom.abs() < 1e-12 * uu * dd {
        return None;
    }
    let s = (ud * ray_d.dot(w) - dd * dir.dot(w)) / denom;
    let p = origin + dir * s;
    p.is_finite().then_some(p)
}

/// `p` moved onto the axis line through `anchor` (orthogonal projection) —
/// how an object snap is honoured under a lock: only its distance along the
/// axis counts.
pub fn project_onto_axis(anchor: DVec3, axis: char, p: DVec3) -> DVec3 {
    match axis_vector(axis) {
        Some(u) => anchor + u * (p - anchor).dot(u),
        None => p,
    }
}

/// The axis `p` lies on from `anchor` (both other coordinates equal within a
/// relative 1e-9), or `None` (including `p == anchor`). Colours the rubber
/// band whatever put the point there: inference, lock, ortho or a snap.
pub fn axis_of(anchor: DVec3, p: DVec3) -> Option<char> {
    let d = p - anchor;
    let len = d.length();
    if len.is_nan() || len <= 1e-9 {
        return None;
    }
    let eps = 1e-9 * len.max(1.0);
    let zero = |v: f64| v.abs() <= eps;
    match (zero(d.x), zero(d.y), zero(d.z)) {
        (false, true, true) => Some('x'),
        (true, false, true) => Some('y'),
        (true, true, false) => Some('z'),
        _ => None,
    }
}

/// An arrow-key axis lock, tied to the point it was set from: once the next
/// point is picked the anchor no longer matches and the lock lapses (SketchUp
/// releases it with the click).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AxisLock {
    lock: Option<(char, DVec3)>,
}

impl AxisLock {
    /// Lock `axis` from `anchor`; the same axis again unlocks. Returns the
    /// axis now locked (`None` = unlocked, or `axis` is not x/y/z).
    pub fn toggle(&mut self, axis: char, anchor: DVec3) -> Option<char> {
        let axis = axis.to_ascii_lowercase();
        if axis_vector(axis).is_none() {
            return self.get(Some(anchor));
        }
        self.lock = if self.get(Some(anchor)) == Some(axis) {
            None
        } else {
            Some((axis, anchor))
        };
        self.get(Some(anchor))
    }

    /// The locked axis while `anchor` is still the point it was set from.
    pub fn get(&self, anchor: Option<DVec3>) -> Option<char> {
        let (a, p) = self.lock?;
        (anchor? == p).then_some(a)
    }

    pub fn clear(&mut self) {
        self.lock = None;
    }
}

/// The cursor point under a lock on `axis` from `anchor`: an object snap
/// projected onto the axis, else the axis point nearest the cursor ray, else
/// the anchor itself (ray parallel to the axis, e.g. blue in a plan view).
pub fn constrain_to_axis(
    anchor: DVec3,
    axis: char,
    snap: Option<DVec3>,
    ray: Option<(DVec3, DVec3)>,
) -> DVec3 {
    match axis_vector(axis) {
        Some(u) => constrain_to_line(anchor, u, snap, ray),
        None => anchor,
    }
}

/// [`constrain_to_axis`] along any unit direction `u` (an edge lock, #373).
pub fn constrain_to_line(
    anchor: DVec3,
    u: DVec3,
    snap: Option<DVec3>,
    ray: Option<(DVec3, DVec3)>,
) -> DVec3 {
    if let Some(s) = snap {
        return anchor + u * (s - anchor).dot(u);
    }
    ray.and_then(|(o, d)| closest_on_line_to_ray(anchor, u, o, d))
        .unwrap_or(anchor)
}

/// The ↓ lock (#373): the band held parallel or perpendicular to the
/// reference edge, along unit `dir`, tied to its anchor like [`AxisLock`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EdgeLock {
    lock: Option<(EdgeRel, DVec3, DVec3)>,
}

impl EdgeLock {
    /// ↓: unlock when locked at `anchor`, else lock `want` (the relation the
    /// band shows, or parallel to the last hovered edge). Returns the lock
    /// now held.
    pub fn toggle(&mut self, want: Option<(EdgeRel, DVec3)>, anchor: DVec3) -> Option<(EdgeRel, DVec3)> {
        self.lock = match (self.get(Some(anchor)), want) {
            (Some(_), _) | (None, None) => None,
            (None, Some((rel, d))) => Some((rel, d, anchor)),
        };
        self.get(Some(anchor))
    }

    /// The lock while `anchor` is still the point it was set from.
    pub fn get(&self, anchor: Option<DVec3>) -> Option<(EdgeRel, DVec3)> {
        let (r, d, p) = self.lock?;
        (anchor? == p).then_some((r, d))
    }

    pub fn clear(&mut self) {
        self.lock = None;
    }
}

/// The cursor that gives a typed bare distance its direction: `cursor`, or —
/// when a locked axis has the cursor sitting on the anchor — one unit along
/// the axis, so "5" still goes 5 along it.
pub fn typed_direction_cursor(anchor: DVec3, axis: Option<char>, cursor: Option<DVec3>) -> Option<DVec3> {
    typed_direction_along(anchor, axis.and_then(axis_vector), cursor)
}

/// [`typed_direction_cursor`] for any locked unit direction `u` (#373).
pub fn typed_direction_along(anchor: DVec3, u: Option<DVec3>, cursor: Option<DVec3>) -> Option<DVec3> {
    match (u, cursor) {
        (Some(u), Some(c)) if c.distance(anchor) < 1e-9 => Some(anchor + u),
        (Some(u), None) => Some(anchor + u),
        _ => cursor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 3/4 view: X to the right-down, Y to the right-up, Z straight up.
    fn axis_px(a: char) -> Option<Vec2> {
        match a {
            'x' => Some(Vec2::new(87.0, 50.0)),
            'y' => Some(Vec2::new(87.0, -50.0)),
            'z' => Some(Vec2::new(0.0, -100.0)),
            _ => None,
        }
    }

    fn rot(v: Vec2, deg: f32) -> Vec2 {
        Vec2::from_angle(deg.to_radians()).rotate(v)
    }

    #[test]
    fn infers_each_axis_within_the_tolerance_either_way() {
        for a in AXES {
            let d = axis_px(a).unwrap().normalize() * 80.0;
            assert_eq!(infer_axis(rot(d, 4.0), axis_px, INFER_TOL_DEG), Some(a));
            assert_eq!(infer_axis(rot(-d, -4.0), axis_px, INFER_TOL_DEG), Some(a));
        }
    }

    #[test]
    fn no_inference_past_the_tolerance_or_on_a_short_band() {
        let x = axis_px('x').unwrap().normalize();
        assert_eq!(infer_axis(rot(x * 80.0, 6.0), axis_px, INFER_TOL_DEG), None);
        assert_eq!(infer_axis(x * (INFER_MIN_PX - 1.0), axis_px, INFER_TOL_DEG), None);
        assert_eq!(infer_axis(Vec2::ZERO, axis_px, INFER_TOL_DEG), None);
    }

    #[test]
    fn foreshortened_axis_is_not_inferred() {
        // Plan view: Z projects to a dot (well, 2 px) — moving up is not blue.
        let plan = |a: char| match a {
            'x' => Some(Vec2::new(100.0, 0.0)),
            'y' => Some(Vec2::new(0.0, -100.0)),
            _ => Some(Vec2::new(0.0, -2.0)),
        };
        assert_eq!(infer_axis(Vec2::new(0.0, -50.0), plan, INFER_TOL_DEG), Some('y'));
        let plan_no_y = |a: char| (a != 'y').then(|| plan(a).unwrap());
        assert_eq!(infer_axis(Vec2::new(0.0, -50.0), plan_no_y, INFER_TOL_DEG), None);
    }

    #[test]
    fn lock_toggles_and_lapses_with_the_next_point() {
        let p = DVec3::new(1.0, 2.0, 0.0);
        let mut l = AxisLock::default();
        assert_eq!(l.toggle('x', p), Some('x'));
        assert_eq!(l.get(Some(p)), Some('x'));
        assert_eq!(l.toggle('z', p), Some('z'), "another arrow switches axis");
        assert_eq!(l.toggle('z', p), None, "same arrow again unlocks");
        assert_eq!(l.toggle('y', p), Some('y'));
        assert_eq!(l.get(Some(DVec3::ZERO)), None, "a new pick releases it");
        assert_eq!(l.get(None), None);
        // A lock from the old anchor does not count as "same axis" at a new one.
        assert_eq!(l.toggle('y', DVec3::ZERO), Some('y'));
        l.clear();
        assert_eq!(l.get(Some(DVec3::ZERO)), None);
        assert_eq!(l.toggle('q', p), None, "not an axis");
    }

    #[test]
    fn z_lock_follows_the_cursor_ray_up_the_vertical_line() {
        let last = DVec3::new(2.0, 3.0, 0.0);
        // A ray from an elevated eye that passes 4 above the last point.
        let eye = DVec3::new(-10.0, -10.0, 10.0);
        let target = last + DVec3::new(0.0, 0.0, 4.0);
        let ray = (eye, (target - eye).normalize());
        let p = constrain_to_axis(last, 'z', None, Some(ray));
        assert!(p.distance(target) < 1e-9, "{p}");
        assert_eq!(axis_of(last, p), Some('z'));
        // Looking straight down the vertical line: stays on the anchor.
        let down = (last + DVec3::Z * 10.0, -DVec3::Z);
        assert_eq!(constrain_to_axis(last, 'z', None, Some(down)), last);
    }

    #[test]
    fn x_lock_projects_snaps_and_rays_onto_the_axis() {
        let last = DVec3::new(1.0, 1.0, 0.0);
        let snap = DVec3::new(4.0, 7.0, 2.0);
        assert_eq!(constrain_to_axis(last, 'x', Some(snap), None), DVec3::new(4.0, 1.0, 0.0));
        let ray = (DVec3::new(6.0, -5.0, 5.0), DVec3::new(0.0, 1.0, -1.0).normalize());
        let p = constrain_to_axis(last, 'x', None, Some(ray));
        assert!(p.distance(DVec3::new(6.0, 1.0, 0.0)) < 1e-9, "{p}");
    }

    #[test]
    fn axis_of_reads_the_band_direction() {
        let a = DVec3::new(1.0, 2.0, 3.0);
        assert_eq!(axis_of(a, a + DVec3::X * -4.0), Some('x'));
        assert_eq!(axis_of(a, a + DVec3::Y * 0.5), Some('y'));
        assert_eq!(axis_of(a, a + DVec3::Z * 9.0), Some('z'));
        assert_eq!(axis_of(a, a + DVec3::new(1.0, 1.0, 0.0)), None);
        assert_eq!(axis_of(a, a), None);
    }

    #[test]
    fn typed_distance_direction_falls_back_to_the_locked_axis() {
        let a = DVec3::new(1.0, 1.0, 0.0);
        assert_eq!(typed_direction_cursor(a, Some('z'), Some(a)), Some(a + DVec3::Z));
        assert_eq!(typed_direction_cursor(a, Some('y'), None), Some(a + DVec3::Y));
        let c = a + DVec3::X * 3.0;
        assert_eq!(typed_direction_cursor(a, Some('x'), Some(c)), Some(c));
        assert_eq!(typed_direction_cursor(a, None, None), None);
    }
    /// Screen = world XY ×10, y down (a top view); nothing is behind it.
    fn top(p: DVec3) -> Option<Vec2> {
        Some(Vec2::new(p.x as f32 * 10.0, -p.y as f32 * 10.0))
    }

    fn down_ray(x: f64, y: f64) -> (DVec3, DVec3) {
        (DVec3::new(x, y, 50.0), DVec3::NEG_Z)
    }

    #[test]
    fn on_edge_projects_the_cursor_onto_the_nearest_edge() {
        let edges = [
            Edge { a: DVec3::new(0.0, 0.0, 0.0), b: DVec3::new(10.0, 0.0, 0.0) },
            Edge { a: DVec3::new(0.0, 2.0, 0.0), b: DVec3::new(10.0, 2.0, 0.0) },
        ];
        // 0.4 below the first edge = 4 px: within 10 px, nearer than the other.
        let c = top(DVec3::new(3.0, 0.4, 0.0)).unwrap();
        let (i, p) = on_edge(&edges, c, 10.0, top, Some(down_ray(3.0, 0.4)), None).unwrap();
        assert_eq!(i, 0);
        assert!(p.distance(DVec3::new(3.0, 0.0, 0.0)) < 1e-9, "{p}");
        // Without a ray: screen interpolation, same point in a top view.
        let (_, q) = on_edge(&edges, c, 10.0, top, None, None).unwrap();
        assert!(q.distance(p) < 1e-6);
        // 1.1 off both = 11 px: outside the radius.
        assert!(on_edge(&edges, top(DVec3::new(3.0, -1.1, 0.0)).unwrap(), 10.0, top, None, None).is_none());
        // Past the end the point clamps to the end, still on the edge.
        let (_, e) = on_edge(&edges, top(DVec3::new(10.5, 0.0, 0.0)).unwrap(), 10.0, top, Some(down_ray(10.5, 0.0)), None).unwrap();
        assert!(e.distance(DVec3::new(10.0, 0.0, 0.0)) < 1e-9, "{e}");
        // A top view of a box: its top and bottom edges coincide on screen;
        // the top one (nearer the eye) wins, and with the top face hit at
        // depth 47 (z = 3) the bottom one is hidden.
        let stacked = [
            Edge { a: DVec3::ZERO, b: DVec3::new(4.0, 0.0, 0.0) },
            Edge { a: DVec3::new(0.0, 0.0, 3.0), b: DVec3::new(4.0, 0.0, 3.0) },
        ];
        let c = top(DVec3::new(1.0, 0.2, 0.0)).unwrap();
        let (i, p) = on_edge(&stacked, c, 10.0, top, Some(down_ray(1.0, 0.2)), None).unwrap();
        assert_eq!((i, p.z), (1, 3.0));
        let (i, _) = on_edge(&stacked[..1], c, 10.0, top, Some(down_ray(1.0, 0.2)), None).unwrap();
        assert_eq!(i, 0);
        assert!(on_edge(&stacked[..1], c, 10.0, top, Some(down_ray(1.0, 0.2)), Some(47.0)).is_none(), "hidden");
    }

    #[test]
    fn extend_edge_only_past_the_ends_and_near_the_line() {
        let e = Edge { a: DVec3::ZERO, b: DVec3::new(4.0, 4.0, 0.0) };
        let at = |x: f64, y: f64| extend_edge(e, top(DVec3::new(x, y, 0.0)).unwrap(), 10.0, top, down_ray(x, y));
        let p = at(6.05, 5.95).expect("past b, 0.07 off the line");
        assert!(p.distance(DVec3::new(6.0, 6.0, 0.0)) < 1e-9, "{p}");
        assert!(at(-2.0, -2.0).is_some(), "past a");
        assert!(at(2.0, 2.0).is_none(), "on the edge itself");
        assert!(at(6.0, 4.0).is_none(), "off the line");
    }

    #[test]
    fn edge_parallel_and_perpendicular_detection_thresholds() {
        // A 30° edge on the ground, seen from the top.
        let r = 30f64.to_radians();
        let e = Edge { a: DVec3::ZERO, b: DVec3::new(r.cos(), r.sin(), 0.0) * 5.0 };
        let dir_px = |u: DVec3| top(u * 10.0);
        let band = |deg: f32| Vec2::from_angle(-deg.to_radians()) * 80.0;
        let inf = |deg| infer_direction(band(deg), dir_px, Some((e, DVec3::Z)), INFER_TOL_DEG);
        let par = |i: Option<Inferred>| matches!(i, Some(Inferred::Edge(EdgeRel::Parallel, _)));
        let perp = |i: Option<Inferred>| matches!(i, Some(Inferred::Edge(EdgeRel::Perpendicular, _)));
        assert!(par(inf(34.0)) && par(inf(26.0)) && par(inf(210.0)), "within 5° either way");
        assert!(perp(inf(120.0 + 4.0)) && perp(inf(300.0)));
        assert_eq!(inf(36.0), None, "past the tolerance");
        assert_eq!(inf(2.0), Some(Inferred::Axis('x')), "axes still infer");
        // The perpendicular runs in the drawing plane, square to the edge.
        let Some(Inferred::Edge(_, d)) = inf(120.0) else { panic!() };
        assert!(d.dot(e.dir().unwrap()).abs() < 1e-9 && d.z.abs() < 1e-9);
        // An edge along red: the tie goes to the axis.
        let red = Edge { a: DVec3::ZERO, b: DVec3::X };
        assert_eq!(infer_direction(band(0.0), dir_px, Some((red, DVec3::Z)), INFER_TOL_DEG), Some(Inferred::Axis('x')));
        // A vertical edge on the ground plane has no perpendicular direction.
        let post = Edge { a: DVec3::ZERO, b: DVec3::Z };
        assert_eq!(post.rel_dir(EdgeRel::Perpendicular, DVec3::Z), None);
        assert!(Edge { a: DVec3::ONE, b: DVec3::ONE }.dir().is_none());
        assert!(e.same_as(&Edge { a: e.b, b: e.a }, 1e-9));
    }

    #[test]
    fn down_arrow_locks_the_edge_relation_and_toggles_off() {
        let a = DVec3::new(1.0, 1.0, 0.0);
        let d = DVec3::new(0.6, 0.8, 0.0);
        let mut l = EdgeLock::default();
        assert_eq!(l.toggle(None, a), None, "no edge hovered yet");
        assert_eq!(l.toggle(Some((EdgeRel::Perpendicular, d)), a), Some((EdgeRel::Perpendicular, d)));
        assert_eq!(l.toggle(Some((EdgeRel::Parallel, d)), a), None, "same key again unlocks");
        l.toggle(Some((EdgeRel::Parallel, d)), a);
        assert_eq!(l.get(Some(DVec3::ZERO)), None, "the next pick releases it");
        // Under the lock the cursor rides the line; a snap keeps its distance.
        let p = constrain_to_line(a, d, Some(a + DVec3::new(3.0, 4.0, 0.0) + DVec3::new(0.8, -0.6, 0.0)), None);
        assert!(p.distance(a + d * 5.0) < 1e-9, "{p}");
        let q = constrain_to_line(a, d, None, Some(down_ray(1.6 + 0.1, 1.8 - 0.1)));
        assert!(q.distance(a + d) < 0.2);
        assert_eq!(typed_direction_along(a, Some(d), Some(a)), Some(a + d));
    }
}
