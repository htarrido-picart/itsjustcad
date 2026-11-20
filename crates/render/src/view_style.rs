// SPDX-License-Identifier: AGPL-3.0-only

//! Named viewport styles (#362, SketchUp's Styles): one bundle of every
//! display setting, switched in one step.
//!
//! A [`ViewStyle`] carries the display mode, colour mode, lighting, profile
//! and mesh edges, the sketchy character, the edge style (#361), the fog
//! (#363), the grid/axes overlays and the background gradient. Missing keys
//! take the defaults, which are today's look, so a style saved by an older
//! build keeps loading, and unknown keys from a newer one are ignored.
//!
//! The built-in styles ([`BuiltinStyle`]) are read-only; saved styles live in
//! the document as serialized `ViewStyle`s (see `Document::named_styles`) so
//! a file carries its own styles. The struct is self-contained and `Copy`
//! so other features (scenes, named views) can reference one later.

use serde::{Deserialize, Serialize};

use crate::{ColorMode, DisplayMode, EdgeStyle, FogStyle, LightMode, SketchyParams};

/// Every display setting a style switches (#362).
///
/// The display and colour modes are per viewport; applying a style sets them
/// on the active pane (or on every pane), the rest app-wide.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewStyle {
    pub display: DisplayMode,
    pub color: ColorMode,
    pub light: LightMode,
    /// Thick profile / silhouette edges.
    pub profiles: bool,
    /// Thin mesh feature edges in Shaded mode.
    pub mesh_edges: bool,
    /// Hand-drawn edge character (forced on by [`DisplayMode::Sketchy`]).
    pub sketchy: SketchyParams,
    pub edges: EdgeStyle,
    pub fog: FogStyle,
    pub grid: bool,
    pub axes: bool,
    /// Sky/ground gradient behind the scene. `None` follows
    /// [`Self::profiles`], the coupling the `massing` preset has always had.
    pub gradient: Option<bool>,
}

impl Default for ViewStyle {
    /// Today's look: shaded, by layer, working light, mesh edges, grid and
    /// axes on, everything else off.
    fn default() -> Self {
        ViewStyle {
            display: DisplayMode::Shaded,
            color: ColorMode::ByLayer,
            light: LightMode::Working,
            profiles: false,
            mesh_edges: true,
            sketchy: SketchyParams::default(),
            edges: EdgeStyle::default(),
            fog: FogStyle::default(),
            grid: true,
            axes: true,
            gradient: None,
        }
    }
}

impl ViewStyle {
    /// Whether the sky/ground gradient is drawn.
    pub fn gradient_on(self) -> bool {
        self.gradient.unwrap_or(self.profiles)
    }
}

/// The read-only styles every document has (#362).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuiltinStyle {
    /// Today's default look.
    Modelling,
    /// Profiles, depth cue, sun light, light fog and the sky gradient.
    Presentation,
    /// Hand-drawn edges on paper.
    Sketch,
    /// Hidden-line drawing in the theme colours.
    HiddenLine,
    /// See-through faces with every edge.
    XRay,
    /// Working light, profiles and the sky gradient (the `massing` preset).
    Massing,
}

impl BuiltinStyle {
    pub const ALL: [BuiltinStyle; 6] = [
        BuiltinStyle::Modelling,
        BuiltinStyle::Presentation,
        BuiltinStyle::Sketch,
        BuiltinStyle::HiddenLine,
        BuiltinStyle::XRay,
        BuiltinStyle::Massing,
    ];

