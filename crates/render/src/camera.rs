use glam::{Mat4, Vec3};

/// Z-up orbit camera with Rhino-style controls (RMB orbit, Shift+RMB pan, scroll dolly).
#[derive(Clone, Copy, Debug)]
pub struct OrbitCamera {
    pub target: Vec3,
    pub distance: f32,
    /// Radians around +Z, 0 = looking from +X.
    pub yaw: f32,
    /// Radians above the XY plane.
    pub pitch: f32,
    pub fov_y: f32,
    /// Parallel projection — true for the standard plan/elevation views
    /// (architects need measurable drawings, not perspective foreshortening).
    pub ortho: bool,
}

impl Default for OrbitCamera {
    fn default() -> Self {
        Self {
            target: Vec3::new(0.0, 0.0, 1.0),
            distance: 30.0,
            yaw: -std::f32::consts::FRAC_PI_4,
            pitch: 0.5,
            fov_y: 45f32.to_radians(),
            ortho: false,
        }
    }
}

impl OrbitCamera {
    pub fn eye(&self) -> Vec3 {
        let (sp, cp) = self.pitch.sin_cos();
        let (sy, cy) = self.yaw.sin_cos();
        self.target + self.distance * Vec3::new(cp * cy, cp * sy, sp)
    }

    /// Orbit sensitivity: 0.005 rad/px ≈ 0.29°/px, matching legacy CAD feel
    /// (AutoCAD/Rhino interactive rotation is roughly 0.25–0.35°/px).
    pub fn orbit(&mut self, dx: f32, dy: f32) {
        self.yaw -= dx * 0.005;
        self.pitch = (self.pitch + dy * 0.005).clamp(
            -std::f32::consts::FRAC_PI_2,
            std::f32::consts::FRAC_PI_2,
        );
    }

    /// Pan in screen space; keeps the point under the cursor roughly fixed.
    pub fn pan(&mut self, dx: f32, dy: f32) {
        let eye = self.eye();
        let forward = (self.target - eye).normalize();
        let right = forward.cross(Vec3::Z).normalize();
        let up = right.cross(forward);
        let scale = self.distance * 0.0016;
        self.target += (-right * dx + up * dy) * scale;
    }

    pub fn dolly(&mut self, scroll: f32) {
        self.distance = (self.distance * (1.0 - scroll * 0.002)).clamp(0.05, 1.0e5);
    }

    pub fn view_proj(&self, aspect: f32) -> Mat4 {
        // Straight-down/up views degenerate with a Z up-vector; fall back to Y.
        let up = if self.pitch.abs() > 1.55 { Vec3::Y } else { Vec3::Z };
        let view = Mat4::look_at_rh(self.eye(), self.target, up);
        let aspect = aspect.max(1e-3);
        let proj = if self.ortho {
            // Half-height matches the perspective frustum at the target, so
            // dolly (distance) still reads as zoom and view switches keep the
            // apparent scale.
            let half_h = self.distance * (self.fov_y * 0.5).tan();
            let half_w = half_h * aspect;
            // Symmetric depth range: parallel views must not clip geometry
            // that sits behind the eye plane.
            Mat4::orthographic_rh(-half_w, half_w, -half_h, half_h, -5.0e4, 5.0e4)
        } else {
            Mat4::perspective_rh(self.fov_y, aspect, 0.05, 5.0e4)
        };
        proj * view
    }

    /// Rhino-style standard views. Keeps target and distance.
    pub fn set_view(&mut self, view: StandardView) {
        use std::f32::consts::{FRAC_PI_2, FRAC_PI_4, PI};
        let (yaw, pitch) = match view {
            StandardView::Top => (-FRAC_PI_2, FRAC_PI_2),
            StandardView::Bottom => (-FRAC_PI_2, -FRAC_PI_2),
            StandardView::Front => (-FRAC_PI_2, 0.0),
            StandardView::Back => (FRAC_PI_2, 0.0),
            StandardView::Right => (0.0, 0.0),
            StandardView::Left => (PI, 0.0),
            StandardView::Perspective => (-FRAC_PI_4, 0.5),
        };
        self.yaw = yaw;
        self.pitch = pitch;
        // Plan/elevation views are parallel projections; only the 3D view is
        // perspective. Sticky until the next view switch (Rhino behavior).
        self.ortho = view != StandardView::Perspective;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StandardView {
    Top,
    Bottom,
    Front,
    Back,
    Left,
    Right,
    Perspective,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen_xy(m: Mat4, p: Vec3) -> glam::Vec2 {
        let c = m * p.extend(1.0);
        glam::Vec2::new(c.x / c.w, c.y / c.w)
    }

    #[test]
    fn standard_views_are_ortho_perspective_is_not() {
        let mut cam = OrbitCamera::default();
        assert!(!cam.ortho);
        cam.set_view(StandardView::Top);
        assert!(cam.ortho);
        cam.set_view(StandardView::Perspective);
        assert!(!cam.ortho);
    }

    #[test]
    fn top_view_ortho_has_no_foreshortening() {
        let mut cam = OrbitCamera::default();
        cam.set_view(StandardView::Top);
        let m = cam.view_proj(1.5);
        // Points differing only in height project to the same pixel.
        let a = screen_xy(m, Vec3::new(5.0, 5.0, 0.0));
        let b = screen_xy(m, Vec3::new(5.0, 5.0, 10.0));
        assert!((a - b).length() < 1e-5, "ortho must not foreshorten: {a} vs {b}");

        // Same points under perspective must land on different pixels.
        cam.set_view(StandardView::Perspective);
        cam.set_view(StandardView::Top); // back to top orientation…
        cam.ortho = false; // …but force perspective projection
        let m = cam.view_proj(1.5);
        let a = screen_xy(m, Vec3::new(5.0, 5.0, 0.0));
        let b = screen_xy(m, Vec3::new(5.0, 5.0, 10.0));
        assert!((a - b).length() > 1e-3, "perspective must foreshorten");
    }

    #[test]
    fn ortho_dolly_still_zooms() {
        let mut cam = OrbitCamera::default();
        cam.set_view(StandardView::Front);
        let before = screen_xy(cam.view_proj(1.0), Vec3::new(5.0, 0.0, 0.0));
        cam.distance *= 2.0;
        let after = screen_xy(cam.view_proj(1.0), Vec3::new(5.0, 0.0, 0.0));
        // Doubling distance halves the apparent size (point moves toward center).
        assert!((after.x.abs() - before.x.abs() / 2.0).abs() < 1e-4);
    }

    #[test]
    fn ortho_keeps_geometry_behind_eye_plane() {
        let mut cam = OrbitCamera::default();
        cam.set_view(StandardView::Top);
        cam.distance = 10.0;
        let m = cam.view_proj(1.0);
        // A point 100 above the eye still lands inside the depth range.
        let c = m * Vec3::new(0.0, 0.0, 110.0).extend(1.0);
        let z = c.z / c.w;
        assert!((0.0..=1.0).contains(&z) || (-1.0..=1.0).contains(&z), "z={z}");
    }
}
