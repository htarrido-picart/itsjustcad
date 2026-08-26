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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelTab {
    Deck,
    Sessions,
    Model,
}

impl PanelTab {
    /// All tabs in display order (Chat first).
    pub const ALL: [PanelTab; 3] = [PanelTab::Deck, PanelTab::Sessions, PanelTab::Model];

    pub fn label(self) -> &'static str {
        match self {
            PanelTab::Deck => "Chat",
            PanelTab::Sessions => "Sessions",
            PanelTab::Model => "Layers",
        }
    }

    /// The Lucide [`crate::icons::Icon`] shown beside this tab's label.
    pub fn icon(self) -> crate::icons::Icon {
        match self {
            PanelTab::Deck => crate::icons::Icon::Chat,
            PanelTab::Sessions => crate::icons::Icon::Sessions,
            PanelTab::Model => crate::icons::Icon::Layers,
        }
    }
}

/// The ONE constant width (points) the right dock renders at for ALL tabs.
/// This is the Layers panel's width; Chat and Sessions render at the same
/// width so switching tabs never resizes the dock. The user may still drag to
/// resize (the dragged width persists across tab switches), but a tab switch
/// alone always leaves the width unchanged — see [`dock_width`].
pub const DOCK_WIDTH: f32 = 320.0;

/// Minimum dock width (px) — the resize floor. The dock never shrinks below
/// this; its maximum is half the window width (enforced in `right_panel`).
pub const DOCK_MIN: f32 = 280.0;

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
}

impl Default for TabState {
    fn default() -> Self {
        // Chat is the default-selected tab on open.
        Self {
            active: PanelTab::Deck,
            collapsed: false,
        }
    }
}

impl TabState {
    pub fn active(self) -> PanelTab {
        self.active
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
    /// of the UI wants to reveal a specific tab, e.g. Cmd+\ → Deck).
    pub fn show(&mut self, tab: PanelTab) {
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
pub fn strip_ui(
    ui: &mut egui::Ui,
    icons: &crate::icons::Icons,
    state: TabState,
) -> Option<PanelTab> {
    let mut clicked = None;
    ui.horizontal(|ui| {
        for tab in PanelTab::ALL {
            let selected = tab == state.active && !state.collapsed;
            // Icon tinted to accent when selected, foreground otherwise — a
            // borderless segmented-control look (icon + label, no chrome).
            let size = ui.text_style_height(&egui::TextStyle::Body);
            let color = if selected {
                ui.visuals().selection.bg_fill
            } else {
                ui.visuals().weak_text_color()
            };
            let img = icons.image(ui.ctx(), tab.icon(), size, color);
            let resp = ui
                .horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.add(img);
                    ui.selectable_label(selected, tab.label())
                })
                .inner;
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
        assert_eq!(PanelTab::ALL.len(), 3);
        let labels: Vec<_> = PanelTab::ALL.iter().map(|t| t.label()).collect();
        assert_eq!(labels, ["Chat", "Sessions", "Layers"]);
        assert_eq!(labels[0], "Chat", "Chat must be the first tab");
        assert!(!labels.contains(&"History"));
        assert!(!labels.contains(&"Model"));
        assert!(!labels.contains(&"Deck"));
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
        for tab in PanelTab::ALL {
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
        let labels: Vec<_> = PanelTab::ALL.iter().map(|t| t.label()).collect();
        for i in 0..labels.len() {
            for j in (i + 1)..labels.len() {
                assert_ne!(labels[i], labels[j]);
            }
        }
    }
}
