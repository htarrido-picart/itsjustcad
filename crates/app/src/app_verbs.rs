// SPDX-License-Identifier: AGPL-3.0-only
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
    /// Frame only the current selection (`zs` / `zoomselected`).
    ZoomSelected,
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
    /// Toggle thick profile edges (`profileedges [on|off]`).
    /// `None` means bare toggle.
    ProfileEdges(Option<bool>),
    /// Toggle the thin mesh feature edges drawn in Shaded mode by default
    /// (`shadededges [on|off]` / `meshedges`). `None` means bare toggle.
    ShadedEdges(Option<bool>),
    /// Show or hide the ground grid lines (`grid [on|off]`, #364). Not the
    /// grid snap, which stays `osnap grid`. `None` means bare toggle.
    Grid(Option<bool>),
    /// Show or hide the world X/Y/Z axes (`axes [on|off]`, #364). `None`
    /// means bare toggle.
    Axes(Option<bool>),
    /// Parallel projection of the active view (`parallel [on|off]` / `ortho
    /// view`, #364); same as `camera parallel` / `camera persp`. `None` means
    /// bare toggle.
    Parallel(Option<bool>),
    /// Dashed occluded edges in the opaque display modes (`backedges
    /// [on|off]`, #361). `None` means bare toggle.
    BackEdges(Option<bool>),
    /// Depth cue outside Sketchy (`depthcue [on|off] [px]`, #361): nearer
    /// edges thicker. `.0` `None` = bare toggle (a lone width turns it on),
    /// `.1` = width of the nearest edges in logical px.
    DepthCue(Option<bool>, Option<f32>),
    /// Edge colour (`edgecolor layer|same|material|axis [#rrggbb]`, #361).
    /// `.0` `None` with no colour reports the setting; a lone colour means
    /// `same`.
    EdgeColor(Option<itsjustcad_render::EdgeColor>, Option<[u8; 3]>),
    /// Edge extension outside Sketchy (`extension [on|off] [m]`, #361).
    /// `.0` `None` = bare toggle (a lone length turns it on), `.1` = the
    /// overshoot in m.
    Extension(Option<bool>, Option<f32>),
    /// Fog (#363): `fog [on|off]`, `fog <start%> <end%>`, `fog colour
    /// bg|custom [#rrggbb]|#rrggbb` (`color` too).
    Fog(FogVerb),
    /// Viewport anti-aliasing (`antialias off|2|4|8` / `aa`): the MSAA sample
    /// count, 1 = off (#356). `None` reports the current setting.
    AntiAlias(Option<u32>),
    /// Toggle 2D top-view planting symbols for planted trees
    /// (`plantsymbols [on|off]` / `plansymbols`). `None` means bare toggle.
    /// When on, each `plant:<id>` mesh also draws its plan drafting glyph flat
    /// on the ground (reads as a planting plan in the top view).
    PlantSymbols(Option<bool>),
    /// Massing display preset (`massing`, alias `clay`): working light +
    /// profile edges + gradient background.
    Massing,
    /// Toggle hand-drawn "sketchy edges" NPR character (`sketchy [on|off]`).
    /// `None` means bare toggle.
    Sketchy(Option<bool>),
    /// Tune the sketchy edge effect (`edgefx jitter=.. extension=.. …`).
    /// Carries the raw `key=value` tokens for the front-end to apply.
    EdgeFx(Vec<String>),
    /// Toggle the transform gumball/gizmo (`gumball [on|off|toggle]` / Cmd+G hotkey).
    /// `None` means bare toggle.
    Gumball(Option<bool>),
    /// Toggle Reduce Motion for animated progress bars (`reducemotion [on|off]`).
    /// `None` means bare toggle.
    ReduceMotion(Option<bool>),
    /// Mouse-navigation preset (`mouse right|middle`, #367). Persisted to
    /// ui.json; view state, never logged. Unknown/missing preset → `None`.
    Mouse(crate::nav::MouseNav),
    /// Toggle at-rest encryption of chat-session files (`chatencryption [on|off]`
    /// / `encryptchats`). `None` means bare toggle. Persisted to ui.json; a
    /// disabled setting writes new saves in plaintext (existing encrypted stores
    /// still load and re-save in the chosen mode).
    ChatEncryption(Option<bool>),
    /// Set the UI language (`language en|es` / `lang es`). Carries the parsed
    /// [`crate::i18n::Lang`]; persisted to ui.json and applied live. A bare
    /// `language` with no/unknown argument is rejected by `classify` (returns
    /// `None`) so the command line reports an error rather than silently no-op.
    Language(crate::i18n::Lang),
    /// Switch the working-style skin (`skin native|classic|nurbs|bim`). Carries
    /// the parsed [`crate::preset::CadOrigin`]; persisted to ui.json and applied
    /// live (palette/accent/fonts + alias table). A bare `skin` with no/unknown
    /// argument is rejected by `classify` (returns `None`) so the command line
    /// reports usage rather than silently no-op. Dark/light stays orthogonal.
    Skin(crate::preset::CadOrigin),
    /// Persist the document (`save [path]`). Argument is the optional path.
    Save(Option<String>),
    /// Command reference (`help [verb]`).
    Help(Option<String>),
    /// Draft an alpha feedback ticket (`feedback bug|idea | title: … | <field>:
    /// …`, fields from the deck's ticket templates). The GUI opens the review sheet with the draft pre-filled;
    /// nothing is sent until the user clicks. Headless warns and ignores it.
    Feedback(itsjustcad_deck::FeedbackDraft),
    /// GUI-only verb with no headless meaning (`template`, `critique`, …).
    /// Carried so the headless runner can warn about the specific name.
    GuiOnly(&'static str),
    /// AI diffusion render of the current view (`render <prompt…>` /
    /// `render cancel`). Carries the raw words after the verb; the GUI parses
    /// them (`diffusion::parse_render_words`), captures the control images and
    /// drives the configured render cassette. Network reaches ONLY the
    /// user-configured backend in render_decks.json; ships unconfigured.
    Render(Vec<String>),
    /// Object-snap control (`osnap on|off` toggles the master switch;
    /// `osnap <kind> on|off` toggles a single kind). `.0` = the target: `None`
    /// for the master switch, `Some(kind_key)` for a per-kind toggle. `.1` = the
    /// requested state (`None` = bare toggle). UI/session state, persisted to
    /// ui.json; never part of the op-log. Driveable from the command line and
    /// the UI plane so the deck can flip snapping.
    Osnap(Option<String>, Option<bool>),
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

/// Parse an `antialias` argument into an MSAA sample count (1 = off).
pub fn parse_msaa(arg: &str) -> Option<u32> {
    Some(match arg.to_ascii_lowercase().as_str() {
        "off" | "none" | "0" | "1" | "1x" => 1,
        "2" | "2x" => 2,
        "4" | "4x" | "on" => 4,
        "8" | "8x" => 8,
        _ => return None,
    })
}

/// A toggle argument: `Some(None)` for a bare toggle (or `toggle`),
/// `Some(Some(on))` for on/off, `None` for anything else.
pub fn on_off(arg: Option<&str>) -> Option<Option<bool>> {
    Some(match arg.map(str::to_ascii_lowercase).as_deref() {
        None | Some("toggle") => None,
        Some("on" | "true" | "1" | "show") => Some(true),
        Some("off" | "false" | "0" | "hide") => Some(false),
        _ => return None,
    })
}

/// `[on|off|toggle] [number]` in either order of presence: the toggle (as
/// [`on_off`]; a lone number means on) and the positive number, if any.
/// `None` for anything else.
fn toggle_with_number<'a>(
    mut words: impl Iterator<Item = &'a str>,
    range: std::ops::RangeInclusive<f32>,
) -> Option<(Option<bool>, Option<f32>)> {
    let num = |w: &str| {
        let v: f32 = w.trim_end_matches("px").trim_end_matches('m').parse().ok()?;
        (v.is_finite() && v > 0.0).then(|| v.clamp(*range.start(), *range.end()))
    };
    let out = match words.next() {
        None => (None, None),
        Some(w) => match num(w) {
            Some(v) => (Some(true), Some(v)),
            None => {
                let on = on_off(Some(w))?;
                match words.next() {
                    None => (on, None),
                    Some(n) => (on, Some(num(n)?)),
                }
            }
        },
    };
    words.next().is_none().then_some(out)
}

