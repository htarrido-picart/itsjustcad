// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Click/tap picking shared by the desktop viewport and the FFI hosts (#188).
//! Screen space is pixels relative to the viewport's top-left corner, y down.
//!
//! Curves have no surface, so a curve is picked by its DRAWN line: within
//! [`PICK_PX`] of its tessellation on screen, or within half the right-of-way
//! of a generated road's centreline. A curve the narrow phase can see is never
//! picked by its bounding box, so a flat closed curve (a site boundary, a lot)
//! no longer wins a tap that merely lands inside it. Everything else falls back
//! to the nearest bounding box the ray crosses.

use glam::{DVec3, Mat4, Vec2, Vec4};

use crate::{Document, Geometry, ObjectId};

/// Screen tolerance of a curve pick, in pixels.
pub const PICK_PX: f32 = 8.0;

/// Chord tolerance for tessellating a curve into screen points.
pub const CURVE_SCREEN_TOL: f64 = 0.05;

/// World → viewport pixels in a `size` viewport; `None` behind the camera.
pub fn project(vp: Mat4, size: Vec2, world: DVec3) -> Option<Vec2> {
    let clip = vp * Vec4::new(world.x as f32, world.y as f32, world.z as f32, 1.0);
    if clip.w.is_nan() || clip.w <= 0.0 {
        return None;
    }
    let ndc = clip.truncate() / clip.w;
    Some(Vec2::new(
        (ndc.x + 1.0) * 0.5 * size.x,
        (1.0 - ndc.y) * 0.5 * size.y,
    ))
}

/// Distance from `p` to the segment `a`–`b`.
pub fn dist_point_seg(p: Vec2, a: Vec2, b: Vec2) -> f32 {
    let ab = b - a;
    let len_sq = ab.length_squared();
    let t = if len_sq <= f32::EPSILON {
        0.0
    } else {
        ((p - a).dot(ab) / len_sq).clamp(0.0, 1.0)
    };
    (p - (a + ab * t)).length()
}

/// Ray parameter of the nearest non-negative slab hit, or `None` on a miss.
pub fn ray_aabb(origin: DVec3, dir: DVec3, min: DVec3, max: DVec3) -> Option<f64> {
    let inv = dir.recip();
    let t1 = (min - origin) * inv;
    let t2 = (max - origin) * inv;
    let t_min = t1.min(t2).max_element();
    let t_max = t1.max(t2).min_element();
    (t_max >= t_min.max(0.0)).then_some(t_min.max(0.0))
}

/// Screen box `(min, max)` covering a world AABB; `None` when a corner is
/// behind the camera.
fn screen_box(vp: Mat4, size: Vec2, bb: kernel_mesh::Aabb) -> Option<(Vec2, Vec2)> {
    let mut out: Option<(Vec2, Vec2)> = None;
    for i in 0..8 {
        let corner = DVec3::new(
            if i & 1 == 0 { bb.min.x } else { bb.max.x },
            if i & 2 == 0 { bb.min.y } else { bb.max.y },
            if i & 4 == 0 { bb.min.z } else { bb.max.z },
        );
        let p = project(vp, size, corner)?;
        out = Some(out.map_or((p, p), |(lo, hi)| (lo.min(p), hi.max(p))));
    }
    out
}

/// Screen distance from `pos` to a curve's drawn line, and whether `pos` lies
/// within `half_width` (world units, a road's half right-of-way) of it:
/// `Ok((d, in_band))`, `Ok((INFINITY, false))` for an empty curve, `Err` when a
/// tessellated point is behind the camera.
fn curve_pick(
    curve: &kernel_curve::Curve,
    half_width: Option<f64>,
    vp: Mat4,
    size: Vec2,
    pos: Vec2,
) -> Result<(f32, bool), ()> {
    let mut world = curve.tessellate(CURVE_SCREEN_TOL);
    if curve.is_closed() && world.len() >= 2 {
        world.push(world[0]);
    }
    let pts: Vec<Vec2> = world
        .iter()
        .map(|w| project(vp, size, *w).ok_or(()))
        .collect::<Result<_, _>>()?;
    let mut best = f32::INFINITY;
    let mut in_band = false;
    if pts.len() == 1 {
        best = (pts[0] - pos).length();
    }
    for (i, w) in pts.windows(2).enumerate() {
        let d = dist_point_seg(pos, w[0], w[1]);
        best = best.min(d);
        if let Some(hw) = half_width {
            // Half the right-of-way across this segment, on screen.
            let (a, b) = (world[i], world[i + 1]);
            let across = DVec3::new(a.y - b.y, b.x - a.x, 0.0).normalize_or_zero() * hw;
            let mid = (a + b) * 0.5;
            if let (Some(m), Some(e)) = (project(vp, size, mid), project(vp, size, mid + across)) {
                in_band |= d <= (e - m).length();
            }
        }
    }
    Ok((best, in_band))
}

