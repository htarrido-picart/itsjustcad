// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! The "LLM ▸ API Keys…" settings dialog (W2).
//!
//! A modeless OS window — the same `show_viewport_immediate` pattern as Model
//! Setup — that lets the user set/update either cloud provider's API key and
//! pick a model at any time, not just during onboarding. It owns a snapshot of
//! `decks.json` loaded when the window opens; every change is written straight
//! back (0600, via `DecksFile::save`). Keys are never logged and the entry
//! field is masked. When anything changes, `ui()` returns `true` so the app can
//! reload the live deck pane.

use itsjustcad_deck::{probe, CloudProvider, DeckConfig, DecksFile, ProbeInfo};
use tokio::sync::oneshot;

/// Per-provider probe state. `Checking` holds the in-flight `GET /models` task.
enum ApiProbe {
    Idle,
    Checking(oneshot::Receiver<Result<ProbeInfo, String>>),
}

/// One row of the dialog, bound to a single cloud provider.
struct ProviderRow {
    provider: CloudProvider,
    /// Masked buffer for a freshly pasted literal key (cleared after Save).
    key_input: String,
    probe: ApiProbe,
    /// Models reported by the last successful probe (drives the picker).
    models: Vec<String>,
    /// Last action/probe result, shown as a small caption.
    status: Option<String>,
}

impl ProviderRow {
    fn new(provider: CloudProvider) -> Self {
        Self {
            provider,
            key_input: String::new(),
            probe: ApiProbe::Idle,
            models: Vec::new(),
            status: None,
        }
    }

    /// Describe the current key and its source for the status line.
    fn key_status(decks: &DecksFile, provider: CloudProvider) -> String {
        match decks.provider_index(provider) {
            None => "No key set.".to_string(),
            Some(i) => match decks.decks[i].api_key.as_deref() {
                None => "No key set.".to_string(),
                Some(k) if k.starts_with("env:") => {
                    let var = &k[4..];
                    if std::env::var(var).is_ok() {
                        format!("Key set from environment (${var}).")
                    } else {
                        format!("Configured to read ${var}, but it is not set.")
                    }
                }
                Some(_) => "Key set (stored in decks.json).".to_string(),
            },
        }
    }

    /// Render the row. Returns `true` if it mutated (and saved) `decks`.
    fn ui(
        &mut self,
        ui: &mut egui::Ui,
        decks: &mut DecksFile,
        handle: &tokio::runtime::Handle,
    ) -> bool {
        let mut changed = false;
        let p = self.provider;

        ui.label(egui::RichText::new(p.label()).strong().size(15.0));
        ui.label(egui::RichText::new(Self::key_status(decks, p)).weak());

        // Current model (from the cassette, or the provider default if none).
        let cur_model = decks
            .provider_index(p)
            .map(|i| decks.decks[i].model.clone())
            .unwrap_or_else(|| p.default_model().to_string());

        // ── Key entry (masked) ────────────────────────────────────────────────
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.key_input)
                    .password(true)
                    .hint_text(format!("Paste {} API key", p.label()))
                    .desired_width(260.0),
            );
            let can_save = !self.key_input.trim().is_empty();
            if ui.add_enabled(can_save, egui::Button::new("Save key")).clicked() {
                decks.set_cloud_provider(p, Some(self.key_input.trim().to_string()), None);
                decks.save();
                self.key_input.clear();
                self.status = Some("Key saved to decks.json (0600).".to_string());
                changed = true;
            }
        });

        // Prefer env-var indirection when the variable is present — the key
        // never touches disk that way.
        if std::env::var(p.env_var()).is_ok()
            && ui
                .button(format!("Use environment variable ${}", p.env_var()))
                .on_hover_text("Reads the key from the environment instead of storing a literal.")
                .clicked()
        {
            decks.set_cloud_provider(p, Some(format!("env:{}", p.env_var())), None);
            decks.save();
            self.status = Some(format!("Now reading ${}.", p.env_var()));
            changed = true;
        }

        // ── Probe + model picker ──────────────────────────────────────────────
        ui.horizontal(|ui| {
            if ui.button("Probe models").clicked() {
                let cfg = DeckConfig {
                    name: p.cassette_name().to_string(),
                    kind: p.deck_kind(),
                    base_url: p.base_url().to_string(),
                    model: cur_model.clone(),
                    api_key: decks
                        .provider_index(p)
                        .and_then(|i| decks.decks[i].api_key.clone()),
                    grammar: false,
                    terse: None,
                };
                let (tx, rx) = oneshot::channel();
                self.probe = ApiProbe::Checking(rx);
                self.status = Some("Probing…".to_string());
                handle.spawn(async move {
                    let _ = tx.send(probe(&cfg).await);
                });
            }

            // Model picker — fed by the last probe, or just the current model.
            let mut model = cur_model.clone();
            let options: Vec<String> = if self.models.is_empty() {
                vec![cur_model.clone()]
            } else {
                self.models.clone()
            };
            egui::ComboBox::from_id_salt(("api_model", p.cassette_name()))
                .selected_text(&model)
                .show_ui(ui, |ui| {
                    for m in &options {
                        ui.selectable_value(&mut model, m.clone(), m);
                    }
                });
            if model != cur_model {
                decks.set_cloud_provider(p, None, Some(model));
                decks.save();
                self.status = Some("Model updated.".to_string());
                changed = true;
            }

            if matches!(self.probe, ApiProbe::Checking(_)) {
                ui.spinner();
            }
        });

        // Advance an in-flight probe (non-blocking).
        if let ApiProbe::Checking(rx) = &mut self.probe {
            match rx.try_recv() {
                Ok(Ok(info)) => {
                    self.status = Some(if info.models.is_empty() {
                        "Probe OK, but the endpoint listed no models.".to_string()
                    } else {
                        format!("Found {} models.", info.models.len())
                    });
                    self.models = info.models;
                    self.probe = ApiProbe::Idle;
                }
                Ok(Err(reason)) => {
                    self.status = Some(format!("Probe failed: {reason}"));
                    self.probe = ApiProbe::Idle;
                }
                Err(oneshot::error::TryRecvError::Empty) => {}
                Err(oneshot::error::TryRecvError::Closed) => {
                    self.status = Some("Probe task died.".to_string());
                    self.probe = ApiProbe::Idle;
                }
            }
        }

        if let Some(s) = &self.status {
            ui.label(egui::RichText::new(s).weak().italics());
        }

        changed
    }
}

