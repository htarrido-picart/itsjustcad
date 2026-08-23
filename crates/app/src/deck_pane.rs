use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use mydrafter_commands::{parse, Session};
use mydrafter_deck::{
    make_deck, probe, system_prompt, warm_model, ChatMessage, ChatRequest, DeckDelta, DecksFile,
    ExtractEvent, Extractor, ProbeInfo, Role, WarmOutcome,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tokio::sync::oneshot;

enum ProbeState {
    Unknown,
    Checking(oneshot::Receiver<Result<ProbeInfo, String>>),
    Ready(ProbeInfo),
    Unavailable(String),
}

enum WarmState {
    Idle,
    Warming {
        rx: oneshot::Receiver<Result<WarmOutcome, String>>,
        started: std::time::Instant,
    },
    Warm,
    NotApplicable,
    Failed(String),
}

const MAX_RETRIES: u8 = 2;

struct ExecutedCommand {
    line: String,
    /// Ok: outcome message; Err: error text.
    result: Result<String, String>,
}

enum Entry {
    User(String),
    Deck(String),
    Status(String),
    /// All commands of one turn — rendered as a card; clicking opens the
    /// command detail child pane.
    Commands(Vec<ExecutedCommand>),
}

/// Deck pane navigation: chat, or a full-pane command detail with a back button.
enum PaneView {
    Chat,
    /// Detail of a finished turn (index into the transcript).
    Detail(usize),
    /// Detail of the in-flight turn.
    LiveDetail,
}

const OK_COLOR: egui::Color32 = egui::Color32::from_rgb(70, 160, 90);
const ERR_COLOR: egui::Color32 = egui::Color32::from_rgb(200, 80, 70);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(90, 160, 255);

fn commands_header(commands: &[ExecutedCommand]) -> egui::RichText {
    let failed = commands.iter().filter(|c| c.result.is_err()).count();
    if failed > 0 {
        egui::RichText::new(format!("⚠ {} command(s), {failed} failed", commands.len()))
            .color(ERR_COLOR)
    } else {
        egui::RichText::new(format!("✓ {} command(s) drawn", commands.len())).color(OK_COLOR)
    }
}

/// Clickable summary card in the chat flow.
fn commands_card(ui: &mut egui::Ui, commands: &[ExecutedCommand]) -> bool {
    let failed = commands.iter().filter(|c| c.result.is_err()).count();
    let (text, color) = if failed > 0 {
        (
            format!("⚠ {} command(s), {failed} failed  ›", commands.len()),
            ERR_COLOR,
        )
    } else {
        (format!("✓ {} command(s) drawn  ›", commands.len()), OK_COLOR)
    };
    ui.add(
        egui::Button::new(egui::RichText::new(text).color(color))
            .min_size(egui::vec2(ui.available_width(), 30.0)),
    )
    .on_hover_text("show the drafted commands")
    .clicked()
}

/// Full formatted code view for the detail pane.
fn render_command_code(ui: &mut egui::Ui, commands: &[ExecutedCommand]) {
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    egui::Frame::group(ui.style())
        .fill(ui.visuals().code_bg_color)
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            for cmd in commands {
                let ok = cmd.result.is_ok();
                let mut job = egui::text::LayoutJob::default();
                let (verb, rest) = cmd.line.split_once(' ').unwrap_or((cmd.line.as_str(), ""));
                job.append(
                    if ok { "✓ " } else { "✗ " },
                    0.0,
                    egui::TextFormat::simple(font.clone(), if ok { OK_COLOR } else { ERR_COLOR }),
                );
                job.append(verb, 0.0, egui::TextFormat::simple(font.clone(), ACCENT));
                if !rest.is_empty() {
                    job.append(
                        &format!(" {rest}"),
                        0.0,
                        egui::TextFormat::simple(font.clone(), ui.visuals().text_color()),
                    );
                }
                ui.label(job);
                match &cmd.result {
                    Ok(msg) => {
                        ui.label(egui::RichText::new(format!("   {msg}")).weak().small());
                    }
                    Err(e) => {
                        ui.label(
                            egui::RichText::new(format!("   {e}")).color(ERR_COLOR).small(),
                        );
                    }
                }
            }
        });
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
    /// Commands executed during the in-flight turn.
    current_commands: Vec<ExecutedCommand>,
    retries: u8,
    /// Deck is disabled until the active cassette probes healthy.
    probe: ProbeState,
    probed_deck: Option<usize>,
    /// Background model preload (Ollama cold-load tax).
    warm: WarmState,
    warmed_model: Option<String>,
    /// Provider-side conversation handle (claude-code sessions).
    session_id: Option<String>,
    view: PaneView,
    markdown: CommonMarkCache,
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
            current_commands: Vec::new(),
            retries: 0,
            probe: ProbeState::Unknown,
            probed_deck: None,
            warm: WarmState::Idle,
            warmed_model: None,
            session_id: None,
            view: PaneView::Chat,
            markdown: CommonMarkCache::default(),
        }
    }
}

