// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Working-style skin presets — Classic command line, NURBS modeler, BIM-style,
//! or ItsJustCAD default.
//!
//! Each preset encodes: viewport background, grid colors, ui/cmd font size,
//! accent color, command-line flavor, right-click repeat-last flag, and
//! a command alias map. The palette is ItsJustCAD's own.
//!
//! Pure module; no egui or wgpu imports — tested standalone.

/// Which working style (skin) the user picked.
///
/// Serialized to `ui.json` as `"classic"` / `"nurbs"` / `"bim"` / `"none"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CadOrigin {
    Classic,
    Nurbs,
    Bim,
    /// ItsJustCAD default (no working-style skin).
    #[default]
    None,
}

impl CadOrigin {
    /// Plain (unlocalized) English name. The UI now labels skins through the
    /// i18n catalog via [`Self::label_key`]; this stays as a stable building
    /// block for logs / tests / non-localized surfaces.
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            CadOrigin::Classic => "Classic command line",
            CadOrigin::Nurbs => "NURBS modeler",
            CadOrigin::Bim => "BIM-style",
            CadOrigin::None => "ItsJustCAD default",
        }
    }

    /// The i18n key for this skin's onboarding / settings label. Names go through
    /// the `t()` catalog so the picker localizes.
    pub fn label_key(self) -> &'static str {
        match self {
            CadOrigin::Classic => "skin.classic",
            CadOrigin::Nurbs => "skin.nurbs",
            CadOrigin::Bim => "skin.bim",
            CadOrigin::None => "skin.native",
        }
    }

    /// The short slug accepted by the `skin` verb (`"native"`, `"classic"`,
    /// `"nurbs"`, `"bim"`). Distinct from the serde representation only for
    /// `None` → `"native"` (friendlier verb word).
    pub fn code(self) -> &'static str {
        match self {
            CadOrigin::Classic => "classic",
            CadOrigin::Nurbs => "nurbs",
            CadOrigin::Bim => "bim",
            CadOrigin::None => "native",
        }
    }

    /// Parse a verb-supplied / persisted skin name back into a [`CadOrigin`].
    /// Accepts common synonyms (`native`/`default`/`itsjustcad`/`none` →
    /// [`CadOrigin::None`]). Case-insensitive. Unknown → `None` (caller keeps
    /// its current skin and reports usage).
    pub fn from_code(s: &str) -> Option<CadOrigin> {
        match s.trim().to_ascii_lowercase().as_str() {
            "native" | "default" | "itsjustcad" | "none" => Some(CadOrigin::None),
            "classic" => Some(CadOrigin::Classic),
            "nurbs" => Some(CadOrigin::Nurbs),
            "bim" => Some(CadOrigin::Bim),
            _ => None,
        }
    }

    /// All skins in onboarding-picker order (Native default first).
    pub fn all() -> [CadOrigin; 4] {
        [
            CadOrigin::None,
            CadOrigin::Classic,
            CadOrigin::Nurbs,
            CadOrigin::Bim,
        ]
    }
}

/// sRGB color as `[r, g, b, a]` in 0.0–1.0.
pub type Rgba = [f32; 4];

/// Where the command line docks. The NURBS-modeler style puts it at the top
/// (below the menu bar), the Classic style at the bottom. Drives layout order
/// in `app.rs` and the autosuggest popup direction (down from a top line, up
/// from a bottom line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandLinePos {
    Top,
    Bottom,
}

/// Which side the docked tab panel (Layers/Properties/History/Deck) sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelSide {
    // Left is wired by Layer 2's panel-side logic; kept for future skins.
    #[allow(dead_code)]
    Left,
    Right,
}

/// Menu-bar grouping flavor. Modeler and Drafting group the same registry
/// verbs under different top-level menu names (see `crate::menu`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuStyle {
    Modeler,
    Drafting,
}

