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
    /// Toggle the thin mesh feature edges drawn in Shaded mode by default
    /// (`shadededges [on|off]` / `meshedges`). `None` means bare toggle.
    ShadedEdges(Option<bool>),
    /// SketchUp display preset (`sketchup`): working light + profile edges +
    /// gradient background.
    SketchUp,
    /// Toggle hand-drawn "sketchy edges" NPR character (`sketchy [on|off]`).
    /// `None` means bare toggle.
    Sketchy(Option<bool>),
    /// Tune the sketchy edge effect (`edgefx jitter=.. extension=.. …`).
    /// Carries the raw `key=value` tokens for the front-end to apply.
    EdgeFx(Vec<String>),
    /// Toggle the transform gumball/gizmo (`gumball [on|off|toggle]` / G hotkey).
    /// `None` means bare toggle.
    Gumball(Option<bool>),
    /// Toggle Reduce Motion for animated progress bars (`reducemotion [on|off]`).
    /// `None` means bare toggle.
    ReduceMotion(Option<bool>),
    /// Persist the document (`save [path]`). Argument is the optional path.
    Save(Option<String>),
    /// Command reference (`help [verb]`).
    Help(Option<String>),
    /// GUI-only verb with no headless meaning (`template`, `critique`, …).
    /// Carried so the headless runner can warn about the specific name.
    GuiOnly(&'static str),
    /// Georeferenced satellite/OSM basemap underlay
    /// (`basemap [osm|sat] [span_m] [opacity]` | `basemap off`). View/session
    /// state, opt-in, NEVER logged. Carries the parsed options for the front-end
    /// to fetch/stitch (the GUI reaches the network; headless uses the cache).
    Basemap(BasemapArgs),
}

/// Parsed `basemap` options. Defaults chosen for a site-scale context image.
#[derive(Clone, Debug, PartialEq)]
pub struct BasemapArgs {
    /// `true` clears the basemap (`basemap off|clear|none`).
    pub clear: bool,
    /// Provider slug (`osm` default, or `sat`).
    pub provider: String,
    /// Side length of the covered square in meters.
    pub span_m: f64,
    /// Blend opacity 0..1.
    pub opacity: f32,
}

impl Default for BasemapArgs {
    fn default() -> Self {
        Self {
            clear: false,
            provider: "osm".into(),
            span_m: 500.0,
            opacity: 0.85,
        }
    }
}

/// Parse the tokens after the `basemap` verb into [`BasemapArgs`]. Accepts, in
/// any order after the optional provider: a span in meters and an opacity in
/// `0..=1`. `off`/`clear`/`none` requests removal.
pub fn parse_basemap_args<'a>(mut words: impl Iterator<Item = &'a str>) -> BasemapArgs {
    let mut args = BasemapArgs::default();
    if let Some(first) = words.next() {
        match first.to_ascii_lowercase().as_str() {
            "off" | "clear" | "none" => {
                args.clear = true;
                return args;
            }
            "osm" | "sat" | "satellite" | "imagery" | "esri" => {
                args.provider = first.to_ascii_lowercase();
            }
            other => apply_numeric(&mut args, other),
        }
    }
    for w in words {
        apply_numeric(&mut args, w);
    }
    args
}

