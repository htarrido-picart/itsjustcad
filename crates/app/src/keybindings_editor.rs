// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! The "LLM ▸ Keybindings…" editor (W4).
//!
//! A modeless OS window (same `show_viewport_immediate` pattern as Model Setup
//! / API Keys) that lists the user's hotkeys, captures a new chord with a
//! "Press keys…" button, validates the target verb, warns on conflicts, and
//! persists to `keybindings.json`. It also exposes the assistant opt-in and a
//! reset-to-defaults escape hatch. It operates on a borrowed [`KeybindingsFile`]
//! and saves through it on every change.

use crate::keybindings::{KeyCombo, KeybindingsFile};

/// UI-only state for the editor window (the bindings themselves live on the app).
#[derive(Default)]
pub(crate) struct KeybindingsEditor {
    /// Verb text for the pending new binding (with autocomplete).
    new_verb: String,
    /// Key chord typed literally, e.g. "Cmd+J" / "cmd+shift+k" / "f8".
    key_input: String,
    /// Last action feedback (saved / conflict / rejected).
    status: Option<String>,
}

impl KeybindingsEditor {
    /// Render the window when `*show` is true. Mutates + saves `bindings` on any
    /// change; flips `*show` to false when the window is closed.
    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        show: &mut bool,
        bindings: &mut KeybindingsFile,
    ) {
        if !*show {
            return;
        }
        let mut wants_close = false;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("keybindings_window"),
            egui::ViewportBuilder::default()
                .with_title("Keybindings")
                .with_inner_size([520.0, 480.0])
                .with_min_inner_size([460.0, 360.0])
                .with_resizable(true),
            |vctx, _class| {
                egui::CentralPanel::default().show(vctx, |ui| {
                    ui.label(egui::RichText::new("Keybindings").strong().size(18.0));
                    ui.label(
                        egui::RichText::new(
                            "Bind a key chord to any command. User bindings override the \
                             built-in defaults; they don't fire while typing or mid-draw.",
                        )
                        .weak(),
                    );
                    ui.separator();

                    // ── Add a binding: 1) pick the command, 2) type the key ──

                    // 1) COMMAND — free text with live autocomplete from the same
                    // catalog the ⌘K palette uses (registry + app verbs). Prefix
                    // matches rank first; clicking fills the field (trailing space
                    // when the verb takes arguments).
                    ui.label(egui::RichText::new("Command").strong());
                    ui.add(
                        egui::TextEdit::singleline(&mut self.new_verb)
                            .hint_text("verb, e.g. select all")
                            .desired_width(f32::INFINITY),
                    );
                    let typed = self.new_verb.trim().to_ascii_lowercase();
                    let token = typed.split_whitespace().next().unwrap_or("");
                    if !token.is_empty() {
                        let mut matches: Vec<(String, String, bool)> = crate::palette::entries()
                            .into_iter()
                            .filter(|e| e.name.to_ascii_lowercase().contains(token))
                            .map(|e| (e.name, e.category, !e.usage.is_empty()))
                            .collect();
                        matches.sort_by_key(|(n, _, _)| !n.to_ascii_lowercase().starts_with(token));
                        let exact =
                            matches.len() == 1 && matches[0].0.eq_ignore_ascii_case(&typed);
                        if !exact && !matches.is_empty() {
                            egui::ScrollArea::vertical()
                                .id_salt("verb_suggest")
                                .max_height(110.0)
                                .show(ui, |ui| {
                                    for (name, group, has_args) in matches.into_iter().take(8) {
                                        let resp = ui.selectable_label(
                                            false,
                                            egui::RichText::new(format!("{name}   ·  {group}")),
                                        );
                                        if resp.clicked() {
                                            self.new_verb =
                                                if has_args { format!("{name} ") } else { name };
                                        }
                                    }
                                });
                        }
                    }

                    ui.add_space(6.0);

                    // 2) KEY — focus the field and press the chord: a chord with a
                    // Cmd/Alt modifier is CAPTURED straight into the field (those
                    // produce no text event, so a plain TextEdit would stay empty).
                    // Bare keys can still be typed literally in any case ("f8",
                    // "delete"). Shows a live parse preview, then Add.
                    ui.label(egui::RichText::new("Key").strong());
                    ui.horizontal(|ui| {
                        let field_id = ui.id().with("key_input_field");
                        // Capture BEFORE the TextEdit runs, so e.g. Cmd+A binds the
                        // chord instead of select-all-ing the field text. Only
                        // modifier chords are intercepted + consumed.
                        if ui.memory(|m| m.has_focus(field_id)) {
                            let captured = ui.input_mut(|i| {
                                let hit = i.events.iter().find_map(|e| match e {
                                    egui::Event::Key {
                                        key,
                                        pressed: true,
                                        modifiers,
                                        ..
                                    } if modifiers.command || modifiers.alt => {
                                        Some(KeyCombo::from_egui(*key, *modifiers))
                                    }
                                    _ => None,
                                });
                                if hit.is_some() {
                                    i.events.retain(|e| {
                                        !matches!(
                                            e,
                                            egui::Event::Key { modifiers, pressed: true, .. }
                                                if modifiers.command || modifiers.alt
                                        )
                                    });
                                }
                                hit
                            });
                            if let Some(c) = captured {
                                self.key_input = c.display();
                            }
                            ui.ctx().request_repaint();
                        }
                        ui.add(
                            egui::TextEdit::singleline(&mut self.key_input)
                                .id(field_id)
                                .hint_text("focus, then press Cmd+…  (or type f8)")
                                .desired_width(200.0),
                        );
                        let parsed = KeyCombo::parse(&self.key_input);
                        if let Some(c) = &parsed {
                            ui.label(egui::RichText::new(format!("→ {}", c.display())).weak());
                        } else if !self.key_input.trim().is_empty() {
                            ui.label(egui::RichText::new("unrecognized key").weak().italics());
                        }
                        let ready = !self.new_verb.trim().is_empty() && parsed.is_some();
                        if ui.add_enabled(ready, egui::Button::new("Add")).clicked() {
                            self.try_add(bindings);
                        }
                    });

                    if let Some(s) = &self.status {
                        ui.label(egui::RichText::new(s).weak().italics());
                    }

                    ui.add_space(8.0);
                    ui.separator();

                    // ── Current user bindings ────────────────────────────────
                    ui.label(egui::RichText::new("Your bindings").strong());
                    if bindings.bindings.is_empty() {
                        ui.label(egui::RichText::new("None yet.").weak());
                    } else {
                        let mut remove: Option<KeyCombo> = None;
                        egui::ScrollArea::vertical()
                            .max_height(200.0)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                for b in &bindings.bindings {
                                    ui.horizontal(|ui| {
                                        ui.monospace(b.combo.display());
                                        ui.label("→");
                                        ui.label(&b.verb);
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                if ui.small_button("Remove").clicked() {
                                                    remove = Some(b.combo.clone());
                                                }
                                            },
                                        );
                                    });
                                }
                            });
                        if let Some(combo) = remove {
                            bindings.remove(&combo);
                            bindings.save();
                            self.status = Some(format!("Removed {combo}."));
                        }
                    }

                    ui.add_space(8.0);
                    ui.separator();

                    // ── Assistant opt-in + reset ─────────────────────────────
                    let mut allow = bindings.allow_assistant;
                    if ui
                        .checkbox(&mut allow, "Let the assistant set hotkeys")
                        .on_hover_text(
                            "When on, the chat assistant may run bind_hotkey. Off by default \
                             so a prompt injection can't remap your keys.",
                        )
                        .changed()
                    {
                        bindings.allow_assistant = allow;
                        bindings.save();
                    }
                    if !bindings.bindings.is_empty()
                        && ui
                            .button("Reset to defaults")
                            .on_hover_text("Remove all user bindings (built-in keys stay).")
                            .clicked()
                    {
                        bindings.bindings.clear();
                        bindings.save();
                        self.status = Some("Reset — all user bindings removed.".to_string());
                    }
                });

                if vctx.input(|i| i.viewport().close_requested()) {
                    wants_close = true;
                }
            },
        );

        if wants_close {
            *show = false;
        }
    }

    /// Validate + commit the pending (typed key, verb) pair.
    fn try_add(&mut self, bindings: &mut KeybindingsFile) {
        let Some(combo) = KeyCombo::parse(&self.key_input) else {
            self.status = Some("Type a valid key chord (e.g. Cmd+J).".to_string());
            return;
        };
        let verb = self.new_verb.trim().to_string();
        if combo.is_reserved() {
            self.status = Some(format!("{combo} is reserved by the OS — pick another chord."));
            return;
        }
        if !crate::keybindings::target_is_valid(&verb) {
            self.status = Some(format!("“{verb}” isn't a known verb — not bound."));
            return;
        }
        let overwrote = bindings.set(combo.clone(), verb.clone());
        bindings.save();
        self.key_input.clear();
        self.new_verb.clear();
        self.status = Some(if overwrote {
            format!("Rebound {combo} → {verb}.")
        } else {
            format!("Bound {combo} → {verb}.")
        });
    }
}
