// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! The alpha feedback review sheet (GitHub issue #123), opened by the
//! `feedback` verb (command line or deck) and Help ▸ Report a problem…. The
//! user edits the draft and picks what to include; NOTHING is written, sent or
//! opened until a button is clicked. Bundle building lives in
//! `itsjustcad_deck::feedback`; this module is only the window + the file write.

use itsjustcad_deck::{
    ChatMessage, FeedbackCategory, FeedbackDraft, MAX_DOC_BYTES, Role, last_turns,
};

use crate::i18n::t;
use crate::theme::ColorRoles;
use crate::widgets::{dialog_body, dialog_text};

/// What the user clicked this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SheetAction {
    Send,
    GitHub,
    Cancel,
}

pub struct FeedbackSheet {
    pub draft: FeedbackDraft,
    /// Attach the document (compact JSON) to the bundle. On by default.
    pub attach_doc: bool,
    /// Include the last chat turns in the bundle. On by default.
    pub include_chat: bool,
    /// Compact-JSON size of the document when the sheet opened (cap notice).
    doc_bytes: usize,
}

impl FeedbackSheet {
    pub fn new(draft: FeedbackDraft, doc_bytes: usize) -> Self {
        Self {
            draft,
            attach_doc: true,
            include_chat: true,
            doc_bytes,
        }
    }

    /// Draw the sheet; returns the button clicked (closing the window = Cancel).
    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        roles: &ColorRoles,
        chat: &[ChatMessage],
    ) -> Option<SheetAction> {
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
                    dialog_text(ui, roles, t("feedback.field_title"));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.draft.title)
                            .desired_width(f32::INFINITY),
                    );
                    dialog_text(ui, roles, t("feedback.description"));
                    ui.add(
                        egui::TextEdit::multiline(&mut self.draft.description)
                            .desired_rows(4)
                            .desired_width(f32::INFINITY),
                    );
                    dialog_text(ui, roles, t("feedback.steps"));
                    ui.add(
                        egui::TextEdit::multiline(&mut self.draft.steps)
                            .desired_rows(3)
                            .desired_width(f32::INFINITY),
                    );
                    ui.add_space(crate::theme::Spacing::S);
                    ui.checkbox(&mut self.attach_doc, t("feedback.attach_doc"));
                    if self.attach_doc && self.doc_bytes > MAX_DOC_BYTES {
                        let mb = format!("{:.1}", self.doc_bytes as f64 / (1024.0 * 1024.0));
                        dialog_text(ui, roles, &t("feedback.doc_too_big").replace("{0}", &mb));
                    }
                    ui.checkbox(&mut self.include_chat, t("feedback.include_chat"));
                    if self.include_chat {
                        let turns = last_turns(chat);
                        if turns.is_empty() {
                            dialog_text(ui, roles, t("feedback.no_chat"));
                        } else {
                            egui::ScrollArea::vertical()
                                .max_height(160.0)
                                .show(ui, |ui| {
                                    for m in turns {
                                        let who = match m.role {
                                            Role::User => t("feedback.you"),
                                            Role::Assistant => t("feedback.deck"),
                                        };
                                        ui.label(format!("{who}: {}", m.content));
                                    }
                                });
                        }
                    }
                    ui.add_space(crate::theme::Spacing::S);
                    dialog_text(ui, roles, t("feedback.privacy"));
                    ui.add_space(crate::theme::Spacing::M);
                    ui.horizontal(|ui| {
                        if ui.button(t("feedback.send")).clicked() {
                            action = Some(SheetAction::Send);
                        }
                        if ui.button(t("feedback.github")).clicked() {
                            action = Some(SheetAction::GitHub);
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

/// Write the bundle to `~/Downloads`, falling back to `<config>/feedback/`.
/// Returns the written path.
pub fn write_bundle(name: &str, bundle: &str) -> std::io::Result<std::path::PathBuf> {
    let mut last = std::io::Error::other("no Downloads or config folder");
    for dir in [
        dirs::download_dir(),
        crate::config_dir().map(|d| d.join("feedback")),
    ]
    .into_iter()
    .flatten()
    {
        let path = dir.join(name);
        match std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, bundle)) {
            Ok(()) => return Ok(path),
            Err(e) => last = e,
        }
    }
    Err(last)
}
