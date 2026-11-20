// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! `pushpull` (#370, SketchUp Push/Pull): move a solid's flat face along its
//! normal ([`kernel_mesh::push_pull`]), or make a closed flat curve (a
//! region) a solid. The face is named by a point on it and its normal, not
//! by triangle indices, so a logged op finds the same face on replay.

use glam::DVec3;
use itsjustcad_doc::pick::{FaceRefMiss, face_through, planar_face};
use itsjustcad_doc::{Document, Geometry, ObjectId, SceneObject};

use super::{ApplyOutcome, Inverse, PROFILE_TOL, resolve};
use crate::error::ExecError;
use crate::{Command, Selector};

/// How far a face reference's point may sit off its face: a millimetre, so
/// a point rounded to the millimetre (a typed or guided line) still lands on
/// a sloped face.
pub(super) fn face_tol(point: DVec3) -> f64 {
    1e-3 + 1e-9 * point.abs().max_element()
}

fn invalid(msg: impl Into<String>) -> ExecError {
    ExecError::Invalid(format!("pushpull: {}", msg.into()))
}

pub(super) fn apply(
    doc: &mut Document,
    id: Option<ObjectId>,
    target: Selector,
    point: DVec3,
    normal: Option<DVec3>,
    distance: f64,
    keep: bool,
) -> Result<(Command, Inverse, ApplyOutcome), ExecError> {
    if !distance.is_finite() || distance == 0.0 {
        return Err(invalid("the distance must be a non-zero number"));
    }
    let ids = resolve(doc, &target)?;
    let [obj] = ids[..] else {
        return Err(invalid(format!("selector matched {} objects, expected exactly 1", ids.len())));
    };
    let src = doc.get(obj).expect("resolved id exists");
    let face_ref = |m: &kernel_mesh::Mesh| {
        face_through(m, point, normal, face_tol(point)).map_err(|miss| match miss {
            FaceRefMiss::Off => invalid(format!("no flat face of {obj} at {point}")),
            FaceRefMiss::Ambiguous => {
                invalid(format!("{point} is on an edge of {obj}; give the face normal too"))
            }
        })
    };
    match &src.geometry {
        Geometry::Mesh(m) => {
            let tri = face_ref(m)?;
            let [a, b, c] = itsjustcad_doc::pick::triangle(m, tri).expect("found triangle");
            let n = normal
                .and_then(DVec3::try_normalize)
                .or((b - a).cross(c - a).try_normalize())
                .expect("face_through skips degenerate triangles");
            let (mesh, how) = kernel_mesh::push_pull(m, &planar_face(m, tri), distance, keep)
                .map_err(|e| invalid(e.to_string()))?;
            let slot = &mut doc.get_mut(obj).expect("resolved id exists").geometry;
            let old = std::mem::replace(slot, Geometry::Mesh(mesh));
            Ok((
                Command::PushPull { id: None, target, point, normal: Some(n), distance, keep },
                Inverse::SetGeometry(vec![(obj, old)]),
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("pushpull {obj}: face {} by {distance}", how.label()),
                },
            ))
        }
        Geometry::Curve(curve) => {
            if !curve.is_closed() {
                return Err(invalid("the curve is not closed (close it to make a region)"));
            }
            let pts = curve.tessellate(PROFILE_TOL);
            let n = newell(&pts).try_normalize().ok_or_else(|| invalid("the region has no area"))?;
            let scale = 1.0 + pts.iter().fold(0.0f64, |m, p| m.max(p.abs().max_element()));
            if pts.iter().any(|p| (*p - pts[0]).dot(n).abs() > 1e-6 * scale) {
                return Err(invalid("the curve is not flat"));
            }
            // The region faces the given normal, else up (or as drawn).
            let n = match normal {
                Some(g) if g.dot(n) < 0.0 => -n,
                None if n.z < -1e-9 => -n,
                _ => n,
            };
            let layer = src.layer.clone();
            let on = point - n * (point - pts[0]).dot(n);
            let host = (distance < 0.0)
                .then(|| {
                    doc.objects().find(|o| {
                        o.id != obj
                            && !doc.object_locked(o.id)
                            && matches!(&o.geometry, Geometry::Mesh(hm)
                                if face_through(hm, on, Some(n), face_tol(on)).is_ok())
                    })
                })
                .flatten()
                .map(|o| o.id);
            let normal = Some(n);
            if let Some(host) = host {
                // Pushed into the solid face it lies on: cut the prism out.
                let Some(Geometry::Mesh(hm)) = doc.get(host).map(|o| &o.geometry) else {
                    unreachable!("host is a mesh")
                };
                let eps = 1e-6 * scale;
                let lands = hm.positions().iter().any(|p| ((*p - on).dot(n) - distance).abs() <= eps);
                let bottom = distance - if lands { eps } else { 0.0 };
                let top: Vec<DVec3> = pts.iter().map(|p| *p + n * eps).collect();
                let prism = kernel_mesh::area_member(&top, n, bottom - eps);
                let cut = kernel_mesh::csg_difference(hm, &prism);
                if cut.faces().is_empty() {
                    return Err(invalid(kernel_mesh::PushPullError::RemovesSolid.to_string()));
                }
                let slot = &mut doc.get_mut(host).expect("host exists").geometry;
                let old = std::mem::replace(slot, Geometry::Mesh(cut));
                let consumed = if keep { Vec::new() } else { doc.remove_many(&[obj]) };
                return Ok((
                    Command::PushPull { id: None, target, point, normal, distance, keep },
                    Inverse::Frozen {
                        snapshots: vec![(host, old)],
                        created: Vec::new(),
                        consumed,
                        bakes: Vec::new(),
                    },
                    ApplyOutcome {
                        created: Vec::new(),
                        message: format!("pushpull {obj}: cut into {host} by {distance}"),
                    },
                ));
            }
            let solid = kernel_mesh::area_member(&pts, n, distance);
            let consumed = if keep { Vec::new() } else { doc.remove_many(&[obj]) };
            let id = id.unwrap_or_default();
            doc.insert(SceneObject {
                visible: true,
                id,
                name: None,
                layer,
                color: None,
                material: None,
                lineweight_mm: None,
                edge_tags: None,
                street: None,
                face_values: None,
                geometry: Geometry::Mesh(solid),
            });
            let inverse = if keep {
                Inverse::DeleteCreated(vec![id])
            } else {
                Inverse::Replace { created: vec![id], consumed }
            };
            Ok((
                Command::PushPull { id: Some(id), target, point, normal, distance, keep },
                inverse,
                ApplyOutcome {
                    created: vec![id],
                    message: format!("pushpull {obj}: region -> solid {id} (h={distance})"),
                },
            ))
        }
        _ => Err(invalid("needs a solid or a closed flat curve")),
    }
}

