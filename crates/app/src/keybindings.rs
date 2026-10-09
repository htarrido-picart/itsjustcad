// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! User-definable hotkeys (W4): persistence + a keymap overlay.
//!
//! The built-in [`crate::keymap::keymap`] is a hardcoded pure function. This
//! module layers USER bindings on top: a [`KeybindingsFile`] persisted to
//! `~/.config/itsjustcad/keybindings.json` (0600, like `decks.json`) maps a
//! [`KeyCombo`] to a command-line verb. [`resolve`] consults the user map first
//! and falls back to the built-in keymap, so overrides win but nothing in the
//! default set is lost. Both stay pure and unit-testable.
//!
//! A [`KeyCombo`] round-trips to egui via [`egui::Key::name`] /
//! [`egui::Key::from_name`], so the JSON is human-readable (`"Cmd+Shift+K"`).

use egui::{Key, Modifiers};
use serde::{Deserialize, Serialize};

/// A serializable key chord: modifier flags + a key name (`"A"`, `"F8"`,
/// `"Delete"`, `"Up"`). The key string is always normalized through
/// [`egui::Key::name`] so it round-trips with [`egui::Key::from_name`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyCombo {
    #[serde(default)]
    pub cmd: bool,
    #[serde(default)]
    pub shift: bool,
    #[serde(default)]
    pub alt: bool,
    pub key: String,
}

impl KeyCombo {
    /// Build from a live egui key event.
    pub fn from_egui(key: Key, mods: Modifiers) -> Self {
        Self {
            cmd: mods.command,
            shift: mods.shift,
            alt: mods.alt,
            key: key.name().to_string(),
        }
    }

    /// The egui key this combo names, if still valid.
    pub fn egui_key(&self) -> Option<Key> {
        Key::from_name(&self.key)
    }

    /// Does a live `(key, mods)` event match this combo? Modifiers must match
    /// exactly (no extra held keys) so `Cmd+K` never fires on `Cmd+Shift+K`.
    pub fn matches(&self, key: Key, mods: Modifiers) -> bool {
        self.egui_key() == Some(key)
            && mods.command == self.cmd
            && mods.shift == self.shift
            && mods.alt == self.alt
    }

    /// Parse a display string like `"Cmd+Shift+K"` (case-insensitive modifiers;
    /// `Ctrl`/`Command`/`⌘` all mean the command modifier). `None` if the key
    /// token isn't a recognized egui key.
    pub fn parse(s: &str) -> Option<KeyCombo> {
        let (mut cmd, mut shift, mut alt) = (false, false, false);
        let mut key: Option<String> = None;
        for part in s.split('+').map(str::trim).filter(|p| !p.is_empty()) {
            match part.to_ascii_lowercase().as_str() {
                "cmd" | "command" | "ctrl" | "control" | "meta" | "super" | "⌘" | "⌃" => {
                    cmd = true
                }
                "shift" | "⇧" => shift = true,
                "alt" | "option" | "opt" | "⌥" => alt = true,
                // Accept the key token in any case: "j"/"J", "f8"/"F8",
                // "delete"/"Delete", "up"/"Up". Try verbatim, then uppercased
                // (letters / function keys), then title-cased (named keys).
                _ => {
                    let k = Key::from_name(part)
                        .or_else(|| Key::from_name(&part.to_uppercase()))
                        .or_else(|| Key::from_name(&title_case(part)))?;
                    key = Some(k.name().to_string());
                }
            }
        }
        key.map(|key| KeyCombo { cmd, shift, alt, key })
    }

    /// Render as `"Cmd+Shift+K"` for display and persistence round-trips.
    pub fn display(&self) -> String {
        let mut s = String::new();
        if self.cmd {
            s.push_str("Cmd+");
        }
        if self.shift {
            s.push_str("Shift+");
        }
        if self.alt {
            s.push_str("Alt+");
        }
        s.push_str(&self.key);
        s
    }

    /// OS/AppKit-reserved chords we refuse to bind (quit/close), so a bad
    /// binding can never lock the user out of exiting.
    pub fn is_reserved(&self) -> bool {
        self.cmd && !self.shift && !self.alt && matches!(self.key.as_str(), "Q" | "W")
    }
}