fn hex_to_rgba(hex: u32) -> Rgba {
    let r = ((hex >> 16) & 0xFF) as f32 / 255.0;
    let g = ((hex >> 8) & 0xFF) as f32 / 255.0;
    let b = (hex & 0xFF) as f32 / 255.0;
    [r, g, b, 1.0]
}

/// Full visual + behavioral description of a legacy-CAD skin.
#[derive(Debug, Clone, PartialEq)]
pub struct UiPreset {
    /// Viewport clear color.
    pub bg_color: Rgba,
    /// Grid major line color.
    pub grid_major_color: Rgba,
    /// Grid minor line color.
    pub grid_minor_color: Rgba,
    /// Crosshair cursor color.
    pub crosshair_color: Rgba,
    /// Accent color (selection highlight, active widget).
    pub accent_color: Rgba,
    /// Body / UI font size in logical pixels (before zoom).
    pub ui_font_px: f32,
    /// Command-line font size in logical pixels.
    pub cmd_font_px: f32,
    /// Right-click in viewport repeats last command (NURBS-modeler convention).
    pub right_click_repeat_last: bool,
    /// Ordered alias table: (alias, canonical_command).
    /// Aliases are looked up BEFORE the parser, case-insensitively.
    pub aliases: &'static [(&'static str, &'static str)],
    /// Where the command line docks (top for NURBS modeler, bottom for Classic).
    pub command_line_pos: CommandLinePos,
    /// Which side the docked tab panel sits on.
    pub panel_side: PanelSide,
    /// Default viewport count on first show (4-up for every skin).
    pub default_viewports: u8,
    /// Menu-bar grouping style.
    pub menu_style: MenuStyle,
}

impl UiPreset {
    /// Build the design-token set for this skin. The surface/accent come from
    /// the preset; the remaining roles are derived by luminance offset so the
    /// UI chrome stays internally consistent (see `theme::roles_from`).
    pub fn tokens(&self) -> crate::theme::Tokens {
        let colors = crate::theme::roles_from(self.bg_surface(), self.accent_color);
        crate::theme::Tokens {
            spacing: crate::theme::Spacing::default(),
            type_scale: crate::theme::TypeScale {
                command: self.cmd_font_px,
                body: self.ui_font_px,
                // WCAG-lifted small floor (11 → 12) even for compact skins.
                small: (self.ui_font_px - 1.0).max(12.0),
                panel_title: self.ui_font_px + 3.0,
                ..crate::theme::TypeScale::default()
            },
            colors,
            radii: crate::theme::Radii::default(),
            dark: crate::theme::is_dark(self.bg_surface()),
        }
    }

    /// Localized label for this preset's menu style (About dialog, skin echo).
    pub fn menu_style_label(&self) -> &'static str {
        crate::i18n::t(match self.menu_style {
            MenuStyle::Modeler => "menustyle.modeler",
            MenuStyle::Drafting => "menustyle.drafting",
        })
    }

    /// UI-chrome surface color. This is distinct from the viewport clear color
    /// (`bg_color`): a NURBS-modeler viewport is light gray, but its panel chrome reads
    /// better on a slightly different neutral. For dark skins the two match; for
    /// light skins we nudge the chrome so panels separate from the canvas.
    fn bg_surface(&self) -> Rgba {
        if is_dark(self) {
            self.bg_color
        } else {
            // Light skins: use a near-white chrome so text and dividers read.
            [0.93, 0.93, 0.94, 1.0]
        }
    }
}

// ── Classic command-line preset ────────────────────────────────────────────────
//   bg      #1E242B — dark slate canvas; grid major slightly lighter, minor faint
//   accent  #59A6FF — the ItsJustCAD default accent

