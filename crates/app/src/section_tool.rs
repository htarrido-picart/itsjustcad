// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! The Section Plane tool and the plane widgets (#359, SketchUp). Armed by
//! a bare `sectionplane` (View ▸ Sections ▸ Section Plane): the plane
//! follows the face under the cursor (its outward normal, so the face stays
//! and what lies past it is cut), or the ground; the arrow keys lock the
//! normal to X, Y or Z (→ ← ↑, the #372 colours, again to unlock); Shift
//! holds the orientation so the plane can be moved to another point. A
//! click places it as a logged `sectionplane` op, which makes it the active
//! cut, and the tool ends. Esc cancels.
//!
//! Each plane draws as a frame sized to the model with arrows pointing into
//! the part it keeps and its name; the active one is bold. Double-clicking a
//! frame toggles its cut. This module is pure state and geometry; the
//! viewport side is `app/section_ui.rs`.

use glam::DVec3;
use itsjustcad_commands::{Command, SectionPlaneOp};
use itsjustcad_doc::pick::{FacePlane, face_at, plane_axes};
use itsjustcad_doc::{Document, SectionPlane};

/// Where a click would place a plane.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Placement {
    pub point: DVec3,
    /// Unit normal, toward the side the cut removes.
    pub normal: DVec3,
    /// On a face (else the ground or the held plane).
    pub on_face: bool,
}

#[derive(Default)]
pub(crate) struct SectionPlaneTool {
    armed: bool,
    /// Arrow-key normal lock: 'x', 'y' or 'z'.
    lock: Option<char>,
    /// Shift: the orientation held while it is down.
    hold: Option<DVec3>,
    /// The model's centre and frame half-size per document generation.
    frame: Option<(u64, DVec3, f64)>,
}

/// The unit vector of axis `a`.
fn axis(a: char) -> DVec3 {
    match a {
        'x' => DVec3::X,
        'y' => DVec3::Y,
        _ => DVec3::Z,
    }
}

impl SectionPlaneTool {
    /// A bare `sectionplane` arms the tool.
    pub(crate) fn try_start(&mut self, line: &str) -> bool {
        if line.trim() != "sectionplane" {
            return false;
        }
        self.armed = true;
        self.lock = None;
        self.hold = None;
        true
    }

    pub(crate) fn active(&self) -> bool {
        self.armed
    }

    pub(crate) fn cancel(&mut self) {
        self.armed = false;
        self.lock = None;
        self.hold = None;
    }

    pub(crate) fn lock(&self) -> Option<char> {
        self.lock
    }

    /// An arrow key: lock the normal to axis `a`, or unlock it when it is
    /// already locked there. Returns the new lock.
    pub(crate) fn toggle_lock(&mut self, a: char) -> Option<char> {
        self.lock = if self.lock == Some(a) { None } else { Some(a) };
        self.lock
    }

    /// Shift down (`Some` placement under the cursor) holds its normal;
    /// Shift up releases it.
    pub(crate) fn set_hold(&mut self, shift: bool, current: Option<Placement>) {
        if !shift {
            self.hold = None;
        } else if self.hold.is_none() {
            self.hold = current.map(|p| p.normal);
        }
    }

    pub(crate) fn held(&self) -> bool {
        self.hold.is_some()
    }

    /// The command-line prompt while armed.
    pub(crate) fn prompt(&self) -> Option<String> {
        self.armed.then(|| crate::i18n::t("sectionplane.prompt").to_string())
    }

    /// Where a click with the cursor ray `origin`/`dir` places the plane: on
    /// the face under it with its outward normal, else on the ground facing
    /// up. A held orientation (Shift) or an axis lock overrides the normal;
    /// a locked axis keeps the face's side (its sign follows the face).
    pub(crate) fn placement(&self, doc: &Document, origin: DVec3, dir: DVec3) -> Option<Placement> {
        let face = face_at(doc, origin, dir);
        let (point, mut normal, on_face) = match face {
            Some(f) => (f.point, f.normal, true),
            None => (FacePlane::horizontal(DVec3::ZERO).ray(origin, dir)?, DVec3::Z, false),
        };
        if let Some(n) = self.hold {
            normal = n;
        } else if let Some(a) = self.lock {
            let v = axis(a);
            normal = if normal.dot(v) < 0.0 { -v } else { v };
        }
        Some(Placement { point, normal, on_face })
    }

    /// Click: the `sectionplane` op placing the plane, and the tool ends.
    pub(crate) fn place(&mut self, p: Placement) -> Command {
        self.cancel();
        Command::SectionPlane(SectionPlaneOp::Place { name: None, point: p.point, normal: p.normal })
    }

    /// The model's centre and the frame half-size for `doc` (60% of the
    /// model's diagonal, at least a metre), cached per document generation.
    pub(crate) fn frame(&mut self, doc: &Document) -> (DVec3, f64) {
        match self.frame {
            Some((g, c, h)) if g == doc.generation => (c, h),
            _ => {
                let (c, h) = doc
                    .scene_aabb()
                    .map_or((DVec3::ZERO, 1.0), |b| ((b.min + b.max) * 0.5, ((b.max - b.min).length() * 0.6).max(1.0)));
                self.frame = Some((doc.generation, c, h));
                (c, h)
            }
        }
    }
}

