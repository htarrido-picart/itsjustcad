// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Design-token system for the ItsJustCAD UI.
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

/// 8pt-based spacing scale (SwiftUI/Material 8-pt grid, with a 4pt half-step and
/// a 12pt three-quarter step). All inter-widget gaps and paddings derive from
/// this one scale — call sites reference a token, never a raw pixel literal.
///
/// The exposed scale is `4 / 8 / 12 / 16 / 24 / 32`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spacing {
    /// 4px — tight inner padding, icon↔label gaps.
    pub xs: f32,
    /// 8px — default item spacing (grid unit).
    pub s: f32,
    /// 12px — cozy control padding, compact section gaps.
    pub sm: f32,
    /// 16px — section padding, panel inner margin.
    pub m: f32,
    /// 24px — group separation.
    pub l: f32,
    /// 32px — major regions.
    pub xl: f32,
}

impl Spacing {
    /// 4px — tight inner padding, icon↔label gaps.
    pub const XS: f32 = 4.0;
    /// 8px — default item spacing (grid unit).
    pub const S: f32 = 8.0;
    /// 12px — cozy control padding, compact section gaps.
    pub const SM: f32 = 12.0;
    /// 16px — section padding, panel inner margin.
    pub const M: f32 = 16.0;
    /// 24px — group separation.
    pub const L: f32 = 24.0;
    /// 32px — major regions.
    pub const XL: f32 = 32.0;

    /// 80px (10× grid unit) — the docked command-history scrollback height,
    /// used as the default / minimum when no panel height is available.
    /// Prefer [`Spacing::history_h_for`] for a token-relative height.
    pub const HISTORY_H: f32 = Self::S * 10.0;
    /// 64px (8× grid unit) — the deck chat input row reserve.
    pub const CHAT_INPUT_H: f32 = Self::S * 8.0;

    /// Token-relative history height: 30% of `panel_h`, clamped to
    /// `[HISTORY_H, 240px]`. Grows naturally when the command-line panel is
    /// taller (bottom panel + window resize) while keeping a sensible floor so
    /// a tiny window never collapses it to nothing.
    pub fn history_h_for(panel_h: f32) -> f32 {
        (panel_h * 0.30).clamp(Self::HISTORY_H, 240.0)
    }

    /// 28px — the min interactive height for PRIMARY chrome controls
    /// (toolbar buttons, dialog buttons, combos). HIG/SwiftUI comfortable
    /// target sits ~28–32; dense list rows may still drop to `L` (24).
    pub const HIT_TARGET: f32 = 28.0;
}

impl Default for Spacing {
    fn default() -> Self {
        Self {
            xs: Self::XS,
            s: Self::S,
            sm: Self::SM,
            m: Self::M,
            l: Self::L,
            xl: Self::XL,
        }
    }
}

/// Font weight for a type token. egui's default proportional font ships a
/// single weight, so this is carried as *data* on the token: later batches map
/// it to a real weighted family (or `RichText::strong()` as a stand-in). We
/// deliberately expose no Light/Thin — HIG small UI text never goes sub-Regular.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weight {
    /// Body copy, captions.
    Regular,
    /// Slightly emphasized labels.
    #[allow(dead_code)] // token-layer value consumed by later design batches
    Medium,
    /// Titles, panel headers, buttons that need presence.
    Semibold,
}

/// Type scale in logical pixels (before the egui zoom factor), mapped onto
/// egui [`TextStyle`]s. Sizes follow `docs/ui-legacy-research.md`:
/// command line ~10–11pt, panels ~9pt, status ~8–9pt.
///
/// Each role pairs a size with a [`Weight`]. Sizes are the `*_px` fields (kept
/// as bare `f32` so existing call sites read them directly); the matching
/// `*_weight` field carries the intended weight for the token layer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TypeScale {
    /// Command-line / monospace prompt (~10–11pt → 13px).
    pub command: f32,
    /// Body / panel text (~9–10pt → 13px).
    pub body: f32,
    /// Small: autosuggest, hints, status bar (~8pt → 12px, WCAG-lifted).
    pub small: f32,
    /// Panel titles / headings (15px, semibold).
    pub panel_title: f32,
    /// Section / dialog title (20px, semibold).
    pub title: f32,
    /// Caption / metadata (11px, regular).
    pub caption: f32,

    /// Weight for the command prompt (regular; it is monospace).
    pub command_weight: Weight,
    /// Weight for body text.
    pub body_weight: Weight,
    /// Weight for small text.
    pub small_weight: Weight,
    /// Weight for panel titles (semibold).
    pub panel_title_weight: Weight,
    /// Weight for section titles (semibold).
    pub title_weight: Weight,
    /// Weight for captions (regular).
    pub caption_weight: Weight,
}