pub const CLASSIC_ALIASES: &[(&str, &str)] = &[
    // Short one- to three-letter command aliases (classic CAD convention)
    ("a", "arc"),
    ("ar", "array"),
    ("b", "block"),
    ("br", "break"),
    ("c", "circle"),
    ("ch", "properties"),
    ("co", "copy"),
    ("cp", "copy"),
    ("d", "dimstyle"),
    ("di", "dist"),
    ("div", "divide"),
    ("do", "donut"),
    ("e", "delete"),
    ("el", "ellipse"),
    ("ex", "extend"),
    ("ext", "extrude"),
    ("f", "fillet"),
    ("g", "group"),
    ("h", "hatch"),
    ("i", "insert"),
    ("j", "join"),
    ("l", "line"),
    ("la", "layer"),
    ("len", "lengthen"),
    ("m", "move"),
    ("me", "measure"),
    ("mi", "mirror"),
    ("o", "offset"),
    ("p", "pan"),
    ("pe", "pedit"),
    ("pl", "polyline"),
    ("po", "point"),
    ("pol", "polygon"),
    ("pu", "purge"),
    ("r", "redraw"),
    ("re", "regen"),
    ("rec", "rect"),
    ("reg", "region"),
    ("rev", "revolve"),
    ("ro", "rotate"),
    ("s", "stretch"),
    ("sc", "scale"),
    ("sl", "slice"),
    ("sp", "spell"),
    ("spl", "spline"),
    ("su", "subtract"),
    ("tr", "trim"),
    ("un", "units"),
    ("v", "view"),
    ("w", "wblock"),
    ("x", "explode"),
    ("z", "zoom"),
];

pub const CLASSIC: UiPreset = UiPreset {
    bg_color: [0.118, 0.141, 0.169, 1.0],  // #1E242B
    grid_major_color: [0.212, 0.255, 0.302, 1.0], // #36414D
    grid_minor_color: [0.153, 0.180, 0.212, 1.0], // #272E36
    crosshair_color: [1.0, 1.0, 1.0, 1.0],         // #FFFFFF
    accent_color: [0.35, 0.65, 1.0, 1.0],          // #59A6FF
    ui_font_px: 13.0,
    cmd_font_px: 13.0,
    right_click_repeat_last: false,
    aliases: CLASSIC_ALIASES,
    command_line_pos: CommandLinePos::Bottom,
    panel_side: PanelSide::Right,
    default_viewports: 4,
    menu_style: MenuStyle::Drafting,
};

// ── NURBS-modeler preset ───────────────────────────────────────────────────────
//   bg      #D0D2D4 — light cool-gray canvas
//   right-click = repeat last command

const NURBS_ALIASES: &[(&str, &str)] = &[
    ("c", "circle"),
    ("l", "line"),
    ("r", "rect"),
    ("p", "point"),
    ("pl", "polyline"),
    ("m", "move"),
    ("cp", "copy"),
    ("ro", "rotate"),
    ("sc", "scale"),
    ("mi", "mirror"),
    ("e", "delete"),
    ("tr", "trim"),
    ("ex", "extend"),
    ("f", "fillet"),
    ("ch", "chamfer"),
    ("of", "offset"),
    ("j", "join"),
    ("exp", "explode"),
];

pub const NURBS: UiPreset = UiPreset {
    bg_color: [0.816, 0.824, 0.831, 1.0],           // #D0D2D4
    grid_major_color: [0.620, 0.631, 0.643, 1.0],   // #9EA1A4
    grid_minor_color: [0.769, 0.776, 0.784, 1.0],   // #C4C6C8
    crosshair_color: [0.227, 0.235, 0.247, 1.0],    // #3A3C3F
    accent_color: [0.910, 0.910, 0.910, 1.0],        // #E8E8E8 panel chrome
    ui_font_px: 13.0,
    cmd_font_px: 13.0,
    right_click_repeat_last: true,
    aliases: NURBS_ALIASES,
    command_line_pos: CommandLinePos::Top,
    panel_side: PanelSide::Right,
    default_viewports: 4,
    menu_style: MenuStyle::Modeler,
};

// ── BIM-style preset ───────────────────────────────────────────────────────────
//   bg      #FFFFFF — white 2D canvas, panel-first
//   accent  #2F6FB2
//   No command aliases (panel-first workflow)

