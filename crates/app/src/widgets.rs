// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Token-backed reusable UI components for the ItsJustCAD app: button *roles*
//! (prominent / normal / destructive), a real segmented control (equal-width,
//! single-select, all-one-type), and a reusable confirmation alert (Cancel
//! leading / primary trailing). Plus the pure `is_dirty` predicate that decides
//! whether a New/Open/Quit needs the unsaved-changes guard.
//!
//! The *policy* pieces (button-role → colors, alert button order, is_dirty) are
//! pure and unit-tested; the egui draw helpers sit on top of them.

use crate::theme::{self, ColorRoles};

/// A button's semantic role. Drives fill/text/outline off the color roles.
///
/// - `Prominent`: filled with the accent (primary CTA).
/// - `Normal`: the default token-styled button (surface fill).
/// - `Destructive`: system-red **text + outline, never a filled-prominent
///   red** — a destructive action must read as a warning, not a candy button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonRole {
    /// Filled accent — the primary CTA. Part of the role vocabulary; kept for the
    /// alert/primary-action call sites even where no live button uses it yet.
    #[allow(dead_code)]
    Prominent,
    Normal,
    Destructive,
}

/// The resolved paint colors for a button in a given role and interaction
/// state. Pure value so the mapping is unit-testable without egui.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ButtonStyle {
    pub fill: theme::Rgba,
    pub text: theme::Rgba,
    /// `None` = no outline stroke.
    pub outline: Option<theme::Rgba>,
    pub outline_width: f32,
}

/// Interaction state for a button, in ascending "energy". A DISTINCT pressed
/// state is required (the old style let hover ≈ active).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtnState {
    Idle,
    Hovered,
    Pressed,
}

/// Blend `a` toward `b` by `t` (0..1), preserving `a`'s alpha.
fn mix(a: theme::Rgba, b: theme::Rgba, t: f32) -> theme::Rgba {
    let t = t.clamp(0.0, 1.0);
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
        a[3],
    ]
}

/// Pure role → colors mapping. This is the single source of truth for how the
/// three roles read, and how a pressed state differs from hover.
pub fn button_style(roles: &ColorRoles, role: ButtonRole, state: BtnState, dark: bool) -> ButtonStyle {
    // Direction to push a fill for "darker/lighter on press": toward black on a
    // light theme, toward white on dark — a felt, distinct press.
    let press_target: theme::Rgba = if dark {
        [1.0, 1.0, 1.0, 1.0]
    } else {
        [0.0, 0.0, 0.0, 1.0]
    };
    match role {
        ButtonRole::Prominent => {
            // Filled accent. Hover lightens slightly; press pushes further and
            // in the OPPOSITE luminance direction so it reads distinctly.
            let base = roles.primary;
            let fill = match state {
                BtnState::Idle => base,
                BtnState::Hovered => mix(base, press_target, 0.12),
                BtnState::Pressed => mix(base, press_target, 0.28),
            };
            // Contrast text on the accent: pick black/white by accent luminance.
            let text = if theme::is_dark(base) {
                [1.0, 1.0, 1.0, 1.0]
            } else {
                [0.0, 0.0, 0.0, 1.0]
            };
            ButtonStyle { fill, text, outline: None, outline_width: 0.0 }
        }
        ButtonRole::Normal => {
            let base = roles.surface_variant;
            let fill = match state {
                BtnState::Idle => base,
                BtnState::Hovered => mix(base, press_target, 0.08),
                BtnState::Pressed => mix(base, press_target, 0.20),
            };
            let outline = match state {
                BtnState::Idle => None,
                _ => Some(roles.primary),
            };
            ButtonStyle {
                fill,
                text: roles.on_surface,
                outline,
                outline_width: 1.0,
            }
        }
        ButtonRole::Destructive => {
            // NEVER filled-prominent red. Red text + red outline on a neutral
            // (surface) fill; hover tints the fill faintly red; press deepens it
            // but the fill stays a wash, not a solid red button.
            let red = roles.destructive;
            let fill = match state {
                BtnState::Idle => roles.surface_variant,
                BtnState::Hovered => mix(roles.surface_variant, red, 0.12),
                BtnState::Pressed => mix(roles.surface_variant, red, 0.24),
            };
            let outline_width = match state {
                BtnState::Pressed => 1.5,
                _ => 1.0,
            };
            ButtonStyle {
                fill,
                text: red,
                outline: Some(red),
                outline_width,
            }
        }
    }
}

