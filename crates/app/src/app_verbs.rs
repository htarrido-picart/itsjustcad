// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Shared app-level verb dispatch.
//!
//! Some verbs are handled by the app rather than the command substrate
//! (`itsjustcad_commands::parse`): view/camera/display state, `ze`, `save`,
//! `help`, and GUI-only actions like `template`/`critique`. The GUI
//! (`App::execute_line`) and the headless runner (`headless.rs`) both need to
//! recognise the same set, so the classification lives here as one table
//! instead of being duplicated.
//!
//! This module intentionally does NOT execute GUI side effects — it only
//! *classifies* a line so each caller can act:
//!   * the GUI keeps its rich `execute_line` match (camera panes, deck, etc.);
//!   * the headless runner applies the view-affecting verbs to its offscreen
//!     camera, prints `help`, honours `save`, and warns-and-ignores GUI-only
//!     verbs instead of erroring.

use itsjustcad_render::{DisplayMode, LightMode, StandardView};

/// An app-level verb that the substrate parser does not own.
#[derive(Clone, Debug, PartialEq)]
pub enum AppVerb {
    /// Frame the scene extents (`ze` / `zoomextents`).
    ZoomExtents,
    /// Standard view direction (`top`/`front`/`persp`/…).
    View(StandardView),
    /// Camera projection / lens
    /// (`camera 2point|persp|pano|fisheye [fov]|<n>mm|phone|…`).
    /// `.0` = first argument (mode/lens), `.1` = optional second argument
    /// (the fisheye field of view in degrees). Carried raw so each front-end
    /// applies it as it can.
    Camera(Option<String>, Option<String>),
    /// Display mode of the active viewport (`display shaded|wireframe|…`).
    Display(DisplayMode),
    /// Lighting model (`lightmode working|sun|presentation`).
    Light(LightMode),
    /// Toggle SketchUp-style thick profile edges (`profileedges [on|off]`).
    /// `None` means bare toggle.
    ProfileEdges(Option<bool>),
    /// SketchUp display preset (`sketchup`): working light + profile edges +
    /// gradient background.
    SketchUp,
    /// Toggle hand-drawn "sketchy edges" NPR character (`sketchy [on|off]`).
    /// `None` means bare toggle.
    Sketchy(Option<bool>),
    /// Tune the sketchy edge effect (`edgefx jitter=.. extension=.. …`).
    /// Carries the raw `key=value` tokens for the front-end to apply.
    EdgeFx(Vec<String>),
    /// Persist the document (`save [path]`). Argument is the optional path.
    Save(Option<String>),
    /// Command reference (`help [verb]`).
    Help(Option<String>),
    /// GUI-only verb with no headless meaning (`template`, `critique`, …).
    /// Carried so the headless runner can warn about the specific name.
    GuiOnly(&'static str),
}

/// Map a standard-view verb name to its [`StandardView`].
pub fn standard_view(name: &str) -> Option<StandardView> {
    Some(match name {
        "top" => StandardView::Top,
        "bottom" => StandardView::Bottom,
        "front" => StandardView::Front,
        "back" => StandardView::Back,
        "left" => StandardView::Left,
        "right" => StandardView::Right,
        "persp" | "perspective" => StandardView::Perspective,
        _ => return None,
    })
}

/// Classify a command line as an app-level verb, or `None` if it should fall
/// through to the substrate parser / draw tools. The single source of truth
/// for which verbs the app owns.
pub fn classify(line: &str) -> Option<AppVerb> {
    let mut words = line.split_whitespace();
    let verb = words.next()?;
    Some(match verb {
        "ze" | "zoomextents" => AppVerb::ZoomExtents,
        "display" => AppVerb::Display(words.next().and_then(DisplayMode::parse)?),
        "lightmode" | "light" => AppVerb::Light(words.next().and_then(LightMode::parse)?),
        "profileedges" | "profiles" => AppVerb::ProfileEdges(match words.next() {
            Some("on" | "true" | "1") => Some(true),
            Some("off" | "false" | "0") => Some(false),
            None => None,
            _ => return None,
        }),
        "sketchup" | "su" => AppVerb::SketchUp,
        "sketchy" => AppVerb::Sketchy(match words.next() {
            Some("on" | "true" | "1") => Some(true),
            Some("off" | "false" | "0") => Some(false),
            None => None,
            _ => return None,
        }),
        "edgefx" => AppVerb::EdgeFx(words.map(str::to_owned).collect()),
        "camera" => AppVerb::Camera(
            words.next().map(str::to_ascii_lowercase),
            words.next().map(str::to_ascii_lowercase),
        ),
        "save" => AppVerb::Save(words.next().map(str::to_owned)),
        "help" => AppVerb::Help(words.next().map(str::to_owned)),
        "template" => AppVerb::GuiOnly("template"),
        "critique" => AppVerb::GuiOnly("critique"),
        other => AppVerb::View(standard_view(other)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_view_verbs() {
        assert_eq!(classify("ze"), Some(AppVerb::ZoomExtents));
        assert_eq!(classify("zoomextents"), Some(AppVerb::ZoomExtents));
        assert_eq!(classify("front"), Some(AppVerb::View(StandardView::Front)));
        assert_eq!(classify("persp"), Some(AppVerb::View(StandardView::Perspective)));
    }

    #[test]
    fn classifies_display_and_camera() {
        assert!(matches!(classify("display pencil"), Some(AppVerb::Display(_))));
        // Unknown display mode is not an app verb (falls through).
        assert_eq!(classify("display bogus"), None);
        assert_eq!(classify("camera 35mm"), Some(AppVerb::Camera(Some("35mm".into()), None)));
        assert_eq!(
            classify("camera fisheye 120"),
            Some(AppVerb::Camera(Some("fisheye".into()), Some("120".into())))
        );
    }

    #[test]
    fn classifies_lighting_and_preset_verbs() {
        assert_eq!(classify("lightmode sun"), Some(AppVerb::Light(LightMode::Sun)));
        assert_eq!(classify("light working"), Some(AppVerb::Light(LightMode::Working)));
        // Unknown light mode falls through (not an app verb).
        assert_eq!(classify("lightmode bogus"), None);
        assert_eq!(classify("profileedges on"), Some(AppVerb::ProfileEdges(Some(true))));
        assert_eq!(classify("profileedges off"), Some(AppVerb::ProfileEdges(Some(false))));
        assert_eq!(classify("profiles"), Some(AppVerb::ProfileEdges(None)));
        assert_eq!(classify("profileedges garbage"), None);
        assert_eq!(classify("sketchup"), Some(AppVerb::SketchUp));
        assert_eq!(classify("su"), Some(AppVerb::SketchUp));
    }

    #[test]
    fn classifies_sketchy_and_edgefx() {
        assert_eq!(classify("sketchy on"), Some(AppVerb::Sketchy(Some(true))));
        assert_eq!(classify("sketchy off"), Some(AppVerb::Sketchy(Some(false))));
        assert_eq!(classify("sketchy"), Some(AppVerb::Sketchy(None)));
        assert_eq!(classify("sketchy garbage"), None);
        assert_eq!(
            classify("edgefx jitter=.05 extension=.1"),
            Some(AppVerb::EdgeFx(vec!["jitter=.05".into(), "extension=.1".into()]))
        );
        assert_eq!(classify("edgefx"), Some(AppVerb::EdgeFx(vec![])));
    }

    #[test]
    fn classifies_save_help_gui_only() {
        assert_eq!(classify("save out.json"), Some(AppVerb::Save(Some("out.json".into()))));
        assert_eq!(classify("save"), Some(AppVerb::Save(None)));
        assert_eq!(classify("help box"), Some(AppVerb::Help(Some("box".into()))));
        assert_eq!(classify("template"), Some(AppVerb::GuiOnly("template")));
        assert_eq!(classify("critique looks off"), Some(AppVerb::GuiOnly("critique")));
    }

    #[test]
    fn substrate_verbs_fall_through() {
        assert_eq!(classify("box 0,0,0 1,1,1"), None);
        assert_eq!(classify("layer walls"), None);
        assert_eq!(classify(""), None);
    }
}
