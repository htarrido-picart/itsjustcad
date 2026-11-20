// SPDX-License-Identifier: AGPL-3.0-only

//! Viewport ambient occlusion (#358, SketchUp's Face Style ▸ Ambient
//! Occlusion): concave corners and the faces close to each other darken.
//!
//! One [`AoStyle`] holds every setting so a named style (#362) can serialize
//! it whole. The default is off, today's look.
//!
//! The occlusion is screen-space (see `crate::ao` for the passes): it is
//! computed per pane from a depth + normal prepass of the faces, blurred
//! depth-aware, and darkens the face fill toward [`AoStyle::color`] before the
//! fog (#363) is applied. It acts on the opaque fills only
//! ([`crate::DisplayMode::takes_ao`]: Shaded and its Massing style, Pencil and
//! Sketchy); wireframe, x-ray, ghosted and hidden line skip it. It is
//! GPU-only: a change rewrites the camera uniform and never rebuilds the
//! scene.

use serde::{Deserialize, Serialize};

/// How [`AoStyle::distance`] is measured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AoUnits {
    /// A fraction of the scene's bounding-box diagonal, so one setting reads
    /// the same on a chair and a site.
    #[default]
    Scene,
    /// World units (model metres), like SketchUp's Distance.
    World,
}

/// Every ambient-occlusion setting of the viewport (#358). Serialized into
/// `ui.json` and named styles; missing keys take the defaults.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AoStyle {
    pub on: bool,
    /// How far a face looks for occluders (the sampling radius), in
    /// [`Self::units`].
    pub distance: f32,
    pub units: AoUnits,
    /// How dark a fully occluded point gets, 0..1.
    pub intensity: f32,
    /// The colour occluded faces darken toward, sRGB bytes (black).
    pub color: [u8; 3],
}

/// Default distance: 3% of the scene diagonal.
pub const AO_DISTANCE: f32 = 0.03;
/// Default intensity.
pub const AO_INTENSITY: f32 = 0.6;

impl Default for AoStyle {
    fn default() -> Self {
        AoStyle {
            on: false,
            distance: AO_DISTANCE,
            units: AoUnits::Scene,
            intensity: AO_INTENSITY,
            color: [0, 0, 0],
        }
    }
}

impl AoStyle {
    /// The sampling radius in world units for a scene whose bounding-box
    /// diagonal is `scene_diagonal`. Bad values (hand-edited `ui.json`) fall
    /// back to the defaults; never zero.
    pub fn radius(self, scene_diagonal: f32) -> f32 {
        let d = if self.distance.is_finite() && self.distance > 0.0 { self.distance } else { AO_DISTANCE };
        let r = match self.units {
            AoUnits::Scene => d.min(1.0) * scene_diagonal.max(1e-3),
            AoUnits::World => d,
        };
        r.max(1e-4)
    }

    /// [`Self::intensity`] clamped to 0..1.
    pub fn strength(self) -> f32 {
        if self.intensity.is_finite() { self.intensity.clamp(0.0, 1.0) } else { AO_INTENSITY }
    }

    /// [`Self::color`] as RGBA floats in 0..1.
    pub fn rgba(self) -> [f32; 4] {
        let [r, g, b] = self.color.map(|c| c as f32 / 255.0);
        [r, g, b, 1.0]
    }

