use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use mydrafter_commands::{parse, Command, Session};
use serde::{Deserialize, Serialize};
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
/// Hard cap on one deck turn. A wedged CLI subprocess is killed (kill_on_drop)
/// instead of idling for hours; the session revives on the next send.
const TURN_TIMEOUT_SECS: u64 = 600;

#[derive(Serialize, Deserialize)]
struct ExecutedCommand {
    line: String,
    /// Ok: outcome message; Err: error text.
    result: Result<String, String>,
}

#[derive(Serialize, Deserialize)]
enum Entry {
    User(String),
    Deck(String),
    Status(String),
    /// All commands of one turn — rendered as a card; clicking opens the
    /// command detail child pane.
    Commands(Vec<ExecutedCommand>),
}

/// Chat state that survives app restarts — the provider-side session handle
/// plus the local transcript. The `claude` CLI keeps its session on disk, so
/// `--resume session_id` revives the conversation (and its prompt cache) even
/// after the subprocess and this app have both exited.
#[derive(Default, Deserialize)]
struct SavedChat {
    session_id: Option<String>,
    messages: Vec<ChatMessage>,
    transcript: Vec<Entry>,
}

/// Borrowed mirror of [`SavedChat`] so saving never clones the transcript.
#[derive(Serialize)]
struct SavedChatRef<'a> {
    session_id: &'a Option<String>,
    messages: &'a [ChatMessage],
    transcript: &'a [Entry],
}

fn saved_chat_path() -> Option<std::path::PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".config")
            .join("mydrafter")
            .join("deck_chat.json"),
    )
}

impl SavedChat {
    fn load() -> Self {
        saved_chat_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

}

impl SavedChatRef<'_> {
    fn save(&self) {
        let Some(path) = saved_chat_path() else { return };
        let _ = std::fs::create_dir_all(path.parent().expect("has parent"));
        // L-1: 0600 — transcript contains user messages and scene digests.
        let _ = crate::journal::write_private(
            &path,
            serde_json::to_string_pretty(self).expect("serializes").as_bytes(),
        );
    }
}

#[cfg(test)]
mod saved_chat_tests {
    use super::*;

    #[test]
    fn saved_chat_roundtrip() {
        let entries = vec![
            Entry::User("make a slab".into()),
            Entry::Deck("done".into()),
            Entry::Status("turn done in 1.0s".into()),
            Entry::Commands(vec![ExecutedCommand {
                line: "box 0,0,0 5,5,3".into(),
                result: Ok("box abc123".into()),
            }]),
        ];
        let messages = vec![ChatMessage {
            role: Role::User,
            content: "make a slab".into(),
        }];
        let session_id = Some("sess-1".to_string());
        let json = serde_json::to_string(&SavedChatRef {
            session_id: &session_id,
            messages: &messages,
            transcript: &entries,
        })
        .unwrap();
        let back: SavedChat = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session_id.as_deref(), Some("sess-1"));
        assert_eq!(back.messages.len(), 1);
        assert_eq!(back.transcript.len(), 4);
        assert!(matches!(&back.transcript[3], Entry::Commands(c) if c.len() == 1));
    }

    #[test]
    fn saved_chat_tolerates_missing_file_shape() {
        // Old/corrupt files must fall back to default, not crash the app.
        assert!(serde_json::from_str::<SavedChat>("{bad json").is_err());
        let empty: SavedChat = serde_json::from_str("{}").unwrap_or_default();
        assert!(empty.session_id.is_none() && empty.transcript.is_empty());
    }
}

/// The filesystem path a side-effecting command targets, if any.
fn deck_command_path(cmd: &Command) -> Option<&str> {
    match cmd {
        Command::Export { path }
        | Command::Print { path, .. }
        | Command::Import { path }
        | Command::Terrain { path }
        | Command::OsmFile { path }
        | Command::Underlay { path, .. } => Some(path.as_str()),
        _ => None,
    }
}

