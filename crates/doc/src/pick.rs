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

use std::sync::{Arc, Mutex, PoisonError};

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
    pick_object_where(doc, vp, size, pos, origin, dir, |_| true)
}

/// [`pick_object`] among the visible objects `keep` accepts (#199: the child
/// of a parametric object under a click that hit the object's boundary).
pub fn pick_object_where(
    doc: &Document,
    vp: Mat4,
    size: Vec2,
    pos: Vec2,
    origin: DVec3,
    dir: DVec3,
    keep: impl Fn(&crate::SceneObject) -> bool,
) -> Option<ObjectId> {
    // Boxes come from the cache; an instance's box reads `blocks`, which
    // mutate without touching the objects, so it is computed per click. The
    // objects are walked in map order (a creation-order walk is one map
    // lookup per object, most of a click's cost) and put back in creation
    // order by rank, so ties resolve exactly as before.
    let cached = doc.pick_boxes.get(doc);
    // `layer_visible` without a map lookup per object.
    let hidden: Vec<&str> =
        doc.layers.iter().filter(|(_, l)| !l.visible).map(|(n, _)| n.as_str()).collect();
    let mut slots = vec![None; cached.len()];
    for (o, &(rank, bb)) in doc.objects_by_id().zip(cached.iter()) {
        if rank != u32::MAX && o.visible && !hidden.contains(&o.layer.as_str()) && keep(o) {
            let bb = match o.geometry {
                Geometry::Instance { .. } => doc.geometry_aabb(&o.geometry),
                _ => bb,
            };
            // What the active section cut removes whole is not there (#359).
            if doc.section_cut().is_some_and(|c| c.cuts_box(bb.min, bb.max)) {
                continue;
            }
            slots[rank as usize] = Some((o, bb));
        }
    }
    let cands: Vec<_> = slots.into_iter().flatten().collect();
    pick_among(&cands, vp, size, pos, |seen| {
        // Linear scan: on the site fixture (46k objects) it costs less than
        // a BVH build per click (#301). Strict `<` keeps the lowest index on
        // an exact tie, as `Bvh::ray_closest_hit` does (#310).
        let mut best: Option<(usize, f64)> = None;
        for (i, (_, bb)) in cands.iter().enumerate() {
            if let Some(t) = (!seen[i]).then(|| ray_aabb(origin, dir, bb.min, bb.max)).flatten()
                && best.is_none_or(|(_, bt)| t < bt)
            {
                best = Some((i, t));
            }
        }
        best.map(|(i, _)| i)
    })
}

/// The pre-#301 [`pick_object_where`]: boxes gathered and a BVH built on
/// every call. Kept as the oracle the cached pick must match.
#[doc(hidden)]
pub fn pick_object_where_fresh(
    doc: &Document,
    vp: Mat4,
    size: Vec2,
    pos: Vec2,
    origin: DVec3,
    dir: DVec3,
    keep: impl Fn(&crate::SceneObject) -> bool,
) -> Option<ObjectId> {
    let cands: Vec<_> = doc
        .objects()
        .filter(|o| o.visible && doc.layer_visible(&o.layer) && keep(o))
        .map(|o| (o, doc.geometry_aabb(&o.geometry)))
        .filter(|(_, bb)| doc.section_cut().is_none_or(|c| !c.cuts_box(bb.min, bb.max)))
        .collect();
    pick_among(&cands, vp, size, pos, |seen| {
        let boxes: Vec<_> = cands.iter().map(|&(_, bb)| bb).collect();
        let bvh = kernel_mesh::Bvh::build(&boxes);
        let best = bvh.ray_closest_hit(origin, dir, f64::INFINITY, |i| {
            let i = i as usize;
            let bb = boxes[i];
            (!seen[i]).then(|| ray_aabb(origin, dir, bb.min, bb.max)).flatten()
        });
        best.map(|(i, _)| i as usize)
    })
}