impl Default for TypeScale {
    fn default() -> Self {
        Self {
            command: 13.0,
            body: 13.0,
            // WCAG: small UI text lifted 11 → 12 so it clears the contrast floor
            // at its size class.
            small: 12.0,
            panel_title: 15.0,
            title: 20.0,
            caption: 11.0,
            command_weight: Weight::Regular,
            body_weight: Weight::Regular,
            small_weight: Weight::Regular,
            panel_title_weight: Weight::Semibold,
            title_weight: Weight::Semibold,
            caption_weight: Weight::Regular,
        }
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
    /// Elevated surface for overlays that float ABOVE panels — dialogs,
    /// popovers, autosuggest, the download chip. In dark mode this reads
    /// *lighter* than `surface` (elevation-by-color, not shadow/blur); in light
    /// mode it stays near-white so overlays lift off the panel.
    pub surface_elevated: Rgba,
    /// Primary text / icon color on `surface`.
    pub on_surface: Rgba,
    /// Dimmed text (hints, disabled labels).
    pub on_surface_variant: Rgba,
    /// Third label tier (timestamps, faint metadata) — dimmer than
    /// `on_surface_variant` but still legible on `surface`.
    pub on_surface_tertiary: Rgba,
    /// Primary / accent (selection highlight, active tab).
    pub primary: Rgba,
    /// Destructive / error (system-red): cancel, delete, download failure.
    pub destructive: Rgba,
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
        Self {
            small: 3.0,
            medium: 5.0,
        }
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

/// WCAG 2.1 relative luminance of an sRGB color (0–1). Unlike [`luminance`]
/// (fast BT.709 perceptual weight for the light/dark decision), this applies
/// the sRGB gamma-linearization the WCAG contrast formula requires.
// Building block for the contrast tests and later a11y-audit call sites.
#[allow(dead_code)]
pub fn wcag_relative_luminance(c: Rgba) -> f32 {
    let lin = |ch: f32| -> f32 {
        if ch <= 0.03928 {
            ch / 12.92
        } else {
            ((ch + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
}

/// WCAG 2.1 contrast ratio between two colors, in `1.0..=21.0`. Ignores alpha
/// (assumes opaque compositing). `4.5:1` is the AA floor for normal text,
/// `3:1` for large/bold text and UI components.
// Consumed by the WCAG contrast tests; a public a11y building block.
#[allow(dead_code)]
pub fn contrast_ratio(a: Rgba, b: Rgba) -> f32 {
    let la = wcag_relative_luminance(a);
    let lb = wcag_relative_luminance(b);
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
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
    let on = if dark {
        [0.90, 0.92, 0.95, 1.0]
    } else {
        [0.10, 0.11, 0.12, 1.0]
    };
    // on_surface_variant is LIFTED (dark: 0.60→0.72, light: 0.40→0.36) so it
    // clears WCAG 4.5:1 against `surface` on every skin — see contrast tests.
    let on_variant = if dark {
        [0.72, 0.74, 0.77, 1.0]
    } else {
        [0.33, 0.34, 0.37, 1.0]
    };
    // Third tier: dimmer than variant but still ≥ the large-text floor.
    let on_tertiary = if dark {
        [0.58, 0.60, 0.63, 1.0]
    } else {
        [0.42, 0.43, 0.46, 1.0]
    };
    // Elevated surface: lighter than `surface` on dark skins (elevation-by-color),
    // near-white on light skins so overlays lift off the panel.
    let surface_elevated = if dark {
        shift(surface, 0.06)
    } else {
        [1.0, 1.0, 1.0, surface[3]]
    };
    // System-red, tuned per theme for contrast against the surface.
    let destructive = if dark {
        [1.0, 0.42, 0.40, 1.0]
    } else {
        [0.80, 0.16, 0.14, 1.0]
    };
    ColorRoles {
        surface,
        surface_variant: shift(surface, 0.05),
        surface_elevated,
        on_surface: on,
        on_surface_variant: on_variant,
        on_surface_tertiary: on_tertiary,
        primary: accent,
        destructive,
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
    style.text_styles.insert(
        egui::TextStyle::Monospace,
        egui::FontId::monospace(ts.command),
    );
    style
        .text_styles
        .insert(egui::TextStyle::Body, egui::FontId::proportional(ts.body));
    style
        .text_styles
        .insert(egui::TextStyle::Button, egui::FontId::proportional(ts.body));
    style
        .text_styles
        .insert(egui::TextStyle::Small, egui::FontId::proportional(ts.small));
    style.text_styles.insert(
        egui::TextStyle::Heading,
        egui::FontId::proportional(ts.panel_title),
    );

    // ── Spacing scale → egui spacing (8pt grid) ────────────────────────
    let sp = t.spacing;
    style.spacing.item_spacing = egui::vec2(sp.s, sp.xs);
    // Cozier, SwiftUI-like control padding drawn straight off the grid.
    style.spacing.button_padding = egui::vec2(sp.sm, sp.xs);
    style.spacing.menu_margin = egui::Margin::same(sp.xs as i8);
    style.spacing.indent = sp.m;
    // ── Accessibility: minimum hit-target ──────────────────────────────
    // SwiftUI/HIG floor is ~44pt; egui's default (~14) is too small. Raise the
    // interactive minimum so every PRIMARY chrome control (toolbar/dialog
    // buttons, combos, fields) meets a comfortable ~28px target. Dense list
    // rows that want to stay at 24 opt down locally with `interact_size`.
    style.spacing.interact_size.y = style
        .spacing
        .interact_size
        .y
        .max(Spacing::HIT_TARGET);
    style.spacing.button_padding.y = style.spacing.button_padding.y.max(sp.xs);

    // ── Color roles → egui visuals ─────────────────────────────────────
    let c = t.colors;
    let v = &mut style.visuals;
    let primary = to_color32(c.primary);
    let surface = to_color32(c.surface);
    let surface_variant = to_color32(c.surface_variant);
    let surface_elevated = to_color32(c.surface_elevated);
    let on_surface = to_color32(c.on_surface);
    let on_variant = to_color32(c.on_surface_variant);
    let outline = to_color32(c.outline);

    v.selection.bg_fill = primary;
    v.selection.stroke = egui::Stroke::new(1.5, primary);
    v.hyperlink_color = primary;
    v.panel_fill = surface;
    // Overlays (dialogs/popovers/menus/autosuggest/download-chip) float ABOVE
    // panels: egui draws Window/menu/popup frames off `window_fill`, so mapping
    // it to `surface_elevated` gives elevation-by-color (lighter in dark mode).
    v.window_fill = surface_elevated;
    v.extreme_bg_color = surface_variant; // text edit background
    v.faint_bg_color = surface_variant;
    v.override_text_color = Some(on_surface);
    v.window_stroke = egui::Stroke::new(1.0, outline);

    // Widget state colors: idle → hover → active use surface_variant → primary.
    v.widgets.noninteractive.bg_fill = surface;
    v.widgets.noninteractive.weak_bg_fill = surface;
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, on_variant);
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, outline);

    // Borderless idle controls (SwiftUI style): no heavy outline at rest —
    // affordance comes from spacing + subtle fills, with borders only on hover.
    v.widgets.inactive.bg_fill = surface_variant;
    v.widgets.inactive.weak_bg_fill = surface_variant;
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, on_surface);
    v.widgets.inactive.bg_stroke = egui::Stroke::NONE;

    v.widgets.hovered.bg_fill = surface_variant;
    v.widgets.hovered.weak_bg_fill = surface_variant;
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.5, on_surface);
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, primary);

    v.widgets.active.bg_fill = primary;
    v.widgets.active.weak_bg_fill = primary;
    v.widgets.active.fg_stroke = egui::Stroke::new(1.5, on_surface);
    v.widgets.active.bg_stroke = egui::Stroke::new(1.0, primary);

    // ── Accessibility: a visible keyboard focus ring ───────────────────
    // A clear 2px accent ring on the focused widget (SwiftUI/HIG focus cue).
    v.widgets.hovered.expansion = 1.0;
    v.selection.stroke = egui::Stroke::new(2.0, primary);

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
        assert_eq!(s.sm, 12.0);
        assert_eq!(s.m, 16.0);
        assert_eq!(s.l, 24.0);
        assert_eq!(s.xl, 32.0);
        // Each step (past xs) is a multiple of 4, and the 8-grid steps of 8.
        for v in [s.xs, s.s, s.sm, s.m, s.l, s.xl] {
            assert_eq!(v % 4.0, 0.0, "{v} not on 4px grid");
        }
        for v in [s.s, s.m, s.xl] {
            assert_eq!(v % 8.0, 0.0, "{v} not on 8px grid");
        }
    }

    #[test]
    fn spacing_scale_is_the_documented_ramp() {
        // The one exposed scale is exactly 4 / 8 / 12 / 16 / 24 / 32.
        let s = Spacing::default();
        assert_eq!(
            [s.xs, s.s, s.sm, s.m, s.l, s.xl],
            [4.0, 8.0, 12.0, 16.0, 24.0, 32.0]
        );
        // Strictly increasing.
        let ramp = [s.xs, s.s, s.sm, s.m, s.l, s.xl];
        for w in ramp.windows(2) {
            assert!(w[0] < w[1], "ramp not strictly increasing at {w:?}");
        }
    }

    #[test]
    fn apply_to_style_enforces_min_hit_target_and_borderless_idle() {
        let mut style = egui::Style::default();
        let tokens = Tokens {
            spacing: Spacing::default(),
            type_scale: TypeScale::default(),
            colors: roles_from([0.13, 0.14, 0.16, 1.0], [0.35, 0.65, 1.0, 1.0]),
            radii: Radii::default(),
            dark: true,
        };
        apply_to_style(&mut style, &tokens);
        // Min hit-target height meets the ~28px PRIMARY-chrome floor for a11y
        // (above the old 24 group-spacing floor).
        assert!(style.spacing.interact_size.y >= Spacing::HIT_TARGET);
        assert!(Spacing::HIT_TARGET >= tokens.spacing.l);
        // Idle controls are borderless (no outline until hover).
        assert_eq!(style.visuals.widgets.inactive.bg_stroke, egui::Stroke::NONE);
        // Focus/selection ring is a visible 2px accent.
        assert_eq!(style.visuals.selection.stroke.width, 2.0);
    }

    #[test]
    fn type_scale_orders_small_to_title() {
        let t = TypeScale::default();
        assert!(t.small < t.body);
        assert!(t.body <= t.command);
        assert!(t.command <= t.panel_title);
    }

    // ── WCAG contrast ────────────────────────────────────────────────────

    #[test]
    fn contrast_ratio_extremes() {
        // Black on white is the 21:1 ceiling; identical colors are 1:1.
        let bw = contrast_ratio([0.0, 0.0, 0.0, 1.0], [1.0, 1.0, 1.0, 1.0]);
        assert!((bw - 21.0).abs() < 0.1, "black/white should be ~21:1, got {bw}");
        let same = contrast_ratio([0.5, 0.5, 0.5, 1.0], [0.5, 0.5, 0.5, 1.0]);
        assert!((same - 1.0).abs() < 1e-4, "same color must be 1:1, got {same}");
        // Symmetric.
        let a = [0.2, 0.3, 0.4, 1.0];
        let b = [0.8, 0.7, 0.6, 1.0];
        assert!((contrast_ratio(a, b) - contrast_ratio(b, a)).abs() < 1e-5);
    }

    /// on_surface and on_surface_variant must clear WCAG AA (4.5:1) against
    /// `surface`, on BOTH skins (light + dark) and BOTH themes.
    #[test]
    fn body_text_meets_wcag_aa_on_all_skins() {
        // Representative dark skin (ItsJustCAD) and light skin (Rhino gray / white).
        let surfaces = [
            [0.129, 0.157, 0.188, 1.0], // dark skin surface
            [0.831, 0.831, 0.831, 1.0], // Rhino light gray
            [1.0, 1.0, 1.0, 1.0],       // white light skin
        ];
        for surface in surfaces {
            let r = roles_from(surface, [0.0, 0.631, 0.945, 1.0]);
            let primary_c = contrast_ratio(r.on_surface, r.surface);
            let variant_c = contrast_ratio(r.on_surface_variant, r.surface);
            assert!(
                primary_c >= 4.5,
                "on_surface {primary_c:.2}:1 < 4.5 on surface {surface:?}"
            );
            assert!(
                variant_c >= 4.5,
                "on_surface_variant {variant_c:.2}:1 < 4.5 on surface {surface:?}"
            );
        }
    }

    /// Small text (`small` size class) with the variant color must still clear
    /// AA against the surface — the reason `small` was lifted 11 → 12.
    #[test]
    fn small_text_variant_meets_wcag_aa() {
        assert!(TypeScale::default().small >= 12.0, "small must be lifted to 12");
        for surface in [[0.129, 0.157, 0.188, 1.0], [1.0, 1.0, 1.0, 1.0]] {
            let r = roles_from(surface, [0.0, 0.631, 0.945, 1.0]);
            assert!(
                contrast_ratio(r.on_surface_variant, r.surface) >= 4.5,
                "small variant text below AA on {surface:?}"
            );
        }
    }

    /// The third label tier is dimmer than the variant but still clears the
    /// large/UI-text floor (3:1) against the surface.
    #[test]
    fn tertiary_is_dimmer_but_legible() {
        for surface in [
            [0.129, 0.157, 0.188, 1.0],
            [0.831, 0.831, 0.831, 1.0],
            [1.0, 1.0, 1.0, 1.0],
        ] {
            let r = roles_from(surface, [0.0, 0.631, 0.945, 1.0]);
            let tertiary_c = contrast_ratio(r.on_surface_tertiary, r.surface);
            let variant_c = contrast_ratio(r.on_surface_variant, r.surface);
            assert!(tertiary_c >= 3.0, "tertiary {tertiary_c:.2}:1 < 3.0 on {surface:?}");
            assert!(
                tertiary_c < variant_c,
                "tertiary must be dimmer than variant on {surface:?}"
            );
        }
    }

    // ── New color roles ──────────────────────────────────────────────────

    #[test]
    fn surface_elevated_is_lighter_in_dark_mode() {
        let dark = roles_from([0.13, 0.14, 0.16, 1.0], [0.35, 0.65, 1.0, 1.0]);
        assert!(
            luminance(dark.surface_elevated) > luminance(dark.surface),
            "dark-mode elevation must be lighter than surface (elevation-by-color)"
        );
        // Light mode: elevated stays at/above the surface (lifts off the panel).
        let light = roles_from([0.90, 0.90, 0.90, 1.0], [0.0, 0.44, 0.75, 1.0]);
        assert!(luminance(light.surface_elevated) >= luminance(light.surface));
    }

    #[test]
    fn destructive_is_reddish_and_legible() {
        for surface in [[0.13, 0.14, 0.16, 1.0], [1.0, 1.0, 1.0, 1.0]] {
            let r = roles_from(surface, [0.0, 0.631, 0.945, 1.0]);
            // Red channel dominates.
            assert!(
                r.destructive[0] > r.destructive[1] && r.destructive[0] > r.destructive[2],
                "destructive must read red on {surface:?}"
            );
            // Visible against the surface (UI-component floor).
            assert!(
                contrast_ratio(r.destructive, r.surface) >= 3.0,
                "destructive below 3:1 on {surface:?}"
            );
        }
    }

    #[test]
    fn apply_maps_surface_elevated_to_window_fill() {
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
        // Overlays (window/menu/popup) draw off window_fill = elevated;
        // panels stay on surface. So overlays are visibly lighter than panels.
        assert_eq!(style.visuals.window_fill, to_color32(colors.surface_elevated));
        assert_eq!(style.visuals.panel_fill, to_color32(colors.surface));
        assert_ne!(style.visuals.window_fill, style.visuals.panel_fill);
    }

    // ── Type weights ─────────────────────────────────────────────────────

    #[test]
    fn type_weights_titles_are_semibold_no_light() {
        let t = TypeScale::default();
        assert_eq!(t.panel_title_weight, Weight::Semibold);
        assert_eq!(t.title_weight, Weight::Semibold);
        assert_eq!(t.body_weight, Weight::Regular);
        assert_eq!(t.caption_weight, Weight::Regular);
        // panel_title is 15, title 20, caption 11.
        assert_eq!(t.panel_title, 15.0);
        assert_eq!(t.title, 20.0);
        assert_eq!(t.caption, 11.0);
        // Ordering: caption ≤ small ≤ body ≤ command ≤ panel_title ≤ title.
        assert!(t.caption <= t.small);
        assert!(t.small <= t.body);
        assert!(t.body <= t.command);
        assert!(t.command <= t.panel_title);
        assert!(t.panel_title < t.title);
        // The three weights are distinct and ordered light→heavy; no sub-Regular.
        assert_ne!(Weight::Regular, Weight::Medium);
        assert_ne!(Weight::Medium, Weight::Semibold);
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
            type_scale: TypeScale {
                command: 15.0,
                body: 14.0,
                small: 10.0,
                panel_title: 18.0,
                ..TypeScale::default()
            },
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

    /// Guard: the overlay/chat/command draw sites this batch tokenized must not
    /// regress to raw pixel spacing literals. If a future edit reintroduces a
    /// bare `Margin::same(6/8)`, a `max_height(80.0)`, `input_height = 64.0`, or
    /// the `[-12.0, -12.0]` chip anchor, this fails and points back at the token.
    #[test]
    fn draw_sites_use_spacing_tokens_not_raw_literals() {
        let deck = include_str!("deck_pane.rs");
        let cmd = include_str!("command_line.rs");
        let app = include_str!("app.rs");
        let banned: &[(&str, &str)] = &[
            (deck, "inner_margin(egui::Margin::same(6))"),
            (deck, "inner_margin(egui::Margin::same(8))"),
            (deck, "input_height = 64.0"),
            (cmd, ".max_height(80.0)"),
            (app, "[-12.0, -12.0]"),
        ];
        for (src, pat) in banned {
            assert!(
                !src.contains(pat),
                "raw spacing literal `{pat}` reintroduced — use a Spacing token"
            );
        }
        // And the tokens must actually be referenced where we wired them.
        assert!(deck.contains("Spacing::CHAT_INPUT_H"));
        // command_line uses history_h_for (which delegates to HISTORY_H) for
        // token-relative height; either form satisfies the no-raw-literals rule.
        assert!(
            cmd.contains("history_h_for") || cmd.contains("Spacing::HISTORY_H"),
            "command_line.rs must use history_h_for or Spacing::HISTORY_H"
        );
        assert!(app.contains("Spacing::SM"));
    }

    #[test]
    fn history_h_for_clamps_and_scales() {
        // Floor: tiny panels never drop below HISTORY_H.
        assert_eq!(
            Spacing::history_h_for(0.0),
            Spacing::HISTORY_H,
            "floor must be HISTORY_H"
        );
        assert_eq!(
            Spacing::history_h_for(200.0),
            Spacing::HISTORY_H,
            "200px panel: 30% = 60 < HISTORY_H, so clamp to floor"
        );
        // 30% of a 400px panel = 120, within [80, 240].
        let h = Spacing::history_h_for(400.0);
        assert!((h - 120.0).abs() < 0.1, "400px→30%=120, got {h}");
        // Ceiling: very tall panels cap at 240.
        assert_eq!(Spacing::history_h_for(1200.0), 240.0, "ceiling must be 240");
        // Result is always in [HISTORY_H, 240].
        for pct in [0, 100, 300, 500, 800, 1200] {
            let h = Spacing::history_h_for(pct as f32);
            assert!(
                (Spacing::HISTORY_H..=240.0).contains(&h),
                "history_h_for({pct}) = {h} out of range"
            );
        }
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
