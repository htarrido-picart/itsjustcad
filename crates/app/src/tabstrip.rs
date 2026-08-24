// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Hand-rolled tab-strip state machine + a thin egui renderer.
//!
//! Used by the right docked panel (Layers / Properties / History / Deck) and,
//! in a simpler form, by the viewport tab bar. The *state* — which tab is
//! active, whether the panel is collapsed — is a pure value type with no egui
//! dependency, so its transitions are unit-tested standalone. The `ui` helper
//! only draws the strip and reports clicks back.

/// The four tabs of the right docked panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelTab {
    Layers,
    Properties,
    History,
    Deck,
}

impl PanelTab {
    /// All tabs in display order.
    pub const ALL: [PanelTab; 4] =
        [PanelTab::Layers, PanelTab::Properties, PanelTab::History, PanelTab::Deck];

    pub fn label(self) -> &'static str {
        match self {
            PanelTab::Layers => "Layers",
            PanelTab::Properties => "Properties",
            PanelTab::History => "History",
            PanelTab::Deck => "Deck",
        }
    }
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
        Self { active: PanelTab::Layers, collapsed: false }
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
pub fn strip_ui(ui: &mut egui::Ui, state: TabState) -> Option<PanelTab> {
    let mut clicked = None;
    ui.horizontal(|ui| {
        for tab in PanelTab::ALL {
            let selected = tab == state.active && !state.collapsed;
            if ui.selectable_label(selected, tab.label()).clicked() {
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
        assert!(tabs.iter().filter(|(l, _)| l.eq_ignore_ascii_case("top")).count() == 1);
        assert_eq!(tabs[4].0, "custom");
    }

    #[test]
    fn default_is_layers_open() {
        let s = TabState::default();
        assert_eq!(s.active(), PanelTab::Layers);
        assert!(!s.is_collapsed());
    }

    #[test]
    fn click_other_tab_activates_it() {
        let mut s = TabState::default();
        s.click(PanelTab::History);
        assert_eq!(s.active(), PanelTab::History);
        assert!(!s.is_collapsed());
    }

    #[test]
    fn click_active_tab_collapses_then_expands() {
        let mut s = TabState::default();
        s.click(PanelTab::Layers); // active → collapse
        assert!(s.is_collapsed());
        assert_eq!(s.active(), PanelTab::Layers);
        s.click(PanelTab::Layers); // active again → expand
        assert!(!s.is_collapsed());
    }

    #[test]
    fn click_different_tab_while_collapsed_expands() {
        let mut s = TabState::default();
        s.click(PanelTab::Layers); // collapse
        assert!(s.is_collapsed());
        s.click(PanelTab::Deck); // switch → must expand
        assert_eq!(s.active(), PanelTab::Deck);
        assert!(!s.is_collapsed());
    }

    #[test]
    fn show_forces_tab_open() {
        let mut s = TabState::default();
        s.click(PanelTab::Layers); // collapse
        s.show(PanelTab::Deck);
        assert_eq!(s.active(), PanelTab::Deck);
        assert!(!s.is_collapsed());
    }

    #[test]
    fn toggle_collapsed_keeps_active() {
        let mut s = TabState::default();
        s.click(PanelTab::Properties);
        s.toggle_collapsed();
        assert!(s.is_collapsed());
        assert_eq!(s.active(), PanelTab::Properties);
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