/// The pick among `cands` (eligible objects and their world boxes, in
/// creation order): the nearest curve within its pick band, else the index
/// `nearest_box` returns, given which candidates the curve pass already saw.
fn pick_among(
    cands: &[(&crate::SceneObject, kernel_mesh::Aabb)],
    vp: Mat4,
    size: Vec2,
    pos: Vec2,
    nearest_box: impl FnOnce(&[bool]) -> Option<usize>,
) -> Option<ObjectId> {
    // Nearest line within PICK_PX; failing that, the road whose right-of-way
    // holds the tap (nearest centreline), so a block edge along a road still
    // wins a tap right on it.
    let mut near: Option<(f32, ObjectId)> = None;
    let mut road: Option<(f32, ObjectId)> = None;
    let mut seen = vec![false; cands.len()];
    for (i, &(o, bb)) in cands.iter().enumerate() {
        let Geometry::Curve(c) = &o.geometry else {
            continue;
        };
        let half_width = o.street.as_ref().map(|s| s.width * 0.5);
        // Cheap reject: a plain curve whose screen box (grown by the band)
        // misses the click can't be picked, by its line or by its box.
        if half_width.is_none()
            && screen_box(vp, size, bb).is_some_and(|(lo, hi)| {
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
        let closer = |slot: &mut Option<(f32, ObjectId)>| {
            if slot.is_none_or(|(bd, _)| d < bd) {
                *slot = Some((d, o.id));
            }
        };
        if d <= PICK_PX {
            closer(&mut near);
        } else if in_band {
            closer(&mut road);
        }
    }
    if let Some((_, id)) = near.or(road) {
        return Some(id);
    }
    nearest_box(&seen).map(|i| cands[i].0.id)
}

/// Ray parameter of the nearest VISIBLE geometry under the cursor at `pos`
/// (viewport pixels, `size` the viewport) along the pick ray `origin`/`dir`,
/// or `None` when the ray meets nothing: the "what is under the cursor" depth
/// that cursor zoom (#365) and orbit (#366) pivot on. Unlike [`pick_object`] it
/// answers WHERE, not WHICH: a solid counts only where the ray crosses one of
/// its triangles (not anywhere in its bounding box), a curve where it is
/// within [`PICK_PX`] of its drawn line, everything else (instances, text,
/// point clouds, wire-only lattices) where the ray enters its box.
pub fn ray_hit(
    doc: &Document,
    vp: Mat4,
    size: Vec2,
    pos: Vec2,
    origin: DVec3,
    dir: DVec3,
) -> Option<f64> {
    let mut best: Option<f64> = None;
    let cut = doc.section_cut();
    let kept = |t: f64| cut.is_none_or(|c| c.keeps(origin + dir * t));
    for (o, bb) in visible_objects(doc) {
        let t = match &o.geometry {
            Geometry::Curve(c) => {
                let far = screen_box(vp, size, bb).is_some_and(|(lo, hi)| {
                    let (lo, hi) = (lo - Vec2::splat(PICK_PX), hi + Vec2::splat(PICK_PX));
                    pos.x < lo.x || pos.y < lo.y || pos.x > hi.x || pos.y > hi.y
                });
                match curve_pick(c, None, vp, size, pos) {
                    Ok((d, _)) if !far && d <= PICK_PX => ray_curve_t(c, origin, dir).filter(|t| kept(*t)),
                    _ => None,
                }
            }
            g => match g.mesh().filter(|m| !m.faces().is_empty()) {
                Some(m) => ray_aabb(origin, dir, bb.min, bb.max)
                    .and_then(|_| mesh_hit(m, origin, dir, cut))
                    .map(|(_, t)| t),
                None => ray_aabb(origin, dir, bb.min, bb.max).filter(|t| kept(*t)),
            },
        };
        if let Some(t) = t.filter(|t| t.is_finite())
            && best.is_none_or(|b| t < b)
        {
            best = Some(t);
        }
    }
    best
}

/// Every visible object (visible itself, on a visible layer, in the creation
/// order) with its world box, for the ray queries. Objects the active
/// section cut removes whole (#359) are left out.
fn visible_objects(doc: &Document) -> impl Iterator<Item = (&crate::SceneObject, kernel_mesh::Aabb)> {
    let cut = doc.section_cut();
    let cached = doc.pick_boxes.get(doc);
    let hidden: Vec<&str> =
        doc.layers.iter().filter(|(_, l)| !l.visible).map(|(n, _)| n.as_str()).collect();
    doc.objects_by_id()
        .zip((0..cached.len()).map(move |i| cached[i]))
        .filter(move |(o, (rank, _))| {
            *rank != u32::MAX && o.visible && !hidden.contains(&o.layer.as_str())
        })
        .map(move |(o, (_, bb))| match o.geometry {
            Geometry::Instance { .. } => (o, doc.geometry_aabb(&o.geometry)),
            _ => (o, bb),
        })
        .filter(move |(_, bb)| cut.is_none_or(|c| !c.cuts_box(bb.min, bb.max)))
}

/// The nearest triangle of `m` the ray crosses: `(triangle index, ray t)`.
/// With a section `cut` (#359), hits on the cut side do not count: the ray
/// passes through what the viewport does not draw.
fn mesh_hit(
    m: &kernel_mesh::Mesh,
    origin: DVec3,
    dir: DVec3,
    cut: Option<&crate::SectionPlane>,
) -> Option<(usize, f64)> {
    let p = m.positions();
    m.faces()
        .iter()
        .enumerate()
        .filter_map(|(i, f)| {
            let v = |k: usize| p.get(f[k] as usize).copied();
            kernel_mesh::ray_triangle(origin, dir, v(0)?, v(1)?, v(2)?).map(|t| (i, t))
        })
        .filter(|(_, t)| t.is_finite() && cut.is_none_or(|c| c.keeps(origin + dir * *t)))
        .min_by(|a, b| a.1.total_cmp(&b.1))
}

/// A plane a face lies on, or that a drawing is held to (#371): through
/// `origin`, with unit `normal`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FacePlane {
    pub origin: DVec3,
    pub normal: DVec3,
}

impl FacePlane {
    /// The plane through `origin` with normal `normal` (normalized); `None`
    /// for a zero or non-finite normal.
    pub fn new(origin: DVec3, normal: DVec3) -> Option<Self> {
        let n = normal.try_normalize()?;
        (origin.is_finite()).then_some(Self { origin, normal: n })
    }

    /// The horizontal (world XY) plane through `origin`.
    pub fn horizontal(origin: DVec3) -> Self {
        Self { origin, normal: DVec3::Z }
    }

    /// The in-plane axes a shape drawn on it uses ([`plane_axes`]).
    pub fn axes(&self) -> (DVec3, DVec3) {
        plane_axes(self.normal)
    }

    /// Whether the plane is world-horizontal (normal ±Z).
    pub fn is_horizontal(&self) -> bool {
        self.normal.z.abs() > 1.0 - 1e-9
    }

    /// `p` moved onto the plane along its normal.
    pub fn project(&self, p: DVec3) -> DVec3 {
        p - self.normal * (p - self.origin).dot(self.normal)
    }

    /// Where the ray `origin + t·dir` (t > 0) meets the plane, or `None` when
    /// it runs parallel or points away.
    pub fn ray(&self, origin: DVec3, dir: DVec3) -> Option<DVec3> {
        let den = dir.dot(self.normal);
        if den.abs() < 1e-12 {
            return None;
        }
        let t = (self.origin - origin).dot(self.normal) / den;
        (t > 0.0 && t.is_finite()).then(|| origin + dir * t)
    }

    /// Whether `p` lies on the plane within `tol`.
    pub fn contains(&self, p: DVec3, tol: f64) -> bool {
        (p - self.origin).dot(self.normal).abs() <= tol
    }
}

/// The in-plane `(x, y)` unit axes for a plane with normal `n`: world X and
/// Y for a horizontal plane (either way up), else `x` horizontal (Z × n) and
/// `y = n × x`, up the slope (up a wall). The same rule the `rect`/`circle`/
/// `polygon` commands use for their `normal` argument, so a shape drawn on a
/// face replays from the normal alone.
pub fn plane_axes(n: DVec3) -> (DVec3, DVec3) {
    let n = n.normalize_or_zero();
    if n.z.abs() > 1.0 - 1e-9 || n == DVec3::ZERO {
        return (DVec3::X, DVec3::Y);
    }
    let x = DVec3::Z.cross(n).normalize();
    (x, n.cross(x))
}

/// What the cursor ray meets on a solid's surface (#371): the face picking
/// drawing on faces uses, and Push/Pull builds on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FaceHit {
    /// The object hit.
    pub object: ObjectId,
    /// The triangle hit, an index into the object's mesh faces;
    /// [`planar_face`] widens it to the whole flat face.
    pub face: usize,
    /// Ray parameter of the hit.
    pub t: f64,
    /// World point of the hit.
    pub point: DVec3,
    /// Unit normal of the triangle by its winding (outward on a closed solid).
    pub normal: DVec3,
}

