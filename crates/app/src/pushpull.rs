// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! The Push/Pull tool (#370, SketchUp). Armed by a bare `pushpull` (or
//! Shift+P): hovering tints the flat face (or closed flat curve) under the
//! cursor; a click grabs it; moving drags it along its normal with a ghost of
//! the moved face; a click (or a drag's release) commits, as does Enter after
//! typing an exact distance. Hovering another face parallel to the dragged
//! one, or a snap point, takes the distance to its height ("On Face" / "On
//! Point"). A double-click repeats the last distance on the face clicked.
//! Ctrl or Option keeps the old face's edges (a new face is stacked on it).
//!
//! The tool only emits [`Command::PushPull`]: one logged op, one undo step.
//! This module is pure state; the viewport feeds it rays and hits.

use glam::DVec3;
use itsjustcad_commands::{Command, Selector};
use itsjustcad_doc::pick::{FaceHit, face_outline, planar_face, region_at};
use itsjustcad_doc::{Document, Geometry, ObjectId};

/// What a click grabs: a solid's flat face, or a region (closed flat curve).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Target {
    pub object: ObjectId,
    /// Where the face was clicked (it names the face in the command).
    pub point: DVec3,
    /// Unit normal; a pull moves along it.
    pub normal: DVec3,
}

/// The inference a dragged distance took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tip {
    /// The height of a face parallel to the dragged one.
    OnFace,
    /// The height of a snap point.
    OnPoint,
}

impl Tip {
    pub(crate) fn text(self) -> &'static str {
        crate::i18n::t(match self {
            Tip::OnFace => "face.on",
            Tip::OnPoint => "pushpull.on_point",
        })
    }
}

/// A face being dragged.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Drag {
    pub target: Target,
    /// The face's outline, for the ghost.
    pub outline: Vec<[DVec3; 2]>,
    /// Where along the normal line the cursor grabbed it.
    grab: Option<f64>,
    pub distance: f64,
    pub tip: Option<Tip>,
}

#[derive(Default)]
pub(crate) struct PushPullTool {
    armed: bool,
    drag: Option<Drag>,
    /// Typed distance (SketchUp's Measurements box).
    input: String,
    /// The last committed distance, repeated by a double-click.
    last: Option<f64>,
}

/// The face or region under the cursor ray: a region drawn on a face wins
/// over the face (it lies on it).
pub(crate) fn target_at(doc: &Document, origin: DVec3, dir: DVec3) -> Option<Target> {
    let face = itsjustcad_doc::pick::face_at(doc, origin, dir);
    let region = region_at(doc, origin, dir);
    let tol = 1e-6 * (1.0 + origin.abs().max_element());
    let hit: FaceHit = match (face, region) {
        (Some(f), Some(r)) => if r.t <= f.t + tol { r } else { f },
        (f, r) => f.or(r)?,
    };
    Some(Target { object: hit.object, point: hit.point, normal: hit.normal })
}

/// The ghost outline of `t`'s face.
pub(crate) fn outline(doc: &Document, t: &Target) -> Vec<[DVec3; 2]> {
    match doc.get(t.object).map(|o| &o.geometry) {
        Some(Geometry::Mesh(m)) => {
            let tol = 1e-3 + 1e-9 * t.point.abs().max_element();
            itsjustcad_doc::pick::face_through(m, t.point, Some(t.normal), tol)
                .map(|tri| face_outline(m, &planar_face(m, tri)))
                .unwrap_or_default()
        }
        Some(Geometry::Curve(c)) => {
            let pts = c.tessellate(0.01);
            (0..pts.len()).map(|i| [pts[i], pts[(i + 1) % pts.len()]]).collect()
        }
        _ => Vec::new(),
    }
}

/// Where along the line `p + s·n` the ray `o + t·d` passes closest; `None`
/// when the ray runs along the line (looking straight down the normal).
pub(crate) fn line_param(p: DVec3, n: DVec3, o: DVec3, d: DVec3) -> Option<f64> {
    let d = d.try_normalize()?;
    let b = n.dot(d);
    let den = 1.0 - b * b;
    if den < 1e-9 {
        return None;
    }
    let w = p - o;
    Some((b * d.dot(w) - n.dot(w)) / den)
}

/// The distance the face at `p` (normal `n`) takes from what the cursor is
/// over: a snap point's height along `n`, else a parallel face's. Its own
/// plane (height 0) infers nothing.
pub(crate) fn infer(
    p: DVec3,
    n: DVec3,
    snap: Option<DVec3>,
    face: Option<(DVec3, DVec3)>,
) -> Option<(f64, Tip)> {
    let tol = 1e-9 * (1.0 + p.abs().max_element());
    let height = |q: DVec3| Some((q - p).dot(n)).filter(|h| h.abs() > tol);
    snap.and_then(height)
        .map(|h| (h, Tip::OnPoint))
        .or_else(|| {
            face.filter(|(_, fnorm)| fnorm.dot(n).abs() > 0.999)
                .and_then(|(q, _)| height(q))
                .map(|h| (h, Tip::OnFace))
        })
}

