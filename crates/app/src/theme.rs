//! Design-token system for the mydrafter UI.
//!
//! A small Material/SwiftUI-informed token layer that replaces scattered
//! `Color32::from_rgb` calls and ad-hoc font sizes with named, semantic values.
//! One [`apply`] call maps a [`Tokens`] set onto an egui [`Style`] (visuals +
//! text styles + spacing), so every widget inherits the same spacing scale,
//! type scale, and semantic color roles.
//!
//! The three legacy skins (Rhino / AutoCAD / Revit) are expressed as token
//! sets (see [`crate::preset`]) that feed this module — nothing paints raw
//! egui overrides any more.
//!
//! Pure module: the token *types* and the pure mapping helpers carry no egui
//! state beyond the `Style` they are handed, so the numeric relationships
//! (spacing scale, type scale, luminance) are unit-testable standalone.

/// 8px-based spacing scale (Material's 8dp grid, with a 4px half-step).
/// All inter-widget gaps and paddings derive from these four values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spacing {
    /// 4px — tight inner padding, icon gaps.
    pub xs: f32,
    /// 8px — default item spacing.
    pub s: f32,
    /// 16px — section padding, panel inner margin.
    pub m: f32,
    /// 24px — group separation.
    pub l: f32,
    /// 32px — major regions.
    pub xl: f32,
}

impl Default for Spacing {
    fn default() -> Self {
        Self { xs: 4.0, s: 8.0, m: 16.0, l: 24.0, xl: 32.0 }
    }
}

/// Type scale in logical pixels (before the egui zoom factor), mapped onto
/// egui [`TextStyle`]s. Sizes follow `docs/ui-legacy-research.md`:
/// command line ~10–11pt, panels ~9pt, status ~8–9pt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TypeScale {
    /// Command-line / monospace prompt (~10–11pt → 13px).
    pub command: f32,
    /// Body / panel text (~9–10pt → 13px).
    pub body: f32,
    /// Small: autosuggest, hints, status bar (~8pt → 11px).
    pub small: f32,
    /// Panel titles / headings (~11pt → 14px).
    pub panel_title: f32,
}

impl Default for TypeScale {
    fn default() -> Self {
        Self { command: 13.0, body: 13.0, small: 11.0, panel_title: 14.0 }
    }
}

/// sRGB color as `[r, g, b, a]` in 0.0–1.0 (matches [`crate::preset::Rgba`]).
pub type Rgba = [f32; 4];

/// Semantic color roles (Material 3 / SwiftUI naming). Widgets reference a
/// role, never a literal color, so a skin swap re-tints the whole UI.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorRoles {
    /// Base panel / window background.
    pub surface: Rgba,
    /// Slightly raised or inset surface (input fields, popups, tab strip).
    pub surface_variant: Rgba,
    /// Primary text / icon color on `surface`.
    pub on_surface: Rgba,
    /// Dimmed text (hints, disabled labels).
    pub on_surface_variant: Rgba,
    /// Primary / accent (selection highlight, active tab).
    pub primary: Rgba,
    /// Divider / border lines.
    pub outline: Rgba,
    /// Elevation shadow color (usually near-black, low alpha).
    pub shadow: Rgba,
}

/// Corner radii in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Radii {
    /// Small controls (buttons, inputs).
    pub small: f32,
    /// Panels, popups.
    pub medium: f32,
}

impl Default for Radii {
    fn default() -> Self {
        Self { small: 3.0, medium: 5.0 }
    }
}

/// A complete design-token set. One of these per skin; [`apply`] stamps it
/// onto an egui [`Style`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tokens {
    pub spacing: Spacing,
    pub type_scale: TypeScale,
    pub colors: ColorRoles,
    pub radii: Radii,
    /// True when `surface` is dark — drives egui's dark/light base visuals.
    pub dark: bool,
}

/// Perceptual luminance (BT.709) of an sRGB color.
pub fn luminance(c: Rgba) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// True when a color reads as dark (luminance < 0.5).
pub fn is_dark(c: Rgba) -> bool {
    luminance(c) < 0.5
}

/// Convert an `Rgba` (0–1) to an egui `Color32`.
pub fn to_color32(c: Rgba) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(
        (c[0] * 255.0).round() as u8,
        (c[1] * 255.0).round() as u8,
        (c[2] * 255.0).round() as u8,
        (c[3] * 255.0).round() as u8,
    )
}

