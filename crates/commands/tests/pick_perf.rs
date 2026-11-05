// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart
//
// #301: per-click pick latency on a real site, split into box gathering, BVH
// build and query. Needs a site document:
// `BVH_SITE_FIXTURE=/path/site.json cargo test --release -p itsjustcad-commands
// --test pick_perf -- --ignored --nocapture`.

use std::time::Instant;

use glam::{DVec3, Mat4, Vec2, Vec3, Vec4};
use itsjustcad_doc::pick::{pick_object, pick_object_where_fresh, ray_aabb};

/// Pick ray through pixel `pos` of a `size` viewport (the hosts' `screen_ray`).
fn screen_ray(vp: Mat4, size: Vec2, pos: Vec2) -> (DVec3, DVec3) {
    let inv = vp.inverse();
    let ndc = Vec2::new(pos.x / size.x * 2.0 - 1.0, 1.0 - pos.y / size.y * 2.0);
    let un = |z: f32| {
        let p = inv * Vec4::new(ndc.x, ndc.y, z, 1.0);
        (p.truncate() / p.w).as_dvec3()
    };
    let origin = un(0.0);
    (origin, (un(1.0) - origin).normalize())
}

#[test]
#[ignore = "needs BVH_SITE_FIXTURE"]
fn pick_latency_on_site_fixture() {
    let path = std::env::var("BVH_SITE_FIXTURE").expect("BVH_SITE_FIXTURE");
    let s = itsjustcad_commands::io::load_file(std::path::Path::new(&path)).expect("load site");
    let doc = &s.doc;
    let bb = doc.scene_aabb().expect("site geometry");
    let (c, ext) = ((bb.min + bb.max) * 0.5, (bb.max - bb.min).max_element());
    let eye = c + DVec3::new(-0.2, -0.4, 0.8) * ext;
    let size = Vec2::new(1600.0, 1000.0);
    let vp = Mat4::perspective_rh(0.8, size.x / size.y, 1.0, (ext * 4.0) as f32)
        * Mat4::look_at_rh(eye.as_vec3(), c.as_vec3(), Vec3::Z);

    let clicks: Vec<Vec2> = (0..40)
        .flat_map(|i| {
            (0..5).map(move |j| Vec2::new(i as f32 * 40.0 + 7.0, j as f32 * 200.0 + 50.0))
        })
        .collect();
    let ms = |t: Instant| t.elapsed().as_secs_f64() * 1e3;
    let (mut gather, mut build, mut query, mut linear) = (0.0, 0.0, 0.0, 0.0);
    let (mut fresh, mut cached) = (0.0, 0.0);
    let mut hits = 0;
    for &pos in &clicks {
        let (origin, dir) = screen_ray(vp, size, pos);
        let t = Instant::now();
        let visible: Vec<_> = doc
            .objects()
            .filter(|o| o.visible && doc.layer_visible(&o.layer))
            .collect();
        let boxes: Vec<_> = visible
            .iter()
            .map(|o| doc.geometry_aabb(&o.geometry))
            .collect();
        gather += ms(t);
        let t = Instant::now();
        let bvh = kernel_mesh::Bvh::build(&boxes);
        build += ms(t);
        let test = |i: u32| ray_aabb(origin, dir, boxes[i as usize].min, boxes[i as usize].max);
        let t = Instant::now();
        let a = bvh.ray_closest_hit(origin, dir, f64::INFINITY, test);
        query += ms(t);
        let t = Instant::now();
        let mut b: Option<(u32, f64)> = None;
        for i in 0..boxes.len() as u32 {
            if let Some(t) = test(i)
                && b.is_none_or(|(_, bt)| t < bt)
            {
                b = Some((i, t));
            }
        }
        linear += ms(t);
        assert_eq!(a, b);
        let t = Instant::now();
        let old = pick_object_where_fresh(doc, vp, size, pos, origin, dir, |_| true);
        fresh += ms(t);
        let t = Instant::now();
        let new = pick_object(doc, vp, size, pos, origin, dir);
        cached += ms(t);
        assert_eq!(new, old, "click {pos}");
        hits += usize::from(new.is_some());
    }
    // Desktop clicks go through `pick_chain`: one pick, then more to drill
    // into a generator's outputs.
    let t = Instant::now();
    let chains = clicks
        .iter()
        .filter(|&&pos| {
            let (origin, dir) = screen_ray(vp, size, pos);
            itsjustcad_commands::lot::hierarchy::pick_chain(doc, vp, size, pos, origin, dir)
                .is_some()
        })
        .count();
    let chain = ms(t) / clicks.len() as f64;
    // Cold: the first pick after an edit rebuilds the box cache.
    let mut cold_doc = doc.clone();
    let (origin, dir) = screen_ray(vp, size, clicks[0]);
    let t = Instant::now();
    pick_object(&cold_doc, vp, size, clicks[0], origin, dir);
    let cold = ms(t);
    let id = cold_doc.objects().next().unwrap().id;
    cold_doc.get_mut(id);
    let t = Instant::now();
    pick_object(&cold_doc, vp, size, clicks[0], origin, dir);
    let cold_edit = ms(t);
    let n = clicks.len() as f64;
    let curves = doc
        .objects()
        .filter(|o| matches!(o.geometry, itsjustcad_doc::Geometry::Curve(_)))
        .count();
    println!(
        "objects {} (curves {curves}), clicks {} hit {hits}\nper click ms: gather {:.3} build {:.3} query {:.4} linear {:.3}\npick fresh {:.3} cached {:.3} cold {cold:.3} after edit {cold_edit:.3}\npick_chain {chain:.3} ({chains} hits)",
        doc.objects().count(),
        clicks.len(),
        gather / n,
        build / n,
        query / n,
        linear / n,
        fresh / n,
        cached / n,
    );
}
