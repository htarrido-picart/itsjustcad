//! Legacy-CAD skin presets — AutoCAD, Rhino, Revit, or mydrafter default.
//!
//! Each preset encodes: viewport background, grid colors, ui/cmd font size,
//! accent color, command-line flavor, right-click repeat-last flag, and
//! a command alias map.
//!
//! All values sourced from `docs/ui-legacy-research.md`.
//! Pure module; no egui or wgpu imports — tested standalone.

/// Which CAD origin the user is migrating from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CadOrigin {
    AutoCAD,
    Rhino,
    Revit,
    /// mydrafter default (no legacy skin).
    #[default]
    None,
}

impl CadOrigin {
    pub fn label(self) -> &'static str {
        match self {
            CadOrigin::AutoCAD => "AutoCAD",
            CadOrigin::Rhino => "Rhino",
            CadOrigin::Revit => "Revit",
            CadOrigin::None => "mydrafter default",
        }
    }
}

/// sRGB color as `[r, g, b, a]` in 0.0–1.0.
pub type Rgba = [f32; 4];

/// Where the command line docks. Rhino puts it at the top (below the menu
/// bar), AutoCAD at the bottom. Drives layout order in `app.rs` and the
/// autosuggest popup direction (down from a top line, up from a bottom line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandLinePos {
    Top,
    Bottom,
}

/// Which side the docked tab panel (Layers/Properties/History/Deck) sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelSide {
    // Left is wired by Layer 2's panel-side logic; kept for AutoCAD-style skins.
    #[allow(dead_code)]
    Left,
    Right,
}

/// Menu-bar grouping flavor. Rhino and AutoCAD group the same registry verbs
/// under different top-level menu names (see `crate::menu`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuStyle {
    Rhino,
    AutoCAD,
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
    /// Right-click in viewport repeats last command (Rhino convention).
    pub right_click_repeat_last: bool,
    /// Ordered alias table: (alias, canonical_command).
    /// Aliases are looked up BEFORE the parser, case-insensitively.
    pub aliases: &'static [(&'static str, &'static str)],
    /// Where the command line docks (top for Rhino, bottom for AutoCAD).
    pub command_line_pos: CommandLinePos,
    /// Which side the docked tab panel sits on.
    pub panel_side: PanelSide,
    /// Default viewport count on first show (Rhino → 4-up, others single).
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
                small: (self.ui_font_px - 2.0).max(8.0),
                panel_title: self.ui_font_px + 1.0,
            },
            colors,
            radii: crate::theme::Radii::default(),
            dark: crate::theme::is_dark(self.bg_surface()),
        }
    }

    /// Human label for this preset's menu style (About dialog).
    pub fn menu_style_label(&self) -> &'static str {
        match self.menu_style {
            MenuStyle::Rhino => "Rhino-style menus",
            MenuStyle::AutoCAD => "AutoCAD-style menus",
        }
    }

    /// UI-chrome surface color. This is distinct from the viewport clear color
    /// (`bg_color`): a Rhino viewport is light gray, but its panel chrome reads
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

// ── AutoCAD preset ────────────────────────────────────────────────────────────
//   bg      #212830  (RGB 33,40,48) — dark blue-gray model space
//   grid major slightly lighter; grid minor barely visible
//   accent  #00A1F1 (AutoCAD blue)

