// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Drawing on faces (#371, SketchUp "On Face"). While a draw tool or a guided
//! point step is armed, the cursor ray picks the nearest face of a solid
//! ([`itsjustcad_doc::pick::face_at`]); an unsnapped cursor lands on it, else
//! on the ground. A rect/circle/polygon keeps to the plane of its first click
//! ([`crate::draw_tool::DrawTool::plane`]).
//!
//! Shift (#372 + #371): pressed while the rubber band runs on an axis, it
//! holds that axis; else, over a face, it holds that face's plane (the cursor
//! stays on it, even off the face); else it is ortho. Either hold lasts while
//! Shift is down.

use glam::DVec3;
use itsjustcad_doc::pick::{FaceHit, FacePlane, planar_face, triangle};
use itsjustcad_doc::{Document, ObjectId};

/// What a Shift press holds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ShiftHold {
    /// The axis the rubber band runs on.
    Axis(char),
    /// The plane of the face under the cursor.
    Plane(FacePlane),
    /// Neither: Shift is ortho.
    Ortho,
}

/// What pressing Shift holds: the axis the band is on (`on_axis`), else the
/// plane of the face under the cursor, else nothing (ortho).
pub(crate) fn shift_press(on_axis: Option<char>, face: Option<FacePlane>) -> ShiftHold {
    match (on_axis, face) {
        (Some(a), _) => ShiftHold::Axis(a),
        (None, Some(p)) => ShiftHold::Plane(p),
        (None, None) => ShiftHold::Ortho,
    }
}

/// The cursor point before any constraint: the object snap, else the face
/// under the cursor, else the ground point.
pub(crate) fn base_cursor(
    snap: Option<DVec3>,
    face: Option<&FaceHit>,
    ground: Option<DVec3>,
) -> Option<DVec3> {
    snap.or(face.map(|f| f.point)).or(ground)
}

/// The cursor held to `plane`: an object snap moved onto it, else where the
/// cursor ray meets it, else `fallback` moved onto it (ray parallel to it).
pub(crate) fn on_plane(
    plane: FacePlane,
    snap: Option<DVec3>,
    ray: Option<(DVec3, DVec3)>,
    fallback: Option<DVec3>,
) -> Option<DVec3> {
    match snap {
        Some(s) => Some(plane.project(s)),
        None => ray
            .and_then(|(o, d)| plane.ray(o, d))
            .or(fallback.map(|p| plane.project(p))),
    }
}

/// Whether `p` lies on `plane` (a relative micron).
pub(crate) fn lies_on(plane: FacePlane, p: DVec3) -> bool {
    plane.contains(p, 1e-6 * (1.0 + p.abs().max_element()))
}

/// The flat face under the cursor, cached for the hover cue: a face is only
/// recomputed when the hit leaves it or the document changes.
pub(crate) struct FaceCue {
    object: ObjectId,
    generation: u64,
    tris: Vec<usize>,
    corners: Vec<[DVec3; 3]>,
}

/// World triangles of the flat face `hit` lies on, through `cache`.
pub(crate) fn face_cue<'a>(
    cache: &'a mut Option<FaceCue>,
    doc: &Document,
    hit: &FaceHit,
) -> &'a [[DVec3; 3]] {
    let fresh = cache.as_ref().is_some_and(|c| {
        c.object == hit.object
            && c.generation == doc.generation
            && c.tris.binary_search(&hit.face).is_ok()
    });
    if !fresh {
        let mesh = doc.get(hit.object).and_then(|o| o.geometry.mesh());
        let tris = mesh.map(|m| planar_face(m, hit.face)).unwrap_or_default();
        let corners = mesh
            .map(|m| tris.iter().filter_map(|&i| triangle(m, i)).collect())
            .unwrap_or_default();
        *cache = Some(FaceCue { object: hit.object, generation: doc.generation, tris, corners });
    }
    cache.as_ref().map_or(&[], |c| &c.corners)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(point: DVec3, normal: DVec3) -> FaceHit {
        FaceHit { object: ObjectId::new(), face: 0, t: 1.0, point, normal }
    }

    #[test]
    fn shift_holds_the_axis_else_the_face_plane_else_ortho() {
        let wall = FacePlane::new(DVec3::new(2.0, 0.0, 0.0), DVec3::X).unwrap();
        assert_eq!(shift_press(Some('x'), Some(wall)), ShiftHold::Axis('x'), "axis first");
        assert_eq!(shift_press(None, Some(wall)), ShiftHold::Plane(wall));
        assert_eq!(shift_press(None, None), ShiftHold::Ortho);
    }

    #[test]
    fn a_held_plane_keeps_the_cursor_on_it_off_the_face() {
        let wall = FacePlane::new(DVec3::new(2.0, 0.0, 0.0), DVec3::X).unwrap();
        // The cursor ray now misses the box and would hit the ground far
        // behind; held, it stays on the wall's plane.
        let ray = (DVec3::new(-8.0, 5.0, 1.0), DVec3::new(1.0, 0.0, -0.05).normalize());
        let p = on_plane(wall, None, Some(ray), Some(DVec3::new(12.0, 5.0, 0.0))).unwrap();
        assert!((p - DVec3::new(2.0, 5.0, 0.5)).length() < 1e-9, "{p}");
        // A snap off the plane is moved onto it.
        let s = on_plane(wall, Some(DVec3::new(5.0, 1.0, 1.0)), Some(ray), None).unwrap();
        assert_eq!(s, DVec3::new(2.0, 1.0, 1.0));
        // A ray parallel to the plane falls back to the projected point.
        let par = (DVec3::new(-8.0, 5.0, 1.0), DVec3::Y);
        assert_eq!(on_plane(wall, None, Some(par), Some(DVec3::new(7.0, 1.0, 0.0))), Some(DVec3::new(2.0, 1.0, 0.0)));
    }

    #[test]
    fn the_cursor_is_the_snap_else_the_face_else_the_ground() {
        let f = hit(DVec3::new(1.0, 1.0, 3.0), DVec3::Z);
        let ground = Some(DVec3::new(1.2, 1.2, 0.0));
        let snap = Some(DVec3::new(2.0, 2.0, 3.0));
        assert_eq!(base_cursor(snap, Some(&f), ground), snap);
        assert_eq!(base_cursor(None, Some(&f), ground), Some(f.point));
        assert_eq!(base_cursor(None, None, ground), ground, "ground fallback");
        assert!(lies_on(f.plane(), DVec3::new(9.0, -4.0, 3.0)));
        assert!(!lies_on(f.plane(), DVec3::new(9.0, -4.0, 3.01)));
    }

    #[test]
    fn the_cue_covers_the_flat_face_and_follows_the_document() {
        let mut s = itsjustcad_commands::Session::default();
        s.run(itsjustcad_commands::parse("box 0,0,0 2,2,3").unwrap()).unwrap();
        let doc = &s.doc;
        let id = doc.objects().next().unwrap().id;
        let top = itsjustcad_doc::pick::face_at(doc, DVec3::new(1.0, 0.5, 9.0), DVec3::NEG_Z).unwrap();
        assert_eq!(top.object, id);
        let mut cache = None;
        assert_eq!(face_cue(&mut cache, doc, &top).len(), 2, "both top triangles");
        let other = FaceHit { face: if top.face == 2 { 3 } else { 2 }, ..top };
        face_cue(&mut cache, doc, &other);
        assert_eq!(cache.as_ref().unwrap().generation, doc.generation, "reused");
    }
}