impl FaceHit {
    /// The plane of the face hit, through the hit point.
    pub fn plane(&self) -> FacePlane {
        FacePlane { origin: self.point, normal: self.normal }
    }
}

/// The nearest face of a visible solid (any object with a triangle mesh) the
/// ray `origin`/`dir` crosses, or `None` when it meets no surface. Unlike
/// [`ray_hit`], curves and box-only objects (instances, text, point clouds)
/// neither hit nor occlude: only a surface has a plane to draw on.
pub fn face_at(doc: &Document, origin: DVec3, dir: DVec3) -> Option<FaceHit> {
    let mut best: Option<(&crate::SceneObject, usize, f64)> = None;
    for (o, bb) in visible_objects(doc) {
        let Some(m) = o.geometry.mesh().filter(|m| !m.faces().is_empty()) else {
            continue;
        };
        let Some(near) = ray_aabb(origin, dir, bb.min, bb.max) else {
            continue;
        };
        if best.is_some_and(|(_, _, bt)| near > bt) {
            continue;
        }
        if let Some((i, t)) = mesh_hit(m, origin, dir, doc.section_cut())
            && best.is_none_or(|(_, _, bt)| t < bt)
        {
            best = Some((o, i, t));
        }
    }
    let (o, face, t) = best?;
    let m = o.geometry.mesh()?;
    let [a, b, c] = triangle(m, face)?;
    Some(FaceHit {
        object: o.id,
        face,
        t,
        point: origin + dir * t,
        normal: (b - a).cross(c - a).try_normalize()?,
    })
}

/// Corners of triangle `i` of `m`.
pub fn triangle(m: &kernel_mesh::Mesh, i: usize) -> Option<[DVec3; 3]> {
    let f = m.faces().get(i)?;
    let p = m.positions();
    Some([p.get(f[0] as usize)?, p.get(f[1] as usize)?, p.get(f[2] as usize)?].map(|v| *v))
}