/// Whether `candidate` resolves inside `root`. Purely lexical (no fs access, so
/// it works for not-yet-created export targets): both are normalized by folding
/// out `.`/`..` segments, then we check the prefix. A candidate that climbs
/// above `root` with `..` — or an absolute path elsewhere — is rejected. This
/// is the deck path sandbox (C-2/H-7): a `../../etc/passwd` import is "outside".
fn path_within(root: &std::path::Path, candidate: &std::path::Path) -> bool {
    use std::path::{Component, PathBuf};
    // Resolve the candidate against root when it is relative.
    let joined: PathBuf = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let normalize = |p: &std::path::Path| -> Option<PathBuf> {
        let mut out = PathBuf::new();
        for c in p.components() {
            match c {
                Component::ParentDir => {
                    // Refuse to climb above the filesystem-anchored prefix.
                    if !out.pop() {
                        return None;
                    }
                }
                Component::CurDir => {}
                other => out.push(other.as_os_str()),
            }
        }
        Some(out)
    };
    let (Some(root_n), Some(cand_n)) = (normalize(root), normalize(&joined)) else {
        return false;
    };
    cand_n.starts_with(&root_n)
}

/// A side-effecting command the deck emitted that is waiting for a human OK
/// before it touches the filesystem (security C-2 / H-7). The raw `line` is
/// kept so, once confirmed, it flows through the same execute+log path as any
/// other command and replays identically.
struct PendingSideEffect {
    line: String,
    cmd: Command,
    /// Human-readable "export → /path" summary for the confirm affordance.
    summary: String,
    /// True when the target path escaped the sandbox root — surfaced in the
    /// affordance so the user knows the deck is writing/reading outside the doc.
    outside_sandbox: bool,
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
    /// The current turn is a vision critique: the claude-code cassette gets a
    /// Read *scoped to the single critique screenshot* (H-1) plus a 2nd agentic
    /// step (open the shot, then answer). A critique turn is prose-only — it
    /// runs no draft commands (H-2). Set by `send_critique`, cleared in
    /// `finish_turn` BEFORE any retry so file access is granted exactly once.
    vision_turn: bool,
    /// The critique button was clicked; the app owns the viewport screenshot so
    /// it polls and clears this, then drives the capture + `send_critique`.
    critique_requested: bool,
    /// Deck-emitted side-effecting commands (export/print/import/…) awaiting an
    /// explicit human [run]/[skip]. They never touch the filesystem until the
    /// user confirms (security C-2 / H-7).
    pending_side_effects: Vec<PendingSideEffect>,
    /// Opt-in: auto-run deck side-effecting commands whose path stays inside the
    /// sandbox root, without a per-command prompt. Defaults OFF. Paths that
    /// escape the sandbox are still queued even when this is on.
    allow_deck_side_effects: bool,
    /// Sandbox root for deck-originated fs paths — the current document's
    /// directory when known, else the mydrafter documents dir. Set by the app.
    sandbox_root: Option<std::path::PathBuf>,
}

impl Default for DeckPane {
    fn default() -> Self {
        let saved = SavedChat::load();
        let mut transcript = saved.transcript;
        if let Some(sid) = &saved.session_id {
            transcript.push(Entry::Status(format!(
                "revived session {}",
                &sid[..sid.len().min(8)]
            )));
        }
        Self {
            decks: DecksFile::load_or_default(),
            input: String::new(),
            transcript,
            messages: saved.messages,
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
            session_id: saved.session_id,
            view: PaneView::Chat,
            markdown: CommonMarkCache::default(),
            vision_turn: false,
            critique_requested: false,
            pending_side_effects: Vec::new(),
            allow_deck_side_effects: false,
            sandbox_root: None,
        }
    }
}

impl DeckPane {
    pub fn busy(&self) -> bool {
        self.rx.is_some()
    }

    /// Poll+clear the critique button. The app drives the viewport screenshot
    /// (which the pane can't reach) and then calls `send_critique`.
    pub fn take_critique_request(&mut self) -> bool {
        std::mem::take(&mut self.critique_requested)
    }

    /// Set the sandbox root used to judge whether a deck-emitted fs path is
    /// "inside the document's directory". Called by the app when a document is
    /// opened/saved. `None` (unknown) makes every deck path require confirmation.
    pub fn set_sandbox_root(&mut self, root: Option<std::path::PathBuf>) {
        self.sandbox_root = root;
    }

    /// Snapshot the chat to disk so an idle/quit/crash can be revived later.
    fn persist_chat(&self) {
        SavedChatRef {
            session_id: &self.session_id,
            messages: &self.messages,
            transcript: &self.transcript,
        }
        .save();
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
        self.vision_turn = false;
        self.input = text.to_string();
        self.send(session, handle);
    }

