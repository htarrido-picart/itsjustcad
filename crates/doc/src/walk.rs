// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Walkthrough physics (#369, SketchUp's Position Camera / Walk): where the
//! eye stands, what floor it is over, and how far it may walk before a face
//! stops it. Pure geometry over the visible, section-clipped surfaces
//! ([`crate::pick::face_at`]); the camera itself is the caller's.
//!
//! The floor under a point is the nearest surface straight below it, else
//! the ground plane z = 0 when the point is above it. Walking keeps the
//! eye's height above that floor, so the eye climbs stairs and ramps and
//! drops off ledges. With collision on, a face ahead (at eye height or at
//! knee height) stops the eye [`WALL_GAP`] short of it, and a rise of more
//! than [`MAX_STEP`] is a wall too; with it off the eye walks through.

use glam::DVec3;

use crate::Document;
use crate::pick::face_at;

/// Default eye height in metres: SketchUp's 5'6" (1.676 m), rounded.
pub const EYE_HEIGHT: f64 = 1.68;
/// How far short of a face ahead the eye stops, in metres.
pub const WALL_GAP: f64 = 0.3;
/// The highest rise walked up in one go (a step or a kerb), in metres; a
/// higher one is a wall when colliding. The knee ray is cast this high
/// above the floor, so stair risers below it do not stop the eye.
pub const MAX_STEP: f64 = 0.6;
/// The lowest an eye may be lowered above its floor while colliding.
pub const MIN_EYE: f64 = 0.1;

/// The floor under `p`: the nearest surface straight below it, or the ground
/// plane z = 0 when that is nearer (or nothing is below) and `p` is above
/// it. `None` below the ground with no surface under it.
pub fn floor_below(doc: &Document, p: DVec3) -> Option<f64> {
    let face = face_at(doc, p, DVec3::NEG_Z).map(|h| h.point.z);
    let ground = (p.z > 0.0).then_some(0.0);
    match (face, ground) {
        (Some(f), Some(g)) => Some(f.max(g)),
        (f, g) => f.or(g),
    }
}

/// Position Camera: the eye standing `height` above `point` (a clicked face
/// or the ground).
pub fn stand(point: DVec3, height: f64) -> DVec3 {
    point + DVec3::Z * height
}

/// The eye at `eye` raised or lowered to `height` above its floor (a typed
/// eye height in Look Around or Walk). With no floor the height is taken
/// above the ground plane.
pub fn at_height(doc: &Document, eye: DVec3, height: f64) -> DVec3 {
    let floor = floor_below(doc, eye).unwrap_or(0.0);
    DVec3::new(eye.x, eye.y, floor + height)
}

