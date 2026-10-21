// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! The alpha feedback review sheet (GitHub issue #123), opened by the
//! `feedback` verb (command line or deck) and Help ▸ Report a problem…. The
//! user edits the draft; NOTHING is opened until a button is clicked, and then
//! only the pre-filled web form in the browser, where the user reviews and submits.
//! The ticket text is built in `itsjustcad_deck::feedback`; this is the window.

use itsjustcad_deck::{FREQ_OPTIONS, FeedbackCategory, FeedbackDraft, FieldKind};

use crate::i18n::t;
use crate::theme::ColorRoles;
use crate::widgets::{dialog_body, dialog_text};

/// What the user clicked this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SheetAction {
    /// Open the pre-filled web form (only possible when one is configured).
    Form,
    Cancel,
}

pub struct FeedbackSheet {
    pub draft: FeedbackDraft,
}

impl FeedbackSheet {
    pub fn new(draft: FeedbackDraft) -> Self {
        Self { draft }
    }

    /// Draw the sheet; returns the button clicked (closing the window = Cancel).
    pub fn ui(&mut self, ctx: &egui::Context, roles: &ColorRoles) -> Option<SheetAction> {
        let mut open = true;
        let mut action = None;
        egui::Window::new(t("feedback.title"))
            .collapsible(true)
            .resizable(true)
            .open(&mut open)
            .show(ctx, |ui| {
                dialog_body(ui, roles, 460.0, |ui| {
                    ui.horizontal(|ui| {
                        ui.selectable_value(
                            &mut self.draft.category,
                            FeedbackCategory::Bug,
                            t("feedback.bug"),
                        );
                        ui.selectable_value(
                            &mut self.draft.category,
                            FeedbackCategory::Idea,
                            t("feedback.idea"),
                        );
                    });
                    // The category's template drives the fields; values typed
                    // for the other category stay in the draft.
                    for f in self.draft.category.template() {
                        dialog_text(ui, roles, t(&format!("feedback.field.{}", f.key)));
                        let hint = t(&format!("feedback.hint.{}", f.key));
                        let value = self.draft.field_mut(f.key);
                        match f.kind {
                            FieldKind::Line => {
                                ui.add(
                                    egui::TextEdit::singleline(value)
                                        .id_salt(f.key)
                                        .hint_text(hint)
                                        .desired_width(f32::INFINITY),
                                );
                            }
                            FieldKind::Text | FieldKind::Steps => {
                                ui.add(
                                    egui::TextEdit::multiline(value)
                                        .id_salt(f.key)
                                        .hint_text(hint)
                                        .desired_rows(3)
                                        .desired_width(f32::INFINITY),
                                );
                            }
                            FieldKind::Freq => {
                                let shown = if value.is_empty() {
                                    hint.to_string()
                                } else {
                                    t(&format!("feedback.freq.{value}")).to_string()
                                };
                                egui::ComboBox::from_id_salt(f.key)
                                    .selected_text(shown)
                                    .show_ui(ui, |ui| {
                                        for o in FREQ_OPTIONS {
                                            ui.selectable_value(
                                                value,
                                                o.to_string(),
                                                t(&format!("feedback.freq.{o}")),
                                            );
                                        }
                                    });
                            }
                        }
                        // Steps read back as the numbered list that gets sent.
                        if f.kind == FieldKind::Steps {
                            for (i, step) in self.draft.steps().iter().enumerate() {
                                ui.weak(format!("{}. {step}", i + 1));
                            }
                        }
                    }
                    ui.add_space(crate::theme::Spacing::S);
                    dialog_text(ui, roles, t("feedback.browser_hint"));
                    if itsjustcad_deck::GOOGLE_FORM.is_none() {
                        dialog_text(ui, roles, t("feedback.form_missing"));
                    }
                    ui.add_space(crate::theme::Spacing::M);
                    ui.horizontal(|ui| {
                        let configured = itsjustcad_deck::GOOGLE_FORM.is_some();
                        let send = ui
                            .add_enabled(configured, egui::Button::new(t("feedback.form")))
                            .on_disabled_hover_text(t("feedback.form_missing"));
                        if send.clicked() {
                            action = Some(SheetAction::Form);
                        }
                        if ui.button(t("feedback.cancel")).clicked() {
                            action = Some(SheetAction::Cancel);
                        }
                    });
                });
            });
        if !open {
            action = Some(SheetAction::Cancel);
        }
        action
    }
}

#[cfg(test)]
mod tests {
    use crate::i18n::{Lang, lookup};

    /// Every template field (and freq option) has a sheet label and hint.
    #[test]
    fn every_template_field_is_localized() {
        let keys = itsjustcad_deck::BUG_TEMPLATE
            .iter()
            .chain(itsjustcad_deck::IDEA_TEMPLATE)
            .flat_map(|f| {
                [
                    format!("feedback.field.{}", f.key),
                    format!("feedback.hint.{}", f.key),
                ]
            })
            .chain(
                itsjustcad_deck::FREQ_OPTIONS
                    .iter()
                    .map(|o| format!("feedback.freq.{o}")),
            );
        for k in keys {
            assert!(lookup(Lang::En, &k).is_some(), "missing i18n key {k}");
        }
    }
}