/// The dialog: two provider rows plus the window scaffold.
pub(crate) struct ApiKeysPanel {
    pub open: bool,
    /// Snapshot of decks.json loaded when the window opens; edits write through.
    decks: DecksFile,
    rows: [ProviderRow; 2],
}

impl Default for ApiKeysPanel {
    fn default() -> Self {
        Self {
            open: false,
            decks: DecksFile::load_or_default(),
            rows: [
                ProviderRow::new(CloudProvider::Anthropic),
                ProviderRow::new(CloudProvider::OpenAi),
            ],
        }
    }
}

impl ApiKeysPanel {
    /// Open the window, reloading decks.json so the dialog reflects any changes
    /// made elsewhere (onboarding, hand-edits) since it was last shown.
    pub fn open(&mut self) {
        self.decks = DecksFile::load_or_default();
        for row in &mut self.rows {
            row.key_input.clear();
            row.status = None;
        }
        self.open = true;
    }

    /// Render the modeless window. Returns `true` on any frame where a key or
    /// model was changed (and saved), so the caller can reload the live deck.
    pub fn ui(&mut self, ctx: &egui::Context, handle: &tokio::runtime::Handle) -> bool {
        if !self.open {
            return false;
        }
        let mut changed = false;
        let mut wants_close = false;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("api_keys_window"),
            egui::ViewportBuilder::default()
                .with_title("API Keys")
                .with_inner_size([460.0, 440.0])
                .with_min_inner_size([420.0, 360.0])
                .with_resizable(true),
            |vctx, _class| {
                egui::CentralPanel::default().show(vctx, |ui| {
                    ui.label(
                        egui::RichText::new("LLM API Keys")
                            .strong()
                            .size(18.0),
                    );
                    ui.label(
                        egui::RichText::new(
                            "Set or update your cloud provider keys. Keys are stored privately \
                             (0600) in decks.json, or read from an environment variable.",
                        )
                        .weak(),
                    );
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for (idx, row) in self.rows.iter_mut().enumerate() {
                                if idx > 0 {
                                    ui.add_space(8.0);
                                    ui.separator();
                                    ui.add_space(8.0);
                                }
                                changed |= row.ui(ui, &mut self.decks, handle);
                            }
                        });
                });
                // Keep polling while a probe is in flight.
                if self
                    .rows
                    .iter()
                    .any(|r| matches!(r.probe, ApiProbe::Checking(_)))
                {
                    vctx.request_repaint();
                }
                if vctx.input(|i| i.viewport().close_requested()) {
                    wants_close = true;
                }
            },
        );

        if wants_close {
            self.open = false;
        }
        changed
    }
}