/// Ray parameter where the ray passes closest to a curve's tessellation (the
/// depth of a curve pick), or `None` for an empty curve.
pub fn ray_curve_t(curve: &kernel_curve::Curve, origin: DVec3, dir: DVec3) -> Option<f64> {
    let mut pts = curve.tessellate(CURVE_SCREEN_TOL);
    if curve.is_closed() && pts.len() >= 2 {
        pts.push(pts[0]);
    }
    // Closest approach of the ray to segment a–b: (squared distance, ray t).
    let seg = |a: DVec3, b: DVec3| {
        let u = b - a;
        let w = origin - a;
        let (uu, ud, wu, wd) = (u.dot(u), u.dot(dir), w.dot(u), w.dot(dir));
        let den = uu - ud * ud; // |dir| = 1
        let s = if uu < 1e-18 || den <= 1e-12 * uu {
            0.0
        } else {
            ((wu - ud * wd) / den).clamp(0.0, 1.0)
        };
        let q = a + u * s;
        let t = (q - origin).dot(dir).max(0.0);
        ((origin + dir * t - q).length_squared(), t)
    };
    let hits = if pts.len() == 1 {
        vec![seg(pts[0], pts[0])]
    } else {
        pts.windows(2).map(|w| seg(w[0], w[1])).collect()
    };
    hits.into_iter()
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, t)| t)
}