/// One Walk step: the eye at `eye` moved by `delta` (metres; its horizontal
/// part walks, its z flies up or down as Shift does). The horizontal move
/// keeps the eye's height above the floor (stairs, slopes, ledges). With
/// `collide`, a face ahead at eye or knee height stops the eye [`WALL_GAP`]
/// short of it, a rise above [`MAX_STEP`] blocks the step, and flying down
/// stops [`MIN_EYE`] above the floor; without it the eye goes where asked.
pub fn step(doc: &Document, eye: DVec3, delta: DVec3, collide: bool) -> DVec3 {
    if !eye.is_finite() || !delta.is_finite() {
        return eye;
    }
    let mut out = eye;
    let flat = DVec3::new(delta.x, delta.y, 0.0);
    let dist = flat.length();
    if dist > 1e-9 {
        let dir = flat / dist;
        let floor0 = floor_below(doc, eye);
        let mut go = dist;
        if collide {
            let feet = floor0.unwrap_or(eye.z - EYE_HEIGHT);
            let knee = (feet + MAX_STEP).min(eye.z);
            for z in [eye.z, knee] {
                if let Some(hit) = face_at(doc, DVec3::new(eye.x, eye.y, z), dir) {
                    go = go.min((hit.t - WALL_GAP).max(0.0));
                }
            }
        }
        let next = eye + dir * go;
        match (floor0, floor_below(doc, next)) {
            (Some(f0), Some(f1)) if collide && f1 - f0 > MAX_STEP => {}
            (Some(f0), Some(f1)) => out = next + DVec3::Z * (f1 - f0),
            _ => out = next,
        }
    }
    if delta.z != 0.0 {
        out.z += delta.z;
        if collide
            && delta.z < 0.0
            && let Some(f) = floor_below(doc, DVec3::new(out.x, out.y, out.z.max(eye.z)))
        {
            out.z = out.z.max((f + MIN_EYE).min(eye.z));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Geometry, ObjectId, SceneObject};

    fn solid(mesh: kernel_mesh::Mesh) -> SceneObject {
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
            geometry: Geometry::Mesh(mesh),
        }
    }

    fn doc_with(meshes: Vec<kernel_mesh::Mesh>) -> Document {
        let mut doc = Document::default();
        for m in meshes {
            doc.insert(solid(m));
        }
        doc
    }

    fn cuboid(min: [f64; 3], size: [f64; 3]) -> kernel_mesh::Mesh {
        kernel_mesh::make_box(DVec3::from(min), DVec3::from(size))
    }

    /// Walk `eye` along +X by `dist` in `n` steps.
    fn walk_x(doc: &Document, mut eye: DVec3, dist: f64, n: usize, collide: bool) -> DVec3 {
        for _ in 0..n {
            eye = step(doc, eye, DVec3::X * (dist / n as f64), collide);
        }
        eye
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn stand_puts_the_eye_above_the_point() {
        let p = DVec3::new(1.0, 2.0, 0.5);
        assert_eq!(stand(p, EYE_HEIGHT), DVec3::new(1.0, 2.0, 0.5 + EYE_HEIGHT));
        let doc = doc_with(vec![cuboid([0.0, 0.0, 0.0], [4.0, 4.0, 1.0])]);
        assert!(close(floor_below(&doc, DVec3::new(2.0, 2.0, 3.0)).unwrap(), 1.0));
        assert!(close(floor_below(&doc, DVec3::new(9.0, 2.0, 3.0)).unwrap(), 0.0), "ground");
        assert_eq!(floor_below(&doc, DVec3::new(9.0, 2.0, -1.0)), None);
        let up = at_height(&doc, DVec3::new(2.0, 2.0, 5.0), 1.7);
        assert!(close(up.z, 2.7), "{up}");
    }

    /// A 0.18 m step: the eye climbs it and keeps its height.
    #[test]
    fn walk_climbs_a_step() {
        let doc = doc_with(vec![cuboid([2.0, -5.0, 0.0], [10.0, 10.0, 0.18])]);
        let eye = walk_x(&doc, DVec3::new(0.0, 0.0, EYE_HEIGHT), 4.0, 40, true);
        assert!(close(eye.x, 4.0), "{eye}");
        assert!(close(eye.z, 0.18 + EYE_HEIGHT), "{eye}");
        // And drops off the far edge onto the ground.
        let eye = walk_x(&doc, eye, 10.0, 100, true);
        assert!(close(eye.z, EYE_HEIGHT), "{eye}");
    }

    /// A 1:10 ramp from x = 0 to x = 10: the eye rises with it.
    #[test]
    fn walk_follows_a_ramp() {
        let ramp = kernel_mesh::Mesh::new(
            vec![
                DVec3::new(0.0, -3.0, 0.0),
                DVec3::new(10.0, -3.0, 1.0),
                DVec3::new(10.0, 3.0, 1.0),
                DVec3::new(0.0, 3.0, 0.0),
            ],
            vec![[0, 1, 2], [0, 2, 3]],
        );
        let doc = doc_with(vec![ramp]);
        let eye = walk_x(&doc, DVec3::new(-2.0, 0.0, EYE_HEIGHT), 7.0, 70, true);
        assert!(close(eye.x, 5.0), "{eye}");
        assert!((eye.z - (0.5 + EYE_HEIGHT)).abs() < 1e-6, "{eye}");
    }

    /// A wall ahead stops the eye 0.3 m short; without collision (Alt) it
    /// walks through.
    #[test]
    fn collision_stops_before_a_wall_and_alt_passes() {
        let doc = doc_with(vec![cuboid([5.0, -5.0, 0.0], [0.2, 10.0, 3.0])]);
        let start = DVec3::new(0.0, 0.0, EYE_HEIGHT);
        let eye = walk_x(&doc, start, 10.0, 50, true);
        assert!((eye.x - (5.0 - WALL_GAP)).abs() < 1e-6, "{eye}");
        let through = walk_x(&doc, start, 10.0, 50, false);
        assert!(close(through.x, 10.0) && close(through.z, EYE_HEIGHT), "{through}");
        // A low wall (below the eye, above the knee) stops it too.
        let doc = doc_with(vec![cuboid([5.0, -5.0, 0.0], [0.2, 10.0, 1.0])]);
        let eye = walk_x(&doc, start, 10.0, 50, true);
        assert!(eye.x < 5.0, "{eye}");
    }

    /// Shift flies up and down; colliding, not through the floor.
    #[test]
    fn flying_stops_above_the_floor() {
        let doc = doc_with(vec![]);
        let eye = step(&doc, DVec3::new(0.0, 0.0, EYE_HEIGHT), DVec3::Z * 2.0, true);
        assert!(close(eye.z, EYE_HEIGHT + 2.0));
        let eye = step(&doc, eye, DVec3::Z * -10.0, true);
        assert!(close(eye.z, MIN_EYE), "{eye}");
        let eye = step(&doc, eye, DVec3::Z * -1.0, false);
        assert!(close(eye.z, MIN_EYE - 1.0), "{eye}");
    }
}
