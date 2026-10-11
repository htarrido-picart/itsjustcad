// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Thin egui glue over the pure [`crate::usage_spec`] tokenizer (issue #28):
//! colour mapping + a `TextEdit.layouter` that syntax-highlights a live command
//! line, plus the shared `/`-palette row renderer. Kept deliberately small — all
//! classification logic is pure and tested in `usage_spec`; this only maps roles
//! to egui colours and lays out glyphs.

use crate::usage_spec::{self, TokenRole};

/// iOS-parity colours for each token role. Teal for numbers/points, orange for
/// selectors/`key:value`, accent for the verb, primary for plain text.
pub fn role_color(role: TokenRole, visuals: &egui::Visuals) -> egui::Color32 {
    match role {
        TokenRole::Verb => visuals.hyperlink_color, // accent
        TokenRole::Number => egui::Color32::from_rgb(0x2a, 0xa1, 0x98), // teal
        TokenRole::Selector => egui::Color32::from_rgb(0xd1, 0x7a, 0x22), // orange
        TokenRole::Plain => visuals.text_color(),
    }
}

/// Build an egui layout job that colours `text` by command-token role. The verb
/// is bold. Used as a `TextEdit.layouter` so the LIVE input is highlighted while
/// typing (not just the committed scrollback).
pub fn highlight_job(
    ui: &egui::Ui,
    text: &str,
    font_id: egui::FontId,
) -> egui::text::LayoutJob {
    let visuals = ui.visuals();
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    let spans = usage_spec::classify_tokens(text);
    let mut cursor = 0usize;
    for span in &spans {
        // Any whitespace/gap before this token keeps the plain colour.
        if span.start > cursor {
            append(&mut job, &text[cursor..span.start], visuals.text_color(), &font_id, false);
        }
        let color = role_color(span.role, visuals);
        let bold = span.role == TokenRole::Verb;
        append(&mut job, &text[span.start..span.end], color, &font_id, bold);
        cursor = span.end;
    }
    if cursor < text.len() {
        append(&mut job, &text[cursor..], visuals.text_color(), &font_id, false);
    }
    job
}

fn append(
    job: &mut egui::text::LayoutJob,
    text: &str,
    color: egui::Color32,
    font_id: &egui::FontId,
    _bold: bool,
) {
    job.append(
        text,
        0.0,
        egui::text::TextFormat {
            font_id: font_id.clone(),
            color,
            ..Default::default()
        },
    );
}

/// Render one committed command line (monospace) with the iOS `✓` prefix and
/// per-token syntax colours. Errors are rendered by the caller in RED instead.
pub fn highlighted_command_label(ui: &mut egui::Ui, line: &str) {
    let font = egui::FontId::monospace(12.0);
    let visuals = ui.visuals().clone();
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.label(
            egui::RichText::new("✓ ")
                .color(egui::Color32::from_rgb(0x3f, 0xb9, 0x50))
                .font(font.clone()),
        );
        let spans = usage_spec::classify_tokens(line);
        let mut cursor = 0usize;
        for span in &spans {
            if span.start > cursor {
                ui.label(egui::RichText::new(&line[cursor..span.start]).font(font.clone()));
            }
            let mut rt = egui::RichText::new(&line[span.start..span.end])
                .color(role_color(span.role, &visuals))
                .font(font.clone());
            if span.role == TokenRole::Verb {
                rt = rt.strong();
            }
            ui.label(rt);
            cursor = span.end;
        }
        if cursor < line.len() {
            ui.label(egui::RichText::new(&line[cursor..]).font(font.clone()));
        }
    });
}