impl DeckPane {
    pub fn busy(&self) -> bool {
        self.rx.is_some()
    }

    /// Chat is enabled only when the endpoint probes healthy AND the model is
    /// resident in memory (or warm-up doesn't apply, e.g. cloud APIs).
    fn ready(&self) -> bool {
        matches!(self.probe, ProbeState::Ready(_))
            && matches!(self.warm, WarmState::Warm | WarmState::NotApplicable)
    }

    fn start_warm(&mut self, handle: &tokio::runtime::Handle) {
        let Some(config) = self.decks.decks.get(self.decks.active).cloned() else {
            return;
        };
        self.warmed_model = Some(config.model.clone());
        let (tx, rx) = oneshot::channel();
        self.warm = WarmState::Warming {
            rx,
            started: std::time::Instant::now(),
        };
        handle.spawn(async move {
            let _ = tx.send(warm_model(&config).await);
        });
    }

    fn poll_warm(&mut self, handle: &tokio::runtime::Handle) {
        // (Re)warm when the endpoint is healthy and the model changed.
        if matches!(self.probe, ProbeState::Ready(_)) {
            let current = self
                .decks
                .decks
                .get(self.decks.active)
                .map(|c| c.model.clone());
            let stale = self.warmed_model != current
                || matches!(self.warm, WarmState::Idle | WarmState::Failed(_));
            if stale && !matches!(self.warm, WarmState::Warming { .. }) {
                self.start_warm(handle);
            }
        }
        if let WarmState::Warming { rx, .. } = &mut self.warm {
            match rx.try_recv() {
                Ok(Ok(WarmOutcome::Warm)) => self.warm = WarmState::Warm,
                Ok(Ok(WarmOutcome::NotApplicable)) => self.warm = WarmState::NotApplicable,
                Ok(Err(e)) => self.warm = WarmState::Failed(e),
                Err(oneshot::error::TryRecvError::Empty) => {}
                Err(oneshot::error::TryRecvError::Closed) => {
                    self.warm = WarmState::Failed("warm-up task died".into());
                }
            }
        }
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
            session_id: self.session_id.clone(),
        };
        let (tx, rx) = unbounded_channel();
        self.rx = Some(rx);
        self.extractor = Extractor::default();
        self.current_response.clear();
        self.streaming_chat.clear();
        self.errors_this_turn.clear();
        self.current_commands.clear();
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
                    if let Err(e) = &result {
                        self.errors_this_turn.push(format!("`{line}` failed: {e}"));
                    }
                    self.current_commands.push(ExecutedCommand {
                        line,
                        result: result.map(|o| o.message),
                    });
                }
            }
        }
    }

    fn finish_turn(&mut self, session: &Session, handle: &tokio::runtime::Handle) {
        if !self.streaming_chat.trim().is_empty() {
            self.transcript
                .push(Entry::Deck(std::mem::take(&mut self.streaming_chat)));
        }
        if !self.current_commands.is_empty() {
            // If the user is watching the live detail, follow it to the
            // finished entry; otherwise stay where they are.
            if matches!(self.view, PaneView::LiveDetail) {
                self.view = PaneView::Detail(self.transcript.len());
            }
            self.transcript
                .push(Entry::Commands(std::mem::take(&mut self.current_commands)));
        } else if matches!(self.view, PaneView::LiveDetail) {
            self.view = PaneView::Chat;
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
                DeckDelta::Session(sid) => {
                    self.session_id = Some(sid);
                }
                DeckDelta::Done => {
                    tracing::info!("deck turn done");
                    done = true;
                    break;
                }
                DeckDelta::Error(e) => {
                    tracing::warn!("deck error: {e}");
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
        self.poll_warm(handle);
        if self.busy()
            || matches!(self.probe, ProbeState::Checking(_))
            || matches!(self.warm, WarmState::Warming { .. })
        {
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
                            self.session_id = None;
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
                self.session_id = None;
            }
        });
        match &self.probe {
            ProbeState::Ready(info) => {
                match &self.warm {
                    WarmState::Warming { started, .. } => {
                        // Ollama exposes no load progress — pseudo-progress
                        // asymptotic to 95% keeps the bar honest but alive.
                        let t = started.elapsed().as_secs_f32();
                        let progress = 0.95 * (1.0 - (-t / 25.0).exp());
                        ui.label(
                            egui::RichText::new(format!(
                                "loading model into memory… {:.0}s (first use of a model takes 30-60s)",
                                t
                            ))
                            .small(),
                        );
                        ui.add(
                            egui::ProgressBar::new(progress)
                                .desired_height(6.0)
                                .animate(true),
                        );
                    }
                    WarmState::Failed(e) => {
                        ui.label(
                            egui::RichText::new(format!("● model load failed: {e}"))
                                .color(egui::Color32::from_rgb(200, 80, 70))
                                .small(),
                        );
                    }
                    _ => {
                        let warm_tag = if matches!(self.warm, WarmState::Warm) {
                            " · model warm"
                        } else {
                            ""
                        };
                        ui.label(
                            egui::RichText::new(format!("● {}{warm_tag}", info.detail))
                                .color(egui::Color32::from_rgb(70, 160, 90))
                                .small(),
                        );
                    }
                }
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

        // Child pane: full command code view with a back button.
        if let Some(commands_view) = match self.view {
            PaneView::Detail(index) => match self.transcript.get(index) {
                Some(Entry::Commands(commands)) => Some(commands.as_slice()),
                _ => None,
            },
            PaneView::LiveDetail => Some(self.current_commands.as_slice()),
            PaneView::Chat => None,
        } {
            let mut back = false;
            ui.horizontal(|ui| {
                back = ui.button("‹ back").clicked();
                ui.label(commands_header(commands_view));
            });
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                render_command_code(ui, commands_view);
            });
            if back {
                self.view = PaneView::Chat;
            }
            return;
        }
        if !matches!(self.view, PaneView::Chat) {
            self.view = PaneView::Chat; // stale index — fall back
        }

        let mut open_detail: Option<PaneView> = None;
        let input_height = 64.0;
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .max_height(ui.available_height() - input_height)
            .show(ui, |ui| {
                for (i, entry) in self.transcript.iter().enumerate() {
                    match entry {
                        Entry::User(t) => {
                            ui.label(egui::RichText::new(format!("you: {t}")).strong());
                        }
                        Entry::Deck(t) => {
                            CommonMarkViewer::new().show(ui, &mut self.markdown, t.trim());
                        }
                        Entry::Commands(commands) => {
                            if commands_card(ui, commands) {
                                open_detail = Some(PaneView::Detail(i));
                            }
                        }
                        Entry::Status(t) => {
                            ui.label(egui::RichText::new(t).weak().italics());
                        }
                    }
                }
                if !self.streaming_chat.trim().is_empty() {
                    CommonMarkViewer::new().show(
                        ui,
                        &mut self.markdown,
                        self.streaming_chat.trim(),
                    );
                }
                // Live turn feedback: waiting → elapsed; streaming → char count.
                if self.busy() {
                    if !self.current_commands.is_empty()
                        && commands_card(ui, &self.current_commands)
                    {
                        open_detail = Some(PaneView::LiveDetail);
                    }
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
        if let Some(view) = open_detail {
            self.view = view;
        }

        ui.separator();
        let hint = if self.ready() {
            "describe what to draw… (Enter sends, Shift+Enter newline)"
        } else if matches!(self.warm, WarmState::Warming { .. }) {
            "loading model — chat enables when warm"
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