/// `"delete"` → `"Delete"`, `"up"` → `"Up"` — first char upper, rest lower.
fn title_case(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().chain(c.flat_map(char::to_lowercase)).collect(),
        None => String::new(),
    }
}

impl std::fmt::Display for KeyCombo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display())
    }
}

/// One user binding: a chord and the command line it runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub combo: KeyCombo,
    pub verb: String,
}

/// Persisted user hotkeys + the assistant opt-in.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct KeybindingsFile {
    #[serde(default)]
    pub bindings: Vec<Binding>,
    /// Opt-in: allow the LLM assistant to set/clear hotkeys via `bind_hotkey`.
    /// OFF by default — a prompt-injected bind could remap keys to destructive
    /// verbs, so the deck plane refuses hotkey changes until the user enables it.
    #[serde(default)]
    pub allow_assistant: bool,
}

pub fn config_path() -> Option<std::path::PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".config")
            .join("itsjustcad")
            .join("keybindings.json"),
    )
}

impl KeybindingsFile {
    pub fn load_or_default() -> Self {
        config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Some(path) = config_path() {
            let _ = std::fs::create_dir_all(path.parent().expect("has parent"));
            let json = serde_json::to_string_pretty(self).expect("serializes");
            let _ = crate::journal::write_private(&path, json.as_bytes());
        }
    }

    /// Verb bound to a live `(key, mods)` event, if any (first match wins).
    pub fn lookup(&self, key: Key, mods: Modifiers) -> Option<&str> {
        self.bindings
            .iter()
            .find(|b| b.combo.matches(key, mods))
            .map(|b| b.verb.as_str())
    }

    /// Add or replace the binding for `combo`. Returns `true` when an existing
    /// binding was overwritten (a conflict the caller may want to surface).
    pub fn set(&mut self, combo: KeyCombo, verb: String) -> bool {
        if let Some(existing) = self.bindings.iter_mut().find(|b| b.combo == combo) {
            existing.verb = verb;
            true
        } else {
            self.bindings.push(Binding { combo, verb });
            false
        }
    }

    /// Remove the binding for `combo`. Returns `true` if one was removed.
    pub fn remove(&mut self, combo: &KeyCombo) -> bool {
        let before = self.bindings.len();
        self.bindings.retain(|b| &b.combo != combo);
        self.bindings.len() != before
    }
}

/// Resolve a key event to a command line: USER bindings first, then the
/// built-in keymap. User bindings are suppressed while typing or mid-draw-pick
/// (same guard the built-in letter verbs use) so a custom chord never hijacks a
/// text field or the draw tool's Esc/Enter.
pub fn resolve(
    key: Key,
    mods: Modifiers,
    ctx: crate::keymap::KeyContext<'_>,
    user: &KeybindingsFile,
) -> Option<String> {
    if !ctx.typing
        && !ctx.draw_active
        && let Some(verb) = user.lookup(key, mods)
    {
        return Some(verb.to_string());
    }
    crate::keymap::keymap(key, mods, ctx)
}

