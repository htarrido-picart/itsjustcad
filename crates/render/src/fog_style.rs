// SPDX-License-Identifier: AGPL-3.0-only

//! Viewport fog (#363, SketchUp's Fog panel): geometry fades toward a fog
//! colour with distance from the camera.
//!
//! One [`FogStyle`] holds every setting so a named style (#362) can serialize
//! it whole. The default is off, today's look.
//!
//! Distances are fractions of the scene's view depth, not world lengths, so
//! one setting reads the same on a 2 m chair and a 2 km site: `0` is the
//! scene's nearest point along the view direction (or the eye, if nearer) and
//! `1` its farthest, the same ramp the depth cue uses (see
//! [`crate::CameraUniform::with_fog`]). Anything past the scene (the ground
//! grid running to the horizon) reads beyond `1`. Fog is a linear fade from
//! none at `start` to the full fog colour at `end`, applied in every scene
//! shader (faces, edges, curves, dashes, back edges, points, the underlay and
//! the grid lines). It is GPU-only: a change rewrites the camera uniform and
//! never rebuilds the scene.

use serde::{Deserialize, Serialize};

/// Where the fog colour comes from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FogColor {
    /// The background behind each pixel: the sky/ground gradient when it is
    /// on (sky above the horizon, the ground haze below, without the grid
    /// lines), else the flat theme or paper colour. Geometry melts into it.
    #[default]
    Background,
    /// [`FogStyle::custom_rgb`].
    Custom,
}

/// Every fog setting of the viewport (#363). Serialized into `ui.json` (and,
/// with #362, into named styles); missing keys take the defaults.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FogStyle {
    pub on: bool,
    /// Where the fade starts, as a fraction (0..1) of the scene's view depth.
    pub start: f32,
    /// Where the fog is complete, as a fraction (0..1) of the scene's view
    /// depth; kept above `start`.
    pub end: f32,
    pub color: FogColor,
    /// The colour of [`FogColor::Custom`], sRGB bytes. Kept while the
    /// background colour is in use, like SketchUp's swatch.
    pub custom_rgb: [u8; 3],
}

/// Default fade start, fraction of the view depth.
pub const FOG_START: f32 = 0.2;
/// Default fade end, fraction of the view depth.
pub const FOG_END: f32 = 1.0;
/// Smallest gap kept between start and end.
const MIN_SPAN: f32 = 0.01;

impl Default for FogStyle {
    fn default() -> Self {
        FogStyle {
            on: false,
            start: FOG_START,
            end: FOG_END,
            color: FogColor::Background,
            custom_rgb: [255, 255, 255],
        }
    }
}

impl FogStyle {
    /// `(start, end)` clamped to 0..1 with `end` at least a little past
    /// `start`, whatever a hand-edited `ui.json` says.
    pub fn range(self) -> (f32, f32) {
        let fin = |v: f32, d: f32| if v.is_finite() { v.clamp(0.0, 1.0) } else { d };
        let start = fin(self.start, FOG_START).min(1.0 - MIN_SPAN);
        let end = fin(self.end, FOG_END).max(start + MIN_SPAN);
        (start, end)
    }

    /// How much fog covers a point at depth fraction `t` (0 = nearest scene
    /// point, 1 = farthest): 0 before `start`, 1 past `end`, linear between.
    /// Mirrors `fog_amount` in `shaders/common.wgsl`; 0 when off.
    pub fn amount(self, t: f32) -> f32 {
        if !self.on {
            return 0.0;
        }
        let (start, end) = self.range();
        ((t - start) / (end - start)).clamp(0.0, 1.0)
    }

    /// [`Self::custom_rgb`] as RGBA floats in 0..1, like every viewport colour.
    pub fn custom_rgba(self) -> [f32; 4] {
        let [r, g, b] = self.custom_rgb.map(|c| c as f32 / 255.0);
        [r, g, b, 1.0]
    }

