// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Hand-rolled tab-strip state machine + a thin egui renderer.
//!
//! Used by the right docked panel (Layers / Properties / Chat) and,
//! in a simpler form, by the viewport tab bar. The *state* — which tab is
//! active, whether the panel is collapsed — is a pure value type with no egui
//! dependency, so its transitions are unit-tested standalone. The `ui` helper
//! only draws the strip and reports clicks back.

/// The tabs of the right docked panel, in display order:
///   - `Deck` (**Chat**): the embedded LLM chat. FIRST and default-selected on
///     open. The deck/cassette internals keep their names — no module churn.
///   - `Sessions`: a browser of this document's stored chats as cards
///     (title + summary + date), with full-text search. Clicking a card loads
///     it into the Chat tab. Promoted OUT of the Chat pane into its own tab.
///   - `Layers` (was "Model"): Layers **and** Properties shown together as
///     stacked, independently-collapsible sections (Rhino-style).
///   - `Blocks` / `Plugins`: DYNAMIC tabs. They appear only while the document
///     has block definitions / the session has installed plugins, or while the
///     user has explicitly opened them (see [`TabState::visible_tabs`]). Pure
///     VIEW + verb-trigger surfaces: every mutation they offer routes through
///     the normal command substrate, never a second mutation path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelTab {
    Deck,
    Sessions,
    Model,
    Blocks,
    Plugins,
}

impl PanelTab {
    /// The always-present tabs in display order (Chat first). Dynamic tabs
    /// (Blocks/Plugins) are appended by [`TabState::visible_tabs`].
    pub const FIXED: [PanelTab; 3] = [PanelTab::Deck, PanelTab::Sessions, PanelTab::Model];

    /// True for tabs that come and go with content (Blocks/Plugins).
    #[allow(dead_code)] // part of the tab-registry API; exercised in tests
    pub fn is_dynamic(self) -> bool {
        matches!(self, PanelTab::Blocks | PanelTab::Plugins)
    }

    pub fn label(self) -> &'static str {
        match self {
            PanelTab::Deck => "Chat",
            PanelTab::Sessions => "Sessions",
            PanelTab::Model => "Layers",
            PanelTab::Blocks => "Blocks",
            PanelTab::Plugins => "Plugins",
        }
    }

    /// The Lucide [`crate::icons::Icon`] shown beside this tab's label.
    pub fn icon(self) -> crate::icons::Icon {
        match self {
            PanelTab::Deck => crate::icons::Icon::Chat,
            PanelTab::Sessions => crate::icons::Icon::Sessions,
            PanelTab::Model => crate::icons::Icon::Layers,
            PanelTab::Blocks => crate::icons::Icon::Solid,     // lucide "package"
            PanelTab::Plugins => crate::icons::Icon::ToolsCat, // lucide "wrench"
        }
    }
}

/// The ONE constant width (points) the right dock renders at for ALL tabs.
/// This is the Layers panel's width; Chat and Sessions render at the same
/// width so switching tabs never resizes the dock. The user may still drag to
/// resize (the dragged width persists across tab switches), but a tab switch
/// alone always leaves the width unchanged — see [`dock_width`].
pub const DOCK_WIDTH: f32 = 280.0;

/// Minimum dock width (px) — the resize floor. The dock never shrinks below
/// this; its maximum is half the window width (enforced in `right_panel`).
pub const DOCK_MIN: f32 = 200.0;

/// The dock's render width for a given tab and a (possibly user-dragged) stored
/// width. The width is INDEPENDENT of which tab is active: every tab renders at
/// the same `stored` width (seeded from [`DOCK_WIDTH`]). This is the single
/// source of truth the panel reads, so a tab switch can never change the width.
pub fn dock_width(_tab: PanelTab, stored: f32) -> f32 {
    // Deliberately ignores `tab`: constant width across all tabs.
    stored
}

/// Panel tab-strip state: the active tab and a collapsed flag. Pure; the
/// transitions below are the whole contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabState {
    active: PanelTab,
    collapsed: bool,
    /// User explicitly opened the Blocks tab (keeps it visible while empty).
    blocks_pinned: bool,
    /// User explicitly opened the Plugins tab (keeps it visible while empty).
    plugins_pinned: bool,
}