impl PushPullTool {
    pub(crate) fn active(&self) -> bool {
        self.armed
    }

    pub(crate) fn drag(&self) -> Option<&Drag> {
        self.drag.as_ref()
    }

    /// Arm on a bare `pushpull`. Returns true when consumed.
    pub(crate) fn try_start(&mut self, line: &str) -> bool {
        if line.trim() != "pushpull" {
            return false;
        }
        self.armed = true;
        self.drag = None;
        self.input.clear();
        true
    }

    /// Esc: drop a drag, else disarm. Returns true while still armed.
    pub(crate) fn escape(&mut self) -> bool {
        if self.drag.take().is_some() {
            self.input.clear();
        } else {
            self.cancel();
        }
        self.armed
    }

    pub(crate) fn cancel(&mut self) {
        self.armed = false;
        self.drag = None;
        self.input.clear();
    }

    pub(crate) fn prompt(&self) -> Option<String> {
        if !self.armed {
            return None;
        }
        use crate::i18n::t;
        Some(match &self.drag {
            None => t("pushpull.pick").to_string(),
            Some(d) if self.input.is_empty() => format!("{}  |  {}", t("pushpull.drag"), num(d.distance)),
            Some(_) => format!("{}  |  {}_", t("pushpull.drag"), self.input),
        })
    }

    /// Grab `target` (its outline from `doc`) with the cursor ray `ray`.
    pub(crate) fn begin(&mut self, doc: &Document, target: Target, ray: Option<(DVec3, DVec3)>) {
        let grab = ray.and_then(|(o, d)| line_param(target.point, target.normal, o, d));
        self.drag = Some(Drag { outline: outline(doc, &target), target, grab, distance: 0.0, tip: None });
        self.input.clear();
    }

    /// Follow the cursor: the ray's distance along the normal from where it
    /// grabbed, unless a snap point or a parallel face infers one.
    pub(crate) fn update(&mut self, ray: Option<(DVec3, DVec3)>, snap: Option<DVec3>, face: Option<(DVec3, DVec3)>) {
        let Some(d) = self.drag.as_mut() else { return };
        let (p, n) = (d.target.point, d.target.normal);
        if let Some((h, tip)) = infer(p, n, snap, face) {
            d.distance = h;
            d.tip = Some(tip);
            return;
        }
        d.tip = None;
        let s = ray.and_then(|(o, dir)| line_param(p, n, o, dir));
        match (s, d.grab) {
            (Some(s), Some(g)) => d.distance = s - g,
            (Some(s), None) => d.grab = Some(s),
            _ => {}
        }
    }

    pub(crate) fn push_input(&mut self, c: char) -> bool {
        if self.drag.is_some() && (c.is_ascii_digit() || matches!(c, '.' | '-' | '+')) {
            self.input.push(c);
            true
        } else {
            false
        }
    }

    pub(crate) fn pop_input(&mut self) -> bool {
        self.input.pop().is_some()
    }

    /// The command for the drag: the typed distance if any, else the
    /// dragged one. `None` (the drag goes on) for a zero or unreadable one.
    pub(crate) fn commit(&mut self, keep: bool) -> Option<Command> {
        let d = self.drag.as_ref()?;
        let distance = if self.input.is_empty() {
            d.distance
        } else {
            let typed = self.input.trim().parse::<f64>().ok();
            self.input.clear();
            typed?
        };
        if !distance.is_finite() || distance.abs() < 1e-9 {
            return None;
        }
        let target = self.drag.take()?.target;
        self.last = Some(distance);
        Some(command(target, distance, keep))
    }

    /// A double-click: the last distance again, on `target`.
    pub(crate) fn repeat(&mut self, target: Target, keep: bool) -> Option<Command> {
        let distance = self.last?;
        self.drag = None;
        self.input.clear();
        Some(command(target, distance, keep))
    }
}

fn command(t: Target, distance: f64, keep: bool) -> Command {
    Command::PushPull {
        id: None,
        target: Selector::Ids { ids: vec![t.object] },
        point: t.point,
        normal: Some(t.normal),
        distance,
        keep,
    }
}