/// The flat face triangle `tri` belongs to: every triangle of `m` coplanar
/// with it (same normal, same plane) and joined to it edge to edge, sorted.
/// Edges join by position, so an unwelded (flat-shaded) mesh still forms one
/// face. The face identity Push/Pull and the face hover cue use.
pub fn planar_face(m: &kernel_mesh::Mesh, tri: usize) -> Vec<usize> {
    use std::collections::HashMap;
    let Some([a, b, c]) = triangle(m, tri) else {
        return Vec::new();
    };
    let Some(n) = (b - a).cross(c - a).try_normalize() else {
        return vec![tri];
    };
    let tol = 1e-6 * (1.0 + a.abs().max_element() + (b - a).length().max((c - a).length()));
    type Key = (i64, i64, i64);
    let key = |p: DVec3| -> Key {
        let q = (p / 1e-6).round();
        (q.x as i64, q.y as i64, q.z as i64)
    };
    // Coplanar candidates and the edges they own.
    let mut edges: HashMap<(Key, Key), Vec<usize>> = HashMap::new();
    for i in 0..m.faces().len() {
        let Some(v) = triangle(m, i) else { continue };
        let same = (v[1] - v[0])
            .cross(v[2] - v[0])
            .try_normalize()
            .is_some_and(|ni| ni.dot(n) > 1.0 - 1e-9)
            && v.iter().all(|p| (*p - a).dot(n).abs() <= tol);
        if !same {
            continue;
        }
        for k in 0..3 {
            let (p, q) = (key(v[k]), key(v[(k + 1) % 3]));
            edges.entry(if p < q { (p, q) } else { (q, p) }).or_default().push(i);
        }
    }
    let mut seen: std::collections::HashSet<usize> = [tri].into();
    let mut stack = vec![tri];
    while let Some(i) = stack.pop() {
        let Some(v) = triangle(m, i) else { continue };
        for k in 0..3 {
            let (p, q) = (key(v[k]), key(v[(k + 1) % 3]));
            for &j in edges.get(&if p < q { (p, q) } else { (q, p) }).into_iter().flatten() {
                if seen.insert(j) {
                    stack.push(j);
                }
            }
        }
    }
    let mut out: Vec<usize> = seen.into_iter().collect();
    out.sort_unstable();
    out
}

/// The closed flat curve (a region, #370) the cursor ray crosses inside,
/// nearest first, as a hit facing the viewer: `face` is 0, `normal` the
/// region's plane normal turned toward the ray's origin. Push/Pull makes a
/// region a solid, so it is a face before it is one.
pub fn region_at(doc: &Document, origin: DVec3, dir: DVec3) -> Option<FaceHit> {
    let mut best: Option<FaceHit> = None;
    for (o, bb) in visible_objects(doc) {
        let Geometry::Curve(c) = &o.geometry else { continue };
        let pad = DVec3::splat(1e-6 * (1.0 + bb.max.abs().max_element()));
        if !c.is_closed() || ray_aabb(origin, dir, bb.min - pad, bb.max + pad).is_none() {
            continue;
        }
        let pts = c.tessellate(0.01);
        let Some(region) = flat_loop(&pts) else { continue };
        let n = if region.normal.dot(dir) > 0.0 { -region.normal } else { region.normal };
        let Some(p) = region.ray(origin, dir) else { continue };
        let t = (p - origin).dot(dir) / dir.length_squared();
        if t < 0.0 || best.as_ref().is_some_and(|b| b.t <= t) || doc.section_cut().is_some_and(|c| !c.keeps(p)) {
            continue;
        }
        let (u, v) = region.axes();
        let flat: Vec<(f64, f64)> =
            pts.iter().map(|q| ((*q - p).dot(u), (*q - p).dot(v))).collect();
        // Even-odd crossings of the +u ray from the hit point.
        let mut inside = false;
        for (i, &(x0, y0)) in flat.iter().enumerate() {
            let (x1, y1) = flat[(i + 1) % flat.len()];
            if (y0 > 0.0) != (y1 > 0.0) && x0 + (0.0 - y0) * (x1 - x0) / (y1 - y0) > 0.0 {
                inside = !inside;
            }
        }
        if inside {
            best = Some(FaceHit { object: o.id, face: 0, t, point: p, normal: n });
        }
    }
    best
}

/// The plane of a flat closed loop (Newell's normal), if it has area and
/// every point lies on it.
fn flat_loop(pts: &[DVec3]) -> Option<FacePlane> {
    let mut n = DVec3::ZERO;
    for (i, p) in pts.iter().enumerate() {
        let q = pts[(i + 1) % pts.len()];
        n += DVec3::new((p.y - q.y) * (p.z + q.z), (p.z - q.z) * (p.x + q.x), (p.x - q.x) * (p.y + q.y));
    }
    let plane = FacePlane::new(*pts.first()?, n)?;
    let tol = 1e-6 * (1.0 + pts.iter().fold(0.0f64, |m, p| m.max(p.abs().max_element())));
    pts.iter().all(|p| plane.contains(*p, tol)).then_some(plane)
}