impl Default for TabState {
    fn default() -> Self {
        // Chat is the default-selected tab on open.
        Self {
            active: PanelTab::Deck,
            collapsed: false,
            blocks_pinned: false,
            plugins_pinned: false,
        }
    }
}

impl TabState {
    pub fn active(self) -> PanelTab {
        self.active
    }

    /// The ordered tab list to draw this frame: the three fixed tabs, then each
    /// dynamic tab that is *visible*. A dynamic tab is visible while it has
    /// content (`has_blocks` / `has_plugins`), while the user has pinned it
    /// open via [`show`](Self::show), or while it is the active tab (the
    /// active tab may never vanish out from under the user mid-look).
    pub fn visible_tabs(self, has_blocks: bool, has_plugins: bool) -> Vec<PanelTab> {
        let mut tabs: Vec<PanelTab> = PanelTab::FIXED.to_vec();
        if has_blocks || self.blocks_pinned || self.active == PanelTab::Blocks {
            tabs.push(PanelTab::Blocks);
        }
        if has_plugins || self.plugins_pinned || self.active == PanelTab::Plugins {
            tabs.push(PanelTab::Plugins);
        }
        tabs
    }

    /// Per-frame reconciliation of the dynamic tabs against live content. An
    /// empty dynamic tab keeps its pin only while it stays active — once the
    /// user navigates away from an empty Blocks/Plugins tab it un-pins, so the
    /// tab disappears (and reappears automatically when content exists again).
    pub fn sync_dynamic(&mut self, has_blocks: bool, has_plugins: bool) {
        if !has_blocks && self.active != PanelTab::Blocks {
            self.blocks_pinned = false;
        }
        if !has_plugins && self.active != PanelTab::Plugins {
            self.plugins_pinned = false;
        }
    }

    pub fn is_collapsed(self) -> bool {
        self.collapsed
    }

    /// Click a tab. Clicking the *active* tab collapses the panel; clicking a
    /// different tab activates it (and un-collapses if it was collapsed). This
    /// is the standard "click active header to hide" affordance.
    pub fn click(&mut self, tab: PanelTab) {
        if self.active == tab {
            self.collapsed = !self.collapsed;
        } else {
            self.active = tab;
            self.collapsed = false;
        }
    }

    /// Force-select a tab and ensure the panel is open (used when another part
    /// of the UI wants to reveal a specific tab, e.g. Cmd+\ → Deck). Showing a
    /// dynamic tab pins it visible even while it has no content yet (so "open
    /// the Blocks tab" from a menu/deck works on an empty document).
    pub fn show(&mut self, tab: PanelTab) {
        match tab {
            PanelTab::Blocks => self.blocks_pinned = true,
            PanelTab::Plugins => self.plugins_pinned = true,
            _ => {}
        }
        self.active = tab;
        self.collapsed = false;
    }

    /// Toggle collapse without changing the active tab.
    #[allow(dead_code)] // part of the state-machine API; exercised in tests
    pub fn toggle_collapsed(&mut self) {
        self.collapsed = !self.collapsed;
    }
}

/// Draw the panel tab strip. Returns the tab clicked this frame, if any. The
/// caller applies the click to its [`TabState`] and paints the active body.
/// Vertical inner padding of a tab (px). Shared so the hide-panel button beside
/// the strip can be forced to the SAME height (`body text + 2×TAB_V_PAD`).
pub const TAB_V_PAD: f32 = 6.0;

