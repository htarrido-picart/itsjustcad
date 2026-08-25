// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

use glam::{Mat4, Vec3};

use crate::pano::PanoProjection;

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
    /// Two-point (architectural) perspective: world verticals stay vertical and
    /// parallel on screen. The eye looks horizontally (pitch is removed from the
    /// view basis) and the pitch is reintroduced as a vertical shear of the
    /// projection, so a tower still "leans into frame" without its edges
    /// converging. Ignored when `ortho`.
    pub two_point: bool,
    /// Non-pinhole projection (panorama / fisheye). When `Some`, `view_proj`
    /// cannot express the image: the renderer must capture a cubemap from the
    /// eye and remap it in a post pass (see [`crate::pano`]). `None` =
    /// ordinary pinhole (ortho / two-point / perspective as above).
    pub pano: Option<PanoProjection>,
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
            two_point: false,
            pano: None,
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
        let aspect = aspect.max(1e-3);
        if self.ortho {
            // Straight-down/up views degenerate with a Z up-vector; fall back to Y.
            let up = if self.pitch.abs() > 1.55 { Vec3::Y } else { Vec3::Z };
            let view = Mat4::look_at_rh(self.eye(), self.target, up);
            // Half-height matches the perspective frustum at the target, so
            // dolly (distance) still reads as zoom and view switches keep the
            // apparent scale.
            let half_h = self.distance * (self.fov_y * 0.5).tan();
            let half_w = half_h * aspect;
            // Symmetric depth range: parallel views must not clip geometry
            // that sits behind the eye plane.
            let proj = Mat4::orthographic_rh(-half_w, half_w, -half_h, half_h, -5.0e4, 5.0e4);
            return proj * view;
        }
        if self.two_point {
            return self.two_point_view_proj(aspect);
        }
        let up = if self.pitch.abs() > 1.55 { Vec3::Y } else { Vec3::Z };
        let view = Mat4::look_at_rh(self.eye(), self.target, up);
        let proj = Mat4::perspective_rh(self.fov_y, aspect, 0.05, 5.0e4);
        proj * view
    }

    /// Two-point perspective: the view basis is levelled (the forward axis is
    /// forced horizontal) so world verticals never converge, and the removed
    /// pitch is folded back in as a vertical shear on the projection. The shear
    /// is `tan(pitch)` scaled by `cot(fov_y/2)` (i.e. divided by the half-height
    /// at unit depth) so panning the pitch tracks the same framing a normal
    /// perspective would give at the target, just without vertical convergence.
    fn two_point_view_proj(&self, aspect: f32) -> Mat4 {
        // Levelled eye: same yaw/target/distance, but the eye sits at target
        // height so the forward axis is horizontal.
        let (sy, cy) = self.yaw.sin_cos();
        let horiz = Vec3::new(cy, sy, 0.0);
        let eye = self.target + self.distance * horiz;
        // Look horizontally toward a level target so verticals stay vertical.
        let level_target = self.target;
        let level_target = Vec3::new(level_target.x, level_target.y, eye.z);
        let view = Mat4::look_at_rh(eye, level_target, Vec3::Z);
        let proj = Mat4::perspective_rh(self.fov_y, aspect, 0.05, 5.0e4);
        // Vertical frame shift: we want `clip.y += shear * clip.w`, which after
        // the perspective divide is a constant `ndc.y += shear` — the whole
        // image slides up/down with pitch while verticals stay parallel. clip.w
        // is carried by proj.z_axis.w (= -1 for perspective_rh), so folding the
        // shift into the z->y coupling makes it survive the divide at every
        // depth. `shear = tan(pitch)/tan(fov_y/2)` maps pitch to NDC so it
        // tracks the framing a normal perspective gives at the target.
        // Negative sign: tilting the eye up (positive pitch) slides scene content
        // *down* the frame, exactly as a real lens shift / tilted sensor does, so
        // a tall tower's top drops toward center as you "look up".
        let shear = -self.pitch.tan() / (self.fov_y * 0.5).tan();
        let mut m = proj; // glam Mat4 is column-major; y-row = .y of each column.
        m.z_axis.y += shear * proj.z_axis.w;
        m.w_axis.y += shear * proj.w_axis.w; // 0 for perspective, kept for clarity.
        m * view
    }

    /// Full-frame lens: 36mm sensor width, `fov = 2*atan(18/f)` (horizontal).
    /// Stored as a vertical fov via the current aspect so framing matches a
    /// real camera's horizontal angle of view.
    pub fn set_lens_mm(&mut self, focal_mm: f32, aspect: f32) {
        let fov_h = fov_for_focal_mm(focal_mm);
        // Convert horizontal fov to vertical for the stored fov_y.
        let half_w = (fov_h * 0.5).tan();
        let half_h = half_w / aspect.max(1e-3);
        self.fov_y = 2.0 * half_h.atan();
        self.ortho = false;
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
        // A standard view switch drops two-point mode; it is a `camera` opt-in.
        self.two_point = false;
        // …and drops any panorama/fisheye projection for the same reason.
        self.pano = None;
    }

    /// Orthonormal view basis `(right, up, forward)` used when capturing the
    /// cubemap for a panorama/fisheye. `forward` is the horizontal look
    /// direction from yaw; pitch tilts the basis up/down. Z-up world, so the
    /// reference up is +Z. Degenerates near the poles are handled by falling
    /// back to +Y, mirroring `view_proj`.
    pub fn view_basis(&self) -> (Vec3, Vec3, Vec3) {
        let forward = (self.target - self.eye()).normalize_or_zero();
        let forward = if forward == Vec3::ZERO { Vec3::X } else { forward };
        let world_up = if forward.z.abs() > 0.999 { Vec3::Y } else { Vec3::Z };
        let right = forward.cross(world_up).normalize_or_zero();
        let right = if right == Vec3::ZERO { Vec3::X } else { right };
        let up = right.cross(forward).normalize();
        (right, up, forward)
    }
}