pub const BIM: UiPreset = UiPreset {
    bg_color: [1.0, 1.0, 1.0, 1.0],                 // #FFFFFF
    grid_major_color: [0.8, 0.8, 0.8, 1.0],          // light gray (no explicit grid)
    grid_minor_color: [0.9, 0.9, 0.9, 1.0],
    crosshair_color: [0.0, 0.0, 0.0, 1.0],            // #000000
    accent_color: [0.184, 0.435, 0.698, 1.0],         // #2F6FB2
    ui_font_px: 13.0,
    cmd_font_px: 13.0,
    right_click_repeat_last: false,
    aliases: &[],
    // Panel-first style; we still dock a command line (ItsJustCAD is
    // command-first), but keep it at the bottom and use the Drafting menus.
    command_line_pos: CommandLinePos::Bottom,
    panel_side: PanelSide::Right,
    default_viewports: 4,
    menu_style: MenuStyle::Drafting,
};

// ── ItsJustCAD default ─────────────────────────────────────────────────────────

pub const ITSJUSTCAD_DEFAULT: UiPreset = UiPreset {
    bg_color: [0.13, 0.14, 0.16, 1.0],
    grid_major_color: [0.22, 0.24, 0.27, 1.0],
    grid_minor_color: [0.18, 0.19, 0.21, 1.0],
    crosshair_color: [0.9, 0.92, 0.95, 1.0],
    accent_color: [0.35, 0.65, 1.0, 1.0],
    ui_font_px: 13.0,
    cmd_font_px: 13.0,
    right_click_repeat_last: false,
    aliases: &[],
    // ItsJustCAD default: command line on top, right tab panel, Modeler menu
    // grouping. Boots to the 4-up layout (Top/Front/Right/Perspective).
    command_line_pos: CommandLinePos::Top,
    panel_side: PanelSide::Right,
    default_viewports: 4,
    menu_style: MenuStyle::Modeler,
};

/// Return the static preset for a given origin.
pub fn preset_for(origin: CadOrigin) -> &'static UiPreset {
    match origin {
        CadOrigin::Classic => &CLASSIC,
        CadOrigin::Nurbs => &NURBS,
        CadOrigin::Bim => &BIM,
        CadOrigin::None => &ITSJUSTCAD_DEFAULT,
    }
}

/// Look up `input` in `aliases` (case-insensitive, single-token only).
/// Returns the canonical command name if found, otherwise `None`.
/// Caller passes the full input; we only alias single-token inputs.
#[allow(dead_code)] // used in tests; kept as a building block for callers
pub fn resolve_alias(
    input: &str,
    aliases: &'static [(&'static str, &'static str)],
) -> Option<&'static str> {
    let trimmed = input.trim();
    let first = trimmed.split_whitespace().next()?;
    let first_lower = first.to_lowercase();
    aliases
        .iter()
        .find(|(alias, _)| *alias == first_lower.as_str())
        .map(|(_, canonical)| *canonical)
}

/// Resolve an alias from the input line.
/// Returns `Some(expanded_line)` when an alias fires, `None` when no alias matches.
/// The expanded line replaces the alias verb with the canonical command name and
/// preserves any trailing arguments.
pub fn expand_alias(input: &str, aliases: &'static [(&'static str, &'static str)]) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let first = trimmed.split_whitespace().next()?;
    let rest = trimmed[first.len()..].trim_start();
    let first_lower = first.to_lowercase();
    let canonical = aliases
        .iter()
        .find(|(alias, _)| *alias == first_lower.as_str())
        .map(|(_, canonical)| *canonical)?;

    if rest.is_empty() {
        Some(canonical.to_string())
    } else {
        Some(format!("{canonical} {rest}"))
    }
}