pub fn strip_ui(
    ui: &mut egui::Ui,
    icons: &crate::icons::Icons,
    state: TabState,
    tabs: &[PanelTab],
) -> Option<PanelTab> {
    let mut clicked = None;
    // Folder-style tabs: rounded TOP corners only; the SELECTED tab is WHITE with
    // a soft shadow (raised), unselected tabs are a recessed off-white. Reads like
    // a row of file folders sitting on the panel below.
    // Route tab fills through the LIVE theme's surface ramp (set by
    // `theme::apply_colors`): the SELECTED tab uses the ELEVATED role
    // (`window_fill`) so it reads raised, unselected tabs use the recessed inset
    // (`faint_bg_color` = surface_variant). Both stay coherent with the dock in
    // dark and light without magic numbers.
    // Folder look. DARK: selected tab = recessed (darker) surface matching the
    // dock content so it wraps as one dark block; unselected = the lighter panel
    // surface (raised behind). LIGHT was already good — selected = panel surface,
    // unselected = the recessed inset.
    let dark = ui.visuals().dark_mode;
    let (sel_fill, unsel_fill) = if dark {
        (crate::theme::recessed_fill(true), ui.visuals().panel_fill)
    } else {
        (ui.visuals().panel_fill, ui.visuals().faint_bg_color)
    };
    let top_round = egui::CornerRadius { nw: 6, ne: 6, sw: 0, se: 0 };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for &tab in tabs {
            let selected = tab == state.active && !state.collapsed;
            let size = ui.text_style_height(&egui::TextStyle::Body);
            let fg = if selected {
                ui.visuals().strong_text_color()
            } else {
                ui.visuals().weak_text_color()
            };
            // Vertical padding = 6 (see TAB_V_PAD): tabs get a little more presence
            // and the hide-panel button matches this exact height.
            let mut frame = egui::Frame::NONE
                .fill(if selected { sel_fill } else { unsel_fill })
                .corner_radius(top_round)
                .inner_margin(egui::Margin::symmetric(
                    crate::theme::Spacing::SM as i8,
                    crate::tabstrip::TAB_V_PAD as i8,
                ));
            if selected {
                frame = frame.shadow(egui::epaint::Shadow {
                    offset: [0, 1],
                    blur: 6,
                    spread: 0,
                    color: egui::Color32::from_black_alpha(50),
                });
            }
            let resp = frame
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        ui.add(icons.image(ui.ctx(), tab.icon(), size, fg));
                        ui.label(egui::RichText::new(tab.label()).color(fg));
                    });
                })
                .response
                .interact(egui::Sense::click());
            if resp.clicked() {
                clicked = Some(tab);
            }
        }
    });
    clicked
}

/// Standard viewport tabs always present at the bottom of the viewport frame.
/// Named saved views are appended after these.
pub const STANDARD_VIEW_TABS: [(&str, &str); 4] = [
    ("Persp", "persp"),
    ("Top", "top"),
    ("Front", "front"),
    ("Right", "right"),
];