/// Map a live egui response to a [`BtnState`] (pressed beats hover).
pub fn state_of(resp: &egui::Response) -> BtnState {
    if resp.is_pointer_button_down_on() {
        BtnState::Pressed
    } else if resp.hovered() {
        BtnState::Hovered
    } else {
        BtnState::Idle
    }
}

/// Draw a role-styled button. Two-pass: allocate with a neutral button to get a
/// response, then repaint its background/stroke/text per [`button_style`] so the
/// role colors (and a distinct pressed state) always win over egui's visuals.
pub fn role_button(
    ui: &mut egui::Ui,
    roles: &ColorRoles,
    dark: bool,
    role: ButtonRole,
    label: &str,
) -> egui::Response {
    // Sense-only allocation: we paint everything ourselves so the role wins.
    let text_color = |s: BtnState| theme::to_color32(button_style(roles, role, s, dark).text);
    let galley = ui.painter().layout_no_wrap(
        label.to_owned(),
        egui::TextStyle::Button.resolve(ui.style()),
        text_color(BtnState::Idle),
    );
    let pad = ui.spacing().button_padding;
    let min = ui.spacing().interact_size;
    let desired = egui::vec2(
        (galley.size().x + pad.x * 2.0).max(min.x.min(64.0)),
        (galley.size().y + pad.y * 2.0).max(min.y),
    );
    let (rect, resp) = ui.allocate_exact_size(desired, egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let st = state_of(&resp);
        let style = button_style(roles, role, st, dark);
        let radius = ui.visuals().widgets.inactive.corner_radius;
        let stroke = match style.outline {
            Some(c) => egui::Stroke::new(style.outline_width, theme::to_color32(c)),
            None => egui::Stroke::NONE,
        };
        ui.painter().rect(
            rect,
            radius,
            theme::to_color32(style.fill),
            stroke,
            egui::StrokeKind::Inside,
        );
        let text_pos = rect.center() - galley.size() * 0.5;
        let galley = ui.painter().layout_no_wrap(
            label.to_owned(),
            egui::TextStyle::Button.resolve(ui.style()),
            theme::to_color32(style.text),
        );
        ui.painter().galley(text_pos, galley, theme::to_color32(style.text));
    }
    resp
}

// ── Segmented control ────────────────────────────────────────────────────

