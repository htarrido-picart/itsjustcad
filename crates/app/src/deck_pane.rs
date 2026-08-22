use mydrafter_commands::{parse, Session};
use mydrafter_deck::{
    make_deck, system_prompt, ChatMessage, ChatRequest, DeckDelta, DecksFile, ExtractEvent,
    Extractor, Role,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

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
    extractor: Extractor,
    current_response: String,
    /// Streaming chat text of the in-flight deck turn (display only).
    streaming_chat: String,
    errors_this_turn: Vec<String>,
    retries: u8,
}

impl Default for DeckPane {
    fn default() -> Self {
        Self {
            decks: DecksFile::load_or_default(),
            input: String::new(),
            transcript: Vec::new(),
            messages: Vec::new(),
            rx: None,
            extractor: Extractor::default(),
            current_response: String::new(),
            streaming_chat: String::new(),
            errors_this_turn: Vec::new(),
            retries: 0,
        }
    }
}

impl DeckPane {
    pub fn busy(&self) -> bool {
        self.rx.is_some()
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
        handle.spawn(async move { deck.stream_chat(req, tx).await });
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
        if self.busy() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(50));
        }

        ui.horizontal(|ui| {
            egui::widgets::global_theme_preference_switch(ui);
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
            if self.busy() {
                ui.spinner();
            }
            if ui.small_button("clear").clicked() {
                self.transcript.clear();
                self.messages.clear();
            }
        });
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
            });

        ui.separator();
        let response = ui.add_enabled(
            !self.busy(),
            egui::TextEdit::multiline(&mut self.input)
                .desired_rows(2)
                .desired_width(f32::INFINITY)
                .hint_text("describe what to draw… (Enter sends, Shift+Enter newline)"),
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