/// The outline of the flat face made of triangles `tris` of `m`: the edges
/// only one of them has (matched by position, as [`planar_face`] joins).
pub fn face_outline(m: &kernel_mesh::Mesh, tris: &[usize]) -> Vec<[DVec3; 2]> {
    use std::collections::BTreeMap;
    type Key = (i64, i64, i64);
    let key = |p: DVec3| -> Key {
        let q = (p / 1e-6).round();
        (q.x as i64, q.y as i64, q.z as i64)
    };
    let mut count: BTreeMap<(Key, Key), (usize, [DVec3; 2])> = BTreeMap::new();
    for v in tris.iter().filter_map(|&i| triangle(m, i)) {
        for k in 0..3 {
            let (a, b) = (v[k], v[(k + 1) % 3]);
            let (ka, kb) = (key(a), key(b));
            let e = count.entry(if ka < kb { (ka, kb) } else { (kb, ka) }).or_insert((0, [a, b]));
            e.0 += 1;
        }
    }
    count.into_values().filter(|(n, _)| *n == 1).map(|(_, e)| e).collect()
}

/// Why a face reference names no face (#370).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaceRefMiss {
    /// No triangle (facing the normal, when one is given) is under the point.
    Off,
    /// No normal was given and the point lies on several flat faces (an edge).
    Ambiguous,
}

/// The triangle of `m` a face reference names (#370, Push/Pull): one that
/// `point` lies on to within `tol`, facing `normal` (to about 2.5°) when one
/// is given, the nearest to the point's plane first. A point and a normal
/// name a face however it is triangulated, so a logged reference replays.
/// With no normal the point must lie on one flat face only.
pub fn face_through(
    m: &kernel_mesh::Mesh,
    point: DVec3,
    normal: Option<DVec3>,
    tol: f64,
) -> Result<usize, FaceRefMiss> {
    let want = normal.and_then(|n| n.try_normalize());
    let mut hits: Vec<(f64, usize, DVec3)> = Vec::new();
    for i in 0..m.faces().len() {
        let Some([a, b, c]) = triangle(m, i) else { continue };
        let Some(n) = (b - a).cross(c - a).try_normalize() else { continue };
        if want.is_some_and(|w| w.dot(n) < 0.999) {
            continue;
        }
        let off = (point - a).dot(n);
        let inside = [(a, b), (b, c), (c, a)]
            .iter()
            .all(|&(p, q)| (q - p).cross(point - p).dot(n) >= -tol * (q - p).length());
        if off.abs() <= tol && inside {
            hits.push((off.abs(), i, n));
        }
    }
    hits.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)));
    let &(_, best, n) = hits.first().ok_or(FaceRefMiss::Off)?;
    if want.is_none() && hits.iter().any(|h| h.2.dot(n) < 0.999) {
        return Err(FaceRefMiss::Ambiguous);
    }
    Ok(best)
}

/// Creation-order rank and world box of every object, in
/// [`Document::objects_by_id`] order, for picking (#301). Built on first use
/// and dropped by every object mutation (`Document` calls
/// [`PickBoxes::invalidate`]). An instance's box is a placeholder: it depends
/// on `blocks`, so [`pick_object_where`] computes it per click.
#[derive(Default)]
pub(crate) struct PickBoxes(Mutex<Option<RankedBoxes>>);

/// `(creation-order rank, world box)` per object, in id order.
type RankedBoxes = Arc<[(u32, kernel_mesh::Aabb)]>;

impl PickBoxes {
    pub(crate) fn invalidate(&mut self) {
        *self.0.get_mut().unwrap_or_else(PoisonError::into_inner) = None;
    }

    fn get(&self, doc: &Document) -> RankedBoxes {
        let mut slot = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        slot.get_or_insert_with(|| {
            let rank: std::collections::HashMap<ObjectId, u32> =
                doc.objects().enumerate().map(|(i, o)| (o.id, i as u32)).collect();
            doc.objects_by_id()
                .map(|o| {
                    let bb = match o.geometry {
                        Geometry::Instance { .. } => o.geometry.aabb(),
                        _ => doc.geometry_aabb(&o.geometry),
                    };
                    // An object missing from the creation order is never
                    // walked by `objects`, so it is never picked.
                    (rank.get(&o.id).copied().unwrap_or(u32::MAX), bb)
                })
                .collect()
        })
        .clone()
    }
}

/// A cache, not document state: clones start cold, and it never makes two
/// documents differ.
impl Clone for PickBoxes {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl PartialEq for PickBoxes {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl std::fmt::Debug for PickBoxes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PickBoxes")
    }
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
            face_values: None,
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

    fn tri(x: f64, y: f64, z: f64, w: f64) -> SceneObject {
        let mesh = kernel_mesh::Mesh::new(
            vec![DVec3::new(x, y, z), DVec3::new(x + w, y, z), DVec3::new(x, y + w, z)],
            vec![[0, 1, 2]],
        );
        SceneObject { geometry: Geometry::Mesh(mesh), ..poly(&[], false) }
    }

    /// `ray_hit` through the top view at pixel `(x, y)`, from z = 0.5 down.
    fn hit(doc: &Document, x: f32, y: f32) -> Option<f64> {
        let (vp, size) = top();
        let w = DVec3::new((x / size.x * 2.0 - 1.0) as f64, (1.0 - y / size.y * 2.0) as f64, 0.5);
        ray_hit(doc, vp, size, Vec2::new(x, y), w, DVec3::NEG_Z)
    }