/// Derive a full set of color roles from just a surface color and an accent.
/// Used by skins that only pin a background + accent (grid/crosshair aside):
/// the on-surface, variant, and outline roles are computed by luminance offset
/// so light and dark skins stay internally consistent.
pub fn roles_from(surface: Rgba, accent: Rgba) -> ColorRoles {
    let dark = is_dark(surface);
    // Offset a channel toward white (dark bg) or black (light bg).
    let shift = |c: Rgba, amt: f32| -> Rgba {
        let d = if dark { amt } else { -amt };
        [
            (c[0] + d).clamp(0.0, 1.0),
            (c[1] + d).clamp(0.0, 1.0),
            (c[2] + d).clamp(0.0, 1.0),
            c[3],
        ]
    };
    let on = if dark { [0.90, 0.92, 0.95, 1.0] } else { [0.10, 0.11, 0.12, 1.0] };
    let on_variant = if dark { [0.60, 0.62, 0.66, 1.0] } else { [0.40, 0.41, 0.44, 1.0] };
    ColorRoles {
        surface,
        surface_variant: shift(surface, 0.05),
        on_surface: on,
        on_surface_variant: on_variant,
        primary: accent,
        outline: shift(surface, 0.14),
        shadow: [0.0, 0.0, 0.0, if dark { 0.55 } else { 0.25 }],
    }
}

/// Stamp a token set onto an egui [`Style`]: base dark/light visuals, semantic
/// color roles, the type scale (text styles), spacing, and corner radii.
/// This is the single place that translates tokens → egui.
pub fn apply(ctx: &egui::Context, tokens: &Tokens) {
    ctx.set_theme(if tokens.dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    });

    let t = *tokens;
    let set = move |style: &mut egui::Style| apply_to_style(style, &t);
    if tokens.dark {
        ctx.style_mut_of(egui::Theme::Dark, set);
    } else {
        ctx.style_mut_of(egui::Theme::Light, set);
    }
}

