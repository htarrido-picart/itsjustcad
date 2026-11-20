// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Edge styles outside Sketchy mode (#361, SketchUp's Styles ▸ Edge
//! settings): back edges, depth cue, edge colour and extension.
//!
//! One [`EdgeStyle`] holds every setting so a named style (#362) can
//! serialize it whole. The defaults are today's look: no back edges, no depth
//! cue, layer colours, no extension.
//!
//! Where each setting acts:
//! * back edges and depth cue are GPU-only, per pane (camera uniform, see
//!   [`crate::CameraUniform::with_edge_style`]): toggling them never rebuilds
//!   the scene;
//! * colour and extension change the snapshot's edge geometry and colours, so
//!   a change re-uploads the scene once.

use serde::{Deserialize, Serialize};

use crate::renderer::AXIS_RGBA;

/// How edges and curves are coloured (SketchUp's Edge colour).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeColor {
    /// Today's colours: curves follow object/layer colour, mesh edges the
    /// theme ink.
    #[default]
    Layer,
    /// Every edge in one colour ([`EdgeStyle::same_rgb`], black by default).
    Same,
    /// The object's material colour (else its object colour, else as Layer).
    Material,
    /// Edges parallel to X, Y or Z in the axis colours; the rest as Layer.
    Axis,
}

impl EdgeColor {
    pub const ALL: [EdgeColor; 4] =
        [EdgeColor::Layer, EdgeColor::Same, EdgeColor::Material, EdgeColor::Axis];

    /// The verb word (`edgecolor <word>`).
    pub fn label(self) -> &'static str {
        match self {
            EdgeColor::Layer => "layer",
            EdgeColor::Same => "same",
            EdgeColor::Material => "material",
            EdgeColor::Axis => "axis",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "layer" | "bylayer" | "default" => EdgeColor::Layer,
            "same" | "all" | "allsame" | "single" => EdgeColor::Same,
            "material" | "bymaterial" | "mat" => EdgeColor::Material,
            "axis" | "byaxis" | "axes" => EdgeColor::Axis,
            _ => return None,
        })
    }
}

/// Every edge-style setting of the viewport (#361). Serialized into
/// `ui.json` (and, with #362, into named styles); missing keys take the
/// defaults, which keep today's look.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EdgeStyle {
    /// Occluded edges drawn dashed and faint in the opaque display modes
    /// (never in X-ray, Ghosted or Wireframe).
    pub back_edges: bool,
    /// Nearer edges drawn thicker, farther ones thinner.
    pub depth_cue: bool,
    /// Width of the nearest edges with depth cue on, logical px.
    pub depth_cue_px: f32,
    pub color: EdgeColor,
    /// The single colour of [`EdgeColor::Same`], sRGB bytes.
    pub same_rgb: [u8; 3],
    /// Edges overshoot their endpoints (outside Sketchy, which has its own).
    pub extension: bool,
    /// Overshoot length, world units (m).
    pub extension_m: f32,
}

/// Default nearest-edge width of the depth cue, logical px.
pub const DEPTH_CUE_PX: f32 = 3.0;
/// Default extension overshoot, m.
pub const EXTENSION_M: f32 = 0.1;
/// Opacity of the dashed back edges.
pub const BACK_EDGE_ALPHA: f32 = 0.4;

impl Default for EdgeStyle {
    fn default() -> Self {
        EdgeStyle {
            back_edges: false,
            depth_cue: false,
            depth_cue_px: DEPTH_CUE_PX,
            color: EdgeColor::Layer,
            same_rgb: [0, 0, 0],
            extension: false,
            extension_m: EXTENSION_M,
        }
    }
}

impl EdgeStyle {
    /// The settings that change the snapshot (colour and extension); the
    /// rest are GPU-only. Two styles with equal keys share one upload.
    pub fn snapshot_key(self) -> (EdgeColor, [u8; 3], bool, u32) {
        (self.color, self.same_rgb, self.extension, self.extension_m.to_bits())
    }

    /// [`Self::same_rgb`] as RGBA floats in 0..1, like every viewport colour.
    pub fn same_rgba(self) -> [f32; 4] {
        let [r, g, b] = self.same_rgb.map(|c| c as f32 / 255.0);
        [r, g, b, 1.0]
    }

    /// The extension overshoot in metres, `0` when off.
    pub fn extension_len(self) -> f32 {
        if self.extension { self.extension_m.max(0.0) } else { 0.0 }
    }