    /// One-line summary for the command line.
    pub fn summary(self) -> String {
        let distance = match self.units {
            AoUnits::Scene => format!("{}% of the scene", (self.distance * 1000.0).round() / 10.0),
            AoUnits::World => format!("{} m", self.distance),
        };
        let [r, g, b] = self.color;
        format!(
            "ambient occlusion: {} · distance {distance} · intensity {} · colour #{r:02x}{g:02x}{b:02x}",
            if self.on { "on" } else { "off" },
            (self.strength() * 100.0).round() / 100.0,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DisplayMode;

    #[test]
    fn defaults_are_off_and_fill_missing_keys() {
        let d = AoStyle::default();
        assert!(!d.on);
        assert_eq!((d.units, d.color), (AoUnits::Scene, [0, 0, 0]));
        let empty: AoStyle = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, d);
        let partial: AoStyle = serde_json::from_str(r#"{ "on": true, "later": 2 }"#).unwrap();
        assert_eq!(partial, AoStyle { on: true, ..d });
        let world = AoStyle { units: AoUnits::World, distance: 0.5, color: [1, 2, 3], ..d };
        let json = serde_json::to_value(world).unwrap();
        assert_eq!(json["units"], "world");
        assert_eq!(serde_json::from_value::<AoStyle>(json).unwrap(), world);
    }

    #[test]
    fn radius_follows_the_units_and_repairs_bad_values() {
        let s = AoStyle { distance: 0.1, ..Default::default() };
        assert!((s.radius(20.0) - 2.0).abs() < 1e-6, "10% of a 20 m diagonal");
        let w = AoStyle { units: AoUnits::World, distance: 0.3, ..s };
        assert_eq!(w.radius(20.0), 0.3, "world units ignore the scene");
        let nan = AoStyle { distance: f32::NAN, ..s };
        assert!((nan.radius(100.0) - 3.0).abs() < 1e-5);
        assert!(AoStyle { distance: -1.0, ..w }.radius(1.0) > 0.0);
        assert_eq!(AoStyle { intensity: 4.0, ..s }.strength(), 1.0);
        assert_eq!(AoStyle { intensity: f32::NAN, ..s }.strength(), AO_INTENSITY);
    }

    #[test]
    fn camera_carries_ao_only_when_on_in_a_fill_mode() {
        let cam = crate::camera_uniform(glam::Mat4::IDENTITY, glam::Vec3::ZERO);
        let off = cam.with_ao(&AoStyle::default(), DisplayMode::Shaded);
        assert_eq!(bytemuck::bytes_of(&off), bytemuck::bytes_of(&cam), "off = today's camera");
        assert!(!off.has_ao());
        let on = AoStyle { on: true, intensity: 0.5, color: [255, 0, 0], ..Default::default() };
        let c = cam.with_ao(&on, DisplayMode::Shaded);
        assert!(c.has_ao());
        assert_eq!(c.ao[..3], [1.0, 0.5, -AO_DISTANCE]);
        assert_eq!(c.ao_color, [1.0, 0.0, 0.0, 1.0]);
        let world = cam.with_ao(&AoStyle { units: AoUnits::World, distance: 0.4, ..on }, DisplayMode::Pencil);
        assert_eq!((world.ao[2], world.ao[3]), (0.4, 0.4), "world radius needs no scene");
        for mode in [DisplayMode::Wireframe, DisplayMode::XRay, DisplayMode::Ghosted, DisplayMode::HiddenLine] {
            assert!(!cam.with_ao(&on, mode).has_ao(), "{mode:?} skips AO");
        }
        for mode in [DisplayMode::Shaded, DisplayMode::Pencil, DisplayMode::Sketchy] {
            assert!(mode.takes_ao(), "{mode:?}");
        }
    }

    #[test]
    fn shader_darkens_before_the_fog() {
        let src = include_str!("shaders/mesh.wgsl");
        assert!(src.contains("fogged(occluded(shade_fill("), "AO first, then fog");
    }

    #[test]
    fn summary_reads_the_settings() {
        let s = AoStyle { on: true, distance: 0.05, intensity: 0.75, ..Default::default() };
        assert_eq!(
            s.summary(),
            "ambient occlusion: on · distance 5% of the scene · intensity 0.75 · colour #000000"
        );
        let w = AoStyle { units: AoUnits::World, distance: 0.5, color: [255, 128, 0], ..s };
        assert!(w.summary().contains("distance 0.5 m") && w.summary().ends_with("#ff8000"));
    }
}
