// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Drag-box and lasso selection for hosts (`ijc_box_select`, `ijc_lasso_select`),
//! the desktop app's Rhino convention ported off egui (`crates/app/src/boxsel.rs`,
//! `obj_matches_box`): a window takes objects fully inside, a crossing anything
//! touched. Curves test their tessellated polyline, other geometry its projected
//! AABB. Pure screen-space math in pixels, y down.

use glam::{DVec3, Mat4, Vec2, Vec4};
use itsjustcad_doc::Geometry;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BoxMode {
    Window,
    Crossing,
}

/// Drag direction → mode. A vertical drag (equal x) is a window.
pub(crate) fn mode(start: Vec2, end: Vec2) -> BoxMode {
    if end.x >= start.x { BoxMode::Window } else { BoxMode::Crossing }
}

/// Axis-aligned screen rect.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Rect {
    pub min: Vec2,
    pub max: Vec2,
}

impl Rect {
    pub fn from_points(a: Vec2, b: Vec2) -> Self {
        Rect { min: a.min(b), max: a.max(b) }
    }
    fn contains(&self, p: Vec2) -> bool {
        p.x >= self.min.x && p.x <= self.max.x && p.y >= self.min.y && p.y <= self.max.y
    }
    fn intersects(&self, r: Rect) -> bool {
        self.min.x <= r.max.x && r.min.x <= self.max.x && self.min.y <= r.max.y && r.min.y <= self.max.y
    }
    fn corners(&self) -> [Vec2; 4] {
        [self.min, Vec2::new(self.max.x, self.min.y), self.max, Vec2::new(self.min.x, self.max.y)]
    }
}

/// A screen region a selection tests objects against.
pub(crate) trait Region {
    fn contains(&self, p: Vec2) -> bool;
    /// Is the whole rect inside?
    fn contains_rect(&self, r: Rect) -> bool;
    fn intersects_rect(&self, r: Rect) -> bool;
    /// Does segment `a`–`b` touch the region?
    fn seg_hits(&self, a: Vec2, b: Vec2) -> bool;
    /// Does segment `a`–`b` cross the boundary? With both ends inside, only a
    /// concave region can be left in between.
    fn seg_crosses_boundary(&self, a: Vec2, b: Vec2) -> bool;
}

impl Region for Rect {
    fn contains(&self, p: Vec2) -> bool {
        Rect::contains(self, p)
    }
    fn contains_rect(&self, r: Rect) -> bool {
        self.contains(r.min) && self.contains(r.max)
    }
    fn intersects_rect(&self, r: Rect) -> bool {
        self.intersects(r)
    }
    fn seg_hits(&self, a: Vec2, b: Vec2) -> bool {
        seg_hits_rect(a, b, *self)
    }
    fn seg_crosses_boundary(&self, _: Vec2, _: Vec2) -> bool {
        false // convex: a segment between two inside points stays inside
    }
}

/// A closed freehand loop (last point joins the first), possibly concave.
/// Inside is even-odd.
pub(crate) struct Lasso {
    pts: Vec<Vec2>,
    bounds: Rect,
}

impl Lasso {
    /// None under three points.
    pub fn new(pts: Vec<Vec2>) -> Option<Self> {
        if pts.len() < 3 {
            return None;
        }
        let bounds = pts.iter().fold(Rect { min: pts[0], max: pts[0] }, |r, &p| Rect { min: r.min.min(p), max: r.max.max(p) });
        Some(Lasso { pts, bounds })
    }
    fn edges(&self) -> impl Iterator<Item = (Vec2, Vec2)> + '_ {
        self.pts.iter().copied().zip(self.pts.iter().copied().cycle().skip(1))
    }
}