/// The object a click at `pos` (viewport pixels, `size` the viewport) picks,
/// given the pick ray `origin`/`dir` through it: the nearest curve within its
/// pick band, else the nearest bounding box the ray crosses among the other
/// visible objects.
pub fn pick_object(
    doc: &Document,
    vp: Mat4,
    size: Vec2,
    pos: Vec2,
    origin: DVec3,
    dir: DVec3,
) -> Option<ObjectId> {
    let visible: Vec<_> = doc
        .objects()
        .filter(|o| o.visible && doc.layer_visible(&o.layer))
        .collect();
    let boxes: Vec<_> = visible
        .iter()
        .map(|o| doc.geometry_aabb(&o.geometry))
        .collect();

    // Nearest line within PICK_PX; failing that, the road whose right-of-way
    // holds the tap (nearest centreline), so a block edge along a road still
    // wins a tap right on it.
    let mut near: Option<(f32, ObjectId)> = None;
    let mut road: Option<(f32, ObjectId)> = None;
    let mut seen = vec![false; visible.len()];
    for (i, o) in visible.iter().enumerate() {
        let Geometry::Curve(c) = &o.geometry else {
            continue;
        };
        let half_width = o.street.as_ref().map(|s| s.width * 0.5);
        // Cheap reject: a plain curve whose screen box (grown by the band)
        // misses the click can't be picked, by its line or by its box.
        if half_width.is_none()
            && screen_box(vp, size, boxes[i]).is_some_and(|(lo, hi)| {
                let (lo, hi) = (lo - Vec2::splat(PICK_PX), hi + Vec2::splat(PICK_PX));
                pos.x < lo.x || pos.y < lo.y || pos.x > hi.x || pos.y > hi.y
            })
        {
            continue;
        }
        let Ok((d, in_band)) = curve_pick(c, half_width, vp, size, pos) else {
            continue;
        };
        seen[i] = true;
        let keep = |slot: &mut Option<(f32, ObjectId)>| {
            if slot.is_none_or(|(bd, _)| d < bd) {
                *slot = Some((d, o.id));
            }
        };
        if d <= PICK_PX {
            keep(&mut near);
        } else if in_band {
            keep(&mut road);
        }
    }
    if let Some((_, id)) = near.or(road) {
        return Some(id);
    }

    let bvh = kernel_mesh::Bvh::build(&boxes);
    let mut best: Option<(f64, ObjectId)> = None;
    for i in bvh.ray_candidates(origin, dir) {
        let i = i as usize;
        if seen[i] {
            continue;
        }
        let bb = boxes[i];
        if let Some(t) = ray_aabb(origin, dir, bb.min, bb.max)
            && best.is_none_or(|(bt, _)| t < bt)
        {
            best = Some((t, visible[i].id));
        }
    }
    best.map(|(_, id)| id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SceneObject;
    use kernel_curve::Curve;

    fn poly(pts: &[(f64, f64)], closed: bool) -> SceneObject {
        SceneObject {
            visible: true,
            id: ObjectId::new(),
            name: None,
            layer: crate::DEFAULT_LAYER.to_string(),
            color: None,
            material: None,
            lineweight_mm: None,
            edge_tags: None,
            street: None,
            geometry: Geometry::Curve(Curve::Polyline {
                points: pts.iter().map(|&(x, y)| DVec3::new(x, y, 0.0)).collect(),
                closed,
            }),
        }
    }

    fn add(doc: &mut Document, o: SceneObject) -> ObjectId {
        let id = o.id;
        doc.insert(o);
        id
    }

    /// Top view of world x,y ∈ [-1,1] onto a 200×200 viewport.
    fn top() -> (Mat4, Vec2) {
        (Mat4::IDENTITY, Vec2::splat(200.0))
    }

    fn pick(doc: &Document, x: f32, y: f32) -> Option<ObjectId> {
        let (vp, size) = top();
        let wx = (x / size.x * 2.0 - 1.0) as f64;
        let wy = (1.0 - y / size.y * 2.0) as f64;
        pick_object(
            doc,
            vp,
            size,
            Vec2::new(x, y),
            DVec3::new(wx, wy, 0.5),
            DVec3::NEG_Z,
        )
    }

    #[test]
    fn line_beats_the_closed_curve_around_it() {
        let mut doc = Document::default();
        let square = add(
            &mut doc,
            poly(&[(-0.9, -0.9), (0.9, -0.9), (0.9, 0.9), (-0.9, 0.9)], true),
        );
        let line = add(&mut doc, poly(&[(0.0, -0.9), (0.0, 0.9)], false));
        assert_eq!(pick(&doc, 100.0, 100.0), Some(line));
        assert_eq!(pick(&doc, 104.0, 100.0), Some(line), "within PICK_PX");
        assert_eq!(
            pick(&doc, 140.0, 100.0),
            None,
            "inside the square, far from both lines"
        );
        assert_eq!(
            pick(&doc, 10.0, 100.0),
            Some(square),
            "on the square's edge"
        );
    }

    #[test]
    fn a_click_inside_a_diagonal_lines_box_misses_it() {
        let mut doc = Document::default();
        let line = add(&mut doc, poly(&[(-0.9, -0.9), (0.9, 0.9)], false));
        assert_eq!(pick(&doc, 100.0, 100.0), Some(line));
        assert_eq!(pick(&doc, 170.0, 150.0), None);
        let c = match &doc.get(line).unwrap().geometry {
            Geometry::Curve(c) => c.clone(),
            _ => unreachable!(),
        };
        let t = ray_curve_t(&c, DVec3::new(0.0, 0.0, 0.5), DVec3::NEG_Z).unwrap();
        assert!((t - 0.5).abs() < 1e-9, "{t}");
    }

    #[test]
    fn a_road_is_picked_across_its_width() {
        let mut doc = Document::default();
        let mut road = poly(&[(0.0, -0.9), (0.0, 0.9)], false);
        // 0.4 wide → ±0.2 world → ±20 px.
        road.street = Some(crate::StreetInfo {
            id: 1,
            name: None,
            class: Default::default(),
            width: 0.4,
            fingerprint: 0,
        });
        let road = add(&mut doc, road);
        assert_eq!(pick(&doc, 118.0, 100.0), Some(road));
        assert_eq!(pick(&doc, 125.0, 100.0), None);
    }
}
