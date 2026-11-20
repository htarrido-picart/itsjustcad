// SPDX-License-Identifier: AGPL-3.0-only

//! Viewport sun shadows (#357, SketchUp's Shadows panel): faces cast on
//! faces and on the ground, lit by the document sun.
//!
//! One [`ShadowStyle`] holds the view settings so a named style (#362) can
//! serialize it whole. The default is off, today's look.
//!
//! The sun itself is document state: `sun` / `location` (logged, undoable)
//! set `doc.sun`, and the shadows follow it. With no sun set, the shadows
//! fall from [`DEFAULT_SUN_DIR`], the Working light's 3/4 key, so the
//! toggle works on any model. The toggle, the light/dark sliders and the
//! ground switch are view state only (`ui.json`), never logged.
//!
//! One directional shadow map (see `crate::shadow`) is fitted to the scene's
//! faces and re-rendered only when the geometry or the sun changes, never per
//! camera frame. Shadows act on the opaque fills only
//! ([`crate::DisplayMode::takes_ao`]: Shaded and its Massing style, Pencil
//! and Sketchy); wireframe, x-ray, ghosted and hidden line skip them.

use serde::{Deserialize, Serialize};

/// Every shadow setting of the viewport (#357). Serialized into `ui.json`
/// and named styles; missing keys take the defaults.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShadowStyle {
    pub on: bool,
    /// Direct (sun) light intensity, 0..1 (SketchUp's Light slider).
    pub light: f32,
    /// Ambient intensity, 0..1: how bright shadowed and sun-facing-away
    /// faces stay (SketchUp's Dark slider).
    pub dark: f32,
    /// The ground plane catches shadows (SketchUp's On Ground).
    pub ground: bool,
}

/// Default direct light, SketchUp's 80.
pub const SHADOW_LIGHT: f32 = 0.8;
/// Default ambient, SketchUp's 45.
pub const SHADOW_DARK: f32 = 0.45;

/// Direction toward the sun when the document has none (X = East, Y =
/// North, Z = Up): the Working light's world-fixed 3/4 key, from the
/// upper front right, so turning shadows on keeps the familiar shading.
pub const DEFAULT_SUN_DIR: [f32; 3] = [0.4, -0.5, 0.75];

impl Default for ShadowStyle {
    fn default() -> Self {
        ShadowStyle { on: false, light: SHADOW_LIGHT, dark: SHADOW_DARK, ground: true }
    }
}

/// `v` clamped to 0..1, or `default` when it is not a number.
fn unit(v: f32, default: f32) -> f32 {
    if v.is_finite() { v.clamp(0.0, 1.0) } else { default }
}

impl ShadowStyle {
    /// [`Self::light`] clamped to 0..1 (bad values from a hand-edited
    /// `ui.json` fall back to the default).
    pub fn light_level(self) -> f32 {
        unit(self.light, SHADOW_LIGHT)
    }

    /// [`Self::dark`] clamped to 0..1.
    pub fn dark_level(self) -> f32 {
        unit(self.dark, SHADOW_DARK)
    }

    /// The unit direction toward the sun the shadows fall from: the
    /// document's, else [`DEFAULT_SUN_DIR`].
    pub fn sun_dir(sun: Option<[f32; 3]>) -> [f32; 3] {
        let d = glam::Vec3::from(sun.unwrap_or(DEFAULT_SUN_DIR)).normalize_or_zero();
        if d == glam::Vec3::ZERO { glam::Vec3::from(DEFAULT_SUN_DIR).normalize().into() } else { d.into() }
    }