    #[test]
    fn ray_hit_is_the_nearest_surface_not_its_box() {
        let mut doc = Document::default();
        // Triangle at z = 0.2 over x,y ∈ [-0.5, 0.5], lower-left half.
        add(&mut doc, tri(-0.5, -0.5, 0.2, 1.0));
        let t = hit(&doc, 60.0, 140.0).unwrap(); // world (-0.4, -0.4)
        assert!((t - 0.3).abs() < 1e-9, "{t}");
        assert_eq!(hit(&doc, 140.0, 60.0), None, "inside its box, off the triangle");
        // A line under the cursor at z = 0 is farther than the triangle.
        add(&mut doc, poly(&[(-0.4, -0.9), (-0.4, 0.9)], false));
        assert!((hit(&doc, 60.0, 140.0).unwrap() - 0.3).abs() < 1e-9);
        assert!((hit(&doc, 60.0, 20.0).unwrap() - 0.5).abs() < 1e-9, "the line alone");
        // Hidden geometry is not under the cursor.
        for id in doc.objects().map(|o| o.id).collect::<Vec<_>>() {
            doc.get_mut(id).unwrap().visible = false;
        }
        assert_eq!(hit(&doc, 60.0, 140.0), None);
    }

    fn solid(corner: DVec3, size: DVec3) -> SceneObject {
        let mesh = kernel_mesh::make_box(corner, size);
        SceneObject { geometry: Geometry::Mesh(mesh), ..poly(&[], false) }
    }

    #[test]
    fn picks_skip_what_the_section_cut_removes() {
        // #359: a cut at z = 2 facing up removes the top half of a 4 m box.
        let mut doc = Document::default();
        let b = add(&mut doc, solid(DVec3::ZERO, DVec3::new(2.0, 2.0, 4.0)));
        let above = add(&mut doc, solid(DVec3::new(0.0, 0.0, 6.0), DVec3::new(2.0, 2.0, 1.0)));
        let (o, d) = (DVec3::new(1.0, 1.0, 10.0), DVec3::NEG_Z);
        assert_eq!(face_at(&doc, o, d).unwrap().object, above);
        doc.section_planes.push(crate::SectionPlane::new("s", DVec3::new(0.0, 0.0, 2.0), DVec3::Z).unwrap());
        doc.active_section = Some("s".into());
        // The box above is cut whole; the ray passes the cut top of `b` and
        // meets its bottom (z = 0) from inside.
        let hit = face_at(&doc, o, d).unwrap();
        assert_eq!(hit.object, b);
        assert!(hit.point.z.abs() < 1e-9, "{hit:?}");
        let vp = Mat4::IDENTITY;
        let size = Vec2::splat(100.0);
        let t = ray_hit(&doc, vp, size, Vec2::splat(50.0), o, d).unwrap();
        assert!((t - 10.0).abs() < 1e-9, "ray_hit skips the cut part too: {t}");
        // A side ray below the cut still meets the wall.
        let side = face_at(&doc, DVec3::new(-5.0, 1.0, 1.0), DVec3::X).unwrap();
        assert!((side.point.x).abs() < 1e-9);
        assert_eq!(pick_object(&doc, vp, size, Vec2::splat(50.0), o, d), Some(b), "the cut-away box is not picked");
        // Hidden cuts (view state) pick the whole model again.
        doc.section_cuts_hidden = true;
        assert_eq!(face_at(&doc, o, d).unwrap().object, above);
    }

    #[test]
    fn face_at_is_the_nearest_face_and_occluded_faces_lose() {
        let mut doc = Document::default();
        // A low box, and a tall one beside it: a ray from the side meets the
        // tall one's -X wall first, so the low box behind it is occluded.
        let low = add(&mut doc, solid(DVec3::new(4.0, 0.0, 0.0), DVec3::new(2.0, 2.0, 1.0)));
        let tall = add(&mut doc, solid(DVec3::new(0.0, 0.0, 0.0), DVec3::new(2.0, 2.0, 3.0)));
        let side = face_at(&doc, DVec3::new(-5.0, 1.0, 0.5), DVec3::X).unwrap();
        assert_eq!(side.object, tall);
        assert!((side.point - DVec3::new(0.0, 1.0, 0.5)).length() < 1e-9);
        assert!((side.normal - DVec3::NEG_X).length() < 1e-9, "outward wall normal");
        // From above, the low box's top (z = 1) is the nearest face there.
        let top = face_at(&doc, DVec3::new(5.0, 1.0, 10.0), DVec3::NEG_Z).unwrap();
        assert_eq!(top.object, low);
        assert!((top.point.z - 1.0).abs() < 1e-9 && top.normal.z > 0.99);
        // Nothing under the ray: no face (the caller falls back to the ground).
        assert_eq!(face_at(&doc, DVec3::new(20.0, 20.0, 10.0), DVec3::NEG_Z), None);
        // A curve is not a face and does not occlude one.
        add(&mut doc, poly(&[(4.5, 1.0), (5.5, 1.0)], false));
        assert_eq!(face_at(&doc, DVec3::new(5.0, 1.0, 10.0), DVec3::NEG_Z).unwrap().object, low);
        // A hidden solid neither hits nor occludes.
        doc.get_mut(tall).unwrap().visible = false;
        let through = face_at(&doc, DVec3::new(-5.0, 1.0, 0.5), DVec3::X).unwrap();
        assert_eq!(through.object, low);
        assert!((through.point.x - 4.0).abs() < 1e-9);
    }