impl Region for Lasso {
    fn contains(&self, p: Vec2) -> bool {
        if !self.bounds.contains(p) {
            return false;
        }
        let mut inside = false;
        for (a, b) in self.edges() {
            if (a.y > p.y) != (b.y > p.y) && p.x < a.x + (p.y - a.y) / (b.y - a.y) * (b.x - a.x) {
                inside = !inside;
            }
        }
        inside
    }
    fn contains_rect(&self, r: Rect) -> bool {
        r.corners().iter().all(|&c| self.contains(c)) && !self.edges().any(|(a, b)| seg_hits_rect(a, b, r))
    }
    fn intersects_rect(&self, r: Rect) -> bool {
        self.bounds.intersects(r)
            && (r.corners().iter().any(|&c| self.contains(c)) || self.edges().any(|(a, b)| seg_hits_rect(a, b, r)))
    }
    fn seg_hits(&self, a: Vec2, b: Vec2) -> bool {
        self.contains(a) || self.contains(b) || self.seg_crosses_boundary(a, b)
    }
    fn seg_crosses_boundary(&self, a: Vec2, b: Vec2) -> bool {
        Rect::from_points(a, b).intersects(self.bounds) && self.edges().any(|(c, d)| segs_cross(a, b, c, d))
    }
}

/// World → screen pixels in a `w`×`h` drawable; None behind the camera.
pub(crate) fn project(vp: Mat4, w: f32, h: f32, world: DVec3) -> Option<Vec2> {
    let clip = vp * Vec4::new(world.x as f32, world.y as f32, world.z as f32, 1.0);
    if clip.w.is_nan() || clip.w <= 0.0 {
        return None;
    }
    let ndc = clip.truncate() / clip.w;
    Some(Vec2::new((ndc.x + 1.0) * 0.5 * w, (1.0 - ndc.y) * 0.5 * h))
}