    /// The name typed after `style`.
    pub fn name(self) -> &'static str {
        match self {
            BuiltinStyle::Modelling => "modelling",
            BuiltinStyle::Presentation => "presentation",
            BuiltinStyle::Sketch => "sketch",
            BuiltinStyle::HiddenLine => "hiddenline",
            BuiltinStyle::XRay => "xray",
            BuiltinStyle::Massing => "massing",
        }
    }

    /// The built-in named `name` (case-insensitive; `modeling` and
    /// `default` alias `modelling`).
    pub fn parse(name: &str) -> Option<Self> {
        let n = name.to_ascii_lowercase();
        match n.as_str() {
            "modeling" | "default" => Some(BuiltinStyle::Modelling),
            _ => Self::ALL.into_iter().find(|b| b.name() == n),
        }
    }

    /// The settings of this style.
    pub fn style(self) -> ViewStyle {
        let d = ViewStyle::default();
        match self {
            BuiltinStyle::Modelling => d,
            BuiltinStyle::Presentation => ViewStyle {
                light: LightMode::Sun,
                profiles: true,
                edges: EdgeStyle { depth_cue: true, ..EdgeStyle::default() },
                fog: FogStyle { on: true, start: 0.5, end: 1.0, ..FogStyle::default() },
                gradient: Some(true),
                ..d
            },
            BuiltinStyle::Sketch => ViewStyle {
                display: DisplayMode::Sketchy,
                profiles: true,
                sketchy: SketchyParams::on(),
                gradient: Some(false),
                ..d
            },
            BuiltinStyle::HiddenLine => ViewStyle {
                display: DisplayMode::HiddenLine,
                profiles: true,
                gradient: Some(false),
                ..d
            },
            BuiltinStyle::XRay => ViewStyle { display: DisplayMode::XRay, ..d },
            BuiltinStyle::Massing => ViewStyle { profiles: true, ..d },
        }
    }

    /// The first built-in whose settings equal `style`, if any.
    pub fn matching(style: &ViewStyle) -> Option<Self> {
        Self::ALL.into_iter().find(|b| b.style() == *style)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_todays_look_and_fills_missing_keys() {
        let d = ViewStyle::default();
        assert_eq!(BuiltinStyle::Modelling.style(), d);
        assert_eq!(d.display, DisplayMode::default());
        assert_eq!(d.edges, EdgeStyle::default());
        assert_eq!(d.fog, FogStyle::default());
        assert!(d.mesh_edges && d.grid && d.axes && !d.profiles && !d.gradient_on());
        let empty: ViewStyle = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, d);
        let partial: ViewStyle =
            serde_json::from_str(r#"{"display":"xray","fog":{"on":true},"later":1}"#).unwrap();
        assert_eq!(partial.display, DisplayMode::XRay);
        assert!(partial.fog.on);
        assert_eq!(partial.fog.end, FogStyle::default().end);
        assert_eq!(partial.light, LightMode::Working);
    }

    #[test]
    fn round_trips_through_json() {
        for b in BuiltinStyle::ALL {
            let s = b.style();
            let back: ViewStyle = serde_json::from_value(serde_json::to_value(s).unwrap()).unwrap();
            assert_eq!(back, s, "{}", b.name());
        }
    }

    #[test]
    fn builtins_set_their_fields() {
        let p = BuiltinStyle::Presentation.style();
        assert!(p.profiles && p.edges.depth_cue && p.fog.on && p.gradient_on());
        assert_eq!(p.light, LightMode::Sun);
        let s = BuiltinStyle::Sketch.style();
        assert_eq!(s.display, DisplayMode::Sketchy);
        assert!(s.sketchy.enabled && !s.gradient_on());
        assert_eq!(BuiltinStyle::HiddenLine.style().display, DisplayMode::HiddenLine);
        assert_eq!(BuiltinStyle::XRay.style().display, DisplayMode::XRay);
        let m = BuiltinStyle::Massing.style();
        assert!(m.profiles && m.gradient_on());
        assert_eq!((m.display, m.light), (DisplayMode::Shaded, LightMode::Working));
        // Every built-in is distinct, so the active one is unambiguous.
        for b in BuiltinStyle::ALL {
            assert_eq!(BuiltinStyle::matching(&b.style()), Some(b));
        }
    }

    #[test]
    fn names_parse_back() {
        for b in BuiltinStyle::ALL {
            assert_eq!(BuiltinStyle::parse(b.name()), Some(b));
        }
        assert_eq!(BuiltinStyle::parse("Modeling"), Some(BuiltinStyle::Modelling));
        assert_eq!(BuiltinStyle::parse("default"), Some(BuiltinStyle::Modelling));
        assert_eq!(BuiltinStyle::parse("plaid"), None);
    }
}
