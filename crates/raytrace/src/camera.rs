// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Perspective camera + primary-ray generation. Conventions match the
//! viewport's [`itsjustcad_render::OrbitCamera`] (Z-up world, right-handed,
//! vertical field of view) so a ray-traced frame lines up with what the user
//! sees in the 3D view. We take the already-resolved eye/target/fov (the app's
//! `build_headless_camera` computes these from the doc + view state) and only
//! generate rays here.

use glam::DVec3;

/// A ray-tracing pinhole camera in f64 world space.
#[derive(Clone, Copy, Debug)]
pub struct Camera {
    origin: DVec3,
    /// Lower-left corner of the virtual image plane at unit forward distance.
    lower_left: DVec3,
    horizontal: DVec3,
    vertical: DVec3,
}

impl Camera {
    /// Build from eye/target/up + vertical fov (radians) + aspect (w/h). Matches
    /// `Mat4::look_at_rh` + `perspective_rh` framing so the ray image aligns with
    /// the rasterised viewport.
    pub fn look_at(eye: DVec3, target: DVec3, up: DVec3, fov_y: f64, aspect: f64) -> Self {
        let half_h = (fov_y * 0.5).tan();
        let half_w = half_h * aspect.max(1e-6);
        // Right-handed view basis: forward is toward the target.
        let forward = (target - eye).normalize_or_zero();
        let forward = if forward == DVec3::ZERO { DVec3::X } else { forward };
        // Guard against a degenerate up (looking straight along it).
        let up = if forward.cross(up).length_squared() < 1e-12 {
            DVec3::Y
        } else {
            up
        };
        let right = forward.cross(up).normalize();
        let true_up = right.cross(forward);
        let horizontal = right * (2.0 * half_w);
        let vertical = true_up * (2.0 * half_h);
        let lower_left = eye + forward - horizontal * 0.5 - vertical * 0.5;
        Self { origin: eye, lower_left, horizontal, vertical }
    }

    pub fn origin(&self) -> DVec3 {
        self.origin
    }

    /// Primary ray direction (unit) for normalised image coords `(s, t)` in
    /// `[0,1]`, with `t = 0` at the bottom of the frame.
    pub fn ray(&self, s: f64, t: f64) -> DVec3 {
        (self.lower_left + self.horizontal * s + self.vertical * t - self.origin).normalize()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn center_ray_points_at_target() {
        let eye = DVec3::new(0.0, -10.0, 0.0);
        let target = DVec3::ZERO;
        let cam = Camera::look_at(eye, target, DVec3::Z, 45f64.to_radians(), 1.0);
        let d = cam.ray(0.5, 0.5);
        let forward = (target - eye).normalize();
        assert!((d - forward).length() < 1e-9, "center ray = forward: {d}");
    }

    #[test]
    fn right_edge_ray_tilts_toward_right() {
        let eye = DVec3::new(0.0, -10.0, 0.0);
        let cam = Camera::look_at(eye, DVec3::ZERO, DVec3::Z, 45f64.to_radians(), 1.0);
        let right = cam.ray(1.0, 0.5);
        // Looking down +Y, world +X is to the right of frame → ray gains +X.
        assert!(right.x > 0.0, "right edge tilts +X: {right}");
    }

    #[test]
    fn top_ray_tilts_up() {
        let eye = DVec3::new(0.0, -10.0, 0.0);
        let cam = Camera::look_at(eye, DVec3::ZERO, DVec3::Z, 45f64.to_radians(), 1.0);
        let top = cam.ray(0.5, 1.0);
        assert!(top.z > 0.0, "top edge tilts +Z: {top}");
    }

    #[test]
    fn all_rays_are_unit() {
        let cam = Camera::look_at(DVec3::new(3.0, 3.0, 3.0), DVec3::ZERO, DVec3::Z, 1.0, 1.6);
        for (s, t) in [(0.0, 0.0), (1.0, 1.0), (0.3, 0.7), (0.5, 0.5)] {
            assert!((cam.ray(s, t).length() - 1.0).abs() < 1e-9);
        }
    }
}