/// A plane's widget in world space: the frame corners (a square of half-size
/// `half` around the point nearest `center`) and arrows from the frame
/// toward the kept side, as `[tail, head]` pairs plus their two head barbs.
pub(crate) struct Widget {
    pub corners: [DVec3; 4],
    pub arrows: Vec<[DVec3; 4]>,
}

pub(crate) fn widget(plane: &SectionPlane, center: DVec3, half: f64) -> Widget {
    let n = plane.normal;
    let (u, v) = plane_axes(n);
    // Centre the frame on the model where it crosses the plane.
    let c = center - n * plane.signed(center);
    let corners = [c - u * half - v * half, c + u * half - v * half, c + u * half + v * half, c - u * half + v * half];
    let len = half * 0.18;
    let arrows = [(-0.6, 0.0), (0.6, 0.0), (0.0, -0.6), (0.0, 0.6)]
        .iter()
        .map(|&(a, b)| {
            let tail = c + u * (half * a) + v * (half * b);
            let head = tail - n * len;
            // Barbs back toward the plane, splayed along the frame edge.
            let side = if a != 0.0 { v } else { u };
            [tail, head, head + n * len * 0.35 + side * len * 0.25, head + n * len * 0.35 - side * len * 0.25]
        })
        .collect();
    Widget { corners, arrows }
}

/// Distance in pixels from `pos` to the polygon outline `pts` (closed).
pub(crate) fn outline_distance(pts: &[egui::Pos2], pos: egui::Pos2) -> f32 {
    (0..pts.len())
        .map(|i| {
            let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
            let ab = b - a;
            let t = ((pos - a).dot(ab) / ab.length_sq().max(1e-6)).clamp(0.0, 1.0);
            (a + ab * t - pos).length()
        })
        .fold(f32::INFINITY, f32::min)
}

#[cfg(test)]
mod tests {
    use super::*;
    use itsjustcad_commands::{Session, parse};

    fn boxed() -> Session {
        let mut s = Session::default();
        s.run(parse("box 0,0,0 4,4,2").unwrap()).unwrap();
        s
    }

    #[test]
    fn places_on_the_face_under_the_cursor_with_its_outward_normal() {
        let s = boxed();
        let mut t = SectionPlaneTool::default();
        assert!(!t.try_start("sectionplane 0,0,0 0,0,1") && t.try_start("sectionplane"));
        // Looking at the -Y wall from the front.
        let p = t.placement(&s.doc, DVec3::new(2.0, -10.0, 1.0), DVec3::Y).unwrap();
        assert!(p.on_face && (p.point - DVec3::new(2.0, 0.0, 1.0)).length() < 1e-9);
        assert!((p.normal - DVec3::NEG_Y).length() < 1e-9, "outward: the wall stays");
        // Off the model: the ground, facing up.
        let g = t.placement(&s.doc, DVec3::new(20.0, 20.0, 5.0), DVec3::NEG_Z).unwrap();
        assert!(!g.on_face && g.normal == DVec3::Z && g.point.z.abs() < 1e-12);
        // An arrow lock keeps the face's side; again unlocks.
        assert_eq!(t.toggle_lock('y'), Some('y'));
        let locked = t.placement(&s.doc, DVec3::new(2.0, -10.0, 1.0), DVec3::Y).unwrap();
        assert_eq!(locked.normal, DVec3::NEG_Y);
        assert_eq!(t.toggle_lock('x'), Some('x'));
        let locked = t.placement(&s.doc, DVec3::new(2.0, -10.0, 1.0), DVec3::Y).unwrap();
        assert_eq!(locked.normal, DVec3::X);
        assert_eq!(t.toggle_lock('x'), None);
        // Shift holds the wall's orientation onto the top face.
        t.set_hold(true, Some(p));
        let held = t.placement(&s.doc, DVec3::new(2.0, 2.0, 10.0), DVec3::NEG_Z).unwrap();
        assert!(held.on_face && (held.point.z - 2.0).abs() < 1e-9 && held.normal == DVec3::NEG_Y);
        t.set_hold(false, None);
        assert!(!t.held());
        // A click places it and ends the tool.
        let cmd = t.place(p);
        assert!(!t.active());
        assert!(matches!(cmd, Command::SectionPlane(SectionPlaneOp::Place { name: None, .. })));
    }

    #[test]
    fn widget_frames_the_model_and_arrows_point_into_the_kept_side() {
        let plane = SectionPlane::new("s", DVec3::new(0.0, 0.0, 1.0), DVec3::Z).unwrap();
        let w = widget(&plane, DVec3::new(2.0, 2.0, 5.0), 3.0);
        assert!(w.corners.iter().all(|c| (c.z - 1.0).abs() < 1e-12), "on the plane");
        let mid = w.corners.iter().copied().sum::<DVec3>() / 4.0;
        assert!((mid - DVec3::new(2.0, 2.0, 1.0)).length() < 1e-12, "centred on the model");
        assert_eq!(w.arrows.len(), 4);
        assert!(w.arrows.iter().all(|a| a[1].z < a[0].z && plane.keeps(a[1])));
        let px = [egui::pos2(0.0, 0.0), egui::pos2(10.0, 0.0), egui::pos2(10.0, 10.0), egui::pos2(0.0, 10.0)];
        assert!((outline_distance(&px, egui::pos2(5.0, 1.0)) - 1.0).abs() < 1e-6);
    }
}