/// Horizontal angle of view for a full-frame (36mm-wide sensor) lens:
/// `fov = 2*atan(18/f)`. Returned in radians.
pub fn fov_for_focal_mm(focal_mm: f32) -> f32 {
    2.0 * (18.0 / focal_mm.max(1e-3)).atan()
}

/// Full-frame-equivalent focal length for a named `camera` preset, or `None`
/// if the token is not a recognised lens/preset. Numeric forms like "35mm" are
/// handled by the caller; this covers the phone lens sims.
///
/// The bare aliases keep the original short spelling (`phone` = the iPhone main
/// wide, `phonewide` = its ultra-wide); the richer set is reachable by name via
/// [`phone_preset`] / [`PHONE_PRESETS`] and the `camera phone <name>` verb.
pub fn preset_focal_mm(name: &str) -> Option<f32> {
    match name {
        // Back-compat shorthands.
        "phone" => Some(26.0),
        "phonewide" => Some(13.0),
        "phonetele" => Some(77.0),
        // Otherwise fall through to the named phone-lens table.
        other => phone_preset(other).map(|p| p.focal_mm),
    }
}

/// A simulated phone camera lens: a 35mm-equivalent focal length (the number
/// phone makers quote), so the same [`fov_for_focal_mm`] full-frame math gives
/// the real horizontal angle of view. Real phone sensors are tiny (a few mm),
/// but the *equivalent* focal already folds in the sensor crop factor, so we
/// only ever need the equivalent to reproduce the framing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhonePreset {
    /// Lookup token used by `camera phone <name>`.
    pub name: &'static str,
    /// 35mm-equivalent focal length in mm.
    pub focal_mm: f32,
    /// Human label for the command echo.
    pub label: &'static str,
}

/// Named phone-lens presets. Focal lengths are the makers' published 35mm
/// equivalents (ultra-wide ≈ 13mm, main wide ≈ 24–26mm, tele ≈ 70–120mm), so
/// `fov_for_focal_mm` reproduces each lens's real horizontal angle of view.
pub const PHONE_PRESETS: &[PhonePreset] = &[
    // Apple iPhone (Pro-class three-camera system).
    PhonePreset { name: "iphone-ultrawide", focal_mm: 13.0, label: "iPhone ultra-wide (13mm eq)" },
    PhonePreset { name: "iphone-main", focal_mm: 26.0, label: "iPhone main wide (26mm eq)" },
    PhonePreset { name: "iphone-tele", focal_mm: 77.0, label: "iPhone telephoto (77mm eq)" },
    // Google Pixel.
    PhonePreset { name: "pixel-ultrawide", focal_mm: 13.0, label: "Pixel ultra-wide (13mm eq)" },
    PhonePreset { name: "pixel-main", focal_mm: 25.0, label: "Pixel main wide (25mm eq)" },
    PhonePreset { name: "pixel-tele", focal_mm: 112.0, label: "Pixel telephoto (112mm eq)" },
    // Samsung Galaxy S-series.
    PhonePreset { name: "galaxy-ultrawide", focal_mm: 13.0, label: "Galaxy ultra-wide (13mm eq)" },
    PhonePreset { name: "galaxy-main", focal_mm: 24.0, label: "Galaxy main wide (24mm eq)" },
    PhonePreset { name: "galaxy-tele", focal_mm: 70.0, label: "Galaxy telephoto (70mm eq)" },
];