/// Alias suggestions for autosuggest: given a prefix and an alias map, return
/// (alias, canonical) pairs whose alias starts with the prefix.
#[allow(dead_code)] // used in tests; building block for autosuggest callers
pub fn alias_suggestions(
    prefix: &str,
    aliases: &'static [(&'static str, &'static str)],
) -> Vec<(&'static str, &'static str)> {
    let lower = prefix.to_lowercase();
    aliases
        .iter()
        .filter(|(alias, _)| alias.starts_with(lower.as_str()))
        .copied()
        .collect()
}

/// True when the preset uses a dark background (luminance < 0.5).
pub fn is_dark(preset: &UiPreset) -> bool {
    let [r, g, b, _] = preset.bg_color;
    // Perceptual luminance (BT.709)
    let lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    lum < 0.5
}

// ── unused import suppressor ──────────────────────────────────────────────────
#[allow(dead_code)]
fn _use_hex_to_rgba() -> Rgba {
    hex_to_rgba(0x000000)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── preset values ─────────────────────────────────────────────────────────

    #[test]
    fn classic_is_dark() {
        assert!(is_dark(&CLASSIC), "Classic should be dark");
    }

    #[test]
    fn nurbs_is_light() {
        assert!(!is_dark(&NURBS), "NURBS should be light");
    }

    #[test]
    fn bim_is_light() {
        assert!(!is_dark(&BIM), "BIM should be light");
    }

    #[test]
    fn classic_bg_is_near_black() {
        let [r, g, b, _] = CLASSIC.bg_color;
        // All channels below 0.2 (hex #1E242B ≈ 0.118, 0.141, 0.169)
        assert!(r < 0.2 && g < 0.2 && b < 0.2, "Classic bg should be very dark: {r},{g},{b}");
    }

    #[test]
    fn bim_bg_is_white() {
        let [r, g, b, _] = BIM.bg_color;
        assert!((r - 1.0).abs() < 0.01 && (g - 1.0).abs() < 0.01 && (b - 1.0).abs() < 0.01);
    }

    #[test]
    fn nurbs_bg_is_light_gray() {
        let [r, g, b, _] = NURBS.bg_color;
        // #D0D2D4 ≈ 0.82 average
        let mid = (r + g + b) / 3.0;
        assert!(mid > 0.7 && mid < 0.95, "NURBS bg should be light gray: {mid}");
    }

    #[test]
    #[allow(clippy::assertions_on_constants)] // intentionally verifying compiled preset values
    fn nurbs_right_click_repeat_last() {
        assert!(NURBS.right_click_repeat_last);
        assert!(!CLASSIC.right_click_repeat_last);
        assert!(!BIM.right_click_repeat_last);
    }

    #[test]
    fn bim_has_no_aliases() {
        assert!(BIM.aliases.is_empty());
    }

    // ── preset_for ────────────────────────────────────────────────────────────

    #[test]
    fn preset_for_returns_correct_preset() {
        assert_eq!(preset_for(CadOrigin::Classic).bg_color, CLASSIC.bg_color);
        assert_eq!(preset_for(CadOrigin::Nurbs).bg_color, NURBS.bg_color);
        assert_eq!(preset_for(CadOrigin::Bim).bg_color, BIM.bg_color);
        assert_eq!(preset_for(CadOrigin::None).bg_color, ITSJUSTCAD_DEFAULT.bg_color);
    }

    /// Each skin, selected by its new code, applies its own palette and alias
    /// table end to end (code → origin → preset).
    #[test]
    fn new_skin_codes_apply_palette_and_aliases() {
        for (code, want) in [
            ("classic", &CLASSIC),
            ("nurbs", &NURBS),
            ("bim", &BIM),
            ("native", &ITSJUSTCAD_DEFAULT),
        ] {
            let p = preset_for(CadOrigin::from_code(code).expect(code));
            assert_eq!(p.bg_color, want.bg_color, "{code}");
            assert_eq!(p.aliases, want.aliases, "{code}");
        }
        let classic = preset_for(CadOrigin::from_code("classic").unwrap()).aliases;
        assert_eq!(expand_alias("l 0,0 1,1", classic).as_deref(), Some("line 0,0 1,1"));
        assert_eq!(expand_alias("su", classic).as_deref(), Some("subtract"));
        let nurbs = preset_for(CadOrigin::from_code("nurbs").unwrap()).aliases;
        assert!(expand_alias("of", nurbs).is_some_and(|e| e.starts_with("offset")));
    }

    // ── expand_alias ──────────────────────────────────────────────────────────

    #[test]
    fn expand_alias_l_to_line() {
        let result = expand_alias("l", CLASSIC_ALIASES);
        assert_eq!(result, Some("line".to_string()));
    }

    #[test]
    fn expand_alias_l_with_args() {
        let result = expand_alias("l 0,0,0 5,0,0", CLASSIC_ALIASES);
        assert_eq!(result, Some("line 0,0,0 5,0,0".to_string()));
    }

    #[test]
    fn expand_alias_c_to_circle() {
        let result = expand_alias("c", CLASSIC_ALIASES);
        assert_eq!(result, Some("circle".to_string()));
    }

    #[test]
    fn expand_alias_e_to_delete() {
        // Both Classic and NURBS map 'e' to delete
        let result = expand_alias("e", CLASSIC_ALIASES);
        assert_eq!(result, Some("delete".to_string()));
    }

    #[test]
    fn expand_alias_pl_to_polyline() {
        let result = expand_alias("pl", CLASSIC_ALIASES);
        assert_eq!(result, Some("polyline".to_string()));
    }

    #[test]
    fn expand_alias_no_match_returns_none() {
        let result = expand_alias("box", CLASSIC_ALIASES);
        assert_eq!(result, None, "full command names should not alias themselves");
    }

    #[test]
    fn expand_alias_case_insensitive() {
        let result = expand_alias("L", CLASSIC_ALIASES);
        assert_eq!(result, Some("line".to_string()));
        let result = expand_alias("PL", CLASSIC_ALIASES);
        assert_eq!(result, Some("polyline".to_string()));
    }

    #[test]
    fn expand_alias_empty_input() {
        let result = expand_alias("", CLASSIC_ALIASES);
        assert_eq!(result, None);
    }

    #[test]
    fn expand_alias_nurbs_of_to_offset() {
        let result = expand_alias("of", NURBS_ALIASES);
        assert_eq!(result, Some("offset".to_string()));
    }

    // ── alias_suggestions ─────────────────────────────────────────────────────

    #[test]
    fn alias_suggestions_l_prefix_includes_l_la_la_len() {
        let suggestions = alias_suggestions("l", CLASSIC_ALIASES);
        let aliases: Vec<_> = suggestions.iter().map(|(a, _)| *a).collect();
        assert!(aliases.contains(&"l"), "{aliases:?}");
        assert!(aliases.contains(&"la"), "{aliases:?}");
        assert!(aliases.contains(&"len"), "{aliases:?}");
    }

    #[test]
    fn alias_suggestions_empty_prefix_returns_all() {
        let all = alias_suggestions("", CLASSIC_ALIASES);
        assert_eq!(all.len(), CLASSIC_ALIASES.len());
    }

    #[test]
    fn alias_suggestions_no_match_returns_empty() {
        let result = alias_suggestions("zzz", CLASSIC_ALIASES);
        assert!(result.is_empty());
    }

    // ── prefs round-trip (CadOrigin serde) ────────────────────────────────────

    #[test]
    fn cad_origin_serde_round_trip() {
        for origin in [
            CadOrigin::Classic,
            CadOrigin::Nurbs,
            CadOrigin::Bim,
            CadOrigin::None,
        ] {
            let json = serde_json::to_string(&origin).unwrap();
            let back: CadOrigin = serde_json::from_str(&json).unwrap();
            assert_eq!(back, origin, "round-trip failed for {origin:?}: {json}");
        }
    }

    /// Mirror of the exact ui.json read/write the app performs for the skin
    /// (`save_cad_origin` writes `to_value`; `load_cad_origin` reads the string
    /// key and `from_value`s it). Kept pure (no fs) so it runs in the unit suite
    /// while exercising the real serialization path a `skin` verb persists.
    #[test]
    fn cad_origin_ui_json_key_round_trip() {
        for origin in CadOrigin::all() {
            let mut ui = serde_json::Map::new();
            // write (as save_cad_origin does)
            ui.insert(
                "cad_origin".to_string(),
                serde_json::to_value(origin).unwrap(),
            );
            let doc = serde_json::Value::Object(ui);
            // read back (as load_cad_origin does)
            let s = doc["cad_origin"].as_str().expect("string key");
            let back: CadOrigin =
                serde_json::from_value(serde_json::Value::String(s.to_string())).unwrap();
            assert_eq!(back, origin, "ui.json skin round-trip failed for {origin:?}");
        }
    }

    #[test]
    fn cad_origin_json_values() {
        assert_eq!(serde_json::to_string(&CadOrigin::Classic).unwrap(), r#""classic""#);
        assert_eq!(serde_json::to_string(&CadOrigin::Nurbs).unwrap(), r#""nurbs""#);
        assert_eq!(serde_json::to_string(&CadOrigin::Bim).unwrap(), r#""bim""#);
        assert_eq!(serde_json::to_string(&CadOrigin::None).unwrap(), r#""none""#);
    }

    /// Only the current codes deserialize; the pre-rename skin codes are
    /// rejected (no migration path).
    #[test]
    fn cad_origin_rejects_old_ui_json_values() {
        for code in ["classic", "nurbs", "bim", "none"] {
            let back: CadOrigin =
                serde_json::from_value(serde_json::Value::String(code.into())).unwrap();
            assert_eq!(serde_json::to_value(back).unwrap(), serde_json::json!(code));
        }
        for old in ["autocad", "acad", "rhino", "revit", "sketchup", "native"] {
            assert!(
                serde_json::from_value::<CadOrigin>(serde_json::Value::String(old.into())).is_err(),
                "{old} must not deserialize"
            );
        }
    }

    #[test]
    fn cad_origin_default_is_none() {
        assert_eq!(CadOrigin::default(), CadOrigin::None);
    }

    // ── skin verb: code parse/roundtrip ────────────────────────────────────────

    #[test]
    fn cad_origin_code_roundtrip() {
        for origin in CadOrigin::all() {
            assert_eq!(
                CadOrigin::from_code(origin.code()),
                Some(origin),
                "code roundtrip failed for {origin:?}"
            );
        }
        // `all()` is the picker order, Native first.
        assert_eq!(CadOrigin::all()[0], CadOrigin::None);
    }

    #[test]
    fn cad_origin_from_code_synonyms_and_case() {
        assert_eq!(CadOrigin::from_code("NATIVE"), Some(CadOrigin::None));
        assert_eq!(CadOrigin::from_code("default"), Some(CadOrigin::None));
        assert_eq!(CadOrigin::from_code("itsjustcad"), Some(CadOrigin::None));
        assert_eq!(CadOrigin::from_code("none"), Some(CadOrigin::None));
        assert_eq!(CadOrigin::from_code(" Classic "), Some(CadOrigin::Classic));
        // Pre-rename codes are unknown now (no migration).
        for old in ["autocad", " AutoCAD ", "acad", "rhino", "revit", "sketchup"] {
            assert_eq!(CadOrigin::from_code(old), None, "{old}");
        }
        // Unknown → None (caller keeps current skin).
        assert_eq!(CadOrigin::from_code(""), None);
    }

    #[test]
    fn every_skin_has_a_label_key() {
        // Each skin maps to a distinct i18n key (asserted present in the catalog
        // by the i18n completeness test).
        let mut keys: Vec<&str> = CadOrigin::all().iter().map(|o| o.label_key()).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), 4, "skin label keys must be distinct");
    }

    // ── aliases are ADDITIVE (never shadow a canonical verb) ────────────────────

    /// The canonical substrate + app verbs an alias must never silently shadow.
    /// If an alias key equals a canonical command name, typing that name would be
    /// rewritten — breaking the "canonical names always still work" guarantee.
    const CANONICAL_VERBS: &[&str] = &[
        "line", "polyline", "circle", "arc", "rect", "point", "box", "sphere",
        "cylinder", "cone", "move", "copy", "rotate", "scale", "mirror", "array",
        "offset", "trim", "extend", "fillet", "chamfer", "join", "explode",
        "delete", "extrude", "revolve", "loft", "sweep", "layer", "group",
        "block", "hatch", "text", "dimension", "units", "zoom", "pan", "view",
    ];

    #[test]
    fn aliases_never_shadow_a_canonical_verb() {
        for (name, aliases) in [
            ("classic", CLASSIC_ALIASES),
            ("nurbs", NURBS_ALIASES),
            ("bim", BIM.aliases),
        ] {
            for (alias, _canonical) in aliases {
                assert!(
                    !CANONICAL_VERBS.contains(alias),
                    "{name} alias '{alias}' shadows a canonical verb — aliases must be additive"
                );
            }
        }
    }

    #[test]
    fn canonical_verb_falls_through_when_not_an_alias() {
        // A full canonical name that is not itself an alias key must NOT expand
        // (it flows unchanged to the substrate parser).
        assert_eq!(expand_alias("box 0,0,0 1,1,1", CLASSIC_ALIASES), None);
        assert_eq!(expand_alias("extrude", CLASSIC_ALIASES), None);
        // And an unknown token in a skin with no aliases (BIM) never expands.
        assert_eq!(expand_alias("l", BIM.aliases), None);
    }

    // ── layout fields ─────────────────────────────────────────────────────────

    #[test]
    fn nurbs_defaults_command_line_top_and_4up() {
        assert_eq!(NURBS.command_line_pos, CommandLinePos::Top);
        assert_eq!(NURBS.default_viewports, 4);
        assert_eq!(NURBS.menu_style, MenuStyle::Modeler);
    }

    #[test]
    fn classic_keeps_command_line_bottom_and_4up() {
        assert_eq!(CLASSIC.command_line_pos, CommandLinePos::Bottom);
        assert_eq!(CLASSIC.default_viewports, 4);
        assert_eq!(CLASSIC.menu_style, MenuStyle::Drafting);
    }

    #[test]
    fn itsjustcad_default_is_modeler_arrangement() {
        // Task M-uilayout: Modeler-default arrangement even for the ItsJustCAD skin.
        assert_eq!(ITSJUSTCAD_DEFAULT.command_line_pos, CommandLinePos::Top);
        // Boots to the 4-up layout.
        assert_eq!(ITSJUSTCAD_DEFAULT.default_viewports, 4);
        assert_eq!(ITSJUSTCAD_DEFAULT.menu_style, MenuStyle::Modeler);
    }

    // ── token sets ────────────────────────────────────────────────────────────

    #[test]
    fn dark_preset_yields_dark_tokens() {
        let t = CLASSIC.tokens();
        assert!(t.dark, "Classic tokens should be dark");
        assert!(!crate::theme::is_dark(t.colors.on_surface), "text must be light on dark");
    }

    #[test]
    fn light_preset_yields_light_tokens() {
        let t = NURBS.tokens();
        assert!(!t.dark, "NURBS tokens should be light");
        assert!(crate::theme::is_dark(t.colors.on_surface), "text must be dark on light");
    }

    #[test]
    fn tokens_type_scale_follows_font_px() {
        let t = ITSJUSTCAD_DEFAULT.tokens();
        assert_eq!(t.type_scale.command, ITSJUSTCAD_DEFAULT.cmd_font_px);
        assert_eq!(t.type_scale.body, ITSJUSTCAD_DEFAULT.ui_font_px);
        assert!(t.type_scale.small < t.type_scale.body);
    }
}
