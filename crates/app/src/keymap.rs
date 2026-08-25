// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

use egui::{Key, Modifiers};

/// App state the shortcut resolver needs; kept as plain data so the keymap
/// stays a pure, unit-testable function.
#[derive(Clone, Copy, Default)]
pub struct KeyContext<'a> {
    /// A text field owns the keyboard — shortcuts must never fire.
    pub typing: bool,
    /// The interactive draw tool is mid-pick; it owns Esc/Enter and single
    /// letters must not start another tool underneath it.
    pub draw_active: bool,
    pub has_selection: bool,
    /// Last executed command line, repeated by Enter/Space (Rhino habit).
    pub last_command: Option<&'a str>,
}

/// Pure keymap: key + modifiers + context -> command line for `execute_line`.
/// Every shortcut resolves to a command string so the op-log substrate stays
/// the single source of truth ("copyselection"/"pasteselection" are the
/// app-level clipboard verbs; the paste itself runs `copy sel ...`).
pub fn keymap(key: Key, mods: Modifiers, ctx: KeyContext<'_>) -> Option<String> {
    if ctx.typing {
        return None;
    }
    let cmd = mods.command && !mods.shift && !mods.alt;
    let cmd_shift = mods.command && mods.shift && !mods.alt;
    let bare = mods.is_none();
    let line: &str = match key {
        // Not while drawing: Backspace edits the numeric input buffer there.
        Key::Delete | Key::Backspace if bare && ctx.has_selection && !ctx.draw_active => {
            "delete sel"
        }
        Key::Z if cmd_shift => "redo",
        Key::Z if cmd => "undo",
        Key::A if cmd => "select all",
        Key::C if cmd && ctx.has_selection => "copyselection",
        Key::V if cmd => "pasteselection",
        Key::S if cmd => "save",
        // Draw tool handles Esc itself while picking; otherwise Esc deselects.
        Key::Escape if bare && !ctx.draw_active && ctx.has_selection => "selectnone",
        Key::Enter | Key::Space if bare && !ctx.draw_active => {
            return ctx.last_command.map(str::to_string);
        }
        Key::L if bare && !ctx.draw_active => "line",
        Key::C if bare && !ctx.draw_active => "circle",
        Key::P if bare && !ctx.draw_active => "polyline",
        Key::R if bare && !ctx.draw_active => "rect",
        // Rhino-style Gumball toggle: bare G flips gizmo visibility. Free key
        // (not a draw verb), works with or without a selection.
        Key::G if bare && !ctx.draw_active => "gumball",
        _ => return None,
    };
    Some(line.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CMD: Modifiers = Modifiers::COMMAND;
    const NONE: Modifiers = Modifiers::NONE;

    fn cmd_shift() -> Modifiers {
        Modifiers::COMMAND | Modifiers::SHIFT
    }

    fn ctx() -> KeyContext<'static> {
        KeyContext {
            typing: false,
            draw_active: false,
            has_selection: true,
            last_command: Some("box 0,0,0 1,1,1"),
        }
    }

    #[test]
    fn typing_suppresses_everything() {
        let c = KeyContext { typing: true, ..ctx() };
        for (key, mods) in [
            (Key::Delete, NONE),
            (Key::Z, CMD),
            (Key::A, CMD),
            (Key::L, NONE),
            (Key::Enter, NONE),
            (Key::S, CMD),
        ] {
            assert_eq!(keymap(key, mods, c), None, "{key:?} fired while typing");
        }
    }

    #[test]
    fn delete_and_backspace_delete_selection() {
        assert_eq!(keymap(Key::Delete, NONE, ctx()).unwrap(), "delete sel");
        assert_eq!(keymap(Key::Backspace, NONE, ctx()).unwrap(), "delete sel");
        // no selection -> no-op (avoids logging a failing op)
        let none = KeyContext { has_selection: false, ..ctx() };
        assert_eq!(keymap(Key::Delete, NONE, none), None);
        // modified delete is not ours
        assert_eq!(keymap(Key::Delete, CMD, ctx()), None);
        // while drawing, Backspace belongs to the numeric input buffer
        let drawing = KeyContext { draw_active: true, ..ctx() };
        assert_eq!(keymap(Key::Backspace, NONE, drawing), None);
        assert_eq!(keymap(Key::Delete, NONE, drawing), None);
    }

    #[test]
    fn undo_redo() {
        assert_eq!(keymap(Key::Z, CMD, ctx()).unwrap(), "undo");
        assert_eq!(keymap(Key::Z, cmd_shift(), ctx()).unwrap(), "redo");
        assert_eq!(keymap(Key::Z, NONE, ctx()), None);
    }

    #[test]
    fn select_all_and_escape_deselect() {
        assert_eq!(keymap(Key::A, CMD, ctx()).unwrap(), "select all");
        assert_eq!(keymap(Key::Escape, NONE, ctx()).unwrap(), "selectnone");
        // empty selection: Esc is a no-op, not a logged op
        let none = KeyContext { has_selection: false, ..ctx() };
        assert_eq!(keymap(Key::Escape, NONE, none), None);
        // draw tool owns Esc while picking
        let drawing = KeyContext { draw_active: true, ..ctx() };
        assert_eq!(keymap(Key::Escape, NONE, drawing), None);
    }

    #[test]
    fn copy_paste_clipboard_verbs() {
        assert_eq!(keymap(Key::C, CMD, ctx()).unwrap(), "copyselection");
        assert_eq!(keymap(Key::V, CMD, ctx()).unwrap(), "pasteselection");
        // copy needs a selection; paste reports "nothing to paste" app-side
        let none = KeyContext { has_selection: false, ..ctx() };
        assert_eq!(keymap(Key::C, CMD, none), None);
        assert_eq!(keymap(Key::V, CMD, none).unwrap(), "pasteselection");
    }

    #[test]
    fn save() {
        assert_eq!(keymap(Key::S, CMD, ctx()).unwrap(), "save");
        assert_eq!(keymap(Key::S, NONE, ctx()), None);
    }

    #[test]
    fn enter_and_space_repeat_last_command() {
        assert_eq!(keymap(Key::Enter, NONE, ctx()).unwrap(), "box 0,0,0 1,1,1");
        assert_eq!(keymap(Key::Space, NONE, ctx()).unwrap(), "box 0,0,0 1,1,1");
        // nothing run yet -> nothing to repeat
        let fresh = KeyContext { last_command: None, ..ctx() };
        assert_eq!(keymap(Key::Enter, NONE, fresh), None);
        // draw tool owns Enter while picking (polyline finish)
        let drawing = KeyContext { draw_active: true, ..ctx() };
        assert_eq!(keymap(Key::Enter, NONE, drawing), None);
    }

    #[test]
    fn single_letters_start_draw_verbs() {
        assert_eq!(keymap(Key::L, NONE, ctx()).unwrap(), "line");
        assert_eq!(keymap(Key::C, NONE, ctx()).unwrap(), "circle");
        assert_eq!(keymap(Key::P, NONE, ctx()).unwrap(), "polyline");
        assert_eq!(keymap(Key::R, NONE, ctx()).unwrap(), "rect");
        // not while a tool is already picking
        let drawing = KeyContext { draw_active: true, ..ctx() };
        for key in [Key::L, Key::C, Key::P, Key::R] {
            assert_eq!(keymap(key, NONE, drawing), None);
        }
        // shifted/modified letters are not shortcuts
        assert_eq!(keymap(Key::L, Modifiers::SHIFT, ctx()), None);
        assert_eq!(keymap(Key::P, CMD, ctx()), None);
    }

    #[test]
    fn cmd_shift_z_is_not_undo_and_cmd_c_beats_circle() {
        // guard-order regressions
        assert_ne!(keymap(Key::Z, cmd_shift(), ctx()).unwrap(), "undo");
        assert_eq!(keymap(Key::C, CMD, ctx()).unwrap(), "copyselection");
    }

    #[test]
    fn g_toggles_gumball_bare_only() {
        // Bare G flips the gizmo, with or without a selection.
        assert_eq!(keymap(Key::G, NONE, ctx()).unwrap(), "gumball");
        let none = KeyContext { has_selection: false, ..ctx() };
        assert_eq!(keymap(Key::G, NONE, none).unwrap(), "gumball");
        // Not while a draw tool owns the keyboard, not with modifiers.
        let drawing = KeyContext { draw_active: true, ..ctx() };
        assert_eq!(keymap(Key::G, NONE, drawing), None);
        assert_eq!(keymap(Key::G, CMD, ctx()), None);
        assert_eq!(keymap(Key::G, Modifiers::SHIFT, ctx()), None);
    }

    #[test]
    fn unmapped_keys_do_nothing() {
        assert_eq!(keymap(Key::Q, NONE, ctx()), None);
        assert_eq!(keymap(Key::B, CMD, ctx()), None);
        assert_eq!(keymap(Key::Num1, NONE, ctx()), None);
    }
}
