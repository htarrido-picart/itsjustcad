// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Right-click Cut / Copy / Paste / Select All for the app's text inputs.
//!
//! egui 0.35 `TextEdit` has no built-in context menu, so we attach one to the
//! widget response. Copy/Cut are selection-aware (read the stored
//! [`egui::text_edit::TextEditState`] cursor range); Paste reads the OS
//! clipboard via `arboard` and inserts at the caret, replacing any selection.
//! Keyboard ⌘C/⌘V/⌘X are handled natively by egui — this only adds the
//! discoverable menu the user asked for.

use egui::text::{CCursor, CCursorRange};

/// Byte offset of the `char_idx`-th char in `s` (or `s.len()` past the end).
fn byte_of_char(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

/// Replace the chars in `[cstart, cend)` of `text` with `ins` (char indices).
fn replace_char_range(text: &mut String, cstart: usize, cend: usize, ins: &str) {
    let b0 = byte_of_char(text, cstart);
    let b1 = byte_of_char(text, cend);
    text.replace_range(b0..b1, ins);
}

/// The substring of `text` in char range `[cstart, cend)`.
fn slice_chars(text: &str, cstart: usize, cend: usize) -> String {
    text.chars().skip(cstart).take(cend.saturating_sub(cstart)).collect()
}

/// Read the OS clipboard as text (best-effort; `None` if unavailable/empty).
fn read_clipboard() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

/// Attach the Cut/Copy/Paste/Select All context menu to a text input.
/// `id` MUST be the `TextEdit`'s id (so the stored selection can be read/set).
pub fn attach(response: &egui::Response, text: &mut String, id: egui::Id) {
    response.context_menu(|ui| {
        let ctx = ui.ctx().clone();
        let mut state = egui::TextEdit::load_state(&ctx, id);
        // Current selection as sorted char indices (start == end → no selection).
        let sel: Option<(usize, usize)> = state
            .as_ref()
            .and_then(|s| s.cursor.char_range())
            .map(|r| {
                let cr = r.as_sorted_char_range();
                (cr.start.0, cr.end.0)
            });
        let has_sel = sel.is_some_and(|(a, b)| a != b);

        let set_range = |state: &mut Option<egui::text_edit::TextEditState>,
                         ctx: &egui::Context,
                         range: CCursorRange| {
            if let Some(st) = state.as_mut() {
                st.cursor.set_char_range(Some(range));
                egui::TextEdit::store_state(ctx, id, st.clone());
            }
        };

        if ui.add_enabled(has_sel, egui::Button::new("Cut")).clicked() {
            if let Some((a, b)) = sel {
                ctx.copy_text(slice_chars(text, a, b));
                replace_char_range(text, a, b, "");
                set_range(&mut state, &ctx, CCursorRange::one(CCursor::new(a)));
            }
            ui.close();
        }
        if ui.add_enabled(has_sel, egui::Button::new("Copy")).clicked() {
            if let Some((a, b)) = sel {
                ctx.copy_text(slice_chars(text, a, b));
            }
            ui.close();
        }
        if ui.button("Paste").clicked() {
            if let Some(clip) = read_clipboard() {
                let (a, b) = sel.unwrap_or_else(|| {
                    let n = text.chars().count();
                    (n, n)
                });
                replace_char_range(text, a, b, &clip);
                let caret = a + clip.chars().count();
                set_range(&mut state, &ctx, CCursorRange::one(CCursor::new(caret)));
            }
            ui.close();
        }
        ui.separator();
        if ui.button("Select All").clicked() {
            let n = text.chars().count();
            set_range(
                &mut state,
                &ctx,
                CCursorRange::two(CCursor::new(0), CCursor::new(n)),
            );
            ui.close();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_offsets_handle_multibyte() {
        // "áé" — each char is 2 bytes in UTF-8.
        assert_eq!(byte_of_char("áé", 0), 0);
        assert_eq!(byte_of_char("áé", 1), 2);
        assert_eq!(byte_of_char("áé", 2), 4);
        // Past the end clamps to len.
        assert_eq!(byte_of_char("abc", 9), 3);
    }

    #[test]
    fn replace_range_cut_and_paste_are_char_correct() {
        let mut s = "hello world".to_string();
        // Cut "world" (chars 6..11).
        replace_char_range(&mut s, 6, 11, "");
        assert_eq!(s, "hello ");
        // Paste "café" at the end (chars 6..6).
        replace_char_range(&mut s, 6, 6, "café");
        assert_eq!(s, "hello café");
        // Replace a multibyte selection ("café" is chars 6..10) with "tea".
        replace_char_range(&mut s, 6, 10, "tea");
        assert_eq!(s, "hello tea");
    }

    #[test]
    fn slice_chars_extracts_selection() {
        assert_eq!(slice_chars("hello world", 6, 11), "world");
        assert_eq!(slice_chars("áéíóú", 1, 4), "éíó");
        assert_eq!(slice_chars("abc", 2, 2), "");
    }
}