fn segs_cross(p0: Vec2, p1: Vec2, p2: Vec2, p3: Vec2) -> bool {
    fn orient(a: Vec2, b: Vec2, c: Vec2) -> f32 {
        (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
    }
    fn on_seg(a: Vec2, b: Vec2, c: Vec2) -> bool {
        c.x >= a.x.min(b.x) && c.x <= a.x.max(b.x) && c.y >= a.y.min(b.y) && c.y <= a.y.max(b.y)
    }
    let d0 = orient(p2, p3, p0);
    let d1 = orient(p2, p3, p1);
    let d2 = orient(p0, p1, p2);
    let d3 = orient(p0, p1, p3);
    if (d0 > 0.0) != (d1 > 0.0) && (d2 > 0.0) != (d3 > 0.0) && (d0 != 0.0 || d1 != 0.0) && (d2 != 0.0 || d3 != 0.0)
    {
        return true;
    }
    (d0 == 0.0 && on_seg(p2, p3, p0))
        || (d1 == 0.0 && on_seg(p2, p3, p1))
        || (d2 == 0.0 && on_seg(p0, p1, p2))
        || (d3 == 0.0 && on_seg(p0, p1, p3))
}

fn seg_hits_rect(a: Vec2, b: Vec2, r: Rect) -> bool {
    if r.contains(a) || r.contains(b) {
        return true;
    }
    let (tl, br) = (r.min, r.max);
    let (tr, bl) = (Vec2::new(br.x, tl.y), Vec2::new(tl.x, br.y));
    segs_cross(a, b, tl, tr) || segs_cross(a, b, tr, br) || segs_cross(a, b, br, bl) || segs_cross(a, b, bl, tl)
}

/// Window: every point on-screen and inside, no segment leaving the region.
/// Crossing: any point inside, or any on-screen segment touching the region.
pub(crate) fn polyline_matches<R: Region>(pts: &[Option<Vec2>], region: &R, mode: BoxMode) -> bool {
    if pts.is_empty() {
        return false;
    }
    let segs = || pts.windows(2).filter_map(|w| Some((w[0]?, w[1]?)));
    match mode {
        BoxMode::Window => {
            pts.iter().all(|p| p.is_some_and(|q| region.contains(q)))
                && !segs().any(|(a, b)| region.seg_crosses_boundary(a, b))
        }
        BoxMode::Crossing => {
            pts.iter().any(|p| p.is_some_and(|q| region.contains(q))) || segs().any(|(a, b)| region.seg_hits(a, b))
        }
    }
}

/// Same chord tolerance as the desktop's curve hit tests.
const CURVE_SCREEN_TOL: f64 = 0.05;

/// Does an object's projected geometry match the region under `mode`?
pub(crate) fn matches<R: Region>(geom: &Geometry, vp: Mat4, w: f32, h: f32, region: &R, mode: BoxMode) -> bool {
    if let Geometry::Curve(c) = geom {
        let mut pts: Vec<Option<Vec2>> = c.tessellate(CURVE_SCREEN_TOL).iter().map(|p| project(vp, w, h, *p)).collect();
        if c.is_closed() && pts.len() >= 2 {
            pts.push(pts[0]);
        }
        return polyline_matches(&pts, region, mode);
    }
    let bb = geom.aabb();
    let mut out: Option<Rect> = None;
    for i in 0..8 {
        let corner = DVec3::new(
            if i & 1 == 0 { bb.min.x } else { bb.max.x },
            if i & 2 == 0 { bb.min.y } else { bb.max.y },
            if i & 4 == 0 { bb.min.z } else { bb.max.z },
        );
        let Some(p) = project(vp, w, h, corner) else { return false };
        out = Some(match out {
            Some(r) => Rect { min: r.min.min(p), max: r.max.max(p) },
            None => Rect { min: p, max: p },
        });
    }
    out.is_some_and(|r| match mode {
        BoxMode::Window => region.contains_rect(r),
        BoxMode::Crossing => region.intersects_rect(r),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_sets_the_mode() {
        assert_eq!(mode(Vec2::new(0.0, 0.0), Vec2::new(10.0, 5.0)), BoxMode::Window);
        assert_eq!(mode(Vec2::new(10.0, 0.0), Vec2::new(0.0, 5.0)), BoxMode::Crossing);
    }

    #[test]
    fn diagonal_line_window_vs_crossing() {
        let line = [Some(Vec2::new(0.0, 0.0)), Some(Vec2::new(100.0, 100.0))];
        let partial = Rect::from_points(Vec2::new(40.0, 40.0), Vec2::new(60.0, 60.0));
        assert!(!polyline_matches(&line, &partial, BoxMode::Window));
        assert!(polyline_matches(&line, &partial, BoxMode::Crossing));
        let corner = Rect::from_points(Vec2::new(80.0, 0.0), Vec2::new(100.0, 20.0));
        assert!(!polyline_matches(&line, &corner, BoxMode::Crossing), "bbox corner is not the line");
        let all = Rect::from_points(Vec2::new(-1.0, -1.0), Vec2::new(101.0, 101.0));
        assert!(polyline_matches(&line, &all, BoxMode::Window));
    }

    /// A U opening upward: arms x 0–30 and 70–100, base y 70–100, notch
    /// x 30–70 above y 70.
    fn u_lasso() -> Lasso {
        let v = |x, y| Vec2::new(x, y);
        Lasso::new(vec![v(0.0, 0.0), v(30.0, 0.0), v(30.0, 70.0), v(70.0, 70.0), v(70.0, 0.0), v(100.0, 0.0), v(100.0, 100.0), v(0.0, 100.0)])
            .unwrap()
    }

    #[test]
    fn concave_lasso_excludes_its_notch() {
        let u = u_lasso();
        assert!(u.contains(Vec2::new(15.0, 50.0)));
        assert!(!u.contains(Vec2::new(50.0, 50.0)), "notch");
        assert!(!u.contains(Vec2::new(150.0, 50.0)));
        let notch = Rect::from_points(Vec2::new(40.0, 20.0), Vec2::new(60.0, 60.0));
        assert!(!u.contains_rect(notch) && !u.intersects_rect(notch));
        let arm = Rect::from_points(Vec2::new(5.0, 10.0), Vec2::new(25.0, 60.0));
        assert!(u.contains_rect(arm));
        // Both ends in the arms, but the segment spans the notch.
        let bridge = [Some(Vec2::new(15.0, 50.0)), Some(Vec2::new(85.0, 50.0))];
        assert!(!polyline_matches(&bridge, &u, BoxMode::Window));
        assert!(polyline_matches(&bridge, &u, BoxMode::Crossing));
        // A rect straddling the notch wall: crossing only.
        let wall = Rect::from_points(Vec2::new(20.0, 40.0), Vec2::new(40.0, 50.0));
        assert!(!u.contains_rect(wall) && u.intersects_rect(wall));
        // A segment crossing the loop with neither end inside still touches it.
        assert!(polyline_matches(&[Some(Vec2::new(-10.0, 90.0)), Some(Vec2::new(110.0, 90.0))], &u, BoxMode::Crossing));
        assert!(Lasso::new(vec![Vec2::ZERO, Vec2::ONE]).is_none());
    }
}