    #[test]
    fn planar_face_widens_a_triangle_to_its_flat_face() {
        let m = kernel_mesh::make_box(DVec3::ZERO, DVec3::new(2.0, 3.0, 4.0));
        // Triangles 2 and 3 are the top; 10 and 11 the +X wall.
        assert_eq!(planar_face(&m, 2), vec![2, 3]);
        assert_eq!(planar_face(&m, 11), vec![10, 11]);
        // Unwelded (one vertex per corner per triangle) still joins by position.
        let p = m.positions();
        let mut pos = Vec::new();
        let faces = m
            .faces()
            .iter()
            .map(|f| {
                let b = pos.len() as u32;
                pos.extend(f.iter().map(|&i| p[i as usize]));
                [b, b + 1, b + 2]
            })
            .collect();
        let flat = kernel_mesh::Mesh::new(pos, faces);
        assert_eq!(planar_face(&flat, 3), vec![2, 3]);
    }

    #[test]
    fn a_point_and_normal_name_a_face_and_its_outline() {
        let m = kernel_mesh::make_box(DVec3::ZERO, DVec3::new(2.0, 3.0, 4.0));
        let top = face_through(&m, DVec3::new(1.0, 1.0, 4.0), Some(DVec3::Z), 1e-6).unwrap();
        assert_eq!(planar_face(&m, top), vec![2, 3]);
        // On the top/+X edge only the normal tells the faces apart.
        let edge = DVec3::new(2.0, 1.0, 4.0);
        assert_eq!(face_through(&m, edge, None, 1e-6), Err(FaceRefMiss::Ambiguous));
        assert!([10, 11].contains(&face_through(&m, edge, Some(DVec3::X), 1e-6).unwrap()));
        assert_eq!(face_through(&m, DVec3::new(1.0, 1.0, 4.1), None, 1e-6), Err(FaceRefMiss::Off));
        let ring = face_outline(&m, &[2, 3]);
        assert_eq!(ring.len(), 4, "the diagonal is inside");
        assert!(ring.iter().flatten().all(|p| p.z == 4.0));
    }

    #[test]
    fn region_at_finds_a_closed_curve_under_the_ray() {
        let mut doc = Document::default();
        let open = add(&mut doc, poly(&[(0.0, 0.0), (4.0, 0.0), (4.0, 4.0)], false));
        assert_eq!(region_at(&doc, DVec3::new(3.0, 1.0, 5.0), DVec3::NEG_Z), None, "{open} is open");
        let l = add(&mut doc, poly(&[(0.0, 0.0), (4.0, 0.0), (4.0, 2.0), (2.0, 2.0), (2.0, 4.0), (0.0, 4.0)], true));
        let hit = region_at(&doc, DVec3::new(1.0, 3.0, 5.0), DVec3::NEG_Z).unwrap();
        assert_eq!(hit.object, l);
        assert!((hit.point - DVec3::new(1.0, 3.0, 0.0)).length() < 1e-9);
        assert_eq!(hit.normal, DVec3::Z, "faces the viewer");
        // From below it faces down; in the L's notch there is no region.
        assert_eq!(region_at(&doc, DVec3::new(1.0, 3.0, -5.0), DVec3::Z).unwrap().normal, DVec3::NEG_Z);
        assert_eq!(region_at(&doc, DVec3::new(3.0, 3.0, 5.0), DVec3::NEG_Z), None);
    }

    #[test]
    fn plane_axes_are_horizontal_x_and_up_the_face() {
        assert_eq!(plane_axes(DVec3::Z), (DVec3::X, DVec3::Y));
        assert_eq!(plane_axes(DVec3::NEG_Z), (DVec3::X, DVec3::Y));
        for n in [DVec3::X, DVec3::NEG_Y, DVec3::new(0.0, -0.6, 0.8)] {
            let (x, y) = plane_axes(n);
            assert!(x.z.abs() < 1e-12, "x is horizontal");
            assert!(y.z > 0.0, "y runs up the face");
            assert!(x.dot(n).abs() < 1e-12 && y.dot(n).abs() < 1e-12);
            assert!((x.cross(y) - n).length() < 1e-12, "right-handed about n");
        }
        let wall = FacePlane::new(DVec3::new(2.0, 0.0, 0.0), DVec3::X).unwrap();
        let p = wall.ray(DVec3::new(-3.0, 1.0, 1.0), DVec3::X).unwrap();
        assert!((p - DVec3::new(2.0, 1.0, 1.0)).length() < 1e-12);
        assert_eq!(wall.ray(DVec3::new(-3.0, 1.0, 1.0), DVec3::Y), None, "parallel");
        assert_eq!(wall.project(DVec3::new(7.0, 1.0, 1.0)), DVec3::new(2.0, 1.0, 1.0));
    }