pub const AUTOCAD_ALIASES: &[(&str, &str)] = &[
    // Core ACAD.PGP set — muscle memory for the largest CAD user base
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

pub const AUTOCAD: UiPreset = UiPreset {
    bg_color: [0.129, 0.157, 0.188, 1.0],  // #212830
    grid_major_color: [0.227, 0.271, 0.314, 1.0], // #3A4550
    grid_minor_color: [0.165, 0.188, 0.220, 1.0], // #2A3038
    crosshair_color: [1.0, 1.0, 1.0, 1.0],         // #FFFFFF
    accent_color: [0.0, 0.631, 0.945, 1.0],         // #00A1F1
    ui_font_px: 13.0,
    cmd_font_px: 13.0,
    right_click_repeat_last: false,
    aliases: AUTOCAD_ALIASES,
    command_line_pos: CommandLinePos::Bottom,
    panel_side: PanelSide::Right,
    default_viewports: 1,
    menu_style: MenuStyle::AutoCAD,
};

// ── Rhino preset ──────────────────────────────────────────────────────────────
//   bg      #D4D4D4  (RGB 212,212,212) — light gray wireframe
//   right-click = repeat last (critical Rhino convention)

const RHINO_ALIASES: &[(&str, &str)] = &[
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

pub const RHINO: UiPreset = UiPreset {
    bg_color: [0.831, 0.831, 0.831, 1.0],           // #D4D4D4
    grid_major_color: [0.627, 0.627, 0.627, 1.0],   // #A0A0A0
    grid_minor_color: [0.784, 0.784, 0.784, 1.0],   // #C8C8C8
    crosshair_color: [0.251, 0.251, 0.251, 1.0],    // #404040
    accent_color: [0.910, 0.910, 0.910, 1.0],        // #E8E8E8 panel chrome
    ui_font_px: 13.0,
    cmd_font_px: 13.0,
    right_click_repeat_last: true,
    aliases: RHINO_ALIASES,
    command_line_pos: CommandLinePos::Top,
    panel_side: PanelSide::Right,
    default_viewports: 4,
    menu_style: MenuStyle::Rhino,
};

// ── Revit preset ──────────────────────────────────────────────────────────────
//   bg      #FFFFFF — pure white 2D canvas
//   No command aliases (Revit has no command line)

pub const REVIT: UiPreset = UiPreset {
    bg_color: [1.0, 1.0, 1.0, 1.0],                 // #FFFFFF
    grid_major_color: [0.8, 0.8, 0.8, 1.0],          // light gray (no explicit grid)
    grid_minor_color: [0.9, 0.9, 0.9, 1.0],
    crosshair_color: [0.0, 0.0, 0.0, 1.0],            // #000000
    accent_color: [0.0, 0.439, 0.753, 1.0],           // #0070C0
    ui_font_px: 13.0,
    cmd_font_px: 13.0,
    right_click_repeat_last: false,
    aliases: &[],
    // Revit has no command line; we still dock one (mydrafter is command-first),
    // but keep it at the bottom and use AutoCAD-style ribbon-ish menus.
    command_line_pos: CommandLinePos::Bottom,
    panel_side: PanelSide::Right,
    default_viewports: 1,
    menu_style: MenuStyle::AutoCAD,
};

// ── mydrafter default ─────────────────────────────────────────────────────────

pub const MYDRAFTER_DEFAULT: UiPreset = UiPreset {
    bg_color: [0.13, 0.14, 0.16, 1.0],
    grid_major_color: [0.22, 0.24, 0.27, 1.0],
    grid_minor_color: [0.18, 0.19, 0.21, 1.0],
    crosshair_color: [0.9, 0.92, 0.95, 1.0],
    accent_color: [0.35, 0.65, 1.0, 1.0],
    ui_font_px: 13.0,
    cmd_font_px: 13.0,
    right_click_repeat_last: false,
    aliases: &[],
    // mydrafter default adopts the Rhino-style arrangement (task M-uilayout):
    // command line on top, right tab panel, 4-up viewports, Rhino menu grouping.
    command_line_pos: CommandLinePos::Top,
    panel_side: PanelSide::Right,
    default_viewports: 4,
    menu_style: MenuStyle::Rhino,
};

/// Return the static preset for a given origin.
pub fn preset_for(origin: CadOrigin) -> &'static UiPreset {
    match origin {
        CadOrigin::AutoCAD => &AUTOCAD,
        CadOrigin::Rhino => &RHINO,
        CadOrigin::Revit => &REVIT,
        CadOrigin::None => &MYDRAFTER_DEFAULT,
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
    fn autocad_is_dark() {
        assert!(is_dark(&AUTOCAD), "AutoCAD should be dark");
    }

    #[test]
    fn rhino_is_light() {
        assert!(!is_dark(&RHINO), "Rhino should be light");
    }

    #[test]
    fn revit_is_light() {
        assert!(!is_dark(&REVIT), "Revit should be light");
    }

    #[test]
    fn autocad_bg_is_near_black() {
        let [r, g, b, _] = AUTOCAD.bg_color;
        // All channels below 0.2 (hex #212830 ≈ 0.129, 0.157, 0.188)
        assert!(r < 0.2 && g < 0.2 && b < 0.2, "AutoCAD bg should be very dark: {r},{g},{b}");
    }

    #[test]
    fn revit_bg_is_white() {
        let [r, g, b, _] = REVIT.bg_color;
        assert!((r - 1.0).abs() < 0.01 && (g - 1.0).abs() < 0.01 && (b - 1.0).abs() < 0.01);
    }

    #[test]
    fn rhino_bg_is_light_gray() {
        let [r, g, b, _] = RHINO.bg_color;
        // #D4D4D4 ≈ 0.831 for all channels
        let mid = (r + g + b) / 3.0;
        assert!(mid > 0.7 && mid < 0.95, "Rhino bg should be light gray: {mid}");
    }

    #[test]
    fn rhino_right_click_repeat_last() {
        assert!(RHINO.right_click_repeat_last);
        assert!(!AUTOCAD.right_click_repeat_last);
        assert!(!REVIT.right_click_repeat_last);
    }

    #[test]
    fn revit_has_no_aliases() {
        assert!(REVIT.aliases.is_empty());
    }

    // ── preset_for ────────────────────────────────────────────────────────────

    #[test]
    fn preset_for_returns_correct_preset() {
        assert_eq!(preset_for(CadOrigin::AutoCAD).bg_color, AUTOCAD.bg_color);
        assert_eq!(preset_for(CadOrigin::Rhino).bg_color, RHINO.bg_color);
        assert_eq!(preset_for(CadOrigin::Revit).bg_color, REVIT.bg_color);
        assert_eq!(preset_for(CadOrigin::None).bg_color, MYDRAFTER_DEFAULT.bg_color);
    }

    // ── expand_alias ──────────────────────────────────────────────────────────

    #[test]
    fn expand_alias_l_to_line() {
        let result = expand_alias("l", AUTOCAD_ALIASES);
        assert_eq!(result, Some("line".to_string()));
    }

    #[test]
    fn expand_alias_l_with_args() {
        let result = expand_alias("l 0,0,0 5,0,0", AUTOCAD_ALIASES);
        assert_eq!(result, Some("line 0,0,0 5,0,0".to_string()));
    }

    #[test]
    fn expand_alias_c_to_circle() {
        let result = expand_alias("c", AUTOCAD_ALIASES);
        assert_eq!(result, Some("circle".to_string()));
    }

    #[test]
    fn expand_alias_e_to_delete() {
        // Both AutoCAD and Rhino map 'e' to delete
        let result = expand_alias("e", AUTOCAD_ALIASES);
        assert_eq!(result, Some("delete".to_string()));
    }

    #[test]
    fn expand_alias_pl_to_polyline() {
        let result = expand_alias("pl", AUTOCAD_ALIASES);
        assert_eq!(result, Some("polyline".to_string()));
    }

    #[test]
    fn expand_alias_no_match_returns_none() {
        let result = expand_alias("box", AUTOCAD_ALIASES);
        assert_eq!(result, None, "full command names should not alias themselves");
    }

    #[test]
    fn expand_alias_case_insensitive() {
        let result = expand_alias("L", AUTOCAD_ALIASES);
        assert_eq!(result, Some("line".to_string()));
        let result = expand_alias("PL", AUTOCAD_ALIASES);
        assert_eq!(result, Some("polyline".to_string()));
    }

    #[test]
    fn expand_alias_empty_input() {
        let result = expand_alias("", AUTOCAD_ALIASES);
        assert_eq!(result, None);
    }

    #[test]
    fn expand_alias_rhino_of_to_offset() {
        let result = expand_alias("of", RHINO_ALIASES);
        assert_eq!(result, Some("offset".to_string()));
    }

    // ── alias_suggestions ─────────────────────────────────────────────────────

    #[test]
    fn alias_suggestions_l_prefix_includes_l_la_la_len() {
        let suggestions = alias_suggestions("l", AUTOCAD_ALIASES);
        let aliases: Vec<_> = suggestions.iter().map(|(a, _)| *a).collect();
        assert!(aliases.contains(&"l"), "{aliases:?}");
        assert!(aliases.contains(&"la"), "{aliases:?}");
        assert!(aliases.contains(&"len"), "{aliases:?}");
    }

    #[test]
    fn alias_suggestions_empty_prefix_returns_all() {
        let all = alias_suggestions("", AUTOCAD_ALIASES);
        assert_eq!(all.len(), AUTOCAD_ALIASES.len());
    }

    #[test]
    fn alias_suggestions_no_match_returns_empty() {
        let result = alias_suggestions("zzz", AUTOCAD_ALIASES);
        assert!(result.is_empty());
    }

    // ── prefs round-trip (CadOrigin serde) ────────────────────────────────────

    #[test]
    fn cad_origin_serde_round_trip() {
        for origin in [
            CadOrigin::AutoCAD,
            CadOrigin::Rhino,
            CadOrigin::Revit,
            CadOrigin::None,
        ] {
            let json = serde_json::to_string(&origin).unwrap();
            let back: CadOrigin = serde_json::from_str(&json).unwrap();
            assert_eq!(back, origin, "round-trip failed for {origin:?}: {json}");
        }
    }

    #[test]
    fn cad_origin_json_values() {
        assert_eq!(serde_json::to_string(&CadOrigin::AutoCAD).unwrap(), r#""autocad""#);
        assert_eq!(serde_json::to_string(&CadOrigin::Rhino).unwrap(), r#""rhino""#);
        assert_eq!(serde_json::to_string(&CadOrigin::Revit).unwrap(), r#""revit""#);
        assert_eq!(serde_json::to_string(&CadOrigin::None).unwrap(), r#""none""#);
    }

    #[test]
    fn cad_origin_default_is_none() {
        assert_eq!(CadOrigin::default(), CadOrigin::None);
    }

    // ── layout fields ─────────────────────────────────────────────────────────

    #[test]
    fn rhino_defaults_command_line_top_and_4up() {
        assert_eq!(RHINO.command_line_pos, CommandLinePos::Top);
        assert_eq!(RHINO.default_viewports, 4);
        assert_eq!(RHINO.menu_style, MenuStyle::Rhino);
    }

    #[test]
    fn autocad_keeps_command_line_bottom_single_viewport() {
        assert_eq!(AUTOCAD.command_line_pos, CommandLinePos::Bottom);
        assert_eq!(AUTOCAD.default_viewports, 1);
        assert_eq!(AUTOCAD.menu_style, MenuStyle::AutoCAD);
    }

    #[test]
    fn mydrafter_default_is_rhino_arrangement() {
        // Task M-uilayout: Rhino-default arrangement even for the mydrafter skin.
        assert_eq!(MYDRAFTER_DEFAULT.command_line_pos, CommandLinePos::Top);
        assert_eq!(MYDRAFTER_DEFAULT.default_viewports, 4);
        assert_eq!(MYDRAFTER_DEFAULT.menu_style, MenuStyle::Rhino);
    }

    // ── token sets ────────────────────────────────────────────────────────────

    #[test]
    fn dark_preset_yields_dark_tokens() {
        let t = AUTOCAD.tokens();
        assert!(t.dark, "AutoCAD tokens should be dark");
        assert!(!crate::theme::is_dark(t.colors.on_surface), "text must be light on dark");
    }

    #[test]
    fn light_preset_yields_light_tokens() {
        let t = RHINO.tokens();
        assert!(!t.dark, "Rhino tokens should be light");
        assert!(crate::theme::is_dark(t.colors.on_surface), "text must be dark on light");
    }

    #[test]
    fn tokens_type_scale_follows_font_px() {
        let t = MYDRAFTER_DEFAULT.tokens();
        assert_eq!(t.type_scale.command, MYDRAFTER_DEFAULT.cmd_font_px);
        assert_eq!(t.type_scale.body, MYDRAFTER_DEFAULT.ui_font_px);
        assert!(t.type_scale.small < t.type_scale.body);
    }
}