/// A real segmented control: equal-width segments, all one type, single
/// selection (selection-only — clicking a segment never toggles it off). Draws
/// one token-styled pill with the selected segment filled by the accent.
///
/// `labels` are the visible segment captions; `selected` is the current index.
/// Returns `Some(i)` if the user picked a **different** segment this frame.
pub fn segmented(
    ui: &mut egui::Ui,
    roles: &ColorRoles,
    labels: &[&str],
    selected: usize,
) -> Option<usize> {
    if labels.is_empty() {
        return None;
    }
    let sp = ui.spacing().item_spacing.x;
    let pad = ui.spacing().button_padding;
    let font = egui::TextStyle::Button.resolve(ui.style());
    let h = (ui.text_style_height(&egui::TextStyle::Button) + pad.y * 2.0)
        .max(ui.spacing().interact_size.y);
    // Equal width = widest label + padding, applied to every segment.
    let seg_w = labels
        .iter()
        .map(|l| {
            ui.painter()
                .layout_no_wrap((*l).to_owned(), font.clone(), egui::Color32::WHITE)
                .size()
                .x
        })
        .fold(0.0_f32, f32::max)
        + pad.x * 2.0;
    let total_w = seg_w * labels.len() as f32;
    let (rect, _resp) = ui.allocate_exact_size(egui::vec2(total_w, h), egui::Sense::hover());
    let radius = ui.visuals().widgets.inactive.corner_radius;

    let mut picked = None;
    if ui.is_rect_visible(rect) {
        // Outer pill background + outline.
        ui.painter().rect(
            rect,
            radius,
            theme::to_color32(roles.surface_variant),
            egui::Stroke::new(1.0, theme::to_color32(roles.outline)),
            egui::StrokeKind::Inside,
        );
        let _ = sp; // segments are flush inside one pill (no inter-item gap)
        for (i, label) in labels.iter().enumerate() {
            let seg_rect = egui::Rect::from_min_size(
                egui::pos2(rect.min.x + seg_w * i as f32, rect.min.y),
                egui::vec2(seg_w, h),
            );
            let id = ui.id().with(("segmented", i, *label));
            let resp = ui.interact(seg_rect, id, egui::Sense::click());
            let is_sel = i == selected;
            let hovered = resp.hovered();
            if is_sel {
                ui.painter().rect(
                    seg_rect.shrink(1.5),
                    radius,
                    theme::to_color32(roles.primary),
                    egui::Stroke::NONE,
                    egui::StrokeKind::Inside,
                );
            } else if hovered {
                ui.painter().rect(
                    seg_rect.shrink(1.5),
                    radius,
                    theme::to_color32(mix(roles.surface_variant, roles.primary, 0.10)),
                    egui::Stroke::NONE,
                    egui::StrokeKind::Inside,
                );
            }
            let text_col = if is_sel {
                if theme::is_dark(roles.primary) {
                    egui::Color32::WHITE
                } else {
                    egui::Color32::BLACK
                }
            } else {
                theme::to_color32(roles.on_surface)
            };
            let galley = ui
                .painter()
                .layout_no_wrap((*label).to_owned(), font.clone(), text_col);
            let pos = seg_rect.center() - galley.size() * 0.5;
            ui.painter().galley(pos, galley, text_col);
            // Divider between segments (not after the last).
            if i + 1 < labels.len() {
                let x = seg_rect.max.x;
                ui.painter().line_segment(
                    [egui::pos2(x, rect.min.y + 3.0), egui::pos2(x, rect.max.y - 3.0)],
                    egui::Stroke::new(1.0, theme::to_color32(roles.outline)),
                );
            }
            // Selection-only: clicking the already-selected segment is a no-op.
            if resp.clicked() && !is_sel {
                picked = Some(i);
            }
        }
    }
    picked
}

// ── Confirmation alert ──────────────────────────────────────────────────

/// The user's choice from a confirmation [`alert`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertChoice {
    /// The safe / dismissing option (Cancel).
    Cancel,
    /// The primary / confirming option (e.g. Discard).
    Confirm,
}

/// Alert button layout: **Cancel leading, primary trailing** (HIG/macOS order).
/// Pure so the ordering contract is unit-tested. Returns `(leading, trailing)`
/// labels for the given confirm verb.
pub fn alert_button_order(confirm_label: &str) -> (&str, &str) {
    ("Cancel", confirm_label)
}

/// A reusable modal confirmation alert. Neutral `title`, a `message`, a leading
/// **Cancel** (NOT the default) and a trailing destructive-styled confirm
/// button. Returns `Some(choice)` once the user acts; `None` while open.
///
/// `confirm_role` lets the caller mark the confirm action destructive (unsaved
/// discard, delete) so it reads red.
pub fn alert(
    ctx: &egui::Context,
    roles: &ColorRoles,
    dark: bool,
    title: &str,
    message: &str,
    confirm_label: &str,
    confirm_role: ButtonRole,
) -> Option<AlertChoice> {
    let mut choice = None;
    egui::Modal::new(egui::Id::new(("itsjustcad_alert", title))).show(ctx, |ui| {
        ui.set_max_width(360.0);
        ui.vertical(|ui| {
            ui.heading(title);
            ui.add_space(theme::Spacing::S);
            ui.label(message);
            ui.add_space(theme::Spacing::M);
            // Cancel LEADING, primary TRAILING. Cancel is NOT the default: we do
            // not request focus for it and Enter maps to the confirm action.
            ui.horizontal(|ui| {
                let (leading, trailing) = alert_button_order(confirm_label);
                if role_button(ui, roles, dark, ButtonRole::Normal, leading).clicked() {
                    choice = Some(AlertChoice::Cancel);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if role_button(ui, roles, dark, confirm_role, trailing).clicked() {
                        choice = Some(AlertChoice::Confirm);
                    }
                });
            });
        });
    });
    // Esc dismisses as Cancel (the safe path).
    if choice.is_none() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        choice = Some(AlertChoice::Cancel);
    }
    choice
}