/// One `fog` verb (#363).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FogVerb {
    /// `fog [on|off|toggle]`; `None` = bare toggle.
    Toggle(Option<bool>),
    /// `fog <start%> <end%>`: the fade's start and end as fractions (0..1)
    /// of the view depth, start below end. Turns the fog on.
    Range(f32, f32),
    /// `fog colour [bg|custom] [#rrggbb]`: the colour source and the custom
    /// colour (a lone colour means custom). Both `None` reports.
    Colour(Option<itsjustcad_render::FogColor>, Option<[u8; 3]>),
}

/// The words after `fog`.
fn parse_fog<'a>(mut words: impl Iterator<Item = &'a str>) -> Option<FogVerb> {
    use itsjustcad_render::{parse_hex_rgb, FogColor};
    let pct = |w: &str| {
        let v: f32 = w.trim_end_matches('%').parse().ok()?;
        (v.is_finite() && (0.0..=100.0).contains(&v)).then_some(v / 100.0)
    };
    let out = match words.next() {
        Some(w) if w.eq_ignore_ascii_case("colour") || w.eq_ignore_ascii_case("color") => {
            match words.next().map(str::to_ascii_lowercase).as_deref() {
                None => FogVerb::Colour(None, None),
                Some("bg" | "background") => FogVerb::Colour(Some(FogColor::Background), None),
                Some("custom") => FogVerb::Colour(
                    Some(FogColor::Custom),
                    match words.next() {
                        None => None,
                        Some(hex) => Some(parse_hex_rgb(hex)?),
                    },
                ),
                Some(hex) => FogVerb::Colour(Some(FogColor::Custom), Some(parse_hex_rgb(hex)?)),
            }
        }
        Some(w) if pct(w).is_some() => {
            let (start, end) = (pct(w)?, pct(words.next()?)?);
            if start >= end {
                return None;
            }
            FogVerb::Range(start, end)
        }
        w => FogVerb::Toggle(on_off(w)?),
    };
    words.next().is_none().then_some(out)
}