/// Look up a named phone-lens preset (case-sensitive; callers lower-case first).
pub fn phone_preset(name: &str) -> Option<PhonePreset> {
    PHONE_PRESETS.iter().copied().find(|p| p.name == name)
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
    fn fov_matches_known_full_frame_lenses() {
        // Canonical horizontal angles of view for a 36mm-wide sensor.
        // (references: 50mm≈39.6°, 35mm≈54.4°, 24mm≈73.7°, 85mm≈23.9°, 15mm≈100.4°)
        for (f, deg) in [(50.0, 39.6), (35.0, 54.4), (24.0, 73.7), (85.0, 23.9), (15.0, 100.4)] {
            let got = fov_for_focal_mm(f).to_degrees();
            assert!((got - deg).abs() < 0.2, "{f}mm -> {got}° expected ~{deg}°");
        }
    }

    #[test]
    fn longer_lens_is_narrower_fov() {
        assert!(fov_for_focal_mm(85.0) < fov_for_focal_mm(50.0));
        assert!(fov_for_focal_mm(50.0) < fov_for_focal_mm(15.0));
    }

    #[test]
    fn phone_presets_map_to_equivalents() {
        assert_eq!(preset_focal_mm("phone"), Some(26.0));
        assert_eq!(preset_focal_mm("phonewide"), Some(13.0));
        assert_eq!(preset_focal_mm("phonetele"), Some(77.0));
        assert_eq!(preset_focal_mm("nope"), None);
        // Ultra-wide phone is wider than the main camera.
        assert!(fov_for_focal_mm(13.0) > fov_for_focal_mm(26.0));
    }

    #[test]
    fn named_phone_presets_resolve_and_order_by_fov() {
        // Every named preset resolves through both the table and the shorthand.
        for p in PHONE_PRESETS {
            assert_eq!(phone_preset(p.name), Some(*p));
            assert_eq!(preset_focal_mm(p.name), Some(p.focal_mm));
        }
        assert_eq!(phone_preset("does-not-exist"), None);
        // Within one phone the ordering ultra-wide < main < tele holds (wider FOV
        // for shorter focal).
        let uw = phone_preset("iphone-ultrawide").unwrap().focal_mm;
        let main = phone_preset("iphone-main").unwrap().focal_mm;
        let tele = phone_preset("iphone-tele").unwrap().focal_mm;
        assert!(uw < main && main < tele);
        assert!(fov_for_focal_mm(uw) > fov_for_focal_mm(main));
        assert!(fov_for_focal_mm(main) > fov_for_focal_mm(tele));
    }

    #[test]
    fn iphone_main_matches_documented_fov() {
        // 26mm-equivalent full-frame gives ~69.4° horizontal AoV.
        let deg = fov_for_focal_mm(preset_focal_mm("phone").unwrap()).to_degrees();
        assert!((deg - 69.4).abs() < 0.5, "iPhone main ~26mm -> {deg}°");
    }

    #[test]
    fn set_lens_stores_horizontal_fov_via_aspect() {
        let mut cam = OrbitCamera::default();
        let aspect = 16.0 / 9.0;
        cam.set_lens_mm(50.0, aspect);
        // Reconstruct the horizontal fov from the stored vertical one.
        let half_h = (cam.fov_y * 0.5).tan();
        let fov_h = 2.0 * (half_h * aspect).atan();
        assert!((fov_h - fov_for_focal_mm(50.0)).abs() < 1e-4);
        assert!(!cam.ortho);
    }

    #[test]
    fn two_point_keeps_verticals_parallel() {
        let mut cam = OrbitCamera {
            // Look up at a tower: a pitch that would make a normal perspective
            // converge the verticals noticeably.
            target: Vec3::new(0.0, 0.0, 5.0),
            distance: 30.0,
            yaw: -std::f32::consts::FRAC_PI_2, // face +Y-ish, verticals across x
            pitch: 0.6,
            two_point: true,
            ..OrbitCamera::default()
        };
        let m = cam.view_proj(1.5);

        // Two vertical edges of a tower at x = ±4. For each, compare the screen
        // x at the base vs. the top; a true vertical stays at constant screen x.
        for x in [-4.0f32, 4.0] {
            let base = screen_xy(m, Vec3::new(x, 8.0, 0.0));
            let top = screen_xy(m, Vec3::new(x, 8.0, 40.0));
            assert!(
                (base.x - top.x).abs() < 1e-4,
                "two-point vertical must not lean: base.x={} top.x={}",
                base.x,
                top.x
            );
        }

        // Sanity: a normal perspective at the same pitch WOULD converge them.
        cam.two_point = false;
        let m = cam.view_proj(1.5);
        let base = screen_xy(m, Vec3::new(4.0, 8.0, 0.0));
        let top = screen_xy(m, Vec3::new(4.0, 8.0, 40.0));
        assert!(
            (base.x - top.x).abs() > 1e-3,
            "normal perspective should converge verticals"
        );
    }

    #[test]
    fn two_point_pitch_shifts_frame_vertically() {
        // Increasing pitch slides the framing up (the tower top moves toward
        // center) without tilting verticals.
        let mut cam = OrbitCamera {
            target: Vec3::new(0.0, 0.0, 5.0),
            distance: 30.0,
            yaw: -std::f32::consts::FRAC_PI_2,
            two_point: true,
            ..OrbitCamera::default()
        };

        cam.pitch = 0.0;
        let top_low = screen_xy(cam.view_proj(1.5), Vec3::new(0.0, 8.0, 40.0)).y;
        cam.pitch = 0.6;
        let top_high = screen_xy(cam.view_proj(1.5), Vec3::new(0.0, 8.0, 40.0)).y;
        assert!(top_high < top_low, "looking up should lower the top's NDC y toward center: {top_low} -> {top_high}");
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