fn num(v: f64) -> String {
    format!("{:.3}", v + 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use itsjustcad_commands::{Session, parse};

    fn session(lines: &[&str]) -> Session {
        let mut s = Session::default();
        for l in lines {
            s.run(parse(l).unwrap()).unwrap();
        }
        s
    }

    fn down(x: f64, y: f64) -> (DVec3, DVec3) {
        (DVec3::new(x, y, 20.0), DVec3::NEG_Z)
    }

    fn top_of(s: &Session) -> f64 {
        s.doc.objects().find_map(|o| o.geometry.mesh().map(|m| m.aabb().max.z)).unwrap()
    }

    #[test]
    fn click_drag_click_pulls_a_box_top_up() {
        let mut s = session(&["box 0,0,0 2,2,3"]);
        let mut t = PushPullTool::default();
        assert!(t.try_start("pushpull") && t.active());
        let (o, d) = down(1.0, 1.0);
        let target = target_at(&s.doc, o, d).unwrap();
        assert_eq!(target.normal, DVec3::Z);
        assert!((target.point - DVec3::new(1.0, 1.0, 3.0)).length() < 1e-9);
        // Grab with a ray from the side, then raise the cursor by 2.
        let side = |z: f64| Some((DVec3::new(-10.0, 1.0, z), DVec3::X));
        t.begin(&s.doc, target, side(3.0));
        assert_eq!(t.drag().unwrap().outline.len(), 4);
        t.update(side(5.0), None, None);
        assert!((t.drag().unwrap().distance - 2.0).abs() < 1e-9);
        let cmd = t.commit(false).unwrap();
        assert!(t.drag().is_none() && t.active(), "the tool stays armed");
        s.run(cmd).unwrap();
        assert!((top_of(&s) - 5.0).abs() < 1e-9);
        assert_eq!(s.save_log().len(), 2, "one logged op");
        s.run(Command::Undo).unwrap();
        assert_eq!(top_of(&s), 3.0, "one undo step");
    }

    #[test]
    fn a_typed_distance_wins_and_a_double_click_repeats_it() {
        let mut s = session(&["box 0,0,0 2,2,3", "box 5,0,0 2,2,1"]);
        let mut t = PushPullTool::default();
        t.try_start("pushpull");
        let (o, d) = down(1.0, 1.0);
        t.begin(&s.doc, target_at(&s.doc, o, d).unwrap(), None);
        for c in "1.5".chars() {
            assert!(t.push_input(c));
        }
        assert!(t.prompt().unwrap().ends_with("1.5_"));
        s.run(t.commit(false).unwrap()).unwrap();
        assert_eq!(top_of(&s), 4.5);
        // Double-click the low box's top: +1.5 again.
        let (o, d) = down(6.0, 1.0);
        let cmd = t.repeat(target_at(&s.doc, o, d).unwrap(), false).unwrap();
        s.run(cmd).unwrap();
        let low = s.doc.objects().nth(1).unwrap().geometry.mesh().unwrap().aabb().max.z;
        assert_eq!(low, 2.5);
        // A zero distance does not commit.
        t.begin(&s.doc, target_at(&s.doc, o, d).unwrap(), None);
        assert_eq!(t.commit(false), None);
        assert!(t.escape(), "Esc drops the drag");
        assert!(t.drag().is_none());
        assert!(!t.escape(), "a second Esc disarms");
    }

    #[test]
    fn hovering_another_face_takes_its_height() {
        let s = session(&["box 0,0,0 2,2,1", "box 5,0,0 2,2,4"]);
        let mut t = PushPullTool::default();
        t.try_start("pushpull");
        let (o, d) = down(1.0, 1.0);
        t.begin(&s.doc, target_at(&s.doc, o, d).unwrap(), Some((DVec3::new(-10.0, 1.0, 1.0), DVec3::X)));
        // The cursor over the tall box's top (z = 4): the low top goes to 4.
        let tall = target_at(&s.doc, DVec3::new(6.0, 1.0, 20.0), DVec3::NEG_Z).unwrap();
        t.update(Some((DVec3::new(-10.0, 1.0, 1.7), DVec3::X)), None, Some((tall.point, tall.normal)));
        let drag = t.drag().unwrap();
        assert_eq!((drag.distance, drag.tip), (3.0, Some(Tip::OnFace)));
        // A wall (not parallel) infers nothing; a snap point does.
        assert_eq!(infer(DVec3::Z, DVec3::Z, None, Some((DVec3::new(5.0, 1.0, 2.0), DVec3::X))), None);
        assert_eq!(infer(DVec3::Z, DVec3::Z, Some(DVec3::new(9.0, 9.0, 0.0)), None), Some((-1.0, Tip::OnPoint)));
        // Its own plane infers nothing.
        assert_eq!(infer(DVec3::Z, DVec3::Z, Some(DVec3::new(9.0, 9.0, 1.0)), None), None);
    }

    #[test]
    fn a_region_on_the_ground_is_the_target_and_becomes_a_solid() {
        let mut s = session(&["rect 0,0,0 4 3"]);
        let mut t = PushPullTool::default();
        t.try_start("pushpull");
        let (o, d) = down(1.0, 1.0);
        let target = target_at(&s.doc, o, d).unwrap();
        assert_eq!(target.normal, DVec3::Z);
        t.begin(&s.doc, target, None);
        assert_eq!(t.drag().unwrap().outline.len(), 4);
        for c in "2".chars() {
            t.push_input(c);
        }
        s.run(t.commit(true).unwrap()).unwrap();
        assert_eq!(s.doc.len(), 2, "keep: the curve stays beside the solid");
        assert_eq!(top_of(&s), 2.0);
    }

    #[test]
    fn line_param_is_the_nearest_point_on_the_normal_line() {
        let s = line_param(DVec3::ZERO, DVec3::Z, DVec3::new(-5.0, 0.0, 2.0), DVec3::X).unwrap();
        assert!((s - 2.0).abs() < 1e-12);
        assert_eq!(line_param(DVec3::ZERO, DVec3::Z, DVec3::new(0.0, 0.0, 9.0), DVec3::NEG_Z), None);
    }
}