/// A fog verb applied to `style` (#363), shared by the app and the headless
/// runner. Other verbs leave it unchanged; a bare `fog colour` only reports.
pub fn apply_fog_style(
    mut style: itsjustcad_render::FogStyle,
    verb: &AppVerb,
) -> itsjustcad_render::FogStyle {
    match *verb {
        AppVerb::Fog(FogVerb::Toggle(on)) => style.on = on.unwrap_or(!style.on),
        AppVerb::Fog(FogVerb::Range(start, end)) => {
            style.on = true;
            style.start = start;
            style.end = end;
        }
        AppVerb::Fog(FogVerb::Colour(color, rgb)) => {
            if let Some(c) = color {
                style.color = c;
            }
            if let Some(rgb) = rgb {
                style.custom_rgb = rgb;
            }
        }
        _ => {}
    }
    style
}

/// `edgecolor [layer|same|material|axis] [#rrggbb]`.
fn parse_edge_color<'a>(
    mut words: impl Iterator<Item = &'a str>,
) -> Option<(Option<itsjustcad_render::EdgeColor>, Option<[u8; 3]>)> {
    use itsjustcad_render::{parse_hex_rgb, EdgeColor};
    let out = match words.next() {
        None => (None, None),
        Some(w) => match (EdgeColor::parse(w), parse_hex_rgb(w)) {
            (Some(c), _) => match words.next() {
                None => (Some(c), None),
                Some(hex) => (Some(c), Some(parse_hex_rgb(hex)?)),
            },
            (None, Some(rgb)) => (Some(EdgeColor::Same), Some(rgb)),
            (None, None) => return None,
        },
    };
    words.next().is_none().then_some(out)
}

/// An edge-style verb applied to `style` (#361), shared by the app and the
/// headless runner. Other verbs leave it unchanged; a bare `edgecolor`
/// only reports.
pub fn apply_edge_style(
    mut style: itsjustcad_render::EdgeStyle,
    verb: &AppVerb,
) -> itsjustcad_render::EdgeStyle {
    match *verb {
        AppVerb::BackEdges(on) => style.back_edges = on.unwrap_or(!style.back_edges),
        AppVerb::DepthCue(on, px) => {
            style.depth_cue = on.unwrap_or(!style.depth_cue);
            if let Some(px) = px {
                style.depth_cue_px = px;
            }
        }
        AppVerb::EdgeColor(color, rgb) => {
            if let Some(c) = color {
                style.color = c;
            }
            if let Some(rgb) = rgb {
                style.same_rgb = rgb;
            }
        }
        AppVerb::Extension(on, m) => {
            style.extension = on.unwrap_or(!style.extension);
            if let Some(m) = m {
                style.extension_m = m;
            }
        }
        _ => {}
    }
    style
}