    /// One-line summary for the command line.
    pub fn summary(self) -> String {
        let (start, end) = self.range();
        let [r, g, b] = self.custom_rgb;
        let colour = match self.color {
            FogColor::Background => "background".to_string(),
            FogColor::Custom => format!("#{r:02x}{g:02x}{b:02x}"),
        };
        format!(
            "fog: {} · {}%–{}% of the view depth · colour {colour}",
            if self.on { "on" } else { "off" },
            (start * 100.0).round(),
            (end * 100.0).round(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_off_and_fill_missing_keys() {
        let d = FogStyle::default();
        assert!(!d.on);
        assert_eq!(d.color, FogColor::Background);
        assert_eq!(d.amount(5.0), 0.0, "off never fogs");
        let partial: FogStyle = serde_json::from_str(r#"{ "on": true }"#).unwrap();
        assert_eq!(partial, FogStyle { on: true, ..d });
        let empty: FogStyle = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, d);
        let custom = FogStyle { color: FogColor::Custom, custom_rgb: [1, 2, 3], ..d };
        let json = serde_json::to_value(custom).unwrap();
        assert_eq!(json["color"], "custom");
        assert_eq!(serde_json::from_value::<FogStyle>(json).unwrap(), custom);
    }

    #[test]
    fn amount_is_a_linear_fade_between_start_and_end() {
        let f = FogStyle { on: true, start: 0.25, end: 0.75, ..Default::default() };
        assert_eq!(f.amount(0.0), 0.0);
        assert_eq!(f.amount(0.25), 0.0);
        assert_eq!(f.amount(0.5), 0.5);
        assert_eq!(f.amount(0.75), 1.0);
        assert_eq!(f.amount(3.0), 1.0, "past the scene stays fogged");
        // Bad ranges are repaired, never divide by zero.
        let bad = FogStyle { on: true, start: 0.8, end: 0.2, ..Default::default() };
        let (s, e) = bad.range();
        assert!(e > s && (s, e) == (0.8, 0.81));
        let nan = FogStyle { start: f32::NAN, end: 7.0, ..Default::default() };
        assert_eq!(nan.range(), (FOG_START, 1.0));
        assert_eq!(FogStyle { start: 1.0, ..Default::default() }.range(), (0.99, 1.0));
    }

    #[test]
    fn camera_carries_fog_only_when_on() {
        let cam = crate::camera_uniform(glam::Mat4::IDENTITY, glam::Vec3::ZERO);
        let off = cam.with_fog(&FogStyle::default(), [0.5; 4]);
        assert_eq!(bytemuck::bytes_of(&off), bytemuck::bytes_of(&cam), "off = today's camera");
        assert!(!off.has_fog());
        let on = FogStyle { on: true, start: 0.1, end: 0.4, ..Default::default() };
        let bg = cam.with_fog(&on, [0.1, 0.2, 0.3, 1.0]);
        assert!(bg.has_fog());
        assert_eq!(bg.fog, [0.1, 0.4, 1.0, 1.0]);
        assert_eq!(bg.fog_color, [0.1, 0.2, 0.3, 1.0]);
        let custom = FogStyle { color: FogColor::Custom, custom_rgb: [255, 0, 0], ..on };
        let c = cam.with_fog(&custom, [0.1, 0.2, 0.3, 1.0]);
        assert_eq!((c.fog[3], c.fog_color), (0.0, [1.0, 0.0, 0.0, 1.0]));
    }

    #[test]
    fn shader_mirrors_the_fade() {
        let src = include_str!("shaders/common.wgsl");
        assert!(src.contains("fn fog_amount"));
        assert!(src.contains("(t - camera.fog.x) / max(camera.fog.y - camera.fog.x"));
    }

    #[test]
    fn summary_reads_percentages_and_colour() {
        let f = FogStyle { on: true, start: 0.1, end: 0.9, ..Default::default() };
        assert_eq!(f.summary(), "fog: on · 10%–90% of the view depth · colour background");
        let c = FogStyle { color: FogColor::Custom, custom_rgb: [255, 128, 0], ..f };
        assert!(c.summary().ends_with("colour #ff8000"));
    }
}