    /// Send a viewport critique: a vision turn whose prompt already points at
    /// the on-disk screenshot. The claude-code cassette gets a Read scoped to
    /// exactly that one screenshot (H-1) for this turn only — never re-granted
    /// on a retry (H-2). The turn is prose-only and runs no draft commands.
    pub fn send_critique(
        &mut self,
        prompt: &str,
        session: &Session,
        handle: &tokio::runtime::Handle,
    ) {
        self.vision_turn = true;
        self.input = prompt.to_string();
        self.send(session, handle);
    }

    fn start_turn(&mut self, session: &Session, handle: &tokio::runtime::Handle) {
        if let Err(reason) = self.decks.check_local_only() {
            self.transcript.push(Entry::Status(format!("blocked: {reason}")));
            return;
        }
        let Some(config) = self.decks.decks.get(self.decks.active) else {
            self.transcript
                .push(Entry::Status("no deck configured".into()));
            return;
        };
        let deck = make_deck(config);
        let mut req = ChatRequest::text(
            system_prompt(&crate::scene::digest(&session.doc), &session.plugins),
            self.messages.clone(),
            String::new(),
            4096,
            0.2,
            self.session_id.clone(),
        );
        if self.vision_turn {
            // SECURITY (H-1): grant NO unscoped Read. Instead point the adapter
            // at the single fixed critique screenshot; it derives a Read scoped
            // to exactly that file (no arbitrary read, no `decks.json` key
            // exfiltration via an attacker-controlled scene name). A 2nd agentic
            // step lets the model open the shot then answer. Claude-code cassette
            // only; HTTP adapters ignore these fields (vision there is a cut).
            req.vision_shot_path = Some(crate::app::CRITIQUE_SHOT_PATH.to_string());
            req.max_turns = 2;
        }
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
        self.persist_chat();
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
        self.persist_chat();
        self.start_turn(session, handle);
    }

    fn handle_extract_events(&mut self, events: Vec<ExtractEvent>, session: &mut Session) {
        for event in events {
            match event {
                ExtractEvent::Chat(text) => self.streaming_chat.push_str(&text),
                // SECURITY (H-2): a vision-critique turn is prose-only. Any
                // ```draft``` block a critique produces is NOT parsed, run, or
                // queued — a turn that was granted file access must not also be
                // able to drive side-effecting commands. Show it as prose so the
                // user still sees what the model said.
                ExtractEvent::Command(line) if self.vision_turn => {
                    self.streaming_chat.push_str("\n```draft\n");
                    self.streaming_chat.push_str(&line);
                    self.streaming_chat.push_str("\n```\n");
                }
                ExtractEvent::Command(line) => {
                    // Parse first so we can classify. A parse error is reported
                    // exactly as before (no execution attempted).
                    let cmd = match parse(&line) {
                        Ok(cmd) => cmd,
                        Err(e) => {
                            let e = e.to_string();
                            self.errors_this_turn.push(format!("`{line}` failed: {e}"));
                            self.current_commands.push(ExecutedCommand {
                                line,
                                result: Err(e),
                            });
                            continue;
                        }
                    };
                    // SECURITY (C-2/H-7): a side-effecting command *emitted by
                    // the deck* never touches the filesystem without an explicit
                    // human OK. Pure geometry ops auto-run as before.
                    if cmd.is_side_effecting() {
                        let outside = self.path_outside_sandbox(&cmd);
                        // Opt-in fast path: user allowed deck side-effects AND
                        // the path stays inside the sandbox → run without a
                        // per-command prompt. Escaping paths are always queued.
                        if self.allow_deck_side_effects && !outside {
                            self.run_deck_command(&line, cmd, session);
                        } else {
                            let summary = cmd
                                .side_effect_summary()
                                .unwrap_or_else(|| line.clone());
                            self.pending_side_effects.push(PendingSideEffect {
                                line,
                                cmd,
                                summary,
                                outside_sandbox: outside,
                            });
                        }
                        continue;
                    }
                    self.run_deck_command(&line, cmd, session);
                }
            }
        }
    }

    /// Execute a (parsed) deck command through the same session path the human
    /// command line uses, recording the outcome in this turn's command list.
    fn run_deck_command(&mut self, line: &str, cmd: Command, session: &mut Session) {
        let result = session.run(cmd).map_err(|e| e.to_string());
        if let Err(e) = &result {
            self.errors_this_turn.push(format!("`{line}` failed: {e}"));
        }
        self.current_commands.push(ExecutedCommand {
            line: line.to_string(),
            result: result.map(|o| o.message),
        });
    }