    /// Every click of a grid over the top view through the cached pick and
    /// the pre-#301 rebuild, unfiltered and filtered to meshes; returns the
    /// picks. Each call also warms the cache, so the next mutation must
    /// invalidate it.
    fn assert_cached_matches_fresh(doc: &Document) -> Vec<Option<ObjectId>> {
        let (vp, size) = top();
        let meshes = |o: &SceneObject| matches!(o.geometry, Geometry::Mesh(_));
        let mut out = Vec::new();
        for i in 0..40 {
            for j in 0..40 {
                let pos = Vec2::new(i as f32 * 5.0 + 1.0, j as f32 * 5.0 + 1.0);
                let o = DVec3::new((pos.x / 100.0 - 1.0) as f64, (1.0 - pos.y / 100.0) as f64, 0.5);
                let d = DVec3::NEG_Z;
                let all = pick_object(doc, vp, size, pos, o, d);
                assert_eq!(all, pick_object_where_fresh(doc, vp, size, pos, o, d, |_| true), "{pos}");
                let m = pick_object_where(doc, vp, size, pos, o, d, meshes);
                assert_eq!(m, pick_object_where_fresh(doc, vp, size, pos, o, d, meshes), "{pos}");
                out.extend([all, m]);
            }
        }
        out
    }

    #[test]
    fn cached_pick_matches_fresh_across_document_mutations() {
        let mut doc = Document::default();
        // Exact depth ties (same z, overlapping) resolve by creation order.
        let a = add(&mut doc, tri(-0.8, -0.8, 0.0, 1.2));
        let b = add(&mut doc, tri(-0.6, -0.6, 0.0, 1.2));
        let c = add(&mut doc, tri(-0.2, -0.9, -0.3, 1.0));
        add(&mut doc, poly(&[(-0.9, 0.5), (0.9, 0.5)], false));
        add(&mut doc, poly(&[(-0.5, -0.5), (0.5, -0.5), (0.5, 0.5)], true));
        let before = assert_cached_matches_fresh(&doc);

        // Sinking `a` below `b` through `get_mut` hands `b` their overlap.
        if let Some(Geometry::Mesh(m)) = doc.get_mut(a).map(|o| &mut o.geometry) {
            *m = kernel_mesh::Mesh::new(
                m.positions().iter().map(|p| *p + DVec3::new(0.0, 0.0, -0.2)).collect(),
                m.faces().to_vec(),
            );
        }
        assert_ne!(assert_cached_matches_fresh(&doc), before, "the edit is visible");

        add(&mut doc, tri(0.0, 0.0, 0.1, 0.5));
        assert_cached_matches_fresh(&doc);
        let (ob, ib) = doc.remove(b).unwrap();
        assert_cached_matches_fresh(&doc);
        doc.restore(ob, ib);
        assert_cached_matches_fresh(&doc);
        let removed = doc.remove_many(&[a, c]);
        assert_cached_matches_fresh(&doc);
        doc.restore_many(&removed);
        assert_cached_matches_fresh(&doc);
        for o in doc.objects_mut() {
            if o.id == b {
                o.visible = false;
            }
        }
        assert_cached_matches_fresh(&doc);

        // Layer visibility is read per click, even when set without a bump.
        doc.get_mut(c).unwrap().layer = "hidden".into();
        assert_cached_matches_fresh(&doc);
        doc.layers.insert("hidden".into(), crate::LayerStyle { visible: false, ..Default::default() });
        assert_cached_matches_fresh(&doc);
        doc.layers.get_mut("hidden").unwrap().visible = true;
        assert_cached_matches_fresh(&doc);
    }

    #[test]
    fn an_instance_box_follows_its_block_without_invalidation() {
        let mut doc = Document::default();
        let tri_block = |z: f64| match tri(0.0, 0.0, z, 0.5).geometry {
            Geometry::Mesh(m) => vec![crate::BlockGeometry::Mesh(m)],
            _ => unreachable!(),
        };
        doc.blocks.insert("b".into(), tri_block(0.0));
        add(&mut doc, tri(-0.3, -0.3, 0.1, 0.8));
        let mut inst = poly(&[], false);
        inst.geometry = Geometry::Instance {
            block: "b".into(),
            position: DVec3::new(-0.2, -0.2, 0.0),
            rotation_deg: 0.0,
            scale: 1.0,
            source: None,
            params: Default::default(),
            clip: None,
        };
        let inst = add(&mut doc, inst);
        let low = assert_cached_matches_fresh(&doc);
        // A direct `blocks` edit touches no object, so the cache stays warm.
        doc.blocks.insert("b".into(), tri_block(0.3));
        let high = assert_cached_matches_fresh(&doc);
        assert!(high.contains(&Some(inst)) && low != high, "the raised block wins clicks");
    }
}