/// Classify a command line as an app-level verb, or `None` if it should fall
/// through to the substrate parser / draw tools. The single source of truth
/// for which verbs the app owns.
pub fn classify(line: &str) -> Option<AppVerb> {
    let mut words = line.split_whitespace();
    let verb = words.next()?;
    Some(match verb {
        "ze" | "zoomextents" => AppVerb::ZoomExtents,
        "zs" | "zoomselected" => AppVerb::ZoomSelected,
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
        // `grid <name> x A:0 …` defines a structural grid (substrate): only a
        // bare or single on/off word is the view toggle.
        "grid" if line.split_whitespace().count() <= 2 => AppVerb::Grid(on_off(words.next())?),
        "axes" => AppVerb::Axes(on_off(words.next())?),
        "parallel" | "parallelprojection" => AppVerb::Parallel(on_off(words.next())?),
        "backedges" => AppVerb::BackEdges(on_off(words.next())?),
        "depthcue" => {
            let (on, px) = toggle_with_number(words, 1.0..=24.0)?;
            AppVerb::DepthCue(on, px)
        }
        "edgecolor" | "edgecolour" => {
            let (c, rgb) = parse_edge_color(words)?;
            AppVerb::EdgeColor(c, rgb)
        }
        "extension" => {
            let (on, m) = toggle_with_number(words, 0.001..=10.0)?;
            AppVerb::Extension(on, m)
        }
        "fog" => AppVerb::Fog(parse_fog(words)?),
        "antialias" | "aa" => AppVerb::AntiAlias(match words.next() {
            None => None,
            Some(arg) => Some(parse_msaa(arg)?),
        }),
        "plantsymbols" | "plansymbols" => AppVerb::PlantSymbols(match words.next() {
            Some("on" | "true" | "1") => Some(true),
            Some("off" | "false" | "0") => Some(false),
            None => None,
            _ => return None,
        }),
        "massing" | "clay" => AppVerb::Massing,
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
        "mouse" => AppVerb::Mouse(crate::nav::MouseNav::from_code(words.next()?)?),
        "chatencryption" | "encryptchats" => AppVerb::ChatEncryption(match words.next() {
            Some("on" | "true" | "1") => Some(true),
            Some("off" | "false" | "0") => Some(false),
            None => None,
            _ => return None,
        }),
        "language" | "lang" => AppVerb::Language(crate::i18n::Lang::from_code(words.next()?)?),
        "skin" | "theme_skin" => AppVerb::Skin(crate::preset::CadOrigin::from_code(words.next()?)?),
        "osnap" | "snap" => {
            // `osnap on|off` → master switch; `osnap <kind> [on|off]` → per-kind.
            let first = words.next();
            match first {
                None => AppVerb::Osnap(None, None), // bare toggle of the master
                Some("on" | "true" | "1") => AppVerb::Osnap(None, Some(true)),
                Some("off" | "false" | "0") => AppVerb::Osnap(None, Some(false)),
                Some("toggle") => AppVerb::Osnap(None, None),
                Some(kind) => {
                    // Must be a recognised snap-kind key (end/mid/cen/int/…);
                    // reject unknown so the command line reports usage.
                    if crate::osnap::SnapKind::from_key(kind).is_none() && kind != "grid" {
                        return None;
                    }
                    let state = match words.next() {
                        Some("on" | "true" | "1") => Some(true),
                        Some("off" | "false" | "0") => Some(false),
                        Some("toggle") | None => None,
                        _ => return None,
                    };
                    AppVerb::Osnap(Some(kind.to_ascii_lowercase()), state)
                }
            }
        }
        "camera" => AppVerb::Camera(
            words.next().map(str::to_ascii_lowercase),
            words.next().map(str::to_ascii_lowercase),
        ),
        "save" => AppVerb::Save(words.next().map(itsjustcad_commands::paths::expand_tilde)),
        "help" => AppVerb::Help(words.next().map(str::to_owned)),
        "render" => AppVerb::Render(words.map(str::to_owned).collect()),
        "template" => AppVerb::GuiOnly("template"),
        "cutexport" => AppVerb::GuiOnly("cutexport"),
        "critique" => AppVerb::GuiOnly("critique"),
        "feedback" => AppVerb::Feedback(itsjustcad_deck::parse_feedback_line(line)?),
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
        assert_eq!(classify("zs"), Some(AppVerb::ZoomSelected));
        assert_eq!(classify("zoomselected"), Some(AppVerb::ZoomSelected));
        assert_eq!(classify("front"), Some(AppVerb::View(StandardView::Front)));
        assert_eq!(classify("persp"), Some(AppVerb::View(StandardView::Perspective)));
    }

    #[test]
    fn classifies_grid_axes_and_parallel_toggles() {
        // #364: view toggles, bare = flip.
        assert_eq!(classify("grid"), Some(AppVerb::Grid(None)));
        assert_eq!(classify("grid off"), Some(AppVerb::Grid(Some(false))));
        assert_eq!(classify("grid ON"), Some(AppVerb::Grid(Some(true))));
        assert_eq!(classify("axes hide"), Some(AppVerb::Axes(Some(false))));
        assert_eq!(classify("axes toggle"), Some(AppVerb::Axes(None)));
        assert_eq!(classify("parallel on"), Some(AppVerb::Parallel(Some(true))));
        assert_eq!(classify("parallel"), Some(AppVerb::Parallel(None)));
        assert_eq!(classify("grid sideways"), None);
        assert_eq!(classify("grid 1 x A:0 B:5"), None, "structural grid stays substrate");
        // The grid snap stays its own verb.
        assert_eq!(classify("osnap grid off"), Some(AppVerb::Osnap(Some("grid".into()), Some(false))));
        assert_eq!(
            classify("camera parallel"),
            Some(AppVerb::Camera(Some("parallel".into()), None))
        );
    }

    #[test]
    fn classifies_edge_style_verbs() {
        use itsjustcad_render::EdgeColor;
        // #361: back edges, depth cue, edge colour, extension.
        assert_eq!(classify("backedges"), Some(AppVerb::BackEdges(None)));
        assert_eq!(classify("backedges on"), Some(AppVerb::BackEdges(Some(true))));
        assert_eq!(classify("backedges dashed"), None);
        assert_eq!(classify("depthcue"), Some(AppVerb::DepthCue(None, None)));
        assert_eq!(classify("depthcue off"), Some(AppVerb::DepthCue(Some(false), None)));
        assert_eq!(classify("depthcue on 4"), Some(AppVerb::DepthCue(Some(true), Some(4.0))));
        assert_eq!(classify("depthcue 5px"), Some(AppVerb::DepthCue(Some(true), Some(5.0))));
        assert_eq!(classify("depthcue on 99"), Some(AppVerb::DepthCue(Some(true), Some(24.0))));
        assert_eq!(classify("depthcue on wide"), None);
        assert_eq!(classify("depthcue on 3 4"), None);
        assert_eq!(classify("depthcue -2"), None);
        assert_eq!(classify("edgecolor"), Some(AppVerb::EdgeColor(None, None)));
        assert_eq!(classify("edgecolor axis"), Some(AppVerb::EdgeColor(Some(EdgeColor::Axis), None)));
        assert_eq!(
            classify("edgecolor same #ff0000"),
            Some(AppVerb::EdgeColor(Some(EdgeColor::Same), Some([255, 0, 0])))
        );
        assert_eq!(
            classify("edgecolour 102030"),
            Some(AppVerb::EdgeColor(Some(EdgeColor::Same), Some([16, 32, 48])))
        );
        assert_eq!(classify("edgecolor material"), Some(AppVerb::EdgeColor(Some(EdgeColor::Material), None)));
        assert_eq!(classify("edgecolor layer"), Some(AppVerb::EdgeColor(Some(EdgeColor::Layer), None)));
        assert_eq!(classify("edgecolor plaid"), None);
        assert_eq!(classify("edgecolor same #12"), None);
        assert_eq!(classify("extension"), Some(AppVerb::Extension(None, None)));
        assert_eq!(classify("extension on 0.2"), Some(AppVerb::Extension(Some(true), Some(0.2))));
        assert_eq!(classify("extension 0.3m"), Some(AppVerb::Extension(Some(true), Some(0.3))));
        assert_eq!(classify("extension off"), Some(AppVerb::Extension(Some(false), None)));
    }

    #[test]
    fn classifies_fog_verbs() {
        use itsjustcad_render::FogColor;
        // #363: toggle, range in percent, colour.
        let fog = |v| Some(AppVerb::Fog(v));
        assert_eq!(classify("fog"), fog(FogVerb::Toggle(None)));
        assert_eq!(classify("fog on"), fog(FogVerb::Toggle(Some(true))));
        assert_eq!(classify("fog off"), fog(FogVerb::Toggle(Some(false))));
        assert_eq!(classify("fog 20 80"), fog(FogVerb::Range(0.2, 0.8)));
        assert_eq!(classify("fog 0% 100%"), fog(FogVerb::Range(0.0, 1.0)));
        assert_eq!(classify("fog 80 20"), None, "start below end");
        assert_eq!(classify("fog 20 120"), None);
        assert_eq!(classify("fog 20"), None);
        assert_eq!(classify("fog 20 30 40"), None);
        assert_eq!(classify("fog thick"), None);
        assert_eq!(classify("fog colour"), fog(FogVerb::Colour(None, None)));
        assert_eq!(classify("fog colour bg"), fog(FogVerb::Colour(Some(FogColor::Background), None)));
        assert_eq!(classify("fog color background"), fog(FogVerb::Colour(Some(FogColor::Background), None)));
        assert_eq!(classify("fog colour custom"), fog(FogVerb::Colour(Some(FogColor::Custom), None)));
        assert_eq!(
            classify("fog color #ff8000"),
            fog(FogVerb::Colour(Some(FogColor::Custom), Some([255, 128, 0])))
        );
        assert_eq!(
            classify("fog colour custom 102030"),
            fog(FogVerb::Colour(Some(FogColor::Custom), Some([16, 32, 48])))
        );
        assert_eq!(classify("fog colour plaid"), None);
        assert_eq!(classify("fog colour bg #ffffff"), None);
    }

    #[test]
    fn fog_verbs_apply_to_the_style() {
        use itsjustcad_render::{FogColor, FogStyle};
        let mut f = FogStyle::default();
        f = apply_fog_style(f, &classify("fog 10 50").unwrap());
        assert!(f.on && (f.start, f.end) == (0.1, 0.5), "a range turns fog on");
        f = apply_fog_style(f, &classify("fog").unwrap());
        assert!(!f.on);
        f = apply_fog_style(f, &classify("fog colour #102030").unwrap());
        assert_eq!((f.color, f.custom_rgb), (FogColor::Custom, [16, 32, 48]));
        // Back to the background keeps the custom colour for later.
        f = apply_fog_style(f, &classify("fog colour bg").unwrap());
        assert_eq!((f.color, f.custom_rgb), (FogColor::Background, [16, 32, 48]));
        assert_eq!(apply_fog_style(f, &classify("fog colour").unwrap()), f, "report only");
        assert_eq!(apply_fog_style(f, &classify("grid off").unwrap()), f);
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
    fn classifies_antialias_verb() {
        assert_eq!(classify("antialias off"), Some(AppVerb::AntiAlias(Some(1))));
        assert_eq!(classify("aa 2"), Some(AppVerb::AntiAlias(Some(2))));
        assert_eq!(classify("aa 4x"), Some(AppVerb::AntiAlias(Some(4))));
        assert_eq!(classify("antialias 8"), Some(AppVerb::AntiAlias(Some(8))));
        assert_eq!(classify("antialias"), Some(AppVerb::AntiAlias(None)));
        // Unsupported counts are rejected (usage error), not silently clamped.
        assert_eq!(classify("antialias 16"), None);
        assert_eq!(classify("aa 3"), None);
    }

    #[test]
    fn classifies_language_verb() {
        use crate::i18n::Lang;
        assert_eq!(classify("language en"), Some(AppVerb::Language(Lang::En)));
        assert_eq!(classify("language es"), Some(AppVerb::Language(Lang::Es)));
        assert_eq!(classify("lang es-CO"), Some(AppVerb::Language(Lang::Es)));
        // Every Romance language parses (M-romance).
        assert_eq!(classify("language pt"), Some(AppVerb::Language(Lang::Pt)));
        assert_eq!(classify("language fr"), Some(AppVerb::Language(Lang::Fr)));
        assert_eq!(classify("language it"), Some(AppVerb::Language(Lang::It)));
        assert_eq!(classify("language ro"), Some(AppVerb::Language(Lang::Ro)));
        assert_eq!(classify("language ca"), Some(AppVerb::Language(Lang::Ca)));
        assert_eq!(classify("language gl"), Some(AppVerb::Language(Lang::Gl)));
        assert_eq!(classify("lang pt-BR"), Some(AppVerb::Language(Lang::Pt)));
        // Bare or unknown argument is not an app verb (falls through → error).
        assert_eq!(classify("language"), None);
        assert_eq!(classify("language de"), None);
    }

    #[test]
    fn classifies_skin_verb() {
        use crate::preset::CadOrigin;
        assert_eq!(classify("skin native"), Some(AppVerb::Skin(CadOrigin::None)));
        assert_eq!(classify("skin classic"), Some(AppVerb::Skin(CadOrigin::Classic)));
        assert_eq!(classify("skin nurbs"), Some(AppVerb::Skin(CadOrigin::Nurbs)));
        assert_eq!(classify("skin bim"), Some(AppVerb::Skin(CadOrigin::Bim)));
        // Synonyms + case-insensitivity.
        assert_eq!(classify("skin CLASSIC"), Some(AppVerb::Skin(CadOrigin::Classic)));
        // Pre-rename codes are rejected (fall through → usage).
        for old in ["skin autocad", "skin ACAD", "skin rhino", "skin revit"] {
            assert_eq!(classify(old), None, "{old}");
        }
        assert_eq!(classify("skin default"), Some(AppVerb::Skin(CadOrigin::None)));
        // Bare or unknown argument falls through → command line reports usage.
        assert_eq!(classify("skin"), None);
        assert_eq!(classify("skin bogus"), None);
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
        assert_eq!(classify("plantsymbols on"), Some(AppVerb::PlantSymbols(Some(true))));
        assert_eq!(classify("plantsymbols off"), Some(AppVerb::PlantSymbols(Some(false))));
        assert_eq!(classify("plansymbols"), Some(AppVerb::PlantSymbols(None)));
        assert_eq!(classify("plantsymbols garbage"), None);
        assert_eq!(classify("massing"), Some(AppVerb::Massing));
        assert_eq!(classify("clay"), Some(AppVerb::Massing));
        // The old brand-named verb is gone.
        assert_eq!(classify("sketchup"), None);
        assert_eq!(classify("su"), None);
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
    fn classifies_mouse() {
        use crate::nav::MouseNav;
        assert_eq!(classify("mouse right"), Some(AppVerb::Mouse(MouseNav::Right)));
        assert_eq!(classify("mouse middle"), Some(AppVerb::Mouse(MouseNav::Middle)));
        assert_eq!(classify("mouse sketchup"), Some(AppVerb::Mouse(MouseNav::Middle)));
        assert_eq!(classify("mouse"), None);
        assert_eq!(classify("mouse wheel"), None);
    }

    #[test]
    fn classifies_reduce_motion() {
        assert_eq!(classify("reducemotion on"), Some(AppVerb::ReduceMotion(Some(true))));
        assert_eq!(classify("reducemotion off"), Some(AppVerb::ReduceMotion(Some(false))));
        assert_eq!(classify("reducemotion 1"), Some(AppVerb::ReduceMotion(Some(true))));
        assert_eq!(classify("reducemotion 0"), Some(AppVerb::ReduceMotion(Some(false))));
        assert_eq!(classify("reducemotion"), Some(AppVerb::ReduceMotion(None)));
        assert_eq!(classify("reducemotion garbage"), None);
        assert_eq!(classify("chatencryption on"), Some(AppVerb::ChatEncryption(Some(true))));
        assert_eq!(classify("encryptchats off"), Some(AppVerb::ChatEncryption(Some(false))));
        assert_eq!(classify("chatencryption"), Some(AppVerb::ChatEncryption(None)));
        assert_eq!(classify("chatencryption garbage"), None);
    }

    #[test]
    fn classifies_osnap() {
        // Master switch.
        assert_eq!(classify("osnap on"), Some(AppVerb::Osnap(None, Some(true))));
        assert_eq!(classify("osnap off"), Some(AppVerb::Osnap(None, Some(false))));
        assert_eq!(classify("osnap"), Some(AppVerb::Osnap(None, None)));
        assert_eq!(classify("snap toggle"), Some(AppVerb::Osnap(None, None)));
        // Per-kind.
        assert_eq!(classify("osnap end on"), Some(AppVerb::Osnap(Some("end".into()), Some(true))));
        assert_eq!(classify("osnap int off"), Some(AppVerb::Osnap(Some("int".into()), Some(false))));
        assert_eq!(classify("osnap tan"), Some(AppVerb::Osnap(Some("tan".into()), None)));
        assert_eq!(classify("osnap grid off"), Some(AppVerb::Osnap(Some("grid".into()), Some(false))));
        // Unknown kind / bad state falls through → usage error.
        assert_eq!(classify("osnap bogus on"), None);
        assert_eq!(classify("osnap end garbage"), None);
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
    fn classifies_feedback() {
        use itsjustcad_deck::{FeedbackCategory, FeedbackDraft};
        assert_eq!(classify("feedback"), Some(AppVerb::Feedback(FeedbackDraft::default())));
        match classify("feedback idea | goal: print | title: Dark sheets").unwrap() {
            AppVerb::Feedback(d) => {
                assert_eq!(d.category, FeedbackCategory::Idea);
                assert_eq!(d.get("title"), "Dark sheets");
                assert_eq!(d.get("goal"), "print");
            }
            other => panic!("{other:?}"),
        }
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
        "plantsymbols",
        "gumball",
        "grid",
        "axes",
        "parallel",
        "backedges",
        "depthcue",
        "edgecolor",
        "extension",
        "fog",
        "ze",
        "zoomextents",
        "zs",
        "zoomselected",
        "top",
        "bottom",
        "front",
        "back",
        "left",
        "right",
        "persp",
        "basemap",
        "render",
        // Opens the review sheet only; the user sends (or not).
        "feedback",
        // `help <verb>` is an ON-DEMAND deck TOOL, not a view verb: the deck
        // emits it to fetch a verb's full syntax + examples (the compact catalog
        // gives only names + one-liners), and the output threads back to the
        // model mid-turn. Advertised (via the compact prompt's help teaching),
        // so it lives here rather than on the denylist.
        "help",
    ];

    /// Verbs that `classify` owns but the deck must NOT advertise: internal /
    /// GUI-only / security-gated / alias forms. Kept explicit so the audit below
    /// can prove `CANONICAL_DECK_APP_VERBS` ∪ denylist == every classify arm,
    /// i.e. no verb is silently dropped from consideration.
    const DECK_APP_VERB_DENYLIST: &[&str] = &[
        "critique",
        "cutexport",     // GUI-only
        "template",     // GUI-only
        "save",         // fs write — refused on deck plane
        "antialias",    // render-quality setting, not a drawing/view verb
        "aa",           // alias of `antialias`
        "reducemotion",  // accessibility toggle, not a drawing/view verb the model reframes with
        "mouse",        // input preference, not a drawing/view verb
        "chatencryption", // privacy setting, not a drawing/view verb
        "encryptchats",   // alias of `chatencryption`
        "lightmode",    // alias of `light`
        "profileedges", // alias of `profiles`
        "shadededges",  // alias of `meshedges`
        "plansymbols",  // alias of `plantsymbols`
        "massing",      // preset; advertised as `massing`, not a standalone canonical token here
        "clay",         // alias of `massing`
        "gizmo",        // alias of `gumball`
        "skin",         // UI skin switch, not a drawing/view verb the model reframes with
        "theme_skin",   // alias of `skin`
        "osnap",        // object-snap toggle: interactive-input setting, not a view/draw verb
        "snap",         // alias of `osnap`
        "parallelprojection", // alias of `parallel`
        "edgecolour",   // alias of `edgecolor`
    ];

    #[test]
    fn app_verb_denylist_partitions_classify() {
        // Every token that `classify` recognises must be accounted for: it is
        // either advertised (in CANONICAL_DECK_APP_VERBS) or explicitly denied
        // (DECK_APP_VERB_DENYLIST). If someone adds a new arm to `classify`
        // without deciding which bucket it belongs to, this fails — forcing an
        // explicit advertise-or-deny decision. Note: `massing`/`clay` map to
        // the Massing preset advertised inline in VIEW_VERB_HELP; they live on the
        // denylist as standalone canonical tokens.
        let all_classify_tokens = [
            "ze",
            "zoomextents",
            "zs",
            "zoomselected",
            "display",
            "lightmode",
            "light",
            "profileedges",
            "profiles",
            "shadededges",
            "meshedges",
            "antialias",
            "aa",
            "plantsymbols",
            "plansymbols",
            "massing",
            "clay",
            "sketchy",
            "edgefx",
            "gumball",
            "gizmo",
            "reducemotion",
            "mouse",
            "chatencryption",
            "encryptchats",
            "skin",
            "theme_skin",
            "osnap",
            "snap",
            "grid",
            "axes",
            "parallel",
            "parallelprojection",
            "backedges",
            "depthcue",
            "edgecolor",
            "edgecolour",
            "extension",
            "fog",
            "camera",
            "save",
            "help",
            "template",
            "cutexport",
            "critique",
            "basemap",
            "render",
            "feedback",
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
            "grid off",
            "axes on",
            "parallel on",
            "camera parallel",
            "backedges on",
            "depthcue on 3",
            "edgecolor axis",
            "extension on",
            "fog on",
            "fog 20 100",
            "fog colour bg",
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
            "render glass pavilion at dusk",
            "render cancel",
            "help geodesic",
            itsjustcad_deck::FEEDBACK_EXAMPLE_LINE,
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
            "massing",
            "clay",
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
            "camera phone phone-ultrawide",
            "basemap sat 800 0.6",
            "basemap off",
            "grid off",
            "axes off",
            "camera parallel",
            "parallel on",
            "backedges on",
            "depthcue on 4",
            "edgecolor same #000000",
            "edgecolor material",
            "edgecolor axis",
            "extension on 0.1",
            "fog on",
            "fog 20 100",
            "fog colour bg",
            "fog colour #ffffff",
            "render glass pavilion at dusk",
            "render cancel",
            itsjustcad_deck::FEEDBACK_EXAMPLE_LINE,
        ] {
            assert!(classify(line).is_some(), "help advertises '{line}' but classify rejects it");
        }
        // Key syntax substrings the deck prompt relies on.
        assert!(h.contains("display shaded|wireframe|xray|ghosted|pencil"));
        assert!(h.contains("light working|sun|presentation"));
        assert!(h.contains("sketchy on|off"));
        assert!(h.contains("camera fisheye [fov]"));
        assert!(h.contains("camera phone <lens>"));
        assert!(h.contains("phone-ultrawide"));
        assert!(h.contains("phone-periscope"));
        assert!(h.contains("zoom to fit"));
        assert!(h.contains("grid on|off") && h.contains("axes on|off"));
        assert!(h.contains("backedges on|off") && h.contains("depthcue on|off [px]"));
        assert!(h.contains("edgecolor layer|same|material|axis"));
        assert!(h.contains("fog on|off") && h.contains("fog <start%> <end%>"));
        assert!(h.contains("fog colour bg|#rrggbb"));
    }

    #[test]
    fn substrate_verbs_fall_through() {
        assert_eq!(classify("box 0,0,0 1,1,1"), None);
        assert_eq!(classify("layer walls"), None);
        assert_eq!(classify(""), None);
    }
}