    /// True when the fs path a side-effecting command targets is NOT inside the
    /// sandbox root (the document's directory). With no known root we treat every
    /// path as outside, so the confirm affordance always appears — fail closed.
    fn path_outside_sandbox(&self, cmd: &Command) -> bool {
        let Some(path) = deck_command_path(cmd) else {
            // No path (shouldn't happen for side-effecting cmds) → be safe.
            return true;
        };
        let Some(root) = &self.sandbox_root else {
            return true;
        };
        !path_within(root, std::path::Path::new(path))
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
        // SECURITY (H-2): a critique is a single vision turn. Clear the flag
        // BEFORE any retry so a re-emit turn never re-grants the scoped Read —
        // file access is granted exactly once, for the original critique, and
        // never on an auto-retry (nor on the next normal send). A vision turn
        // also emits no draft commands (see `handle_extract_events`), so it can
        // never queue the error-driven retry, but clear here regardless to be
        // robust to future changes.
        self.vision_turn = false;
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
        self.persist_chat();
    }

    /// Poll streaming deltas; returns whether the document may have changed.
    fn drain(&mut self, session: &mut Session, handle: &tokio::runtime::Handle) {
        let Some(rx) = &mut self.rx else { return };
        let mut done = false;
        let mut session_changed = false;
        let mut batch = Vec::new();
        while let Ok(delta) = rx.try_recv() {
            match delta {
                DeckDelta::Text(text) => {
                    self.current_response.push_str(&text);
                    batch.push(text);
                }
                DeckDelta::Session(sid) => {
                    self.session_id = Some(sid);
                    session_changed = true;
                }
                DeckDelta::Done => {
                    tracing::info!("deck turn done");
                    done = true;
                    break;
                }
                DeckDelta::Error(e) => {
                    tracing::warn!("deck error: {e}");
                    // The CLI garbage-collects old sessions; if ours is gone,
                    // drop the handle so the next turn starts fresh from the
                    // locally persisted transcript.
                    if self.session_id.is_some() && e.contains("No conversation found") {
                        self.session_id = None;
                        self.transcript.push(Entry::Status(
                            "provider session expired — next turn resends the transcript".into(),
                        ));
                    }
                    self.transcript.push(Entry::Status(format!("deck error: {e}")));
                    self.rx = None;
                    self.turn_started = None;
                    self.persist_chat();
                    return;
                }
            }
        }
        if session_changed {
            // Persist immediately: even if this turn (or the app) dies, the
            // session handle survives for revival.
            self.persist_chat();
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

    /// Drive background work (drain + probe + warm) without rendering anything.
    /// Call every frame even when the pane is hidden so streaming turns keep
    /// making progress and the timeout logic fires correctly.
    /// When active work is in flight, requests a fast repaint via `ctx`.
    pub fn tick(
        &mut self,
        session: &mut Session,
        handle: &tokio::runtime::Handle,
        ctx: &egui::Context,
    ) {
        self.drain(session, handle);
        if self.busy()
            && let Some(started) = self.turn_started
            && started.elapsed().as_secs() > TURN_TIMEOUT_SECS
        {
            self.stop_turn();
            self.transcript.push(Entry::Status(format!(
                "turn killed after {TURN_TIMEOUT_SECS}s — subprocess was stuck"
            )));
        }
        self.poll_probe(handle);
        self.poll_warm(handle);
        // Keep the event loop running while background work is in flight,
        // whether or not the panel is rendered this frame.
        if self.busy()
            || matches!(self.probe, ProbeState::Checking(_))
            || matches!(self.warm, WarmState::Warming { .. })
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    /// Render the confirm affordances for deck-emitted side-effecting commands.
    /// Each queued command shows "deck wants to: export → /path  [run] [skip]".
    /// A path that escaped the sandbox is flagged so the user knows the deck is
    /// touching a file outside the document's directory (security C-2/H-7).
    fn side_effect_confirm_ui(&mut self, ui: &mut egui::Ui, session: &mut Session) {
        if self.pending_side_effects.is_empty() {
            return;
        }
        ui.separator();
        // Global opt-in: run in-sandbox side-effects without a prompt. Escaping
        // paths still require the explicit [run] below even when this is on.
        ui.checkbox(
            &mut self.allow_deck_side_effects,
            "allow deck file side-effects inside the document folder",
        )
        .on_hover_text(
            "when on, deck-emitted export/import inside the document directory run without asking; paths outside always ask",
        );

        let mut run_idx: Option<usize> = None;
        let mut skip_idx: Option<usize> = None;
        for (i, pending) in self.pending_side_effects.iter().enumerate() {
            egui::Frame::group(ui.style())
                .fill(ui.visuals().extreme_bg_color)
                .inner_margin(egui::Margin::same(6))
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new("deck wants to:").strong().color(ACCENT),
                        );
                        ui.label(egui::RichText::new(&pending.summary).monospace());
                    });
                    if pending.outside_sandbox {
                        ui.label(
                            egui::RichText::new(
                                "⚠ path is OUTSIDE the document folder",
                            )
                            .color(ERR_COLOR)
                            .small(),
                        );
                    }
                    ui.horizontal(|ui| {
                        if ui.button("run").clicked() {
                            run_idx = Some(i);
                        }
                        if ui.button("skip").clicked() {
                            skip_idx = Some(i);
                        }
                    });
                });
        }
        // Apply at most one action per frame (indices stay valid).
        if let Some(i) = run_idx {
            let p = self.pending_side_effects.remove(i);
            // The originating turn is already finished, so run it directly and
            // record it as its own transcript entry (current_commands has been
            // drained). This goes through session.run — logged/replayed like any
            // command, so replay reproduces the confirmed side-effect.
            let result = session.run(p.cmd).map_err(|e| e.to_string());
            self.transcript.push(Entry::Commands(vec![ExecutedCommand {
                line: p.line,
                result: result.map(|o| o.message),
            }]));
            self.persist_chat();
        } else if let Some(i) = skip_idx {
            let p = self.pending_side_effects.remove(i);
            self.transcript
                .push(Entry::Status(format!("skipped deck side-effect: {}", p.summary)));
            self.persist_chat();
        }
    }

    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        session: &mut Session,
        handle: &tokio::runtime::Handle,
    ) {
        self.drain(session, handle);
        if self.busy()
            && let Some(started) = self.turn_started
            && started.elapsed().as_secs() > TURN_TIMEOUT_SECS
        {
            self.stop_turn();
            self.transcript.push(Entry::Status(format!(
                "turn killed after {TURN_TIMEOUT_SECS}s — subprocess was stuck"
            )));
        }
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
            // Local-only toggle: when on, only localhost cassettes are shown and
            // cloud sends are blocked.
            let mut local_only = self.decks.local_only;
            if ui
                .checkbox(&mut local_only, "local only")
                .on_hover_text("hide cloud decks and block remote sends")
                .changed()
            {
                self.decks.local_only = local_only;
                // If the active deck became hidden, switch to the first visible one.
                if local_only {
                    let active_is_remote = !mydrafter_deck::is_local_url(
                        self.decks.decks.get(self.decks.active)
                            .map(|d| d.base_url.as_str())
                            .unwrap_or(""),
                    );
                    let first_local = self.decks.visible_decks().map(|(i, _)| i).next();
                    if active_is_remote
                        && let Some(idx) = first_local
                    {
                        self.decks.active = idx;
                        self.session_id = None;
                        self.persist_chat();
                    }
                }
                self.decks.save();
            }
            ui.separator();
            ui.label("deck:");
            // Only show cassettes permitted by the current local_only setting.
            let visible_decks: Vec<(usize, String)> = self
                .decks
                .visible_decks()
                .map(|(i, d)| (i, d.name.clone()))
                .collect();
            egui::ComboBox::from_id_salt("deck_select")
                .selected_text(
                    self.decks
                        .decks
                        .get(self.decks.active)
                        .map(|d| d.name.clone())
                        .unwrap_or_else(|| "—".into()),
                )
                .show_ui(ui, |ui| {
                    for (i, name) in &visible_decks {
                        if ui
                            .selectable_value(&mut self.decks.active, *i, name)
                            .clicked()
                        {
                            self.decks.save();
                            self.session_id = None;
                            self.persist_chat();
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
            if ui
                .add_enabled(!self.busy() && self.ready(), egui::Button::new("critique").small())
                .on_hover_text("screenshot the viewport and ask the deck to assess it")
                .clicked()
            {
                self.critique_requested = true;
            }
            if ui.small_button("clear").clicked() {
                self.transcript.clear();
                self.messages.clear();
                self.session_id = None;
                self.persist_chat();
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

        self.side_effect_confirm_ui(ui, session);

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

#[cfg(test)]
mod side_effect_gate_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// A DeckPane with empty state, isolated from the on-disk chat/decks.
    fn blank_pane() -> DeckPane {
        DeckPane {
            decks: DecksFile::default(),
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
            vision_turn: false,
            critique_requested: false,
            pending_side_effects: Vec::new(),
            allow_deck_side_effects: false,
            sandbox_root: None,
        }
    }

    // --- SECURITY H-1/H-2: vision-critique file access ---

    #[test]
    fn critique_turn_requests_scoped_read_not_unscoped() {
        // H-1: a vision turn must pass the single screenshot path (→ scoped
        // Read), and must NOT put a bare `Read` in allowed_tools.
        let mut pane = blank_pane();
        pane.vision_turn = true;
        let mut req = ChatRequest::text(
            String::new(),
            Vec::new(),
            String::new(),
            4096,
            0.2,
            None,
        );
        // Mirror the vision-turn arm of start_turn.
        if pane.vision_turn {
            req.vision_shot_path = Some(crate::app::CRITIQUE_SHOT_PATH.to_string());
            req.max_turns = 2;
        }
        assert!(
            req.allowed_tools.is_empty(),
            "critique must not grant an unscoped Read tool"
        );
        assert_eq!(
            req.vision_shot_path.as_deref(),
            Some(crate::app::CRITIQUE_SHOT_PATH),
            "critique must scope file access to the one screenshot"
        );
        // And the adapter turns that into a path-scoped Read of exactly that file.
        let scoped = mydrafter_deck::scoped_allowed_tools(
            &req.allowed_tools,
            req.vision_shot_path.as_deref(),
        );
        assert_eq!(
            scoped,
            vec![format!("Read({})", crate::app::CRITIQUE_SHOT_PATH)]
        );
    }

    #[test]
    fn vision_turn_does_not_execute_or_queue_commands() {
        // H-2: a critique turn is prose-only. A ```draft``` export it emits is
        // neither run nor queued — it is shown as prose. Before the fix a
        // Read-granted turn could also drive side-effecting commands.
        let mut pane = blank_pane();
        pane.vision_turn = true;
        let mut session = Session::default();
        let sentinel = std::env::temp_dir()
            .join(format!("mydrafter_vision_evil_{}.csv", std::process::id()));
        let _ = std::fs::remove_file(&sentinel);
        pane.handle_extract_events(
            vec![
                ExtractEvent::Command(format!("export {}", sentinel.display())),
                ExtractEvent::Command("box 0,0,0 1,1,1".into()),
            ],
            &mut session,
        );
        assert!(
            pane.pending_side_effects.is_empty(),
            "vision turn must not queue side-effects"
        );
        assert!(
            pane.current_commands.is_empty(),
            "vision turn must not execute any command"
        );
        assert_eq!(session.doc.len(), 0, "no geometry op should have run");
        assert!(!sentinel.exists());
        assert!(
            pane.streaming_chat.contains("export"),
            "the emitted command should surface as prose"
        );
    }

    #[test]
    fn vision_turn_emits_no_errors_so_no_retry_can_re_grant_read() {
        // H-2 (structural): a critique turn runs no commands, so it can never
        // accumulate `errors_this_turn` — the error-driven retry that would
        // re-enter start_turn (and re-grant the scoped Read) is unreachable. A
        // malicious ```draft``` in a critique response is shown as prose, not
        // executed, and leaves no error to trigger a retry.
        let mut pane = blank_pane();
        pane.vision_turn = true;
        let mut session = Session::default();
        pane.handle_extract_events(
            vec![
                // Even a command that WOULD fail to parse must not register an
                // error on a vision turn (it isn't parsed at all).
                ExtractEvent::Command("export".into()),
                ExtractEvent::Command("totally not a command".into()),
            ],
            &mut session,
        );
        assert!(
            pane.errors_this_turn.is_empty(),
            "a vision turn must produce no command errors → no retry path"
        );
        assert!(pane.current_commands.is_empty());
        assert!(pane.pending_side_effects.is_empty());
    }

    #[test]
    fn deck_emitted_export_is_not_auto_run_but_queued() {
        // THE bug this fix closes: an LLM-drafted export must NOT touch the fs
        // without a human OK. Before the fix, handle_extract_events ran it.
        let mut pane = blank_pane();
        let mut session = Session::default();
        session.run(parse("box 0,0,0 1,1,1").unwrap()).unwrap();
        // Sentinel path in temp; it must never be created.
        let sentinel = std::env::temp_dir()
            .join(format!("mydrafter_evil_{}.csv", std::process::id()));
        let _ = std::fs::remove_file(&sentinel);
        pane.handle_extract_events(
            vec![ExtractEvent::Command(format!("export {}", sentinel.display()))],
            &mut session,
        );
        assert_eq!(
            pane.pending_side_effects.len(),
            1,
            "export must be queued for confirmation"
        );
        assert!(
            pane.current_commands.is_empty(),
            "export must NOT have executed"
        );
        assert!(pane.pending_side_effects[0].summary.contains("export"));
        assert!(
            !sentinel.exists(),
            "the deck export must NOT have written to disk"
        );
    }

    #[test]
    fn deck_emitted_import_of_sensitive_path_is_queued_and_flagged_outside() {
        // import ../../../etc/passwd escapes any sandbox → queued + flagged.
        let mut pane = blank_pane();
        pane.set_sandbox_root(Some(PathBuf::from("/home/u/proj")));
        let mut session = Session::default();
        pane.handle_extract_events(
            vec![ExtractEvent::Command("import ../../../etc/passwd".into())],
            &mut session,
        );
        assert_eq!(pane.pending_side_effects.len(), 1);
        assert!(
            pane.pending_side_effects[0].outside_sandbox,
            "escaping path must be flagged outside the sandbox"
        );
    }

    #[test]
    fn pure_geometry_op_still_auto_runs() {
        let mut pane = blank_pane();
        let mut session = Session::default();
        pane.handle_extract_events(
            vec![ExtractEvent::Command("box 0,0,0 5,5,3".into())],
            &mut session,
        );
        assert!(
            pane.pending_side_effects.is_empty(),
            "pure box must not be queued"
        );
        assert_eq!(pane.current_commands.len(), 1);
        assert!(pane.current_commands[0].result.is_ok());
        assert_eq!(session.doc.len(), 1, "box must be in the document");
    }

    #[test]
    fn allow_toggle_auto_runs_in_sandbox_but_still_gates_outside() {
        // With the opt-in on, an in-sandbox export runs; an outside one is still
        // queued. Uses a temp dir as the sandbox root so the export can write.
        let dir = std::env::temp_dir().join(format!("mydrafter_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut pane = blank_pane();
        pane.allow_deck_side_effects = true;
        pane.set_sandbox_root(Some(dir.clone()));
        let mut session = Session::default();
        session.run(parse("box 0,0,0 1,1,1").unwrap()).unwrap();

        let inside = dir.join("out.csv");
        pane.handle_extract_events(
            vec![ExtractEvent::Command(format!("export {}", inside.display()))],
            &mut session,
        );
        assert!(
            pane.pending_side_effects.is_empty(),
            "in-sandbox export with toggle on must auto-run"
        );
        assert_eq!(pane.current_commands.len(), 1);

        // Outside path is still gated even with the toggle on.
        pane.handle_extract_events(
            vec![ExtractEvent::Command("export /tmp/elsewhere.csv".into())],
            &mut session,
        );
        assert_eq!(
            pane.pending_side_effects.len(),
            1,
            "outside export must still be queued despite the toggle"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_within_rejects_parent_escape_and_absolute_elsewhere() {
        let root = Path::new("/home/u/proj");
        assert!(path_within(root, Path::new("out.dxf")));
        assert!(path_within(root, Path::new("sub/out.dxf")));
        assert!(path_within(root, Path::new("/home/u/proj/sub/out.dxf")));
        assert!(!path_within(root, Path::new("../secret")));
        assert!(!path_within(root, Path::new("../../etc/passwd")));
        assert!(!path_within(root, Path::new("/etc/passwd")));
        // sneaky: climb out then back to a sibling
        assert!(!path_within(root, Path::new("../proj-evil/x")));
    }
}