/// Newell's normal of a closed loop (unnormalised; its length is twice the
/// area).
fn newell(pts: &[DVec3]) -> DVec3 {
    let mut n = DVec3::ZERO;
    for (i, p) in pts.iter().enumerate() {
        let q = pts[(i + 1) % pts.len()];
        n += DVec3::new((p.y - q.y) * (p.z + q.z), (p.z - q.z) * (p.x + q.x), (p.x - q.x) * (p.y + q.y));
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Session, parse};

    fn run(s: &mut Session, line: &str) -> ApplyOutcome {
        s.run(parse(line).unwrap_or_else(|e| panic!("{line}: {e}"))).unwrap_or_else(|e| panic!("{line}: {e}"))
    }

    fn only_mesh(s: &Session) -> (ObjectId, kernel_mesh::Mesh) {
        let mut meshes = s.doc.objects().filter_map(|o| match &o.geometry {
            Geometry::Mesh(m) => Some((o.id, m.clone())),
            _ => None,
        });
        let one = meshes.next().expect("a solid");
        assert!(meshes.next().is_none(), "one solid");
        one
    }

    fn volume(s: &Session) -> f64 {
        kernel_mesh::signed_volume(&only_mesh(s).1)
    }

    fn replays_identically(s: &Session) {
        let log = s.save_log();
        let json = crate::io::to_json(s);
        let back = crate::io::from_json(&json).unwrap();
        for r in [Session::replay(log).unwrap(), back] {
            let a: Vec<_> = s.doc.objects().collect();
            let b: Vec<_> = r.doc.objects().collect();
            assert_eq!(a, b);
        }
    }

    #[test]
    fn pulling_a_box_top_up_adds_height_in_one_undo_step() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 2,2,3");
        let out = run(&mut s, "pushpull last 1,1,3 0,0,1 2");
        assert!(out.message.contains("stretched by 2"), "{}", out.message);
        let (id, m) = only_mesh(&s);
        assert_eq!(m.aabb().max.z, 5.0);
        assert!((volume(&s) - 20.0).abs() < 1e-9);
        let log = s.save_log();
        assert_eq!(log.len(), 2, "one logged op");
        assert!(matches!(&log[1], Command::PushPull { normal: Some(n), .. } if *n == DVec3::Z));
        replays_identically(&s);
        run(&mut s, "undo");
        assert_eq!(only_mesh(&s), (id, kernel_mesh::make_box(DVec3::ZERO, DVec3::new(2.0, 2.0, 3.0))));
        run(&mut s, "redo");
        assert_eq!(only_mesh(&s).1.aabb().max.z, 5.0);
    }

    #[test]
    fn a_point_without_a_normal_names_a_face_unless_it_is_on_an_edge() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 2,2,3");
        // The +X side pushed in, named by a point only (the guided form).
        run(&mut s, "pushpull last 2,1,1.5 -0.5");
        assert_eq!(only_mesh(&s).1.aabb().max, DVec3::new(1.5, 2.0, 3.0));
        assert!(matches!(&s.save_log()[1], Command::PushPull { normal: Some(n), .. } if *n == DVec3::X));
        let err = s.run(parse("pushpull last 1.5,1,3 1").unwrap()).unwrap_err();
        assert!(err.to_string().contains("on an edge"), "{err}");
        let err = s.run(parse("pushpull last 9,9,9 1").unwrap()).unwrap_err();
        assert!(err.to_string().contains("no flat face"), "{err}");
        let err = s.run(parse("pushpull last 1,1,3 0,0,1 -3").unwrap()).unwrap_err();
        assert!(err.to_string().contains("nothing would be left"), "{err}");
        replays_identically(&s);
    }

    #[test]
    fn a_rect_region_becomes_a_solid() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 4 3");
        let out = run(&mut s, "pushpull last 1,1,0 0,0,1 2.5");
        assert_eq!(out.created.len(), 1);
        assert_eq!(s.doc.len(), 1, "the region became the solid");
        assert!((volume(&s) - 30.0).abs() < 1e-9);
        assert_eq!(only_mesh(&s).1.aabb().max.z, 2.5);
        replays_identically(&s);
        run(&mut s, "undo");
        assert!(matches!(s.doc.objects().next().map(|o| &o.geometry), Some(Geometry::Curve(_))));
        // `keep` keeps the curve.
        run(&mut s, "pushpull last 1,1,0 0,0,1 1 keep");
        assert_eq!(s.doc.len(), 2);
    }

    #[test]
    fn a_rect_on_a_face_pushed_in_cuts_a_hole() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 4,4,3");
        run(&mut s, "rect 1,1,3 2 2");
        let out = run(&mut s, "pushpull last 2,2,3 0,0,1 -3");
        assert!(out.message.contains("cut into"), "{}", out.message);
        assert!((volume(&s) - (48.0 - 12.0)).abs() < 1e-6, "{}", volume(&s));
        replays_identically(&s);
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), 2);
        assert!((volume_of_first_mesh(&s) - 48.0).abs() < 1e-9);
    }

    fn volume_of_first_mesh(s: &Session) -> f64 {
        s.doc
            .objects()
            .find_map(|o| match &o.geometry {
                Geometry::Mesh(m) => Some(kernel_mesh::signed_volume(m)),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn parse_forms() {
        let c = parse("pushpull #abc 1,2,3 4").unwrap();
        assert!(matches!(c, Command::PushPull { normal: None, keep: false, distance: 4.0, .. }));
        let c = parse("pushpull last 1,2,3 0,0,1 -2 keep").unwrap();
        assert!(matches!(c, Command::PushPull { normal: Some(_), keep: true, distance: -2.0, .. }));
        assert!(parse("pushpull last 1,2,3").is_err());
    }
}
