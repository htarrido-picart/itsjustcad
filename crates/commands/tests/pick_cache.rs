// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart
//
// #301: the cached pick (boxes kept until an object changes) picks the same
// object as the pre-#301 per-click rebuild after every edit, layer hide/show,
// undo and redo.

use glam::{DVec3, Mat4, Vec2, Vec4};
use itsjustcad_commands::{Session, parse};
use itsjustcad_doc::pick::{pick_object, pick_object_where, pick_object_where_fresh};
use itsjustcad_doc::{Geometry, ObjectId, SceneObject};

/// Every click of a grid over a top view of x,y ∈ [-2, 12], cached against
/// fresh, unfiltered and filtered to meshes. Warms the cache for the next
/// step.
fn picks(s: &Session) -> Vec<Option<ObjectId>> {
    let size = Vec2::splat(280.0);
    let vp = Mat4::orthographic_rh(-2.0, 12.0, -2.0, 12.0, -100.0, 100.0);
    let inv = vp.inverse();
    let meshes = |o: &SceneObject| !matches!(o.geometry, Geometry::Curve(_));
    let mut out = Vec::new();
    for i in 0..56 {
        for j in 0..56 {
            let pos = Vec2::new(i as f32 * 5.0 + 2.0, j as f32 * 5.0 + 2.0);
            let ndc = Vec2::new(pos.x / size.x * 2.0 - 1.0, 1.0 - pos.y / size.y * 2.0);
            let un = |z: f32| {
                let p = inv * Vec4::new(ndc.x, ndc.y, z, 1.0);
                (p.truncate() / p.w).as_dvec3()
            };
            let (o, d): (DVec3, DVec3) = (un(0.0), (un(1.0) - un(0.0)).normalize());
            let doc = &s.doc;
            let all = pick_object(doc, vp, size, pos, o, d);
            assert_eq!(
                all,
                pick_object_where_fresh(doc, vp, size, pos, o, d, |_| true),
                "{pos}"
            );
            let m = pick_object_where(doc, vp, size, pos, o, d, meshes);
            assert_eq!(
                m,
                pick_object_where_fresh(doc, vp, size, pos, o, d, meshes),
                "{pos}"
            );
            out.extend([all, m]);
        }
    }
    out
}

#[test]
fn cached_pick_matches_fresh_across_edits_layers_undo_redo() {
    let mut s = Session::default();
    let mut history = vec![picks(&s)];
    for line in [
        "box 0,0,0 4,4,4",
        "box 2,2,0 4,4,4",
        "line 0,8 10,8",
        "polyline 1,1,0 9,1,0 9,9,0 1,9,0 closed",
        "layer walls",
        "box 6,0,0 3,3,6",
        "hide walls",
        "show walls",
        "move last 0,5,0",
        "hideobj last",
        "box 0,6,0 2,2,2",
        "block last b",
        "insert b 7,-4,0",
        "insert b 12,-2,0 90 2",
        "delete last",
    ] {
        s.run(parse(line).unwrap()).unwrap();
        history.push(picks(&s));
        // `layer` only switches the current layer; `block` only defines one.
        let same = history[history.len() - 1] == history[history.len() - 2];
        assert_eq!(
            same,
            ["layer walls", "block last b"].contains(&line),
            "`{line}` changes a pick"
        );
    }
    let n = history.len();
    for k in (0..n - 1).rev() {
        s.run(parse("undo").unwrap()).unwrap();
        assert_eq!(picks(&s), history[k], "undo to step {k}");
    }
    for (k, want) in history.iter().enumerate().skip(1) {
        s.run(parse("redo").unwrap()).unwrap();
        assert_eq!(&picks(&s), want, "redo to step {k}");
    }
}