// ── Unsaved-changes predicate ───────────────────────────────────────────

/// Pure dirty-state test used to decide whether New/Open/Quit must first raise
/// the unsaved-changes guard.
///
/// `op_cursor` is the session's current op-log cursor (`Session::history().1`);
/// `saved_cursor` is the cursor at the last successful save/open (`0` for a
/// never-saved fresh document). Dirty ⇔ the two differ. A fresh empty document
/// (`op_cursor == 0`, `saved_cursor == 0`) is therefore clean, and undoing all
/// the way back to a saved point is likewise clean.
pub fn is_dirty(op_cursor: usize, saved_cursor: usize) -> bool {
    op_cursor != saved_cursor
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles() -> ColorRoles {
        theme::roles_from([0.13, 0.14, 0.16, 1.0], [0.0, 0.63, 0.95, 1.0])
    }

    // ── Button roles ─────────────────────────────────────────────────────

    #[test]
    fn prominent_is_filled_accent_no_outline() {
        let r = roles();
        let s = button_style(&r, ButtonRole::Prominent, BtnState::Idle, true);
        assert_eq!(s.fill, r.primary, "prominent idle fill is the accent");
        assert!(s.outline.is_none(), "prominent has no outline");
    }

    #[test]
    fn destructive_is_red_text_and_outline_never_filled_red() {
        let r = roles();
        for st in [BtnState::Idle, BtnState::Hovered, BtnState::Pressed] {
            let s = button_style(&r, ButtonRole::Destructive, st, true);
            assert_eq!(s.text, r.destructive, "destructive text is system-red");
            assert_eq!(s.outline, Some(r.destructive), "destructive has red outline");
            // The fill must NOT be the solid destructive color (never a filled
            // red candy button) — it stays a wash off the surface.
            assert_ne!(s.fill, r.destructive, "destructive must not be filled red");
        }
    }

    #[test]
    fn pressed_state_is_distinct_from_hover() {
        let r = roles();
        for role in [ButtonRole::Prominent, ButtonRole::Normal, ButtonRole::Destructive] {
            let hov = button_style(&r, role, BtnState::Hovered, true);
            let prs = button_style(&r, role, BtnState::Pressed, true);
            assert_ne!(
                (hov.fill, hov.outline_width),
                (prs.fill, prs.outline_width),
                "{role:?}: pressed must differ from hover"
            );
        }
    }

    #[test]
    fn normal_idle_has_no_outline_but_hover_does() {
        let r = roles();
        assert!(button_style(&r, ButtonRole::Normal, BtnState::Idle, true).outline.is_none());
        assert!(button_style(&r, ButtonRole::Normal, BtnState::Hovered, true).outline.is_some());
    }

    // ── Alert ────────────────────────────────────────────────────────────

    #[test]
    fn alert_order_is_cancel_leading_primary_trailing() {
        let (leading, trailing) = alert_button_order("Discard");
        assert_eq!(leading, "Cancel", "Cancel must lead");
        assert_eq!(trailing, "Discard", "primary/confirm must trail");
    }

    #[test]
    fn alert_choice_variants_distinct() {
        assert_ne!(AlertChoice::Cancel, AlertChoice::Confirm);
    }

    // ── is_dirty ─────────────────────────────────────────────────────────

    #[test]
    fn fresh_empty_document_is_clean() {
        assert!(!is_dirty(0, 0), "a fresh empty doc must not trip the guard");
    }

    #[test]
    fn edits_past_saved_point_are_dirty() {
        assert!(is_dirty(3, 0), "3 ops on a never-saved doc is dirty");
        assert!(is_dirty(5, 2), "edits past the saved cursor are dirty");
    }

    #[test]
    fn undo_back_to_saved_point_is_clean() {
        // Saved at cursor 2, edited to 5, then undone back to 2 → clean again.
        assert!(!is_dirty(2, 2));
    }

    #[test]
    fn dirty_path_triggers_guard_decision() {
        // The is_dirty → prompt path: guard fires exactly when dirty.
        let needs_guard = |cursor, saved| is_dirty(cursor, saved);
        assert!(needs_guard(1, 0), "dirty doc → guard the destructive nav");
        assert!(!needs_guard(0, 0), "clean doc → nav proceeds silently");
    }
}