/// Pure(-ish) core of [`apply`]: mutate a `Style` in place from tokens.
/// Separated so tests can assert the mapping without a live `Context`.
pub fn apply_to_style(style: &mut egui::Style, t: &Tokens) {
    // ── Type scale → egui text styles ──────────────────────────────────
    let ts = t.type_scale;
    style.text_styles.insert(egui::TextStyle::Monospace, egui::FontId::monospace(ts.command));
    style.text_styles.insert(egui::TextStyle::Body, egui::FontId::proportional(ts.body));
    style.text_styles.insert(egui::TextStyle::Button, egui::FontId::proportional(ts.body));
    style.text_styles.insert(egui::TextStyle::Small, egui::FontId::proportional(ts.small));
    style.text_styles.insert(egui::TextStyle::Heading, egui::FontId::proportional(ts.panel_title));

    // ── Spacing scale → egui spacing ───────────────────────────────────
    let sp = t.spacing;
    style.spacing.item_spacing = egui::vec2(sp.s, t.spacing.xs);
    style.spacing.button_padding = egui::vec2(sp.s, sp.xs);
    style.spacing.menu_margin = egui::Margin::same(sp.xs as i8);
    style.spacing.indent = sp.m;

    // ── Color roles → egui visuals ─────────────────────────────────────
    let c = t.colors;
    let v = &mut style.visuals;
    let primary = to_color32(c.primary);
    let surface = to_color32(c.surface);
    let surface_variant = to_color32(c.surface_variant);
    let on_surface = to_color32(c.on_surface);
    let on_variant = to_color32(c.on_surface_variant);
    let outline = to_color32(c.outline);

    v.selection.bg_fill = primary;
    v.selection.stroke = egui::Stroke::new(1.5, primary);
    v.hyperlink_color = primary;
    v.panel_fill = surface;
    v.window_fill = surface;
    v.extreme_bg_color = surface_variant; // text edit background
    v.faint_bg_color = surface_variant;
    v.override_text_color = Some(on_surface);
    v.window_stroke = egui::Stroke::new(1.0, outline);

    // Widget state colors: idle → hover → active use surface_variant → primary.
    v.widgets.noninteractive.bg_fill = surface;
    v.widgets.noninteractive.weak_bg_fill = surface;
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, on_variant);
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, outline);

    v.widgets.inactive.bg_fill = surface_variant;
    v.widgets.inactive.weak_bg_fill = surface_variant;
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, on_surface);

    v.widgets.hovered.bg_fill = surface_variant;
    v.widgets.hovered.weak_bg_fill = surface_variant;
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.5, on_surface);
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, primary);

    v.widgets.active.bg_fill = primary;
    v.widgets.active.weak_bg_fill = primary;
    v.widgets.active.fg_stroke = egui::Stroke::new(1.5, on_surface);

    // ── Corner radii ───────────────────────────────────────────────────
    let small = egui::CornerRadius::same(t.radii.small as u8);
    let medium = egui::CornerRadius::same(t.radii.medium as u8);
    v.widgets.noninteractive.corner_radius = small;
    v.widgets.inactive.corner_radius = small;
    v.widgets.hovered.corner_radius = small;
    v.widgets.active.corner_radius = small;
    v.window_corner_radius = medium;
    v.menu_corner_radius = medium;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spacing_scale_is_8px_based() {
        let s = Spacing::default();
        assert_eq!(s.xs, 4.0);
        assert_eq!(s.s, 8.0);
        assert_eq!(s.m, 16.0);
        assert_eq!(s.l, 24.0);
        assert_eq!(s.xl, 32.0);
        // Each step (past xs) is a multiple of 8.
        for v in [s.s, s.m, s.xl] {
            assert_eq!(v % 8.0, 0.0, "{v} not on 8px grid");
        }
    }

    #[test]
    fn type_scale_orders_small_to_title() {
        let t = TypeScale::default();
        assert!(t.small < t.body);
        assert!(t.body <= t.command);
        assert!(t.command <= t.panel_title);
    }

    #[test]
    fn luminance_dark_and_light() {
        assert!(is_dark([0.0, 0.0, 0.0, 1.0]));
        assert!(is_dark([0.13, 0.14, 0.16, 1.0]));
        assert!(!is_dark([1.0, 1.0, 1.0, 1.0]));
        assert!(!is_dark([0.831, 0.831, 0.831, 1.0])); // Rhino gray
    }

    #[test]
    fn roles_from_dark_surface_has_light_text() {
        let r = roles_from([0.13, 0.14, 0.16, 1.0], [0.35, 0.65, 1.0, 1.0]);
        assert!(!is_dark(r.on_surface), "text on dark surface must be light");
        assert!(is_dark(r.surface));
        // surface_variant is lighter than surface on a dark skin.
        assert!(luminance(r.surface_variant) > luminance(r.surface));
    }

    #[test]
    fn roles_from_light_surface_has_dark_text() {
        let r = roles_from([1.0, 1.0, 1.0, 1.0], [0.0, 0.44, 0.75, 1.0]);
        assert!(is_dark(r.on_surface), "text on light surface must be dark");
        // surface_variant is darker than surface on a light skin.
        assert!(luminance(r.surface_variant) < luminance(r.surface));
    }

    #[test]
    fn roles_from_preserves_surface_and_accent() {
        let surface = [0.129, 0.157, 0.188, 1.0];
        let accent = [0.0, 0.631, 0.945, 1.0];
        let r = roles_from(surface, accent);
        assert_eq!(r.surface, surface);
        assert_eq!(r.primary, accent);
    }

    #[test]
    fn apply_to_style_sets_text_styles_from_type_scale() {
        let mut style = egui::Style::default();
        let tokens = Tokens {
            spacing: Spacing::default(),
            type_scale: TypeScale { command: 15.0, body: 14.0, small: 10.0, panel_title: 18.0 },
            colors: roles_from([0.13, 0.14, 0.16, 1.0], [0.35, 0.65, 1.0, 1.0]),
            radii: Radii::default(),
            dark: true,
        };
        apply_to_style(&mut style, &tokens);
        assert_eq!(style.text_styles[&egui::TextStyle::Monospace].size, 15.0);
        assert_eq!(style.text_styles[&egui::TextStyle::Body].size, 14.0);
        assert_eq!(style.text_styles[&egui::TextStyle::Small].size, 10.0);
        assert_eq!(style.text_styles[&egui::TextStyle::Heading].size, 18.0);
    }

    #[test]
    fn apply_to_style_sets_selection_to_primary() {
        let mut style = egui::Style::default();
        let colors = roles_from([0.13, 0.14, 0.16, 1.0], [0.35, 0.65, 1.0, 1.0]);
        let tokens = Tokens {
            spacing: Spacing::default(),
            type_scale: TypeScale::default(),
            colors,
            radii: Radii::default(),
            dark: true,
        };
        apply_to_style(&mut style, &tokens);
        assert_eq!(style.visuals.selection.bg_fill, to_color32(colors.primary));
        assert_eq!(style.visuals.panel_fill, to_color32(colors.surface));
    }

    #[test]
    fn apply_to_style_spacing_on_8px_grid() {
        let mut style = egui::Style::default();
        let tokens = Tokens {
            spacing: Spacing::default(),
            type_scale: TypeScale::default(),
            colors: roles_from([1.0, 1.0, 1.0, 1.0], [0.0, 0.44, 0.75, 1.0]),
            radii: Radii::default(),
            dark: false,
        };
        apply_to_style(&mut style, &tokens);
        assert_eq!(style.spacing.item_spacing.x, 8.0);
        assert_eq!(style.spacing.indent, 16.0);
    }
}