/// Fold one numeric token into the args: a value in `0..=1` is opacity, a larger
/// value is a span in meters.
fn apply_numeric(args: &mut BasemapArgs, tok: &str) {
    if let Ok(v) = tok.parse::<f64>() {
        if (0.0..=1.0).contains(&v) {
            args.opacity = v as f32;
        } else if v > 1.0 {
            args.span_m = v;
        }
    }
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
        "shadededges" | "meshedges" => AppVerb::ShadedEdges(match words.next() {
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
        "gumball" | "gizmo" => AppVerb::Gumball(match words.next() {
            Some("on" | "true" | "1") => Some(true),
            Some("off" | "false" | "0") => Some(false),
            Some("toggle") | None => None,
            _ => return None,
        }),
        "reducemotion" => AppVerb::ReduceMotion(match words.next() {
            Some("on" | "true" | "1") => Some(true),
            Some("off" | "false" | "0") => Some(false),
            None => None,
            _ => return None,
        }),
        "camera" => AppVerb::Camera(
            words.next().map(str::to_ascii_lowercase),
            words.next().map(str::to_ascii_lowercase),
        ),
        "save" => AppVerb::Save(words.next().map(str::to_owned)),
        "help" => AppVerb::Help(words.next().map(str::to_owned)),
        "template" => AppVerb::GuiOnly("template"),
        "critique" => AppVerb::GuiOnly("critique"),
        "basemap" => AppVerb::Basemap(parse_basemap_args(words)),
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
        assert_eq!(classify("shadededges on"), Some(AppVerb::ShadedEdges(Some(true))));
        assert_eq!(classify("shadededges off"), Some(AppVerb::ShadedEdges(Some(false))));
        assert_eq!(classify("meshedges"), Some(AppVerb::ShadedEdges(None)));
        assert_eq!(classify("shadededges garbage"), None);
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
    fn classifies_gumball() {
        assert_eq!(classify("gumball on"), Some(AppVerb::Gumball(Some(true))));
        assert_eq!(classify("gumball off"), Some(AppVerb::Gumball(Some(false))));
        assert_eq!(classify("gumball toggle"), Some(AppVerb::Gumball(None)));
        assert_eq!(classify("gumball"), Some(AppVerb::Gumball(None)));
        assert_eq!(classify("gizmo on"), Some(AppVerb::Gumball(Some(true))));
        assert_eq!(classify("gumball garbage"), None);
    }

    #[test]
    fn classifies_reduce_motion() {
        assert_eq!(classify("reducemotion on"), Some(AppVerb::ReduceMotion(Some(true))));
        assert_eq!(classify("reducemotion off"), Some(AppVerb::ReduceMotion(Some(false))));
        assert_eq!(classify("reducemotion 1"), Some(AppVerb::ReduceMotion(Some(true))));
        assert_eq!(classify("reducemotion 0"), Some(AppVerb::ReduceMotion(Some(false))));
        assert_eq!(classify("reducemotion"), Some(AppVerb::ReduceMotion(None)));
        assert_eq!(classify("reducemotion garbage"), None);
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
    fn classifies_basemap_defaults_and_options() {
        // Bare verb → defaults (osm, 500 m, 0.85).
        let a = match classify("basemap").unwrap() {
            AppVerb::Basemap(a) => a,
            other => panic!("{other:?}"),
        };
        assert_eq!(a, BasemapArgs::default());
        // Provider + span + opacity, order-independent.
        let a = match classify("basemap sat 1200 0.5").unwrap() {
            AppVerb::Basemap(a) => a,
            other => panic!("{other:?}"),
        };
        assert_eq!(a.provider, "sat");
        assert_eq!(a.span_m, 1200.0);
        assert_eq!(a.opacity, 0.5);
        assert!(!a.clear);
        // Opacity before span still classifies each by magnitude.
        let a = match classify("basemap 0.3 800").unwrap() {
            AppVerb::Basemap(a) => a,
            other => panic!("{other:?}"),
        };
        assert_eq!(a.opacity, 0.3);
        assert_eq!(a.span_m, 800.0);
        assert_eq!(a.provider, "osm");
    }

    #[test]
    fn classifies_basemap_off() {
        let a = match classify("basemap off").unwrap() {
            AppVerb::Basemap(a) => a,
            other => panic!("{other:?}"),
        };
        assert!(a.clear);
        assert!(matches!(classify("basemap clear"), Some(AppVerb::Basemap(b)) if b.clear));
        assert!(matches!(classify("basemap none"), Some(AppVerb::Basemap(b)) if b.clear));
    }

    /// The canonical set of DECK-RELEVANT app-verb tokens: every user-facing
    /// view/render/camera verb the deck may emit and the app executes through
    /// `execute_line`. This is the single source of truth read by BOTH the
    /// denylist audit (`app_verb_denylist_partitions_classify`) and the prompt
    /// completeness test (`prompt_advertises_every_deck_app_verb`). Adding a new
    /// advertise-worthy app-verb without a `VIEW_VERB_HELP` entry MUST fail the
    /// latter test — that is the durable guard for the app-verb plane.
    const CANONICAL_DECK_APP_VERBS: &[&str] = &[
        "camera",
        "display",
        "light",
        "sketchy",
        "edgefx",
        "profiles",
        "meshedges",
        "gumball",
        "ze",
        "zoomextents",
        "top",
        "bottom",
        "front",
        "back",
        "left",
        "right",
        "persp",
        "basemap",
    ];

    /// Verbs that `classify` owns but the deck must NOT advertise: internal /
    /// GUI-only / security-gated / alias forms. Kept explicit so the audit below
    /// can prove `CANONICAL_DECK_APP_VERBS` ∪ denylist == every classify arm,
    /// i.e. no verb is silently dropped from consideration.
    const DECK_APP_VERB_DENYLIST: &[&str] = &[
        "critique",     // GUI-only
        "template",     // GUI-only
        "save",         // fs write — refused on deck plane
        "help",         // meta, not a view action
        "reducemotion", // accessibility toggle, not a drawing/view verb the model reframes with
        "lightmode",    // alias of `light`
        "profileedges", // alias of `profiles`
        "shadededges",  // alias of `meshedges`
        "sketchup",     // preset; advertised as `sketchup`, not a standalone canonical token here
        "su",           // alias of `sketchup`
        "gizmo",        // alias of `gumball`
    ];

    #[test]
    fn app_verb_denylist_partitions_classify() {
        // Every token that `classify` recognises must be accounted for: it is
        // either advertised (in CANONICAL_DECK_APP_VERBS) or explicitly denied
        // (DECK_APP_VERB_DENYLIST). If someone adds a new arm to `classify`
        // without deciding which bucket it belongs to, this fails — forcing an
        // explicit advertise-or-deny decision. Note: `sketchup`/`su` map to
        // SketchUp preset advertised inline in VIEW_VERB_HELP; they live on the
        // denylist as standalone canonical tokens.
        let all_classify_tokens = [
            "ze",
            "zoomextents",
            "display",
            "lightmode",
            "light",
            "profileedges",
            "profiles",
            "shadededges",
            "meshedges",
            "sketchup",
            "su",
            "sketchy",
            "edgefx",
            "gumball",
            "gizmo",
            "reducemotion",
            "camera",
            "save",
            "help",
            "template",
            "critique",
            "basemap",
            // standard-view fall-through arm:
            "top",
            "bottom",
            "front",
            "back",
            "left",
            "right",
            "persp",
        ];
        for tok in all_classify_tokens {
            let advertised = CANONICAL_DECK_APP_VERBS.contains(&tok);
            let denied = DECK_APP_VERB_DENYLIST.contains(&tok);
            assert!(
                advertised ^ denied,
                "classify token '{tok}' must be in exactly one of \
                 CANONICAL_DECK_APP_VERBS / DECK_APP_VERB_DENYLIST (advertised={advertised}, denied={denied})"
            );
        }
    }

    #[test]
    fn prompt_advertises_every_deck_app_verb() {
        // COMPLETENESS (app-verb plane): every canonical deck-relevant app-verb
        // token must appear in the deck system prompt. Fails if a new
        // advertise-worthy app-verb is added to CANONICAL_DECK_APP_VERBS without
        // a corresponding VIEW_VERB_HELP entry.
        let prompt = itsjustcad_deck::system_prompt("", &itsjustcad_commands::PluginRegistry::new());
        for tok in CANONICAL_DECK_APP_VERBS {
            assert!(
                prompt.contains(tok),
                "deck system prompt is missing canonical app-verb token '{tok}' \
                 — add it to VIEW_VERB_HELP"
            );
        }
        // Each canonical verb must actually be recognised by `classify` (never
        // advertise dead syntax). Verbs that require an argument are exercised
        // with a representative one; the argument-free forms classify bare.
        for line in [
            "camera 2point",
            "display shaded",
            "light sun",
            "sketchy on",
            "edgefx jitter=.05",
            "profiles on",
            "meshedges off",
            "gumball on",
            "ze",
            "zoomextents",
            "top",
            "bottom",
            "front",
            "back",
            "left",
            "right",
            "persp",
            "basemap off",
        ] {
            assert!(
                classify(line).is_some(),
                "canonical app-verb line '{line}' does not classify"
            );
        }
    }

    #[test]
    fn view_verb_help_stays_consistent_with_classify() {
        let h = itsjustcad_deck::VIEW_VERB_HELP;
        // Every verb advertised must actually classify as an app verb, so the
        // model is never told about syntax the dispatcher rejects.
        for line in [
            "ze",
            "zoomextents",
            "top",
            "front",
            "persp",
            "display pencil",
            "display shaded",
            "display xray",
            "display ghosted",
            "sketchup",
            "su",
            "light sun",
            "lightmode working",
            "sketchy on",
            "edgefx jitter=.05",
            "profiles on",
            "profileedges off",
            "camera 2point",
            "camera persp",
            "camera pano",
            "camera fisheye 120",
            "camera 35mm",
            "camera phone iphone-ultrawide",
            "basemap sat 800 0.6",
            "basemap off",
        ] {
            assert!(classify(line).is_some(), "help advertises '{line}' but classify rejects it");
        }
        // Key syntax substrings the deck prompt relies on.
        assert!(h.contains("display shaded|wireframe|xray|ghosted|pencil"));
        assert!(h.contains("light working|sun|presentation"));
        assert!(h.contains("sketchy on|off"));
        assert!(h.contains("camera fisheye [fov]"));
        assert!(h.contains("camera phone <lens>"));
        assert!(h.contains("iphone-ultrawide"));
        assert!(h.contains("galaxy-tele"));
        assert!(h.contains("zoom to fit"));
    }

    #[test]
    fn substrate_verbs_fall_through() {
        assert_eq!(classify("box 0,0,0 1,1,1"), None);
        assert_eq!(classify("layer walls"), None);
        assert_eq!(classify(""), None);
    }
}
