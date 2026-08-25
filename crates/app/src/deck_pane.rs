// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use itsjustcad_commands::{parse, Command, Session};
use serde::{Deserialize, Serialize};
use itsjustcad_deck::{
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
/// How long to wait for a freshly-spawned local model server to answer its
/// health check before giving up. A cold llama.cpp/llamafile start (mmap the
/// weights, warm the first token) can take tens of seconds on a big model.
const LOCAL_RUNTIME_TIMEOUT_SECS: u64 = 120;

/// Whether `config` is a catalog-installed local cassette that ItsJustCAD serves
/// by spawning a subprocess: an `openai_compat` cassette with grammar on whose
/// `model` names a known catalog id. This deliberately excludes user-managed
/// endpoints (e.g. a hand-configured Ollama on :11434) — those the user runs
/// themselves; we only auto-spawn models we downloaded via Model Setup.
fn is_spawnable_local(
    config: &itsjustcad_deck::DeckConfig,
    catalog: &crate::model_catalog::Catalog,
) -> bool {
    config.kind == itsjustcad_deck::DeckKind::OpenaiCompat
        && config.grammar
        && catalog.get(&config.model).is_some()
}

/// Point `decks.active` at the cassette named `name`, returning its index (or
/// `None` if no such cassette). Pure over `decks` so the auto-activate rule is
/// unit-testable without touching disk or a tokio runtime.
fn select_active_by_name(decks: &mut itsjustcad_deck::DecksFile, name: &str) -> Option<usize> {
    let idx = decks.decks.iter().position(|d| d.name == name)?;
    decks.active = idx;
    Some(idx)
}

/// The outcome of ensuring a local runtime for a turn.
enum LocalReady {
    /// The server is healthy; the string is the live base URL to talk to.
    Ready(String),
    /// The server is still starting; retry the turn on a later frame.
    Pending,
    /// The server could not be started or went unhealthy; `String` is why.
    Failed(String),
}

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
            .join("itsjustcad")
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
        .inner_margin(egui::Margin::same(crate::theme::Spacing::S as i8))
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
    /// A user-attached image to analyze on the NEXT send: the claude-code
    /// cassette gets a Read scoped to exactly this file (same mechanism as the
    /// vision critique). Only offered for vision-capable cassettes. Cleared once
    /// the turn is sent.
    attached_image: Option<std::path::PathBuf>,
    /// Deck-emitted side-effecting commands (export/print/import/…) awaiting an
    /// explicit human [run]/[skip]. They never touch the filesystem until the
    /// user confirms (security C-2 / H-7).
    pending_side_effects: Vec<PendingSideEffect>,
    /// Opt-in: auto-run deck side-effecting commands whose path stays inside the
    /// sandbox root, without a per-command prompt. Defaults OFF. Paths that
    /// escape the sandbox are still queued even when this is on.
    allow_deck_side_effects: bool,
    /// Sandbox root for deck-originated fs paths — the current document's
    /// directory when known, else the ItsJustCAD documents dir. Set by the app.
    sandbox_root: Option<std::path::PathBuf>,
    /// Lazily-spawned local model server for the active local cassette. Held so
    /// `kill_on_drop` tears the server down on app exit or a model switch. `None`
    /// until the first turn on a local grammar cassette; replaced when the active
    /// local model changes.
    local_runtime: Option<crate::local_runtime::LocalRuntime>,
    /// Bundled catalog, used to resolve a local cassette's model id to a file +
    /// runtime when spawning the local server.
    catalog: crate::model_catalog::Catalog,
    /// A turn was requested while the local runtime was still starting; the
    /// user's message is already queued, so `tick` retries `start_turn` once the
    /// runtime reports Ready (or drops it on Failed).
    deferred_local_turn: bool,
    /// Opt-in web search (the "allow web search" toggle). Default OFF preserves
    /// the offline/sealed stance. When ON, the turn's `ChatRequest.web_search`
    /// is set: the anthropic cassette attaches the server-side web_search tool,
    /// and the claude-code cassette adds WebSearch/WebFetch. Local grammar
    /// cassettes ignore it (noted as out-of-scope for now).
    allow_web_search: bool,
    /// Multi-session chat store for the CURRENT document, keyed by its uuid and
    /// kept app-local (never written into the shared document). Loaded lazily on
    /// the first `sync_store` once the document uuid is known.
    store: Option<crate::chat_store::DocSessions>,
    /// Current search query over the store's sessions.
    session_search: String,
    /// UI-plane actions the deck emitted this turn (layout changes). Applied to
    /// `ui.json` — NOT the op-log — and pending the app's reconcile. The app
    /// drains these each frame via [`DeckPane::take_ui_actions`].
    pending_ui_actions: Vec<crate::ui_plane::UiAction>,
    /// App-level verb lines the deck emitted this turn (`camera`, `display`,
    /// `lightmode`, `ze`, the standard views, …). These are NOT substrate
    /// document commands and NOT ui-plane actions — they drive the app's
    /// view/camera/display/lighting state. The app drains these each frame via
    /// [`DeckPane::take_app_verbs`] and runs them through the same app-verb-aware
    /// path as the human command line (`App::execute_line`). Never op-logged.
    pending_app_verbs: Vec<String>,
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
            attached_image: None,
            pending_side_effects: Vec::new(),
            allow_deck_side_effects: false,
            sandbox_root: None,
            local_runtime: None,
            catalog: crate::model_catalog::Catalog::load(),
            deferred_local_turn: false,
            allow_web_search: false,
            store: None,
            session_search: String::new(),
            pending_ui_actions: Vec::new(),
            pending_app_verbs: Vec::new(),
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

    /// A model just finished downloading (and its cassette was written to
    /// `decks.json`). Reload decks from disk so this running pane sees the new
    /// cassette, make it the ACTIVE deck, eagerly spawn its local runtime (so the
    /// very next chat turn works with no further user action), and announce it in
    /// the transcript. Returns the cassette name that became active, if found.
    ///
    /// This is the Priority A "download → chat just works" hand-off: without it,
    /// the freshly-installed cassette lives only on disk while the pane keeps
    /// pointing at whatever was active before.
    pub fn activate_installed_model(
        &mut self,
        cassette_name: &str,
        handle: &tokio::runtime::Handle,
    ) -> Option<String> {
        // Reload so the on-disk cassette (written by Model Setup) is visible.
        self.decks = DecksFile::load_or_default();
        let idx = select_active_by_name(&mut self.decks, cassette_name)?;
        self.decks.save();
        // Force a re-probe of the newly-active deck on the next frame.
        self.probed_deck = None;

        let config = self.decks.decks.get(idx).cloned()?;
        // Eagerly bring the local runtime up so the first turn isn't spent
        // waiting on a cold spawn. Ignore the Pending/Ready/Failed outcome here —
        // the pane polls the runtime state each frame and surfaces failures.
        if is_spawnable_local(&config, &self.catalog) {
            let _ = self.ensure_local_runtime(&config, handle);
        }
        self.transcript.push(Entry::Status(format!(
            "Active deck is now '{cassette_name}'. Starting its local runtime — \
             your next message will use it."
        )));
        Some(cassette_name.to_string())
    }

    /// Start a fresh chat session: abort any in-flight turn, drop the provider
    /// conversation handle, and clear the transcript + message history. Used by
    /// File → "New file session". The selected cassette/model are kept.
    pub fn new_session(&mut self) {
        // Archive the outgoing conversation into the per-document store before
        // clearing it, so switching sessions never loses history.
        self.archive_current_session();
        self.stop_turn();
        self.session_id = None;
        self.messages.clear();
        self.transcript.clear();
        self.current_response.clear();
        self.streaming_chat.clear();
        self.current_commands.clear();
        self.errors_this_turn.clear();
        self.input.clear();
        self.attached_image = None;
        self.view = PaneView::Chat;
        self.persist_chat();
    }

    /// Drain the UI-plane actions the deck emitted since the last call. The app
    /// applies each into `ui.json` and reconciles its live widgets. These are
    /// layout changes only — never document mutations.
    pub fn take_ui_actions(&mut self) -> Vec<crate::ui_plane::UiAction> {
        std::mem::take(&mut self.pending_ui_actions)
    }

    /// Drain the app-level verb lines the deck emitted since the last call
    /// (`camera`, `display`, `lightmode`, `ze`, standard views, …). The app runs
    /// each through `App::execute_line` — the same app-verb-aware path the human
    /// command line uses — so the deck can change the camera/view/display like a
    /// person can. These never enter the op-log.
    pub fn take_app_verbs(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_app_verbs)
    }

    /// Point the multi-session store at `doc_uuid`, loading that document's
    /// app-local sessions. Called by the app when a document is opened/saved and
    /// its uuid becomes known. Reloads only when the uuid changes.
    pub fn sync_store(&mut self, doc_uuid: &str) {
        let needs_load = self
            .store
            .as_ref()
            .map(|s| s.doc_uuid != doc_uuid)
            .unwrap_or(true);
        if needs_load {
            self.store = Some(crate::chat_store::DocSessions::load(doc_uuid));
        }
    }

    /// Save the current transcript as (or into) a named session in the store,
    /// keyed by the current document. Private + app-local; the shared document
    /// is never touched. No-op until `sync_store` has established a store.
    pub fn archive_current_session(&mut self) {
        let Some(store) = self.store.as_mut() else { return };
        if self.messages.is_empty() {
            return;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut session = crate::chat_store::ChatSession::new(now);
        for m in &self.messages {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            session.push(role, &m.content, now);
        }
        store.sessions.push(session);
        store.sort_recent();
        store.save();
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

    /// Lazily spawn (or reuse) the local model server for `config` and report
    /// whether this turn can proceed. On the first call for a model it spawns the
    /// subprocess and returns [`LocalReady::Pending`]; subsequent calls poll the
    /// runtime state and return `Ready` (with the live `127.0.0.1:<port>` base
    /// URL) once healthy, or `Failed`. Switching to a different local model drops
    /// the old runtime (killing its server) and spawns the new one.
    ///
    /// Never blocks: the spawn + health-check run on the tokio runtime; this only
    /// reads a shared state and (once) fires the spawn.
    fn ensure_local_runtime(
        &mut self,
        config: &itsjustcad_deck::DeckConfig,
        handle: &tokio::runtime::Handle,
    ) -> LocalReady {
        // Drop a runtime that's serving a different model (a cassette switch).
        if let Some(rt) = &self.local_runtime
            && rt.model_id() != config.model
        {
            self.local_runtime = None; // kill_on_drop tears the old server down
        }

        if let Some(rt) = &self.local_runtime {
            return match rt.state() {
                crate::local_runtime::RuntimeState::Ready { base_url } => {
                    LocalReady::Ready(base_url)
                }
                crate::local_runtime::RuntimeState::Failed { msg } => {
                    // Drop the failed runtime so a later turn can retry a fresh
                    // spawn rather than being stuck on the dead handle.
                    self.local_runtime = None;
                    LocalReady::Failed(msg)
                }
                _ => LocalReady::Pending,
            };
        }

        // First use: resolve the file + runtime, pick a port, spawn.
        let Some(models_dir) = crate::download::models_dir() else {
            return LocalReady::Failed("no home directory for models".into());
        };
        let plan = match crate::local_runtime::resolve_runtime(
            &self.catalog,
            &models_dir,
            &config.model,
        ) {
            Ok(p) => p,
            Err(e) => return LocalReady::Failed(e),
        };
        let port = match crate::local_runtime::free_port() {
            Ok(p) => p,
            Err(e) => return LocalReady::Failed(e),
        };
        match crate::local_runtime::LocalRuntime::spawn(
            handle,
            &config.model,
            &plan,
            port,
            std::time::Duration::from_secs(LOCAL_RUNTIME_TIMEOUT_SECS),
        ) {
            Ok(rt) => {
                self.local_runtime = Some(rt);
                self.transcript
                    .push(Entry::Status("starting local model…".into()));
                LocalReady::Pending
            }
            Err(e) => LocalReady::Failed(e),
        }
    }

    fn start_turn(&mut self, session: &Session, handle: &tokio::runtime::Handle) {
        if let Err(reason) = self.decks.check_local_only() {
            self.transcript.push(Entry::Status(format!("blocked: {reason}")));
            return;
        }
        let Some(config) = self.decks.decks.get(self.decks.active).cloned() else {
            self.transcript
                .push(Entry::Status("no deck configured".into()));
            return;
        };
        // A catalog-installed local cassette (openai_compat + grammar + model is
        // a known catalog id) is served by a subprocess we spawn on demand. If
        // it isn't up yet, kick it off and defer this turn — the pane polls the
        // runtime state and revives the input on the next frame once it's ready.
        let mut config = config;
        if is_spawnable_local(&config, &self.catalog) {
            match self.ensure_local_runtime(&config, handle) {
                LocalReady::Ready(base_url) => config.base_url = base_url,
                LocalReady::Pending => {
                    // Put the message back so the user's turn is not lost; a later
                    // frame retries once the server reports Ready.
                    self.deferred_local_turn = true;
                    return;
                }
                LocalReady::Failed(msg) => {
                    self.transcript
                        .push(Entry::Status(format!("local model failed: {msg}")));
                    return;
                }
            }
        }
        let deck = make_deck(&config);
        let mut req = ChatRequest::text(
            system_prompt(&crate::scene::digest(&session.doc), &session.plugins),
            self.messages.clone(),
            String::new(),
            4096,
            0.2,
            self.session_id.clone(),
        );
        // Opt-in web search: only set when the user toggled it on. Off keeps the
        // request tool-free (offline/sealed). Cassettes that don't support it
        // ignore the flag.
        req.web_search = self.allow_web_search;
        if self.vision_turn {
            // SECURITY (H-1): grant NO unscoped Read. Instead point the adapter
            // at a SINGLE fixed image — either the user-attached image or the
            // critique screenshot — and it derives a Read scoped to exactly that
            // file (no arbitrary read, no `decks.json` key exfiltration via an
            // attacker-controlled scene name). A 2nd agentic step lets the model
            // open the shot then answer. Claude-code cassette only; HTTP adapters
            // ignore these fields (vision there is a cut).
            let shot = self
                .attached_image
                .take()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| crate::app::critique_shot_path().display().to_string());
            req.vision_shot_path = Some(shot);
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
        // A user-attached image makes this a vision turn (scoped Read of that
        // one file, prose-only reply — same security envelope as a critique).
        if self.attached_image.is_some() {
            self.vision_turn = true;
        }
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
                    // UI/SESSION TOOL PLANE (distinct from document commands):
                    // a layout verb changes window state via ui.json, NOT the
                    // op-log. Try it first — the two tool groups are disjoint, so
                    // a real document command never parses as a ui action. A ui
                    // action is queued for the app to apply and never runs
                    // through `session.run`, so it cannot enter the drawing.
                    if let Ok(action) = crate::ui_plane::parse_ui_action(&line) {
                        self.current_commands.push(ExecutedCommand {
                            line: format!("ui: {line}"),
                            result: Ok(action.summary()),
                        });
                        self.pending_ui_actions.push(action);
                        continue;
                    }
                    // APP-VERB PLANE (distinct from document commands and ui
                    // actions): view/camera/display/lighting verbs like
                    // `camera 2point`, `display shaded`, `lightmode sun`, `ze`,
                    // and the standard views are owned by the app, not the
                    // substrate parser (which would reject them). Queue them for
                    // the app to run through `App::execute_line` — the same path
                    // the human command line uses — so the deck can drive the view
                    // exactly like a person. GUI-only verbs (`template`,
                    // `critique`) are intentionally NOT run from the deck; skip
                    // them here so they fall through and are reported as unknown.
                    if let Some(verb) = crate::app_verbs::classify(&line)
                        && !matches!(verb, crate::app_verbs::AppVerb::GuiOnly(_))
                    {
                        self.current_commands.push(ExecutedCommand {
                            line: line.clone(),
                            result: Ok("applied (view/camera)".to_string()),
                        });
                        self.pending_app_verbs.push(line);
                        continue;
                    }
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
        // A turn deferred while the local model was starting: retry once the
        // runtime resolves. `start_turn` re-checks the state and either proceeds
        // (Ready), keeps deferring (still Pending), or reports Failed.
        if self.deferred_local_turn && !self.busy() {
            let still_starting = matches!(
                self.local_runtime.as_ref().map(|r| r.state()),
                Some(crate::local_runtime::RuntimeState::Starting)
            );
            if still_starting {
                ctx.request_repaint_after(std::time::Duration::from_millis(200));
            } else {
                self.deferred_local_turn = false;
                self.start_turn(session, handle);
            }
        }
        self.poll_probe(handle);
        self.poll_warm(handle);
        // Keep the event loop running while background work is in flight,
        // whether or not the panel is rendered this frame.
        if self.busy()
            || self.deferred_local_turn
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
                .inner_margin(egui::Margin::same(crate::theme::Spacing::S as i8))
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

    /// Seed a couple of demo chat sessions into the current store — a dev hook
    /// (`ITSJUSTCAD_DECK_PANE`) so a screenshot can show the switcher + search
    /// populated without a live model. No-op if the store already has sessions.
    pub fn seed_demo_sessions(&mut self) {
        let Some(store) = self.store.as_mut() else { return };
        if !store.sessions.is_empty() {
            return;
        }
        let mut a = crate::chat_store::ChatSession::new(200);
        a.push("user", "make a five by five office core with a stair", 200);
        a.push("assistant", "Drew the core and a stair to level 2.", 201);
        let mut b = crate::chat_store::ChatSession::new(100);
        b.push("user", "add a curtain wall facade to the north side", 100);
        b.push("assistant", "Added a glass facade along the north edge.", 101);
        store.sessions.push(a);
        store.sessions.push(b);
        store.sort_recent();
    }

    /// Multi-session switcher + full-text search over the current document's
    /// stored chats. A search field lists matching sessions with a snippet;
    /// clicking a session loads its transcript (jumping to the matched turn).
    /// Collapsed by default so it never crowds the live chat.
    fn session_browser_ui(&mut self, ui: &mut egui::Ui) {
        let Some(store) = self.store.as_ref() else { return };
        let count = store.sessions.len();
        egui::CollapsingHeader::new(format!("💬 sessions ({count})"))
            .id_salt("deck_sessions")
            .default_open(count > 0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("search:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.session_search)
                            .hint_text("find across this doc's chats")
                            .desired_width(f32::INFINITY),
                    );
                });
                let mut load_session: Option<String> = None;
                // Re-borrow the store immutably for the results (search_field is
                // a separate field, so this does not alias).
                let store = self.store.as_ref().expect("store present");
                if !self.session_search.trim().is_empty() {
                    let hits = store.search(&self.session_search);
                    if hits.is_empty() {
                        ui.label(egui::RichText::new("no matches").weak().italics());
                    }
                    for hit in hits.iter().take(20) {
                        let label = format!("{} — {}", hit.session_title, hit.snippet);
                        if ui.selectable_label(false, label).clicked() {
                            load_session = Some(hit.session_id.clone());
                        }
                    }
                } else {
                    for s in store.sessions.iter().take(20) {
                        let turns = s.turns.len();
                        if ui
                            .selectable_label(false, format!("{} ({turns})", s.title))
                            .clicked()
                        {
                            load_session = Some(s.id.clone());
                        }
                    }
                }
                if let Some(id) = load_session {
                    self.load_stored_session(&id);
                }
            });
    }

    /// Load a stored session's turns into the live transcript for viewing /
    /// continuation. Archives the current conversation first so nothing is lost.
    fn load_stored_session(&mut self, id: &str) {
        // Snapshot the outgoing conversation before replacing it.
        self.archive_current_session();
        let Some(store) = self.store.as_ref() else { return };
        let Some(session) = store.get(id) else { return };
        self.messages = session
            .turns
            .iter()
            .map(|t| ChatMessage {
                role: if t.role == "assistant" { Role::Assistant } else { Role::User },
                content: t.content.clone(),
            })
            .collect();
        self.transcript = session
            .turns
            .iter()
            .map(|t| {
                if t.role == "assistant" {
                    Entry::Deck(t.content.clone())
                } else {
                    Entry::User(t.content.clone())
                }
            })
            .collect();
        // A loaded session is a fresh provider conversation (no live handle).
        self.session_id = None;
        self.session_search.clear();
        self.view = PaneView::Chat;
        self.persist_chat();
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
            // Theme + text-size moved to the menu bar (appearance group); the
            // chat header now starts with the local-only + deck controls.
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
                    let active_is_remote = !itsjustcad_deck::is_local_url(
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
            // Opt-in web search toggle. Default OFF. Only meaningful for cloud
            // cassettes (anthropic server-side tool, claude-code WebSearch);
            // disabled under local-only, which forbids remote calls anyway.
            let web_capable = !self.decks.local_only;
            ui.add_enabled_ui(web_capable, |ui| {
                ui.checkbox(&mut self.allow_web_search, "allow web search")
                    .on_hover_text(
                        "let the model search/fetch the web this turn (cloud cassettes only); off by default to stay sealed",
                    );
            });
            if !web_capable {
                self.allow_web_search = false;
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
            // Local model server status (only when one is starting/failed/ready).
            if let Some(rt) = &self.local_runtime {
                let state = rt.state();
                let color = match state {
                    crate::local_runtime::RuntimeState::Failed { .. } => ERR_COLOR,
                    crate::local_runtime::RuntimeState::Ready { .. } => OK_COLOR,
                    crate::local_runtime::RuntimeState::Starting => ACCENT,
                };
                if matches!(state, crate::local_runtime::RuntimeState::Starting) {
                    ui.spinner();
                }
                ui.colored_label(color, state.caption());
            }
            // No critique button: `critique` is a command-line/chat verb, not a
            // toolbar affordance (the app still honours `critique_requested`).
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

        self.session_browser_ui(ui);

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
        let input_height = crate::theme::Spacing::CHAT_INPUT_H;
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
        // Image attach: only for vision-capable cassettes (we never ship an
        // image to a blind model). Reuses the scoped-Read vision mechanism.
        let vision = self
            .decks
            .decks
            .get(self.decks.active)
            .map(|c| c.supports_vision())
            .unwrap_or(false);
        ui.horizontal(|ui| {
            if vision {
                if ui
                    .add_enabled(!self.busy(), egui::Button::new("🖼"))
                    .on_hover_text("attach an image for the model to analyze")
                    .clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("Image", &["png", "jpg", "jpeg", "webp", "gif"])
                        .pick_file()
                {
                    self.attached_image = Some(path);
                }
                if let Some(img) = self.attached_image.clone() {
                    let name = img
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    ui.label(egui::RichText::new(format!("🖼 {name}")).weak());
                    if ui.small_button("✕").on_hover_text("remove attachment").clicked() {
                        self.attached_image = None;
                    }
                }
            } else {
                ui.add_enabled(false, egui::Button::new("🖼"))
                    .on_hover_text("this cassette has no vision — pick a vision-capable model to attach images");
            }
        });
        let hint = if self.attached_image.is_some() {
            "ask about the attached image… (Enter sends, Shift+Enter newline)"
        } else if self.ready() {
            "describe what to draw… (Enter sends, Shift+Enter newline)"
        } else if matches!(self.warm, WarmState::Warming { .. }) {
            "loading model — chat enables when warm"
        } else {
            "deck unavailable — fix the connection above"
        };
        // Stable, explicit id so focus survives layout/enablement changes across
        // frames (prevents flicker + dropped keystrokes).
        let input_id = egui::Id::new("deck_chat_input");
        // Enable when the deck is ready and idle — BUT never disable the field
        // out from under the user while it holds focus (a turn that starts, or a
        // `ready()`/`busy()` toggle, mid-type would otherwise steal focus and
        // flicker). `send()` still gates on `busy()`, so keeping it enabled is
        // safe: nothing is sent until the field is actually ready.
        let has_focus = ui.memory(|m| m.has_focus(input_id));
        let enabled = (!self.busy() && self.ready()) || has_focus;
        let response = ui.add_enabled(
            enabled,
            egui::TextEdit::multiline(&mut self.input)
                .id(input_id)
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
            attached_image: None,
            pending_side_effects: Vec::new(),
            allow_deck_side_effects: false,
            sandbox_root: None,
            local_runtime: None,
            catalog: crate::model_catalog::Catalog::load(),
            deferred_local_turn: false,
            allow_web_search: false,
            store: None,
            session_search: String::new(),
            pending_ui_actions: Vec::new(),
            pending_app_verbs: Vec::new(),
        }
    }

    // --- Opt-in web search gating at the request-build boundary ---

    #[test]
    fn web_search_flag_defaults_off_and_mirrors_into_request() {
        // The pane's toggle is OFF by default → the turn's ChatRequest carries
        // web_search=false (tool absent from the request). Flipping the toggle
        // sets the flag. This mirrors the `req.web_search = self.allow_web_search`
        // line in start_turn without needing a live model/tokio runtime.
        let mut pane = blank_pane();
        assert!(!pane.allow_web_search, "web search must default OFF");
        let mut req = ChatRequest::text(String::new(), Vec::new(), String::new(), 4096, 0.2, None);
        req.web_search = pane.allow_web_search;
        assert!(!req.web_search, "OFF → request has web_search=false");

        pane.allow_web_search = true;
        req.web_search = pane.allow_web_search;
        assert!(req.web_search, "ON → request has web_search=true");
    }

    #[test]
    fn local_only_forces_web_search_off() {
        // Under local-only (no remote calls permitted) the toggle is force-off,
        // so a sealed session can never emit a web_search request.
        let mut pane = blank_pane();
        pane.allow_web_search = true;
        pane.decks.local_only = true;
        // Mirror the header guard: local-only clears the flag.
        if pane.decks.local_only {
            pane.allow_web_search = false;
        }
        assert!(!pane.allow_web_search);
    }

    // --- Deck app-verb routing (camera/view/display/lighting) ---

    #[test]
    fn deck_camera_verb_is_queued_not_errored() {
        // A deck-emitted `camera 2point` is an APP VERB — the substrate parser
        // rejects it, so before this fix it was reported as a failure. Now it is
        // queued for the app to run through `execute_line`, with no turn error.
        let mut pane = blank_pane();
        let mut session = Session::default();
        pane.handle_extract_events(
            vec![ExtractEvent::Command("camera 2point".to_string())],
            &mut session,
        );
        assert_eq!(pane.take_app_verbs(), vec!["camera 2point".to_string()]);
        assert!(pane.errors_this_turn.is_empty(), "app verb must not error");
    }

    #[test]
    fn deck_view_and_display_verbs_are_queued() {
        let mut pane = blank_pane();
        let mut session = Session::default();
        pane.handle_extract_events(
            vec![
                ExtractEvent::Command("ze".to_string()),
                ExtractEvent::Command("top".to_string()),
                ExtractEvent::Command("display shaded".to_string()),
                ExtractEvent::Command("lightmode sun".to_string()),
            ],
            &mut session,
        );
        assert_eq!(
            pane.take_app_verbs(),
            vec![
                "ze".to_string(),
                "top".to_string(),
                "display shaded".to_string(),
                "lightmode sun".to_string(),
            ]
        );
        assert!(pane.errors_this_turn.is_empty());
    }

    #[test]
    fn deck_gui_only_verb_is_not_queued_as_app_verb() {
        // `template`/`critique` are GUI-only; the deck must NOT run them as app
        // verbs. They fall through to the parser (and are reported as unknown),
        // so nothing lands in the app-verb queue.
        let mut pane = blank_pane();
        let mut session = Session::default();
        pane.handle_extract_events(
            vec![ExtractEvent::Command("template".to_string())],
            &mut session,
        );
        assert!(pane.take_app_verbs().is_empty());
    }

    #[test]
    fn deck_draw_command_still_runs_through_substrate() {
        // A real geometry command is not an app verb: it must still execute via
        // the session (not land in the app-verb queue).
        let mut pane = blank_pane();
        let mut session = Session::default();
        let before = session.doc.len();
        pane.handle_extract_events(
            vec![ExtractEvent::Command("box 0,0,0 1,1,1".to_string())],
            &mut session,
        );
        assert!(pane.take_app_verbs().is_empty(), "draw cmd is not an app verb");
        assert!(session.doc.len() > before, "draw cmd must mutate the doc");
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
        let expected_shot = crate::app::critique_shot_path().display().to_string();
        if pane.vision_turn {
            req.vision_shot_path = Some(expected_shot.clone());
            req.max_turns = 2;
        }
        assert!(
            req.allowed_tools.is_empty(),
            "critique must not grant an unscoped Read tool"
        );
        assert_eq!(
            req.vision_shot_path.as_deref(),
            Some(expected_shot.as_str()),
            "critique must scope file access to the one screenshot"
        );
        // And the adapter turns that into a path-scoped Read of exactly that file.
        let scoped = itsjustcad_deck::scoped_allowed_tools(
            &req.allowed_tools,
            req.vision_shot_path.as_deref(),
        );
        assert_eq!(scoped, vec![format!("Read({expected_shot})")]);
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
            .join(format!("itsjustcad_vision_evil_{}.csv", std::process::id()));
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
            .join(format!("itsjustcad_evil_{}.csv", std::process::id()));
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
        let dir = std::env::temp_dir().join(format!("itsjustcad_test_{}", std::process::id()));
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

    // --- auto-activate (Priority A): flip the active deck to a new cassette ---

    fn deck(name: &str) -> itsjustcad_deck::DeckConfig {
        itsjustcad_deck::DeckConfig {
            name: name.into(),
            kind: itsjustcad_deck::DeckKind::OpenaiCompat,
            base_url: "http://localhost:8080/v1".into(),
            model: name.into(),
            api_key: None,
            grammar: true,
        }
    }

    #[test]
    fn select_active_by_name_flips_active_to_the_installed_cassette() {
        let mut decks = DecksFile {
            decks: vec![deck("cloud"), deck("local-qwen"), deck("other")],
            active: 0,
            local_only: false,
        };
        // The just-downloaded cassette becomes active regardless of prior index.
        let idx = select_active_by_name(&mut decks, "local-qwen");
        assert_eq!(idx, Some(1));
        assert_eq!(decks.active, 1);
    }

    #[test]
    fn select_active_by_name_unknown_is_none_and_leaves_active() {
        let mut decks = DecksFile {
            decks: vec![deck("a"), deck("b")],
            active: 1,
            local_only: false,
        };
        assert_eq!(select_active_by_name(&mut decks, "nope"), None);
        assert_eq!(decks.active, 1, "active must be untouched on a miss");
    }
}
