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
}

impl Default for OrbitCamera {
    fn default() -> Self {
        Self {
            target: Vec3::new(0.0, 0.0, 1.0),
            distance: 30.0,
            yaw: -std::f32::consts::FRAC_PI_4,
            pitch: 0.5,
            fov_y: 45f32.to_radians(),
        }
    }
}

impl OrbitCamera {
    pub fn eye(&self) -> Vec3 {
        let (sp, cp) = self.pitch.sin_cos();
        let (sy, cy) = self.yaw.sin_cos();
        self.target + self.distance * Vec3::new(cp * cy, cp * sy, sp)
    }

    pub fn orbit(&mut self, dx: f32, dy: f32) {
        self.yaw -= dx * 0.01;
        self.pitch = (self.pitch + dy * 0.01).clamp(
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
        let proj = Mat4::perspective_rh(self.fov_y, aspect.max(1e-3), 0.05, 5.0e4);
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