    /// One-line summary for the command line.
    pub fn summary(self) -> String {
        format!(
            "shadows: {} · light {} · dark {} · on ground {}",
            if self.on { "on" } else { "off" },
            (self.light_level() * 100.0).round() / 100.0,
            (self.dark_level() * 100.0).round() / 100.0,
            if self.ground { "on" } else { "off" },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DisplayMode;

    #[test]
    fn defaults_are_off_and_fill_missing_keys() {
        let d = ShadowStyle::default();
        assert!(!d.on && d.ground);
        assert_eq!((d.light, d.dark), (SHADOW_LIGHT, SHADOW_DARK));
        let empty: ShadowStyle = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, d);
        let partial: ShadowStyle = serde_json::from_str(r#"{ "on": true, "later": 2 }"#).unwrap();
        assert_eq!(partial, ShadowStyle { on: true, ..d });
        let s = ShadowStyle { on: true, light: 0.3, dark: 0.9, ground: false };
        assert_eq!(serde_json::from_value::<ShadowStyle>(serde_json::to_value(s).unwrap()).unwrap(), s);
    }

    #[test]
    fn levels_clamp_and_repair_bad_values() {
        let s = ShadowStyle { light: 3.0, dark: -1.0, ..Default::default() };
        assert_eq!((s.light_level(), s.dark_level()), (1.0, 0.0));
        let nan = ShadowStyle { light: f32::NAN, dark: f32::INFINITY, ..s };
        assert_eq!((nan.light_level(), nan.dark_level()), (SHADOW_LIGHT, SHADOW_DARK));
    }

    #[test]
    fn sun_dir_is_the_documents_or_the_default_key() {
        let d = ShadowStyle::sun_dir(None);
        assert!((glam::Vec3::from(d).length() - 1.0).abs() < 1e-5);
        assert!((glam::Vec3::from(d) - glam::Vec3::from(DEFAULT_SUN_DIR).normalize()).length() < 1e-6);
        assert_eq!(ShadowStyle::sun_dir(Some([0.0, 0.0, 2.0])), [0.0, 0.0, 1.0]);
        assert_eq!(ShadowStyle::sun_dir(Some([0.0; 3])), d, "a zero sun falls back");
    }

    #[test]
    fn camera_carries_shadows_only_when_on_in_a_fill_mode() {
        let cam = crate::camera_uniform(glam::Mat4::IDENTITY, glam::Vec3::ZERO);
        let off = cam.with_shadows(&ShadowStyle::default(), DisplayMode::Shaded, Some([0.0, 0.0, 1.0]));
        assert_eq!(bytemuck::bytes_of(&off), bytemuck::bytes_of(&cam), "off = today's camera");
        assert!(!off.has_shadows());
        let on = ShadowStyle { on: true, light: 0.5, dark: 0.25, ground: false };
        let c = cam.with_shadows(&on, DisplayMode::Shaded, Some([0.0, 0.0, 1.0]));
        assert!(c.has_shadows());
        assert_eq!(c.shadow, [1.0, 0.5, 0.25, 0.0]);
        assert_eq!(c.shadow_dir, [0.0, 0.0, 1.0, 0.0]);
        let ground = cam.with_shadows(&ShadowStyle { on: true, ..Default::default() }, DisplayMode::Pencil, None);
        assert_eq!(ground.shadow[3], 1.0, "on ground");
        for mode in [DisplayMode::Wireframe, DisplayMode::XRay, DisplayMode::Ghosted, DisplayMode::HiddenLine] {
            assert!(!cam.with_shadows(&on, mode, None).has_shadows(), "{mode:?} skips shadows");
        }
    }

    #[test]
    fn shaders_shade_with_the_shadow_before_ao_and_fog() {
        let mesh = include_str!("shaders/mesh.wgsl");
        assert!(mesh.contains("fogged(occluded(shade_fill("), "shadow inside the shading, then AO, then fog");
        assert!(mesh.contains("sun_visibility("));
        assert!(include_str!("shaders/grid.wgsl").contains("ground_shadow("), "the ground catches");
    }

    #[test]
    fn summary_reads_the_settings() {
        assert_eq!(
            ShadowStyle::default().summary(),
            "shadows: off · light 0.8 · dark 0.45 · on ground on"
        );
        let s = ShadowStyle { on: true, light: 0.333, ground: false, ..Default::default() };
        assert_eq!(s.summary(), "shadows: on · light 0.33 · dark 0.45 · on ground off");
    }
}
