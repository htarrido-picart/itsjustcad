use mydrafter_commands::{parse, Session};
use mydrafter_deck::{
    make_deck, probe, system_prompt, ChatMessage, ChatRequest, DeckDelta, DecksFile, ExtractEvent,
    Extractor, ProbeInfo, Role,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tokio::sync::oneshot;

enum ProbeState {
    Unknown,
    Checking(oneshot::Receiver<Result<ProbeInfo, String>>),
    Ready(ProbeInfo),
    Unavailable(String),
}

const MAX_RETRIES: u8 = 2;

enum Entry {
    User(String),
    Deck(String),
    CommandOk(String, String),
    CommandErr(String, String),
    Status(String),
}

/// The LLM companion pane. Streams commands out of the active deck and runs
/// them through the same `Session` as the human command line.
pub struct DeckPane {
    pub decks: DecksFile,
    input: String,
    transcript: Vec<Entry>,
    /// Conversation as sent to the API (assistant turns keep the raw fenced text).
    messages: Vec<ChatMessage>,
    rx: Option<UnboundedReceiver<DeckDelta>>,
    turn_task: Option<tokio::task::JoinHandle<()>>,
    turn_started: Option<std::time::Instant>,
    extractor: Extractor,
    current_response: String,
    /// Streaming chat text of the in-flight deck turn (display only).
    streaming_chat: String,
    errors_this_turn: Vec<String>,
    retries: u8,
    /// Deck is disabled until the active cassette probes healthy.
    probe: ProbeState,
    probed_deck: Option<usize>,
}

impl Default for DeckPane {
    fn default() -> Self {
        Self {
            decks: DecksFile::load_or_default(),
            input: String::new(),
            transcript: Vec::new(),
            messages: Vec::new(),
            rx: None,
            turn_task: None,
            turn_started: None,
            extractor: Extractor::default(),
            current_response: String::new(),
            streaming_chat: String::new(),
            errors_this_turn: Vec::new(),
            retries: 0,
            probe: ProbeState::Unknown,
            probed_deck: None,
        }
    }
}

impl DeckPane {
    pub fn busy(&self) -> bool {
        self.rx.is_some()
    }

    fn ready(&self) -> bool {
        matches!(self.probe, ProbeState::Ready(_))
    }

    fn start_probe(&mut self, handle: &tokio::runtime::Handle) {
        let Some(config) = self.decks.decks.get(self.decks.active).cloned() else {
            self.probe = ProbeState::Unavailable("no deck configured".into());
            return;
        };
        self.probed_deck = Some(self.decks.active);
        let (tx, rx) = oneshot::channel();
        self.probe = ProbeState::Checking(rx);
        handle.spawn(async move {
            let _ = tx.send(probe(&config).await);
        });
    }

    fn poll_probe(&mut self, handle: &tokio::runtime::Handle) {
        // Re-probe when the cassette changed or nothing has been probed yet.
        if self.probed_deck != Some(self.decks.active)
            || matches!(self.probe, ProbeState::Unknown)
        {
            self.start_probe(handle);
        }
        if let ProbeState::Checking(rx) = &mut self.probe {
            match rx.try_recv() {
                Ok(Ok(detail)) => self.probe = ProbeState::Ready(detail),
                Ok(Err(reason)) => self.probe = ProbeState::Unavailable(reason),
                Err(oneshot::error::TryRecvError::Empty) => {}
                Err(oneshot::error::TryRecvError::Closed) => {
                    self.probe = ProbeState::Unavailable("probe task died".into());
                }
            }
        }
    }

    pub fn turns_completed(&self) -> bool {
        !self.messages.is_empty() && !self.busy()
    }

    /// Dev scripting entry: send a prompt as if typed in the pane.
    pub fn send_text(
        &mut self,
        text: &str,
        session: &Session,
        handle: &tokio::runtime::Handle,
    ) {
        self.input = text.to_string();
        self.send(session, handle);
    }

    fn start_turn(&mut self, session: &Session, handle: &tokio::runtime::Handle) {
        let Some(config) = self.decks.decks.get(self.decks.active) else {
            self.transcript
                .push(Entry::Status("no deck configured".into()));
            return;
        };
        let deck = make_deck(config);
        let req = ChatRequest {
            system: system_prompt(&crate::scene::digest(&session.doc)),
            messages: self.messages.clone(),
            model: String::new(),
            max_tokens: 4096,
            temperature: 0.2,
        };
        let (tx, rx) = unbounded_channel();
        self.rx = Some(rx);
        self.extractor = Extractor::default();
        self.current_response.clear();
        self.streaming_chat.clear();
        self.errors_this_turn.clear();
        self.turn_task = Some(handle.spawn(async move { deck.stream_chat(req, tx).await }));
        self.turn_started = Some(std::time::Instant::now());
    }

    fn stop_turn(&mut self) {
        if let Some(task) = self.turn_task.take() {
            task.abort();
        }
        self.rx = None;
        self.turn_started = None;
        self.retries = MAX_RETRIES; // no auto-retry after a manual stop
        if !self.current_response.is_empty() {
            self.messages.push(ChatMessage {
                role: Role::Assistant,
                content: std::mem::take(&mut self.current_response),
            });
        }
        self.streaming_chat.clear();
        self.transcript.push(Entry::Status("stopped".into()));
    }

    fn send(&mut self, session: &Session, handle: &tokio::runtime::Handle) {
        let text = std::mem::take(&mut self.input);
        let text = text.trim();
        if text.is_empty() || self.busy() {
            return;
        }
        self.retries = 0;
        self.transcript.push(Entry::User(text.to_string()));
        self.messages.push(ChatMessage {
            role: Role::User,
            content: text.to_string(),
        });
        self.start_turn(session, handle);
    }

    fn handle_extract_events(&mut self, events: Vec<ExtractEvent>, session: &mut Session) {
        for event in events {
            match event {
                ExtractEvent::Chat(text) => self.streaming_chat.push_str(&text),
                ExtractEvent::Command(line) => {
                    let result = parse(&line)
                        .map_err(|e| e.to_string())
                        .and_then(|cmd| session.run(cmd).map_err(|e| e.to_string()));
                    match result {
                        Ok(outcome) => self
                            .transcript
                            .push(Entry::CommandOk(line, outcome.message)),
                        Err(e) => {
                            self.errors_this_turn
                                .push(format!("`{line}` failed: {e}"));
                            self.transcript.push(Entry::CommandErr(line, e));
                        }
                    }
                }
            }
        }
    }

    fn finish_turn(&mut self, session: &Session, handle: &tokio::runtime::Handle) {
        if !self.streaming_chat.trim().is_empty() {
            self.transcript
                .push(Entry::Deck(std::mem::take(&mut self.streaming_chat)));
        }
        self.messages.push(ChatMessage {
            role: Role::Assistant,
            content: std::mem::take(&mut self.current_response),
        });
        if !self.errors_this_turn.is_empty() && self.retries < MAX_RETRIES {
            self.retries += 1;
            let feedback = format!(
                "Some commands failed:\n{}\nCurrent scene:\n{}\nFix and re-emit ONLY the failed or missing commands in a ```draft block.",
                self.errors_this_turn.join("\n"),
                crate::scene::digest(&session.doc),
            );
            self.transcript.push(Entry::Status(format!(
                "retry {}/{MAX_RETRIES}: feeding errors back",
                self.retries
            )));
            self.messages.push(ChatMessage {
                role: Role::User,
                content: feedback,
            });
            self.start_turn(session, handle);
        }
    }

    /// Poll streaming deltas; returns whether the document may have changed.
    fn drain(&mut self, session: &mut Session, handle: &tokio::runtime::Handle) {
        let Some(rx) = &mut self.rx else { return };
        let mut done = false;
        let mut batch = Vec::new();
        while let Ok(delta) = rx.try_recv() {
            match delta {
                DeckDelta::Text(text) => {
                    self.current_response.push_str(&text);
                    batch.push(text);
                }
                DeckDelta::Done => {
                    done = true;
                    break;
                }
                DeckDelta::Error(e) => {
                    self.transcript.push(Entry::Status(format!("deck error: {e}")));
                    self.rx = None;
                    self.turn_started = None;
                    return;
                }
            }
        }
        for text in batch {
            let events = self.extractor.push(&text);
            self.handle_extract_events(events, session);
        }
        if done {
            let events = self.extractor.finish();
            self.handle_extract_events(events, session);
            self.rx = None;
            let elapsed = self
                .turn_started
                .take()
                .map(|t| t.elapsed().as_secs_f32())
                .unwrap_or(0.0);
            self.transcript
                .push(Entry::Status(format!("turn done in {elapsed:.1}s")));
            self.finish_turn(session, handle);
        }
    }

    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        session: &mut Session,
        handle: &tokio::runtime::Handle,
    ) {
        self.drain(session, handle);
        self.poll_probe(handle);
        if self.busy() || matches!(self.probe, ProbeState::Checking(_)) {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(50));
        }

        ui.horizontal(|ui| {
            egui::widgets::global_theme_preference_switch(ui);
            let zoom = ui.ctx().zoom_factor();
            if ui.button("A−").on_hover_text("smaller text (Cmd -)").clicked() {
                ui.ctx().set_zoom_factor((zoom - 0.1).max(0.5));
            }
            if ui.button("A+").on_hover_text("bigger text (Cmd =)").clicked() {
                ui.ctx().set_zoom_factor((zoom + 0.1).min(3.0));
            }
            ui.label(format!("{:.0}%", zoom * 100.0));
            ui.separator();
            ui.label("deck:");
            let names: Vec<String> = self.decks.decks.iter().map(|d| d.name.clone()).collect();
            egui::ComboBox::from_id_salt("deck_select")
                .selected_text(
                    names
                        .get(self.decks.active)
                        .cloned()
                        .unwrap_or_else(|| "—".into()),
                )
                .show_ui(ui, |ui| {
                    for (i, name) in names.iter().enumerate() {
                        if ui
                            .selectable_value(&mut self.decks.active, i, name)
                            .clicked()
                        {
                            self.decks.save();
                        }
                    }
                });
            // Model picker fed by the probe's model list (Ollama: installed
            // models; Anthropic: available models).
            let probe_models = match &self.probe {
                ProbeState::Ready(info) => info.models.clone(),
                _ => Vec::new(),
            };
            if !probe_models.is_empty()
                && let Some(config) = self.decks.decks.get_mut(self.decks.active)
            {
                let mut model = config.model.clone();
                egui::ComboBox::from_id_salt("model_select")
                    .selected_text(&model)
                    .width(120.0)
                    .show_ui(ui, |ui| {
                        for m in &probe_models {
                            ui.selectable_value(&mut model, m.clone(), m);
                        }
                    });
                if model != config.model {
                    config.model = model;
                    self.decks.save();
                    self.probe = ProbeState::Unknown; // re-probe with new model
                }
            }
            if self.busy() {
                ui.spinner();
                if ui.small_button("stop").clicked() {
                    self.stop_turn();
                }
            }
            if ui.small_button("clear").clicked() {
                self.transcript.clear();
                self.messages.clear();
            }
        });
        match &self.probe {
            ProbeState::Ready(info) => {
                ui.label(
                    egui::RichText::new(format!("● {}", info.detail))
                        .color(egui::Color32::from_rgb(70, 160, 90))
                        .small(),
                );
            }
            ProbeState::Unavailable(reason) => {
                let reason = reason.clone();
                let mut retry = false;
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        egui::RichText::new(format!("● {reason}"))
                            .color(egui::Color32::from_rgb(200, 80, 70))
                            .small(),
                    );
                    retry = ui.small_button("retry").clicked();
                });
                if retry {
                    self.probe = ProbeState::Unknown;
                }
            }
            ProbeState::Checking(_) => {
                ui.label(egui::RichText::new("● checking deck…").weak().small());
            }
            ProbeState::Unknown => {}
        }
        ui.separator();

        let input_height = 64.0;
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .max_height(ui.available_height() - input_height)
            .show(ui, |ui| {
                for entry in &self.transcript {
                    match entry {
                        Entry::User(t) => {
                            ui.label(egui::RichText::new(format!("you: {t}")).strong());
                        }
                        Entry::Deck(t) => {
                            ui.label(t.trim());
                        }
                        Entry::CommandOk(cmd, msg) => {
                            ui.monospace(
                                egui::RichText::new(format!("✓ {cmd}   ({msg})"))
                                    .color(egui::Color32::from_rgb(70, 160, 90)),
                            );
                        }
                        Entry::CommandErr(cmd, e) => {
                            ui.monospace(
                                egui::RichText::new(format!("✗ {cmd}\n  {e}"))
                                    .color(egui::Color32::from_rgb(200, 80, 70)),
                            );
                        }
                        Entry::Status(t) => {
                            ui.label(egui::RichText::new(t).weak().italics());
                        }
                    }
                }
                if !self.streaming_chat.trim().is_empty() {
                    ui.label(self.streaming_chat.trim());
                }
                // Live turn feedback: waiting → thinking dots + elapsed;
                // streaming → received char count.
                if self.busy() {
                    let elapsed = self
                        .turn_started
                        .map(|t| t.elapsed().as_secs())
                        .unwrap_or(0);
                    let received = self.current_response.len();
                    ui.horizontal(|ui| {
                        ui.spinner();
                        let status = if received == 0 {
                            format!("waiting for model… {elapsed}s")
                        } else {
                            format!("streaming… {received} chars, {elapsed}s")
                        };
                        ui.label(egui::RichText::new(status).weak().italics());
                    });
                }
            });

        ui.separator();
        let hint = if self.ready() {
            "describe what to draw… (Enter sends, Shift+Enter newline)"
        } else {
            "deck unavailable — fix the connection above"
        };
        let response = ui.add_enabled(
            !self.busy() && self.ready(),
            egui::TextEdit::multiline(&mut self.input)
                .desired_rows(2)
                .desired_width(f32::INFINITY)
                .hint_text(hint),
        );
        let enter = response.has_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
        if enter {
            // remove the newline the TextEdit just inserted
            self.input = self.input.trim_end_matches('\n').to_string();
            self.send(session, handle);
        }
    }
}