/// Is `line`'s first token a real, bindable verb? Checks the command registry,
/// the app-verb table, and a small set of execute_line-only verbs. Advisory —
/// used by the editor + `bind_hotkey` to reject typos before they're stored.
pub fn target_is_valid(line: &str) -> bool {
    let Some(verb) = line.split_whitespace().next() else {
        return false;
    };
    // Verbs handled directly in App::execute_line (not in the registry or the
    // app-verb classifier): clipboard, hotkeys, window layout, etc.
    const EXECUTE_LINE_VERBS: &[&str] = &[
        "copyselection",
        "pasteselection",
        "cut",
        "clear",
        "cls",
        "hotkeys",
        "keybindings",
        "ortho",
    ];
    if EXECUTE_LINE_VERBS.contains(&verb) {
        return true;
    }
    if itsjustcad_commands::registry().iter().any(|s| s.name == verb) {
        return true;
    }
    crate::app_verbs::classify(line).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::KeyContext;

    #[test]
    fn combo_round_trips_through_egui() {
        let c = KeyCombo::from_egui(Key::K, Modifiers::COMMAND | Modifiers::SHIFT);
        assert_eq!(c.key, "K");
        assert!(c.cmd && c.shift && !c.alt);
        assert!(c.matches(Key::K, Modifiers::COMMAND | Modifiers::SHIFT));
        // Exact modifiers: Cmd+Shift+K must not fire on plain Cmd+K.
        assert!(!c.matches(Key::K, Modifiers::COMMAND));
    }

    #[test]
    fn combo_parses_and_displays() {
        let c = KeyCombo::parse("Cmd+Shift+K").unwrap();
        assert_eq!(c, KeyCombo { cmd: true, shift: true, alt: false, key: "K".into() });
        assert_eq!(c.display(), "Cmd+Shift+K");
        // Aliases + whitespace + a function key.
        assert_eq!(KeyCombo::parse("ctrl + alt + F8").unwrap().display(), "Cmd+Alt+F8");
        // Bare key.
        assert_eq!(KeyCombo::parse("Delete").unwrap().display(), "Delete");
        // Unknown key token → None.
        assert!(KeyCombo::parse("Cmd+Nope").is_none());
    }

    #[test]
    fn parse_is_case_insensitive_on_the_key() {
        // Letters and function keys, any case, normalize to egui's canonical name.
        assert_eq!(KeyCombo::parse("cmd+j").unwrap().display(), "Cmd+J");
        assert_eq!(KeyCombo::parse("CMD+J").unwrap().display(), "Cmd+J");
        assert_eq!(KeyCombo::parse("f8").unwrap().display(), "F8");
        // Named keys title-case.
        assert_eq!(KeyCombo::parse("delete").unwrap().display(), "Delete");
        assert_eq!(KeyCombo::parse("cmd+up").unwrap().display(), "Cmd+Up");
    }

    #[test]
    fn reserved_chords_are_flagged() {
        assert!(KeyCombo::parse("Cmd+Q").unwrap().is_reserved());
        assert!(KeyCombo::parse("Cmd+W").unwrap().is_reserved());
        assert!(!KeyCombo::parse("Cmd+Shift+Q").unwrap().is_reserved());
        assert!(!KeyCombo::parse("Cmd+K").unwrap().is_reserved());
    }

    #[test]
    fn set_replaces_and_remove_works() {
        let mut f = KeybindingsFile::default();
        let combo = KeyCombo::parse("Cmd+K").unwrap();
        assert!(!f.set(combo.clone(), "select all".into()), "first set is new");
        assert!(f.set(combo.clone(), "selectnone".into()), "second set overwrites");
        assert_eq!(f.bindings.len(), 1);
        assert_eq!(f.lookup(Key::K, Modifiers::COMMAND), Some("selectnone"));
        assert!(f.remove(&combo));
        assert!(f.lookup(Key::K, Modifiers::COMMAND).is_none());
    }

    #[test]
    fn resolve_prefers_user_then_falls_back() {
        let mut user = KeybindingsFile::default();
        user.set(KeyCombo::parse("Cmd+K").unwrap(), "select all".into());
        let ctx = KeyContext::default();
        // User binding wins.
        assert_eq!(
            resolve(Key::K, Modifiers::COMMAND, ctx, &user).as_deref(),
            Some("select all")
        );
        // Unbound chord falls through to the built-in keymap (Cmd+S = save).
        assert_eq!(
            resolve(Key::S, Modifiers::COMMAND, ctx, &user).as_deref(),
            Some("save")
        );
        // While typing, user bindings are suppressed (and so is the built-in).
        let typing = KeyContext { typing: true, ..ctx };
        assert_eq!(resolve(Key::K, Modifiers::COMMAND, typing, &user), None);
    }

    #[test]
    fn resolve_suppresses_user_binding_mid_draw() {
        let mut user = KeybindingsFile::default();
        user.set(KeyCombo::parse("Cmd+K").unwrap(), "select all".into());
        let drawing = KeyContext { draw_active: true, ..KeyContext::default() };
        assert_eq!(resolve(Key::K, Modifiers::COMMAND, drawing, &user), None);
    }

    #[test]
    fn target_validation() {
        // Registry geometry verb.
        assert!(target_is_valid("box 0,0,0 1,1,1"));
        // App verb.
        assert!(target_is_valid("ze"));
        assert!(target_is_valid("display shaded"));
        // execute_line-only verb.
        assert!(target_is_valid("copyselection"));
        // Garbage.
        assert!(!target_is_valid("definitely_not_a_verb"));
        assert!(!target_is_valid(""));
    }
}