    /// One-line summary for the command line.
    pub fn summary(self) -> String {
        let on = |b: bool| if b { "on" } else { "off" };
        let [r, g, b] = self.same_rgb;
        let color = match self.color {
            EdgeColor::Same => format!("same #{r:02x}{g:02x}{b:02x}"),
            c => c.label().to_string(),
        };
        format!(
            "edges: back {} · depth cue {} ({} px) · colour {color} · extension {}",
            on(self.back_edges),
            on(self.depth_cue),
            self.depth_cue_px,
            if self.extension { format!("{} m", self.extension_m) } else { "off".into() },
        )
    }
}

/// Parse `#rrggbb` / `rrggbb` into sRGB bytes.
pub fn parse_hex_rgb(s: &str) -> Option<[u8; 3]> {
    let h = s.strip_prefix('#').unwrap_or(s);
    if h.len() != 6 || !h.is_ascii() {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some([byte(0)?, byte(2)?, byte(4)?])
}

/// Cosine above which an edge counts as parallel to an axis (about 0.25°).
const AXIS_PARALLEL_COS: f32 = 0.99999;

/// The world axis (0 = X, 1 = Y, 2 = Z) a segment runs parallel to, if any.
pub fn axis_of(a: [f32; 3], b: [f32; 3]) -> Option<usize> {
    let d = glam::Vec3::from(b) - glam::Vec3::from(a);
    let len = d.length();
    if len <= 1e-9 {
        return None;
    }
    let d = d / len;
    (0..3).find(|&i| d[i].abs() >= AXIS_PARALLEL_COS)
}

/// Split a segment soup (point pairs) by axis: the X, Y, Z and "other"
/// segments, in input order within each group.
pub fn split_soup_by_axis(points: &[[f32; 3]]) -> [Vec<[f32; 3]>; 4] {
    let mut out: [Vec<[f32; 3]>; 4] = Default::default();
    for [a, b] in points.as_chunks::<2>().0 {
        out[axis_of(*a, *b).unwrap_or(3)].extend([*a, *b]);
    }
    out
}

/// Split a polyline strip into runs of segments of one axis class, each run
/// returned as `(class, strip)` with class 3 for "other".
pub fn split_strip_by_axis(points: &[[f32; 3]]) -> Vec<(usize, Vec<[f32; 3]>)> {
    let mut out: Vec<(usize, Vec<[f32; 3]>)> = Vec::new();
    for w in points.windows(2) {
        let class = axis_of(w[0], w[1]).unwrap_or(3);
        match out.last_mut() {
            Some((c, run)) if *c == class => run.push(w[1]),
            _ => out.push((class, vec![w[0], w[1]])),
        }
    }
    out
}

/// The colour of an axis class from [`split_soup_by_axis`], `other` for 3.
pub fn axis_class_color(class: usize, other: [f32; 4]) -> [f32; 4] {
    AXIS_RGBA.get(class).copied().unwrap_or(other)
}

/// Every segment of a strip on its own, overshooting both ends by `ext`:
/// SketchUp's extension, one 2-point strip per segment.
pub fn extend_strip(points: &[[f32; 3]], ext: f32) -> Vec<Vec<[f32; 3]>> {
    points
        .windows(2)
        .filter_map(|w| {
            let (a, b) = (glam::Vec3::from(w[0]), glam::Vec3::from(w[1]));
            let d = (b - a).normalize_or_zero();
            (d != glam::Vec3::ZERO).then(|| vec![(a - d * ext).to_array(), (b + d * ext).to_array()])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_keep_todays_look_and_fill_missing_keys() {
        let d = EdgeStyle::default();
        assert!(!d.back_edges && !d.depth_cue && !d.extension);
        assert_eq!(d.color, EdgeColor::Layer);
        assert_eq!(d.same_rgb, [0, 0, 0], "SketchUp: all same = black");
        assert_eq!(d.extension_len(), 0.0);
        // An empty or partial object reads back as the defaults.
        let empty: EdgeStyle = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, d);
        let partial: EdgeStyle =
            serde_json::from_str(r#"{"back_edges":true,"color":"axis"}"#).unwrap();
        assert_eq!(partial, EdgeStyle { back_edges: true, color: EdgeColor::Axis, ..d });
        // Round trip.
        let s = EdgeStyle { depth_cue: true, depth_cue_px: 5.0, same_rgb: [1, 2, 3], ..d };
        let back: EdgeStyle = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        assert!(serde_json::to_string(&s).unwrap().contains("\"color\":\"layer\""));
    }

    #[test]
    fn shader_back_alpha_matches() {
        let src = include_str!("shaders/curve.wgsl");
        assert!(src.contains(&format!("const BACK_ALPHA: f32 = {BACK_EDGE_ALPHA};")));
    }

    #[test]
    fn snapshot_key_ignores_gpu_only_settings() {
        let d = EdgeStyle::default();
        let gpu = EdgeStyle { back_edges: true, depth_cue: true, depth_cue_px: 6.0, ..d };
        assert_eq!(gpu.snapshot_key(), d.snapshot_key());
        assert_ne!(EdgeStyle { color: EdgeColor::Axis, ..d }.snapshot_key(), d.snapshot_key());
        assert_ne!(EdgeStyle { extension: true, ..d }.snapshot_key(), d.snapshot_key());
    }

    #[test]
    fn colour_words_and_hex_parse() {
        for c in EdgeColor::ALL {
            assert_eq!(EdgeColor::parse(c.label()), Some(c));
        }
        assert_eq!(EdgeColor::parse("ByAxis"), Some(EdgeColor::Axis));
        assert_eq!(EdgeColor::parse("plaid"), None);
        assert_eq!(parse_hex_rgb("#ff8000"), Some([255, 128, 0]));
        assert_eq!(parse_hex_rgb("0A0b0C"), Some([10, 11, 12]));
        assert_eq!(parse_hex_rgb("#fff"), None);
        assert_eq!(parse_hex_rgb("#gg0000"), None);
    }

    #[test]
    fn axis_classification() {
        assert_eq!(axis_of([0.0; 3], [5.0, 0.0, 0.0]), Some(0));
        assert_eq!(axis_of([1.0, 1.0, 1.0], [1.0, -3.0, 1.0]), Some(1));
        assert_eq!(axis_of([0.0; 3], [0.0, 0.0, -2.0]), Some(2));
        assert_eq!(axis_of([0.0; 3], [1.0, 1.0, 0.0]), None, "diagonal");
        assert_eq!(axis_of([0.0; 3], [1.0, 0.01, 0.0]), None, "0.6° off is not parallel");
        assert_eq!(axis_of([0.0; 3], [1.0, 1e-4, 0.0]), Some(0), "float noise still is");
        assert_eq!(axis_of([2.0; 3], [2.0; 3]), None, "degenerate");

        let soup = [[0.0; 3], [1.0, 0.0, 0.0], [0.0; 3], [1.0, 1.0, 0.0], [0.0; 3], [0.0, 0.0, 3.0]];
        let [x, y, z, other] = split_soup_by_axis(&soup);
        assert_eq!((x.len(), y.len(), z.len(), other.len()), (2, 0, 2, 2));

        // A rectangle with a diagonal tail: runs X, Y, X, Y, other.
        let strip = [
            [0.0; 3],
            [2.0, 0.0, 0.0],
            [2.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0],
            [1.0, 1.0, 1.0],
        ];
        let runs = split_strip_by_axis(&strip);
        let classes: Vec<usize> = runs.iter().map(|r| r.0).collect();
        assert_eq!(classes, [0, 1, 0, 1, 3]);
        assert_eq!(runs[0].1, vec![[0.0; 3], [2.0, 0.0, 0.0]]);
        // Two collinear X segments stay one run.
        let runs = split_strip_by_axis(&[[0.0; 3], [1.0, 0.0, 0.0], [3.0, 0.0, 0.0]]);
        assert_eq!(runs, vec![(0, vec![[0.0; 3], [1.0, 0.0, 0.0], [3.0, 0.0, 0.0]])]);
        assert_eq!(axis_class_color(2, [0.0; 4]), AXIS_RGBA[2]);
        assert_eq!(axis_class_color(3, [0.5; 4]), [0.5; 4]);
    }

    #[test]
    fn extension_overshoots_every_segment() {
        let segs = extend_strip(&[[0.0; 3], [2.0, 0.0, 0.0], [2.0, 2.0, 0.0]], 0.5);
        assert_eq!(segs, vec![
            vec![[-0.5, 0.0, 0.0], [2.5, 0.0, 0.0]],
            vec![[2.0, -0.5, 0.0], [2.0, 2.5, 0.0]],
        ]);
    }
}
