// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Mouse-navigation presets (#367): which pointer button orbits and pans the
//! viewport. Scroll zooms toward the cursor and a middle double-click recentres
//! in every preset; only the drag buttons differ.
//!
//! Navigation is view state: it never touches the active draw tool, guided
//! step, rubber band or gumball, so a tool resumes exactly where it was.

/// The mouse-navigation preset. Persisted to `ui.json` as `"right"` /
/// `"middle"` under `mouse_nav`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseNav {
    /// Right- or middle-drag orbits, Shift+drag pans (the long-standing default).
    #[default]
    Right,
    /// Only middle-drag orbits, Shift+middle-drag pans; the right button never
    /// navigates (its click keeps repeat-last).
    Middle,
}

/// What a navigation drag does to the camera.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavDrag {
    Orbit,
    Pan,
}

impl MouseNav {
    pub const ALL: [MouseNav; 2] = [MouseNav::Right, MouseNav::Middle];

    /// Verb / `ui.json` slug.
    pub fn code(self) -> &'static str {
        match self {
            MouseNav::Right => "right",
            MouseNav::Middle => "middle",
        }
    }

    /// Parse a verb argument. Case-insensitive; also accepts the names of the
    /// modelers whose mouse each preset matches. Unknown → `None`.
    pub fn from_code(s: &str) -> Option<MouseNav> {
        match s.trim().to_ascii_lowercase().as_str() {
            "right" | "rmb" | "default" | "rhino" => Some(MouseNav::Right),
            "middle" | "mmb" | "sketchup" => Some(MouseNav::Middle),
            _ => None,
        }
    }

    /// i18n key of the preset's label.
    pub fn label_key(self) -> &'static str {
        match self {
            MouseNav::Right => "mouse.right",
            MouseNav::Middle => "mouse.middle",
        }
    }

    /// Whether `button` drives navigation drags in this preset.
    pub fn navigates(self, button: egui::PointerButton) -> bool {
        match button {
            egui::PointerButton::Middle => true,
            egui::PointerButton::Secondary => self == MouseNav::Right,
            _ => false,
        }
    }

    /// What a drag with `button` (Shift held or not) does; `None` when the
    /// button does not navigate, so the drag stays with the tool / selection.
    pub fn drag(self, button: egui::PointerButton, shift: bool) -> Option<NavDrag> {
        self.navigates(button)
            .then_some(if shift { NavDrag::Pan } else { NavDrag::Orbit })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::PointerButton::{Extra1, Middle, Primary, Secondary};

    #[test]
    fn right_preset_orbits_with_right_or_middle_and_pans_with_shift() {
        let m = MouseNav::Right;
        for b in [Secondary, Middle] {
            assert_eq!(m.drag(b, false), Some(NavDrag::Orbit));
            assert_eq!(m.drag(b, true), Some(NavDrag::Pan));
        }
        for b in [Primary, Extra1] {
            assert_eq!(m.drag(b, false), None);
            assert_eq!(m.drag(b, true), None);
        }
    }

    #[test]
    fn middle_preset_navigates_only_with_middle() {
        let m = MouseNav::Middle;
        assert_eq!(m.drag(Middle, false), Some(NavDrag::Orbit));
        assert_eq!(m.drag(Middle, true), Some(NavDrag::Pan));
        for b in [Primary, Secondary, Extra1] {
            assert_eq!(m.drag(b, false), None, "{b:?}");
            assert_eq!(m.drag(b, true), None, "{b:?}");
        }
    }

    #[test]
    fn default_is_right() {
        assert_eq!(MouseNav::default(), MouseNav::Right);
    }

    #[test]
    fn codes_round_trip_and_aliases_parse() {
        for m in MouseNav::ALL {
            assert_eq!(MouseNav::from_code(m.code()), Some(m));
            let json = serde_json::to_value(m).unwrap();
            assert_eq!(json, serde_json::json!(m.code()));
            assert_eq!(serde_json::from_value::<MouseNav>(json).unwrap(), m);
        }
        assert_eq!(MouseNav::from_code("SketchUp"), Some(MouseNav::Middle));
        assert_eq!(MouseNav::from_code("rhino"), Some(MouseNav::Right));
        assert_eq!(MouseNav::from_code("wheel"), None);
    }

    #[test]
    fn every_preset_has_a_label() {
        for m in MouseNav::ALL {
            assert_ne!(crate::i18n::t(m.label_key()), m.label_key());
        }
    }
}
