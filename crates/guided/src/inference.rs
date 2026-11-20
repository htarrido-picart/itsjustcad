// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Coloured axis inference and arrow-key axis lock for point picks (#372,
//! SketchUp red/green/blue). Pure geometry shared by the desktop draw tools
//! and the FFI guided point steps: a rubber band from the last picked point
//! that runs within [`INFER_TOL_DEG`] of a world axis *on screen* snaps onto
//! that axis; a lock ([`AxisLock`]) forces it. Axes are the letters `x`, `y`,
//! `z` used by [`crate::axis_vector`].

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
    let len = band.length();
    if len.is_nan() || len < INFER_MIN_PX {
        return None;
    }
    let dirs: Vec<(char, Vec2)> = AXES
        .iter()
        .filter_map(|&a| axis_px(a).filter(|d| d.is_finite()).map(|d| (a, d)))
        .collect();
    let longest = dirs.iter().map(|(_, d)| d.length()).fold(0.0, f32::max);
    if longest <= f32::EPSILON {
        return None;
    }
    let b = band.normalize();
    let cos_tol = tol_deg.to_radians().cos();
    let mut best: Option<(char, f32)> = None;
    for (a, d) in dirs {
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
    if let Some(s) = snap {
        return project_onto_axis(anchor, axis, s);
    }
    let Some(u) = axis_vector(axis) else {
        return anchor;
    };
    ray.and_then(|(o, d)| closest_on_line_to_ray(anchor, u, o, d))
        .unwrap_or(anchor)
}

/// The cursor that gives a typed bare distance its direction: `cursor`, or —
/// when a locked axis has the cursor sitting on the anchor — one unit along
/// the axis, so "5" still goes 5 along it.
pub fn typed_direction_cursor(anchor: DVec3, axis: Option<char>, cursor: Option<DVec3>) -> Option<DVec3> {
    match (axis.and_then(axis_vector), cursor) {
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
}