/// Build the ordered viewport tab list: the four standard views first, then any
/// saved named views (deduped against the standard names, case-insensitively).
/// Returns `(label, view_verb)` pairs; for a named view the verb restores it
/// via `view <name>`, for a standard view it is the bare view name.
// Pure helper retained for the ordered-tab contract + tests; the viewport bar
// now composes the standard views via a segmented control and appends named
// views itself, so this has no live UI caller.
#[allow(dead_code)]
pub fn viewport_tabs(named: &[String]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = STANDARD_VIEW_TABS
        .iter()
        .map(|(l, v)| ((*l).to_string(), (*v).to_string()))
        .collect();
    for name in named {
        let lower = name.to_ascii_lowercase();
        if STANDARD_VIEW_TABS.iter().any(|(_, v)| *v == lower) {
            continue; // don't duplicate a standard view name
        }
        out.push((name.clone(), format!("view {name}")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_tabs_start_with_four_standard() {
        let tabs = viewport_tabs(&[]);
        assert_eq!(tabs.len(), 4);
        assert_eq!(tabs[0].0, "Persp");
        assert_eq!(tabs[0].1, "persp");
        assert_eq!(tabs[3].0, "Right");
    }

    #[test]
    fn viewport_tabs_appends_named_views() {
        let tabs = viewport_tabs(&["entry".to_string(), "aerial".to_string()]);
        assert_eq!(tabs.len(), 6);
        assert_eq!(tabs[4], ("entry".to_string(), "view entry".to_string()));
        assert_eq!(tabs[5], ("aerial".to_string(), "view aerial".to_string()));
    }

    #[test]
    fn viewport_tabs_dedupes_standard_names() {
        let tabs = viewport_tabs(&["Top".to_string(), "custom".to_string()]);
        // "Top" collides with the standard tab and is dropped.
        assert_eq!(tabs.len(), 5);
        assert!(
            tabs.iter()
                .filter(|(l, _)| l.eq_ignore_ascii_case("top"))
                .count()
                == 1
        );
        assert_eq!(tabs[4].0, "custom");
    }

    #[test]
    fn default_is_chat_open() {
        let s = TabState::default();
        assert_eq!(s.active(), PanelTab::Deck); // "Chat"
        assert!(!s.is_collapsed());
    }

    #[test]
    fn click_other_tab_activates_it() {
        let mut s = TabState::default();
        s.click(PanelTab::Model);
        assert_eq!(s.active(), PanelTab::Model);
        assert!(!s.is_collapsed());
    }

    #[test]
    fn three_tabs_chat_first_then_sessions_then_layers() {
        // Chat is FIRST (and default). Sessions is its own tab. "Model" now
        // shows as "Layers". No History/Deck/Model labels leak.
        assert_eq!(PanelTab::FIXED.len(), 3);
        let labels: Vec<_> = PanelTab::FIXED.iter().map(|t| t.label()).collect();
        assert_eq!(labels, ["Chat", "Sessions", "Layers"]);
        assert_eq!(labels[0], "Chat", "Chat must be the first tab");
        assert!(!labels.contains(&"History"));
        assert!(!labels.contains(&"Model"));
        assert!(!labels.contains(&"Deck"));
    }

    // ---- dynamic tabs (Blocks / Plugins) ----

    #[test]
    fn dynamic_tabs_hidden_without_content() {
        let s = TabState::default();
        let tabs = s.visible_tabs(false, false);
        assert_eq!(tabs, PanelTab::FIXED.to_vec(), "no content → fixed tabs only");
    }

    #[test]
    fn blocks_tab_appears_with_block_definitions() {
        let s = TabState::default();
        let tabs = s.visible_tabs(true, false);
        assert_eq!(tabs, vec![
            PanelTab::Deck,
            PanelTab::Sessions,
            PanelTab::Model,
            PanelTab::Blocks
        ]);
    }

    #[test]
    fn plugins_tab_appears_with_installed_plugins() {
        let s = TabState::default();
        let tabs = s.visible_tabs(false, true);
        assert!(tabs.contains(&PanelTab::Plugins));
        assert!(!tabs.contains(&PanelTab::Blocks));
    }

    #[test]
    fn both_dynamic_tabs_ordered_blocks_before_plugins() {
        let s = TabState::default();
        let tabs = s.visible_tabs(true, true);
        assert_eq!(tabs.len(), 5);
        assert_eq!(tabs[3], PanelTab::Blocks);
        assert_eq!(tabs[4], PanelTab::Plugins);
    }

    #[test]
    fn show_pins_an_empty_dynamic_tab_visible() {
        // "Open the Blocks tab" on an empty document must work: show() pins it.
        let mut s = TabState::default();
        s.show(PanelTab::Blocks);
        assert_eq!(s.active(), PanelTab::Blocks);
        assert!(s.visible_tabs(false, false).contains(&PanelTab::Blocks));
    }

    #[test]
    fn empty_dynamic_tab_disappears_after_navigating_away() {
        let mut s = TabState::default();
        s.show(PanelTab::Plugins); // pinned while empty
        s.sync_dynamic(false, false);
        assert!(s.visible_tabs(false, false).contains(&PanelTab::Plugins), "still active");
        s.click(PanelTab::Deck); // navigate away from the EMPTY tab
        s.sync_dynamic(false, false);
        assert!(
            !s.visible_tabs(false, false).contains(&PanelTab::Plugins),
            "empty + not active + un-pinned → gone"
        );
    }

    #[test]
    fn dynamic_tab_with_content_survives_navigating_away() {
        let mut s = TabState::default();
        s.show(PanelTab::Blocks);
        s.click(PanelTab::Model);
        s.sync_dynamic(true, false); // doc still HAS blocks
        assert!(s.visible_tabs(true, false).contains(&PanelTab::Blocks));
    }

    #[test]
    fn dynamic_tab_reappears_when_content_returns() {
        let mut s = TabState::default();
        s.show(PanelTab::Blocks);
        s.click(PanelTab::Deck);
        s.sync_dynamic(false, false); // un-pins: empty + inactive
        assert!(!s.visible_tabs(false, false).contains(&PanelTab::Blocks));
        // A block definition arrives (e.g. `block last door`): tab is back,
        // no user action needed.
        assert!(s.visible_tabs(true, false).contains(&PanelTab::Blocks));
    }

    #[test]
    fn active_dynamic_tab_never_vanishes() {
        // Deleting the last plugin WHILE the Plugins tab is active must not
        // yank the tab out from under the user.
        let mut s = TabState::default();
        s.show(PanelTab::Plugins);
        s.sync_dynamic(false, false);
        assert!(s.visible_tabs(false, false).contains(&PanelTab::Plugins));
        assert_eq!(s.active(), PanelTab::Plugins);
    }

    #[test]
    fn dynamic_tabs_are_flagged_dynamic() {
        assert!(PanelTab::Blocks.is_dynamic());
        assert!(PanelTab::Plugins.is_dynamic());
        for t in PanelTab::FIXED {
            assert!(!t.is_dynamic());
        }
    }

    #[test]
    fn dynamic_tabs_click_behaves_like_fixed_tabs() {
        // Same tab-strip contract as the fixed tabs: click to activate,
        // click-active to collapse, click-again to expand.
        let mut s = TabState::default();
        s.click(PanelTab::Blocks);
        assert_eq!(s.active(), PanelTab::Blocks);
        assert!(!s.is_collapsed());
        s.click(PanelTab::Blocks);
        assert!(s.is_collapsed());
        s.click(PanelTab::Blocks);
        assert!(!s.is_collapsed());
    }

    #[test]
    fn click_active_tab_collapses_then_expands() {
        let mut s = TabState::default();
        s.click(PanelTab::Deck); // active (Chat) → collapse
        assert!(s.is_collapsed());
        assert_eq!(s.active(), PanelTab::Deck);
        s.click(PanelTab::Deck); // active again → expand
        assert!(!s.is_collapsed());
    }

    #[test]
    fn click_different_tab_while_collapsed_expands() {
        let mut s = TabState::default();
        s.click(PanelTab::Deck); // collapse (Chat is active)
        assert!(s.is_collapsed());
        s.click(PanelTab::Model); // switch → must expand
        assert_eq!(s.active(), PanelTab::Model);
        assert!(!s.is_collapsed());
    }

    #[test]
    fn show_forces_tab_open() {
        let mut s = TabState::default();
        s.click(PanelTab::Deck); // collapse
        s.show(PanelTab::Model);
        assert_eq!(s.active(), PanelTab::Model);
        assert!(!s.is_collapsed());
    }

    #[test]
    fn dock_width_is_constant_across_tab_switches() {
        // The width source must return the SAME width for every tab, so
        // switching tabs never resizes the dock.
        let stored = DOCK_WIDTH;
        let w_chat = dock_width(PanelTab::Deck, stored);
        let w_sessions = dock_width(PanelTab::Sessions, stored);
        let w_layers = dock_width(PanelTab::Model, stored);
        assert_eq!(w_chat, w_sessions);
        assert_eq!(w_sessions, w_layers);
        assert_eq!(w_chat, DOCK_WIDTH);
    }

    #[test]
    fn dock_width_preserves_user_drag_across_tabs() {
        // A user-dragged width persists identically for every tab.
        let dragged = 412.0;
        let s = TabState::default();
        for tab in s.visible_tabs(true, true) {
            assert_eq!(dock_width(tab, dragged), dragged);
        }
    }

    #[test]
    fn sessions_tab_is_selectable() {
        let mut s = TabState::default();
        s.show(PanelTab::Sessions);
        assert_eq!(s.active(), PanelTab::Sessions);
        assert!(!s.is_collapsed());
    }

    #[test]
    fn toggle_collapsed_keeps_active() {
        let mut s = TabState::default(); // Chat active
        s.click(PanelTab::Model); // switch to a non-active tab (Layers)
        s.toggle_collapsed();
        assert!(s.is_collapsed());
        assert_eq!(s.active(), PanelTab::Model);
    }

    #[test]
    fn all_tabs_have_distinct_labels() {
        // Every tab (fixed + dynamic) has a distinct label.
        let labels: Vec<_> = TabState::default()
            .visible_tabs(true, true)
            .iter()
            .map(|t| t.label())
            .collect();
        assert_eq!(labels.len(), 5);
        for i in 0..labels.len() {
            for j in (i + 1)..labels.len() {
                assert_ne!(labels[i], labels[j]);
            }
        }
    }
}
