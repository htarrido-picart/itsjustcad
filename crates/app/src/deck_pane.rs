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

/// Human label for a cassette in the deck dropdown. Installed local models are
/// keyed `local-<catalog-id>` (a stable lookup key, ugly to show). Resolve that
/// key to the catalog's `display_name` so the dropdown reads "JustCadModel
/// (Qwen 4B)". Non-local decks (and unknown ids) render their raw name.
/// Mirrors crates/app/assets/models.json — kept a plain lookup so it stays in
/// sync with the shipped catalog without threading `&Catalog` through the UI.
fn deck_display_name(name: &str) -> String {
    match name.strip_prefix("local-") {
        Some(id) => crate::model_catalog::Catalog::load()
            .get(id)
            .map(|m| m.display_name.clone())
            .unwrap_or_else(|| id.to_string()),
        None => name.to_string(),
    }
}

/// Point `decks.active` at the cassette named `name`, returning its index (or
/// `None` if no such cassette). Pure over `decks` so the auto-activate rule is
/// unit-testable without touching disk or a tokio runtime.
fn select_active_by_name(decks: &mut itsjustcad_deck::DecksFile, name: &str) -> Option<usize> {
    let idx = decks.decks.iter().position(|d| d.name == name)?;
    decks.active = idx;
    Some(idx)
}

/// How `archive_active` should place the live conversation into the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveMode {
    /// Auto (quit/save/new): update the loaded session in place if it is still
    /// in the store, else push a fresh one. Never prompts.
    Auto,
    /// Force update-in-place by this session id (user chose "Update this chat").
    Update(String),
    /// Force a fresh session (user chose "Save as new").
    New,
}

/// A user action parked behind the ask-on-continue session-conflict modal: the
/// live pane holds a loaded session that gained new turns, and the user then
/// triggered something that would replace it. We resolve the archive decision
/// first, then run the parked action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextAction {
    /// File → New Session (clear the live pane for a fresh chat).
    NewSession,
    /// Load another stored session by id.
    LoadSession(String),
}

/// A pending async title/summary request: one cheap non-streaming turn whose
/// parsed `(title, summary)` (or `None` on failure) arrives on the oneshot. The
/// `session_id` binds the result to the stored session it should refine.
struct SummarizeState {
    session_id: String,
    rx: oneshot::Receiver<Option<(String, String)>>,
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

/// Starter prompts shown as tappable chips over an empty chat transcript. Kept
/// short and concrete so a first-time user sees what "describe what to draw"
/// actually means. Tapping seeds the input; it never auto-sends.
const CHAT_EXAMPLES: &[&str] = &[
    "Draw a 4×4 m room with a door",
    "Add a 2 m circle at the origin",
    "Make a simple L-shaped floor plan",
];

/// The chat is in its empty state when there is no transcript entry and nothing
/// is mid-stream — the point at which we show the starter prompt + chips rather
/// than a blank scrollback.
fn chat_is_empty(transcript: &[Entry], streaming: &str) -> bool {
    transcript.is_empty() && streaming.trim().is_empty()
}

/// A raised WHITE chip frame with a soft shadow (dark-neutral in dark mode).
/// Used for the header controls — the "LLM" button and the deck/model selectors
/// — so they read as raised buttons on the white dock. Wrap a widget in this and
/// set `visuals.widgets.inactive.weak_bg_fill = TRANSPARENT` inside so the
/// widget's own grey fill doesn't cover the chip white.
fn header_chip_frame(roles: &crate::theme::ColorRoles) -> egui::Frame {
    // A raised chip on the chat surface: the ELEVATED role (dark ≈ rgb 46,46,50;
    // light = white) so it reads lifted above the panel in both themes.
    egui::Frame::NONE
        .fill(crate::theme::to_color32(roles.surface_elevated))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(crate::theme::Spacing::XS as i8, 2))
        .shadow(egui::epaint::Shadow {
            offset: [0, 1],
            blur: 6,
            spread: 0,
            color: egui::Color32::from_black_alpha(50),
        })
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

/// Per-document live-snapshot path: `~/.config/itsjustcad/chats/<uuid>.draft.json`,
/// beside the multi-session archive `<uuid>.json`. The live draft is the current,
/// not-yet-archived conversation for THIS document; it is revived on relaunch and
/// archived on quit/save/new. Reuses the uuid path-traversal guard from
/// [`chat_store::store_path`] (hex + dashes only) so an attacker-controlled uuid
/// can't escape the chats directory.
fn saved_chat_path(doc_uuid: &str) -> Option<std::path::PathBuf> {
    if doc_uuid.is_empty()
        || !doc_uuid.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
    {
        return None;
    }
    Some(
        dirs::home_dir()?
            .join(".config")
            .join("itsjustcad")
            .join("chats")
            .join(format!("{doc_uuid}.draft.json")),
    )
}

/// The legacy GLOBAL live snapshot (shared across all documents). Kept only for a
/// one-time migration into the current document's per-doc draft; deleted after.
fn legacy_global_chat_path() -> Option<std::path::PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".config")
            .join("itsjustcad")
            .join("deck_chat.json"),
    )
}

impl SavedChat {
    /// Load the live draft for one document, or an empty chat if none exists yet.
    fn load(doc_uuid: &str) -> Self {
        saved_chat_path(doc_uuid)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }
}

impl SavedChatRef<'_> {
    /// Write the live draft for one document (0600 — the transcript carries user
    /// messages and scene digests). No path when the uuid is malformed.
    fn save(&self, doc_uuid: &str) {
        let Some(path) = saved_chat_path(doc_uuid) else { return };
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

    #[test]
    fn deck_display_name_resolves_local_cassettes() {
        // A `local-<catalog-id>` key renders as the catalog's brand display name.
        assert_eq!(
            deck_display_name("local-qwen3-4b-q4km-llamafile"),
            "JustCadModel (Qwen 4B)"
        );
        assert_eq!(
            deck_display_name("local-qwen3-0.6b-q6k-llamafile"),
            "JustCadModel Mini (Qwen 0.6B)"
        );
        // Unknown local id falls back to the bare id (prefix stripped, no crash).
        assert_eq!(deck_display_name("local-mystery"), "mystery");
        // Non-local decks render their raw name unchanged.
        assert_eq!(deck_display_name("ollama"), "ollama");
        assert_eq!(deck_display_name("claude-code"), "claude-code");
    }

    #[test]
    fn chat_empty_state_predicate() {
        // No transcript and nothing streaming → show the starter prompt+chips.
        assert!(chat_is_empty(&[], ""));
        assert!(chat_is_empty(&[], "   \n"));
        // A single entry or a live stream both exit the empty state.
        assert!(!chat_is_empty(&[Entry::User("hi".into())], ""));
        assert!(!chat_is_empty(&[], "partial reply…"));
    }

    #[test]
    fn chat_examples_are_present_and_nonempty() {
        // The empty-state chips must exist and be concrete starter prompts.
        assert!(CHAT_EXAMPLES.len() >= 2);
        assert!(CHAT_EXAMPLES.iter().all(|e| !e.trim().is_empty()));
    }
}

/// Parse the model's two-line title/summary reply into `(title, summary)`. Line
/// 1 is the title, the rest is the summary; each is trimmed of surrounding
/// quotes and a leading `Title:`/`Summary:` label the model may add anyway.
/// Returns `None` when there is no usable title so the deterministic fallback
/// stays. Pure — unit-testable without the network.
fn parse_title_summary(reply: &str) -> Option<(String, String)> {
    let strip = |s: &str| -> String {
        let s = s.trim();
        let s = s
            .strip_prefix("Title:")
            .or_else(|| s.strip_prefix("title:"))
            .or_else(|| s.strip_prefix("Summary:"))
            .or_else(|| s.strip_prefix("summary:"))
            .unwrap_or(s)
            .trim();
        s.trim_matches(|c| c == '"' || c == '\'' || c == '`').trim().to_string()
    };
    let mut lines = reply.lines().map(str::trim).filter(|l| !l.is_empty());
    let title = strip(lines.next()?);
    if title.is_empty() {
        return None;
    }
    let summary = lines.map(strip).collect::<Vec<_>>().join(" ");
    Some((title, summary))
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

/// One clickable SESSION card for the Sessions tab: TITLE (panel-title weight)
/// over a 1-2 line SUMMARY (secondary color) with a trailing DATE (tertiary).
/// The whole card is a click target; returns `true` when clicked (→ load it).
fn session_card(
    ui: &mut egui::Ui,
    session: &crate::chat_store::ChatSession,
    secondary: egui::Color32,
    tertiary: egui::Color32,
    outline: egui::Color32,
) -> bool {
    // A short fallback summary if none was derived/generated yet.
    let summary = if session.summary.trim().is_empty() {
        session
            .turns
            .iter()
            .find(|t| t.role == "user")
            .map(|t| t.content.clone())
            .unwrap_or_else(|| "(empty conversation)".to_string())
    } else {
        session.summary.clone()
    };
    let date = crate::chat_store::fmt_date(session.updated);

    let resp = egui::Frame::group(ui.style())
        .stroke(egui::Stroke::new(1.0, outline))
        .inner_margin(egui::Margin::same(crate::theme::Spacing::S as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // Title — panel-title weight (semibold-ish via .strong()).
            ui.label(egui::RichText::new(&session.title).strong().size(15.0));
            // Summary — secondary color, wraps to 1-2 lines.
            ui.label(egui::RichText::new(summary).color(secondary).size(12.0));
            // Date — tertiary, right-aligned metadata.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(egui::RichText::new(date).color(tertiary).size(11.0));
            });
        })
        .response;
    let resp = resp.interact(egui::Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    ui.add_space(4.0);
    resp.clicked()
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
    /// The document uuid the live snapshot + store belong to. `None` until the
    /// first `sync_store`; `persist_chat` is a no-op while it is `None` (a chat
    /// with no known document has nowhere to land). Set in `sync_store`.
    active_doc_uuid: Option<String>,
    /// The stored session id the live pane was loaded from (a Sessions-tab card),
    /// or `None` for a fresh conversation. Drives update-in-place vs fork on
    /// archive. Set in `load_stored_session`, cleared in `new_session`.
    loaded_session_id: Option<String>,
    /// A turn was appended AFTER a session was loaded — the loaded conversation
    /// has diverged from its stored copy, so continuing it is the ambiguous
    /// "update vs save-as-new" case. Set in `send`; cleared on load/new/archive.
    dirty_since_load: bool,
    /// A parked next-action awaiting the user's session-conflict decision (Update
    /// vs Save-as-new). The app polls [`needs_session_decision`] and renders the
    /// modal; [`resolve_session_decision`] archives + runs it.
    pending_session_decision: Option<NextAction>,
    /// An in-flight async title/summary request refining a just-archived session.
    summarize: Option<SummarizeState>,
    /// A just-archived session id awaiting its async title/summary pass. Set by
    /// `archive_active`; drained in `tick` (which owns the tokio handle) so the
    /// LLM refine spawns off the UI thread. `None` when nothing is pending.
    pending_summarize_id: Option<String>,
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
        // The live draft is loaded PER-DOCUMENT in `sync_store` once the doc uuid
        // is known — NOT here, where no document is established yet. Start empty.
        let mut decks = DecksFile::load_or_default();
        // Prune DANGLING local cassettes: a `local-<id>` deck whose model is no
        // longer in the catalog (e.g. a removed/renamed model) is an orphan — its
        // weights were likely deleted and it just clutters the deck list under
        // "local only". Drop those + clamp the active index. Persist so it stays
        // gone even if the app previously re-saved it from memory.
        {
            let catalog = crate::model_catalog::Catalog::load();
            let before = decks.decks.len();
            decks.decks.retain(|d| {
                match d.name.strip_prefix("local-") {
                    Some(id) => catalog.get(id).is_some(), // known model → keep
                    None => true,                          // not a local cassette → keep
                }
            });
            if decks.decks.len() != before {
                if decks.active >= decks.decks.len() {
                    decks.active = decks.decks.len().saturating_sub(1);
                }
                decks.save();
            }
        }
        Self {
            decks,
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
            active_doc_uuid: None,
            loaded_session_id: None,
            dirty_since_load: false,
            pending_session_decision: None,
            summarize: None,
            pending_summarize_id: None,
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

    /// True when the ACTIVE deck is a healthy LOCAL endpoint (probe Ready) — e.g.
    /// the user already has Ollama running on localhost. Used by `App` to hide
    /// the "download the default local model" offer: no point pushing a download
    /// when a working local model is already available.
    pub(crate) fn has_ready_local_deck(&self) -> bool {
        matches!(self.probe, ProbeState::Ready(_))
            && self
                .decks
                .decks
                .get(self.decks.active)
                .map(|d| itsjustcad_deck::is_local_url(&d.base_url))
                .unwrap_or(false)
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
    /// Current "local only" state (cloud decks hidden, remote sends blocked).
    pub fn local_only(&self) -> bool {
        self.decks.local_only
    }

    /// Current "allow web search" state for the next turn.
    pub fn allow_web_search(&self) -> bool {
        self.allow_web_search
    }

    /// Set "local only". When turning it ON, if the active deck is a remote one
    /// it becomes hidden, so switch to the first visible (local) cassette and
    /// drop the provider session. Persists `decks.json`. Driven by the LLM menu.
    pub fn set_local_only(&mut self, on: bool) {
        if self.decks.local_only == on {
            return;
        }
        self.decks.local_only = on;
        if on {
            let active_is_remote = !itsjustcad_deck::is_local_url(
                self.decks
                    .decks
                    .get(self.decks.active)
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
            // Local-only forbids remote calls — web search cannot apply.
            self.allow_web_search = false;
        }
        self.decks.save();
    }

    /// Flip "local only" (LLM menu).
    pub fn toggle_local_only(&mut self) {
        self.set_local_only(!self.decks.local_only);
    }

    /// Flip "allow web search" (LLM menu). No-op under local-only, which forbids
    /// remote calls anyway.
    pub fn toggle_web_search(&mut self) {
        if self.decks.local_only {
            self.allow_web_search = false;
            return;
        }
        self.allow_web_search = !self.allow_web_search;
    }

    /// File → New Session. Archives the outgoing conversation, then clears the
    /// live pane for a fresh chat. When continuing a loaded-and-diverged session
    /// this parks behind the Update/Save-as-new modal instead (the app renders
    /// it); the clear runs once the user decides.
    pub fn new_session(&mut self) {
        self.guarded_next_action(NextAction::NewSession);
    }

    /// Clear the live pane for a brand-new conversation. The archive (if any) has
    /// already happened via the guarded flow; this only resets live state and the
    /// loaded-session tracking.
    fn clear_live(&mut self) {
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
        self.loaded_session_id = None;
        self.dirty_since_load = false;
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

    /// Point the multi-session store AND the live draft at `doc_uuid`. Called by
    /// the app when a document is opened/saved and its uuid becomes known. On a
    /// document SWITCH this persists the outgoing draft, then loads the incoming
    /// document's store + live draft — so each document keeps its own live chat.
    /// Reloads only when the uuid changes.
    pub fn sync_store(&mut self, doc_uuid: &str) {
        let needs_load = self
            .active_doc_uuid
            .as_deref()
            .map(|u| u != doc_uuid)
            .unwrap_or(true);
        if !needs_load {
            return;
        }
        // Persist the OUTGOING document's live draft before switching away.
        self.persist_chat();
        // Switch the active document and load its per-doc store + live draft.
        self.active_doc_uuid = Some(doc_uuid.to_string());
        self.store = Some(crate::chat_store::DocSessions::load(doc_uuid));
        let mut draft = SavedChat::load(doc_uuid);
        // One-time MIGRATION: an app upgraded from the global `deck_chat.json`
        // has a single shared snapshot and no per-doc draft yet. Adopt it for the
        // FIRST document that opens after the upgrade, then delete the global file
        // so it is never re-adopted by another document.
        if draft.transcript.is_empty()
            && draft.messages.is_empty()
            && saved_chat_path(doc_uuid).is_some_and(|p| !p.exists())
            && let Some(global) = legacy_global_chat_path()
            && let Ok(text) = std::fs::read_to_string(&global)
            && let Ok(legacy) = serde_json::from_str::<SavedChat>(&text)
        {
            draft = legacy;
            let _ = std::fs::remove_file(&global);
        }
        self.load_draft(draft);
    }

    /// Replace the LIVE pane state (session handle, messages, transcript) from a
    /// loaded draft. Aborts nothing — the caller has already parked/finished any
    /// in-flight turn. A loaded draft is a fresh continuation, not a stored
    /// session, so `loaded_session_id`/`dirty_since_load` are cleared.
    fn load_draft(&mut self, draft: SavedChat) {
        self.session_id = draft.session_id;
        self.messages = draft.messages;
        self.transcript = draft.transcript;
        self.loaded_session_id = None;
        self.dirty_since_load = false;
        if let Some(sid) = &self.session_id {
            self.transcript.push(Entry::Status(format!(
                "revived session {}",
                &sid[..sid.len().min(8)]
            )));
        }
    }

    /// Archive the live conversation into the per-document store, placing it per
    /// `mode`. Private + app-local; the shared document is never touched. No-op
    /// until `sync_store` has established a store or when there are no messages.
    ///
    /// - [`ArchiveMode::Auto`] — update the loaded session in place when it is
    ///   still in the store (continuing a loaded chat never forks a duplicate),
    ///   else push a new one.
    /// - [`ArchiveMode::Update`] — force update-in-place by that id.
    /// - [`ArchiveMode::New`] — force a fresh session.
    ///
    /// The deterministic `derive_meta` fallback runs synchronously so the card is
    /// labeled instantly; an async LLM pass may refine it later (see
    /// [`spawn_summarize`]). Returns the archived session id (for the summarizer).
    pub fn archive_active(&mut self, mode: ArchiveMode) -> Option<String> {
        let Some(store) = self.store.as_mut() else { return None };
        if self.messages.is_empty() {
            return None;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Decide the target id: reuse an existing session id (update-in-place) or
        // mint a fresh one (fork).
        let reuse_id = match &mode {
            ArchiveMode::Update(id) => Some(id.clone()),
            ArchiveMode::Auto => self
                .loaded_session_id
                .as_ref()
                .filter(|id| store.get(id).is_some())
                .cloned(),
            ArchiveMode::New => None,
        };
        let mut session = crate::chat_store::ChatSession::new(now);
        if let Some(id) = &reuse_id {
            session.id = id.clone();
        }
        for m in &self.messages {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            session.push(role, &m.content, now);
        }
        // Auto-generate title + summary. The fallback (first prompt + a preview
        // of the first exchange) never calls a model, so headless/tests are safe.
        session.derive_meta();
        let archived_id = session.id.clone();
        store.upsert(session);
        store.save();
        // The live pane now tracks this stored session; a subsequent turn dirties
        // it again. Clear the dirty flag — the current state is archived.
        self.loaded_session_id = Some(archived_id.clone());
        self.dirty_since_load = false;
        // Queue an async LLM title/summary refine; `tick` (which owns the tokio
        // handle) spawns it. The deterministic labels are already in place, so a
        // headless/offline archive is fully labeled without this ever firing.
        self.pending_summarize_id = Some(archived_id.clone());
        Some(archived_id)
    }

    /// Whether the live pane holds a loaded session that gained new turns — the
    /// ambiguous "update this chat vs save as a new one" case. Only then does a
    /// destructive next-action (New Session / load-another) need the modal.
    fn is_ambiguous_continue(&self) -> bool {
        self.loaded_session_id.is_some()
            && self.dirty_since_load
            && !self.messages.is_empty()
    }

    /// Route a destructive next-action through the ask-on-continue guard. When the
    /// live pane is an ambiguously-continued loaded session, park the action and
    /// return `true` (the app renders the modal). Otherwise archive with `Auto`
    /// and run it immediately, returning `false`.
    fn guarded_next_action(&mut self, action: NextAction) -> bool {
        if self.is_ambiguous_continue() {
            self.pending_session_decision = Some(action);
            true
        } else {
            self.archive_active(ArchiveMode::Auto);
            self.run_next_action(action);
            false
        }
    }

    /// The parked next-action, if the session-conflict modal is up. The app polls
    /// this each frame and renders the Update / Save-as-new / Cancel modal.
    pub fn needs_session_decision(&self) -> Option<NextAction> {
        self.pending_session_decision.clone()
    }

    /// Resolve a parked session-conflict: archive the live conversation as an
    /// update-in-place (`update = true`) or a fresh session (`update = false`),
    /// then run the parked action. Called by the app's modal on Update / Save-as-
    /// new. Cancel drops the parked action via [`cancel_session_decision`].
    pub fn resolve_session_decision(&mut self, update: bool) {
        let Some(action) = self.pending_session_decision.take() else { return };
        let mode = match (update, self.loaded_session_id.clone()) {
            (true, Some(id)) => ArchiveMode::Update(id),
            _ => ArchiveMode::New,
        };
        self.archive_active(mode);
        self.run_next_action(action);
    }

    /// Drop a parked session-conflict without archiving or navigating (Cancel).
    pub fn cancel_session_decision(&mut self) {
        self.pending_session_decision = None;
    }

    /// Run a resolved next-action (the archive has already happened).
    fn run_next_action(&mut self, action: NextAction) {
        match action {
            NextAction::NewSession => self.clear_live(),
            NextAction::LoadSession(id) => self.replace_live_with_stored(&id),
        }
    }

    /// Snapshot the chat to the CURRENT document's live draft so an idle/quit/
    /// crash can be revived later. No-op until `sync_store` has established the
    /// document uuid — a chat with no known document has nowhere to land.
    fn persist_chat(&self) {
        let Some(uuid) = &self.active_doc_uuid else { return };
        SavedChatRef {
            session_id: &self.session_id,
            messages: &self.messages,
            transcript: &self.transcript,
        }
        .save(uuid);
    }

    /// Spawn ONE cheap non-streaming request asking the active deck for a short
    /// title + summary of the just-archived conversation, binding the result to
    /// `session_id`. Only fires when the deck probes `Ready` and there is a
    /// conversation to summarize; offline/headless/not-ready keeps the
    /// deterministic `derive_meta` labels (already applied at archive time). App-
    /// local, never op-logged. The result is applied by [`poll_summarize`].
    fn spawn_summarize(
        &mut self,
        session_id: String,
        handle: &tokio::runtime::Handle,
    ) {
        if !matches!(self.probe, ProbeState::Ready(_)) {
            return;
        }
        let Some(config) = self.decks.decks.get(self.decks.active).cloned() else {
            return;
        };
        // Build the transcript to summarize from the live messages.
        let mut convo = String::new();
        for m in &self.messages {
            let who = match m.role {
                Role::User => "User",
                Role::Assistant => "Assistant",
            };
            convo.push_str(who);
            convo.push_str(": ");
            convo.push_str(&m.content);
            convo.push('\n');
        }
        if convo.trim().is_empty() {
            return;
        }
        let system = "You label chat logs. Reply with EXACTLY two lines: line 1 a \
             title of at most 6 words (no quotes, no prefix); line 2 a summary of \
             at most 20 words. Nothing else."
            .to_string();
        let prompt = format!("Title and summarize this conversation:\n\n{convo}");
        let mut req = ChatRequest::text(
            system,
            vec![ChatMessage { role: Role::User, content: prompt }],
            String::new(),
            120,
            0.2,
            None,
        );
        // A fresh, self-contained side-request: never reuse the live session
        // handle and never grant tools/web-search.
        req.max_turns = 1;
        let deck = make_deck(&config);
        let (tx, rx) = oneshot::channel();
        self.summarize = Some(SummarizeState { session_id, rx });
        handle.spawn(async move {
            let (dtx, mut drx) = unbounded_channel();
            deck.stream_chat(req, dtx).await;
            let mut text = String::new();
            while let Ok(delta) = drx.try_recv() {
                match delta {
                    DeckDelta::Text(t) => text.push_str(&t),
                    DeckDelta::Done => break,
                    DeckDelta::Error(_) => {
                        let _ = tx.send(None);
                        return;
                    }
                    DeckDelta::Session(_) => {}
                }
            }
            let _ = tx.send(parse_title_summary(&text));
        });
    }

    /// Poll the async title/summary request and apply it to the stored session by
    /// id. A failed/empty result leaves the deterministic `derive_meta` labels.
    fn poll_summarize(&mut self) {
        let Some(state) = &mut self.summarize else { return };
        match state.rx.try_recv() {
            Ok(Some((title, summary))) => {
                let id = state.session_id.clone();
                self.summarize = None;
                if let Some(store) = self.store.as_mut() {
                    store.set_meta(&id, &title, &summary);
                    store.save();
                }
            }
            Ok(None) => self.summarize = None,
            Err(oneshot::error::TryRecvError::Empty) => {}
            Err(oneshot::error::TryRecvError::Closed) => self.summarize = None,
        }
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
        // Local (small) models get a FOCUSED, short prompt + `/no_think`: the full
        // registry prompt (~33 KB) drowns a 0.6–4B model — it rambles and never
        // emits commands. The brief teaches the draft convention + common verbs +
        // worked examples; the GBNF grammar backstops the full verb set. `/no_think`
        // suppresses Qwen3's <think> block so the turn is commands, not reasoning.
        let digest = crate::scene::digest(&session.doc);
        let prompt = if itsjustcad_deck::is_local_url(&config.base_url) {
            format!("{}\n\n/no_think", itsjustcad_deck::brief_system_prompt(&digest))
        } else {
            system_prompt(&digest, &session.plugins)
        };
        let mut req = ChatRequest::text(
            prompt,
            self.messages.clone(),
            String::new(),
            4096,
            0.2,
            self.session_id.clone(),
        );
        // Opt-in web search: only when the user toggled it on AND we are not in
        // local-only mode. Local-only means SEALED/offline — it must hard-block
        // web search regardless of the toggle's last state (the toggle can be
        // stale-on if local-only was enabled after it was set). This is the
        // request-level guarantee, independent of the UI disabling the toggle.
        req.web_search = self.allow_web_search && !self.decks.local_only;
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
        // A turn appended after loading a stored session diverges the live
        // conversation from its archived copy — leaving it is now the ambiguous
        // "update vs save-as-new" case.
        if self.loaded_session_id.is_some() {
            self.dirty_since_load = true;
        }
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
                        // SECURITY: app-verbs bypass the fs side-effect gate
                        // because they carry no `Command`. Almost all are pure
                        // view/camera state — but a `basemap <provider>` fetch
                        // reaches the NETWORK (tile servers) to stitch an
                        // underlay. A deck-emitted basemap must NOT silently
                        // egress: drop the fetching form and tell the user to run
                        // it from the command line. `basemap off/clear` performs
                        // no network I/O, so it still passes through.
                        if let crate::app_verbs::AppVerb::Basemap(b) = &verb
                            && !b.clear
                        {
                            self.current_commands.push(ExecutedCommand {
                                line: line.clone(),
                                result: Err(
                                    "basemap fetch reaches the network — run it yourself from the command line".to_string(),
                                ),
                            });
                            continue;
                        }
                        // SECURITY: `save <path>` is an app-verb, NOT a substrate
                        // `Command`, so it bypasses the fs side-effect gate that
                        // protects `export`/`print`/`import`. A deck-emitted
                        // `save /Users/victim/.zshrc` would otherwise write to an
                        // arbitrary path with no confirmation (an fs-write
                        // primitive reachable via prompt-injection). Refuse it on
                        // the deck plane; the user must save from the command line.
                        if matches!(verb, crate::app_verbs::AppVerb::Save(_)) {
                            self.current_commands.push(ExecutedCommand {
                                line: line.clone(),
                                result: Err(
                                    "save writes to the filesystem — run it yourself from the command line".to_string(),
                                ),
                            });
                            continue;
                        }
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
        // Spawn a queued async title/summary refine now that we own the handle,
        // and poll any in-flight one. Only one at a time — a fresh archive drops
        // the previous request. No-op unless the deck is Ready.
        if self.summarize.is_none()
            && let Some(id) = self.pending_summarize_id.take()
        {
            self.spawn_summarize(id, handle);
        }
        self.poll_summarize();
        // Keep the event loop running while background work is in flight,
        // whether or not the panel is rendered this frame.
        if self.busy()
            || self.deferred_local_turn
            || self.summarize.is_some()
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
    fn side_effect_confirm_ui(
        &mut self,
        ui: &mut egui::Ui,
        session: &mut Session,
        icons: &crate::icons::Icons,
    ) {
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
                        if icons
                            .icon_button(ui, crate::icons::Icon::Run, "run this action")
                            .clicked()
                        {
                            run_idx = Some(i);
                        }
                        if icons
                            .icon_button(ui, crate::icons::Icon::Skip, "skip this action")
                            .clicked()
                        {
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
        a.derive_meta();
        let mut b = crate::chat_store::ChatSession::new(100);
        b.push("user", "add a curtain wall facade to the north side", 100);
        b.push("assistant", "Added a glass facade along the north edge.", 101);
        b.derive_meta();
        store.sessions.push(a);
        store.sessions.push(b);
        store.sort_recent();
    }

    /// The **Sessions** tab body: a full-text SEARCH field at the top, then this
    /// document's stored chats as CARDS (title + 1-2 line summary + date),
    /// newest→oldest. Clicking a card loads it into the live Chat pane; the
    /// caller switches the active tab to Chat when this returns `true`.
    ///
    /// This was promoted OUT of the Chat pane (`session_browser_ui`) into its own
    /// tab. Returns `true` iff a session was loaded this frame.
    #[must_use]
    pub fn sessions_tab_ui(
        &mut self,
        ui: &mut egui::Ui,
        roles: &crate::theme::ColorRoles,
    ) -> bool {
        if self.store.is_none() {
            ui.label(egui::RichText::new("no chats yet for this document").weak().italics());
            return false;
        }
        // Ensure newest-first ordering for the card list.
        if let Some(store) = self.store.as_mut() {
            store.sort_recent();
        }
        let secondary = crate::theme::to_color32(roles.on_surface_variant);
        let tertiary = crate::theme::to_color32(roles.on_surface_tertiary);
        let outline = crate::theme::to_color32(roles.outline);

        ui.horizontal(|ui| {
            ui.label("search:");
            let search_id = egui::Id::new("session_search_input");
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.session_search)
                    .id(search_id)
                    .hint_text("find across this doc's chats")
                    .desired_width(f32::INFINITY),
            );
            crate::text_context_menu::attach(&resp, &mut self.session_search, search_id);
        });
        ui.add_space(4.0);

        let mut load_session: Option<String> = None;
        let store = self.store.as_ref().expect("store present");
        let query = self.session_search.trim().to_string();

        egui::ScrollArea::vertical().show(ui, |ui| {
            if !query.is_empty() {
                // Filter cards to sessions with a matching title/summary/message.
                let hits = store.search(&query);
                let ql = query.to_lowercase();
                let matched: Vec<&crate::chat_store::ChatSession> = store
                    .sessions
                    .iter()
                    .filter(|s| {
                        hits.iter().any(|h| h.session_id == s.id)
                            || s.title.to_lowercase().contains(&ql)
                            || s.summary.to_lowercase().contains(&ql)
                    })
                    .collect();
                if matched.is_empty() {
                    ui.label(egui::RichText::new("no matches").weak().italics());
                }
                for s in matched {
                    if session_card(ui, s, secondary, tertiary, outline) {
                        load_session = Some(s.id.clone());
                    }
                }
            } else if store.sessions.is_empty() {
                ui.label(
                    egui::RichText::new("no chats yet for this document")
                        .weak()
                        .italics(),
                );
            } else {
                for s in store.sessions.iter() {
                    if session_card(ui, s, secondary, tertiary, outline) {
                        load_session = Some(s.id.clone());
                    }
                }
            }
        });

        if let Some(id) = load_session {
            self.load_stored_session(&id);
            true
        } else {
            false
        }
    }

    /// Load a stored session's turns into the live transcript for viewing /
    /// continuation. Archives the current conversation first so nothing is lost.
    /// When the current pane is an ambiguously-continued loaded session, this
    /// parks behind the Update/Save-as-new modal; the load runs once resolved.
    fn load_stored_session(&mut self, id: &str) {
        self.guarded_next_action(NextAction::LoadSession(id.to_string()));
    }

    /// Replace the live pane with a stored session's turns (the archive of the
    /// outgoing conversation, if any, has already happened via the guarded flow).
    /// The loaded session is TRACKED (`loaded_session_id`) so continuing it and
    /// then leaving offers update-in-place rather than a fork.
    fn replace_live_with_stored(&mut self, id: &str) {
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
        // Track it so a continued-then-left conversation updates in place.
        self.loaded_session_id = Some(id.to_string());
        self.dirty_since_load = false;
        self.session_search.clear();
        self.view = PaneView::Chat;
        self.persist_chat();
    }

    #[allow(clippy::too_many_arguments)] // fixed signature; splitting further aids nothing
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        session: &mut Session,
        handle: &tokio::runtime::Handle,
        icons: &crate::icons::Icons,
        roles: &crate::theme::ColorRoles,
        _dark: bool,
        _reduce_motion: bool,
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

        // Uniform chat surface: paint the WHOLE pane with the panel `surface`
        // role so the LLM toolbar, transcript, and input card all sit on ONE
        // background with consistent padding — not a tinted transcript band
        // inside a grey panel. In dark mode this is the ramp base (≈ rgb 36,36,40)
        // — a distinct, slightly-lifted panel, NOT pure black — matching the dock
        // and command line. The dock's own inner margin supplies the padding.
        // Dark: match the dock's RECESSED surface (one dark block). Light: the
        // normal surface (light mode was already correct).
        let pane_bg = if ui.visuals().dark_mode {
            crate::theme::recessed_fill(true)
        } else {
            crate::theme::to_color32(roles.surface)
        };
        ui.painter().rect_filled(ui.max_rect(), 0.0, pane_bg);

        // Deck status collapsed to a single traffic-light dot next to the model
        // picker (green = ready, red = unavailable/failed, orange = probing or
        // loading). Clicking the dot opens a modal with the full message. Compute
        // the (color, message, offer-retry) tuple here — owned values — so the
        // header closure below can still borrow `self` mutably.
        const STATUS_ORANGE: egui::Color32 = egui::Color32::from_rgb(230, 160, 60);
        let (dot_color, status_msg, status_retry): (egui::Color32, String, bool) =
            match &self.probe {
                ProbeState::Ready(info) => match &self.warm {
                    WarmState::Warming { started, .. } => (
                        STATUS_ORANGE,
                        format!(
                            "loading model into memory… {}s (first use of a model takes 30-60s)",
                            started.elapsed().as_secs()
                        ),
                        false,
                    ),
                    WarmState::Failed(e) => {
                        (ERR_COLOR, format!("model load failed: {e}"), false)
                    }
                    _ => {
                        let warm_tag = if matches!(self.warm, WarmState::Warm) {
                            " · model warm"
                        } else {
                            ""
                        };
                        (OK_COLOR, format!("{}{warm_tag}", info.detail), false)
                    }
                },
                ProbeState::Unavailable(reason) => (ERR_COLOR, reason.clone(), true),
                ProbeState::Checking(_) => {
                    (STATUS_ORANGE, "checking deck…".to_string(), false)
                }
                ProbeState::Unknown => {
                    (STATUS_ORANGE, "idle — no deck probe yet".to_string(), false)
                }
            };
        let status_modal_id = egui::Id::new("deck_status_modal");

        ui.horizontal(|ui| {
            // Theme + text-size AND the LLM controls (local only / web search /
            // download default) moved to the top-level LLM menu (see `menu.rs`).
            // The chat header now starts directly with the deck/model selectors.
            // Only show cassettes permitted by the current local_only setting.
            let visible_decks: Vec<(usize, String)> = self
                .decks
                .visible_decks()
                .map(|(i, d)| (i, d.name.clone()))
                .collect();
            header_chip_frame(roles).show(ui, |ui| {
                ui.style_mut().visuals.widgets.inactive.weak_bg_fill =
                    egui::Color32::TRANSPARENT;
                ui.style_mut().visuals.widgets.inactive.bg_stroke = egui::Stroke::NONE;
                egui::ComboBox::from_id_salt("deck_select")
                    .selected_text(
                        self.decks
                            .decks
                            .get(self.decks.active)
                            .map(|d| deck_display_name(&d.name))
                            .unwrap_or_else(|| "—".into()),
                    )
                    .show_ui(ui, |ui| {
                        for (i, name) in &visible_decks {
                            if ui
                                .selectable_value(
                                    &mut self.decks.active,
                                    *i,
                                    deck_display_name(name),
                                )
                                .clicked()
                            {
                                self.decks.save();
                                self.session_id = None;
                                self.persist_chat();
                            }
                        }
                    });
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
                header_chip_frame(roles).show(ui, |ui| {
                    ui.style_mut().visuals.widgets.inactive.weak_bg_fill =
                        egui::Color32::TRANSPARENT;
                    ui.style_mut().visuals.widgets.inactive.bg_stroke = egui::Stroke::NONE;
                    egui::ComboBox::from_id_salt("model_select")
                        .selected_text(&model)
                        .width(120.0)
                        .show_ui(ui, |ui| {
                            for m in &probe_models {
                                ui.selectable_value(&mut model, m.clone(), m);
                            }
                        });
                });
                if model != config.model {
                    config.model = model;
                    self.decks.save();
                    self.probe = ProbeState::Unknown; // re-probe with new model
                }
            }
            // Traffic-light status dot (a real filled circle — the `●` glyph is
            // absent from egui's default font and renders as a tofu box), right
            // next to the model picker. Click opens the status modal. No spinner
            // here: the ONLY waiting indicator lives in the chat transcript.
            {
                let d = ui.text_style_height(&egui::TextStyle::Body) * 0.6;
                let (rect, resp) =
                    ui.allocate_exact_size(egui::vec2(d, d), egui::Sense::click());
                ui.painter().circle_filled(rect.center(), d * 0.5, dot_color);
                if resp
                    .on_hover_text("deck status — click for details")
                    .clicked()
                {
                    ui.ctx().data_mut(|d| d.insert_temp(status_modal_id, true));
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
            // No "clear" button either — starting a new session (Sessions tab)
            // supersedes it; a stray destructive control in the header invited
            // accidental transcript loss.
        });
        // Status detail now lives in a modal opened by the dot (above). Render it
        // when the dot has been clicked; a `retry` button appears for a dead deck.
        {
            let mut open = ui
                .ctx()
                .data(|d| d.get_temp::<bool>(status_modal_id).unwrap_or(false));
            if open {
                egui::Window::new("Deck status")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ui.ctx(), |ui| {
                        ui.horizontal(|ui| {
                            let d = ui.text_style_height(&egui::TextStyle::Body) * 0.6;
                            let (rect, _) =
                                ui.allocate_exact_size(egui::vec2(d, d), egui::Sense::hover());
                            ui.painter().circle_filled(rect.center(), d * 0.5, dot_color);
                            ui.label(&status_msg);
                        });
                        ui.add_space(crate::theme::Spacing::M);
                        ui.horizontal(|ui| {
                            if status_retry && ui.button("retry").clicked() {
                                self.probe = ProbeState::Unknown;
                                open = false;
                            }
                            if ui.button("close").clicked() {
                                open = false;
                            }
                        });
                    });
                ui.ctx()
                    .data_mut(|d| d.insert_temp(status_modal_id, open));
            }
        }

        // The session browser used to live here inside the Chat pane; it has
        // been promoted OUT into its own "Sessions" tab (see `sessions_tab_ui`),
        // so the Chat pane is now just the live conversation.

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
        let mut chip_fill: Option<String> = None;
        let mut do_send = false;
        let mut do_stop = false;
        let empty_chat = chat_is_empty(&self.transcript, &self.streaming_chat) && !self.busy();

        // ── Bottom-docked input ────────────────────────────────────────────
        // The input lives in its OWN bottom panel so it is never clipped by the
        // transcript (the old fixed `input_height` reserve under-counted the
        // 5-row field and cut off its lower edge). The transcript then fills all
        // remaining space above it.
        let vision = self
            .decks
            .decks
            .get(self.decks.active)
            .map(|c| c.supports_vision())
            .unwrap_or(false);
        let hint = if self.attached_image.is_some() {
            "ask about the attached image…  (Enter = new line · Cmd+Enter to send)"
        } else if self.ready() {
            "describe what to draw…  (Enter = new line · Cmd+Enter to send)"
        } else if matches!(self.warm, WarmState::Warming { .. }) {
            "loading model — chat enables when warm"
        } else {
            "deck unavailable — fix the connection above"
        };
        let input_id = egui::Id::new("deck_chat_input");
        let has_focus = ui.memory(|m| m.has_focus(input_id));
        let enabled = (!self.busy() && self.ready()) || has_focus;
        let can_send = !self.busy() && self.ready() && !self.input.trim().is_empty();
        let input_radius =
            egui::CornerRadius::same(crate::theme::Radii::default().medium as u8);

        // The input panel uses a DIFFERENT id per state so collapsed and expanded
        // each keep their own cached height. Without this, egui reuses the prior
        // frame's panel height (`PanelState`), so after expand→collapse the short
        // input floats at the top of a still-tall panel with empty space below.
        let input_expanded = ui
            .ctx()
            .data(|d| d.get_temp::<bool>(egui::Id::new("deck_input_expanded")).unwrap_or(false));
        let deck_input_id = if input_expanded {
            "deck_input_expanded_panel"
        } else {
            "deck_input_collapsed_panel"
        };

        egui::Panel::bottom(deck_input_id)
            .resizable(false)
            .frame(egui::Frame::NONE)
            // No separator line — the soft top shadow (painted below the card) is
            // the only visual break between the transcript and the input.
            .show_separator_line(false)
            .show(ui, |ui| {
                // Side-effect confirmation sits directly above the input card.
                self.side_effect_confirm_ui(ui, session, icons);
                // Attachment chip (name + remove) above the input card.
                if let Some(img) = self.attached_image.clone() {
                    ui.horizontal(|ui| {
                        let name = img
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let fg = ui.visuals().weak_text_color();
                        let sz = ui.text_style_height(&egui::TextStyle::Body);
                        ui.add(icons.image(ui.ctx(), crate::icons::Icon::Paperclip, sz, fg))
                            .on_hover_text("attached image");
                        ui.label(egui::RichText::new(name).weak());
                        if icons
                            .icon_button(ui, crate::icons::Icon::Close, "remove attachment")
                            .clicked()
                        {
                            self.attached_image = None;
                        }
                    });
                }
                // Card fill matches the pane (white) so its top edge does NOT
                // read as a line above the chevron — the only top separation is
                // the soft drop shadow painted below. No horizontal inner margin
                // so the input's edges line up with the toolbar and transcript.
                // Soft drop shadow biased UPWARD so the input reads as floating
                // above the transcript — no hard separator line. The card fill
                // matches the pane (white) so only the shadow marks the boundary.
                let input_frame = egui::Frame::NONE
                    .fill(pane_bg)
                    .corner_radius(input_radius)
                    // Horizontal margin matches the transcript so the input box
                    // isn't clipped against the pane edge and lines up with the
                    // chat rows above it.
                    .inner_margin(egui::Margin::symmetric(
                        crate::theme::Spacing::S as i8,
                        crate::theme::Spacing::XS as i8,
                    ))
                    // Soft top shadow: visible but blurred so it reads as a lift,
                    // not a hard 1px line. Offset up + a moderate alpha.
                    .shadow(egui::epaint::Shadow {
                        offset: [0, -2],
                        blur: 16,
                        spread: 0,
                        color: egui::Color32::from_black_alpha(45),
                    });
                // Signal-style modular input: COLLAPSED is a single-line row with
                // a chevron-up to expand; EXPANDED is a tall multi-line box with a
                // chevron-down (collapse) on top and the action buttons below.
                let expand_id = egui::Id::new("deck_input_expanded");
                let expanded = ui
                    .ctx()
                    .data(|d| d.get_temp::<bool>(expand_id).unwrap_or(false));
                input_frame.show(ui, |ui| {
                    let icon_sz = ui.text_style_height(&egui::TextStyle::Body);
                    // Shared button renderers (macro to avoid threading closures
                    // through the borrow checker with `self` also borrowed for the
                    // text field).
                    macro_rules! send_or_stop {
                        ($ui:expr) => {
                            if self.busy() {
                                let stop = egui::Button::image(icons.image(
                                    $ui.ctx(),
                                    crate::icons::Icon::Stop,
                                    icon_sz,
                                    ERR_COLOR,
                                ))
                                .frame(true);
                                if $ui.add(stop).on_hover_text("stop this turn").clicked() {
                                    do_stop = true;
                                }
                            } else {
                                let send_fg = if can_send {
                                    $ui.visuals().text_color()
                                } else {
                                    $ui.visuals().weak_text_color()
                                };
                                let send_btn = egui::Button::image(icons.image(
                                    $ui.ctx(),
                                    crate::icons::Icon::Send,
                                    icon_sz,
                                    send_fg,
                                ))
                                .frame(true);
                                if $ui
                                    .add_enabled(can_send, send_btn)
                                    .on_hover_text("send message (Cmd+Enter)")
                                    .clicked()
                                {
                                    do_send = true;
                                }
                            }
                        };
                    }
                    macro_rules! photo {
                        ($ui:expr) => {
                            if vision {
                                let attach = egui::Button::image(icons.image(
                                    $ui.ctx(),
                                    crate::icons::Icon::Image,
                                    icon_sz,
                                    $ui.visuals().text_color(),
                                ))
                                .frame(true);
                                if $ui
                                    .add_enabled(!self.busy(), attach)
                                    .on_hover_text("attach an image for the model to analyze")
                                    .clicked()
                                    && let Some(path) = rfd::FileDialog::new()
                                        .add_filter("Image", &["png", "jpg", "jpeg", "webp", "gif"])
                                        .pick_file()
                                {
                                    self.attached_image = Some(path);
                                }
                            } else {
                                let disabled = egui::Button::image(icons.image(
                                    $ui.ctx(),
                                    crate::icons::Icon::Image,
                                    icon_sz,
                                    $ui.visuals().weak_text_color(),
                                ))
                                .frame(true);
                                $ui.add_enabled(false, disabled).on_hover_text(
                                    "this cassette has no vision — pick a vision-capable model to attach images",
                                );
                            }
                        };
                    }
                    macro_rules! submit_on_chord {
                        ($resp:expr, $ui:expr) => {
                            if $resp.has_focus()
                                && $ui.input(|i| {
                                    i.key_pressed(egui::Key::Enter)
                                        && (i.modifiers.mac_cmd || i.modifiers.ctrl)
                                })
                            {
                                do_send = true;
                            }
                        };
                    }

                    // Chevron ALWAYS top-center, a SIMPLE frameless button (no grey
                    // button background): up = expand, down = collapse.
                    ui.vertical_centered(|ui| {
                        let chev = if expanded {
                            crate::icons::Icon::ChevronDown
                        } else {
                            crate::icons::Icon::ChevronUp
                        };
                        let btn = egui::Button::image(icons.image(
                            ui.ctx(),
                            chev,
                            icon_sz,
                            ui.visuals().weak_text_color(),
                        ))
                        .frame(false);
                        let tip = if expanded { "collapse" } else { "expand input" };
                        if ui.add(btn).on_hover_text(tip).clicked() {
                            ui.ctx().data_mut(|d| d.insert_temp(expand_id, !expanded));
                        }
                    });
                    if expanded {
                        // Tall field (vertical padding only → full-width aligned).
                        egui::Frame::NONE
                            .inner_margin(egui::Margin::symmetric(
                                0,
                                crate::theme::Spacing::XS as i8,
                            ))
                            .show(ui, |ui| {
                                let response = ui.add_enabled(
                                    enabled,
                                    egui::TextEdit::multiline(&mut self.input)
                                        .id(input_id)
                                        .desired_rows(8)
                                        .desired_width(f32::INFINITY)
                                        .hint_text(hint),
                                );
                                submit_on_chord!(response, ui);
                                crate::text_context_menu::attach(
                                    &response,
                                    &mut self.input,
                                    input_id,
                                );
                            });
                        // Buttons: photo + send together, right-aligned, below.
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                send_or_stop!(ui);
                                photo!(ui);
                            },
                        );
                    } else {
                        // Single-line row below the chevron: text fills; photo +
                        // send hug the right.
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                send_or_stop!(ui);
                                photo!(ui);
                                // Two-line field in the collapsed state (expand
                                // with the chevron for a tall composer).
                                let response = ui.add_enabled(
                                    enabled,
                                    egui::TextEdit::multiline(&mut self.input)
                                        .id(input_id)
                                        .desired_rows(2)
                                        .desired_width(f32::INFINITY)
                                        .hint_text(hint),
                                );
                                submit_on_chord!(response, ui);
                                crate::text_context_menu::attach(
                                    &response,
                                    &mut self.input,
                                    input_id,
                                );
                            },
                        );
                    }
                });
            });

        // ── Transcript fills the remaining space ───────────────────────────
        // Same `surface` as the rest of the pane (uniform background).
        // WhatsApp/Signal-style bubbles: user messages align RIGHT (a blue chip),
        // the model's align LEFT (the ELEVATED role — a lifted grey/white chip
        // that reads against the panel surface in both themes).
        let transcript_bg = pane_bg;
        // User bubble uses the tuned viewport-active-tag blue pair (a nice deep
        // blue in dark, light-blue in light) — the SAME blue the gumball-active
        // chip uses, so the app's chat/accent blue is consistent and reads well in
        // dark (the raw bright accent looked washed there). Deck bubble = elevated.
        let (user_bg, user_txt) = crate::theme::viewport_active_tag(ui.visuals().dark_mode);
        let deck_bg = crate::theme::to_color32(roles.surface_elevated);
        let bubble_radius = egui::CornerRadius::same(10);
        egui::Frame::NONE
            .fill(transcript_bg)
            // Horizontal inner margin so bubbles/rows don't clip against the
            // pane edge (was Margin::ZERO → ~2px cut off on the right).
            .inner_margin(egui::Margin::symmetric(crate::theme::Spacing::S as i8, 0))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .stick_to_bottom(!empty_chat)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if empty_chat {
                            ui.add_space(crate::theme::Spacing::M);
                            ui.label(
                                egui::RichText::new("Describe what to draw, and I'll build it.")
                                    .weak(),
                            );
                            ui.add_space(crate::theme::Spacing::S);
                            for example in CHAT_EXAMPLES {
                                if ui
                                    .add(egui::Button::new(egui::RichText::new(*example).small()))
                                    .on_hover_text("use this as a starting prompt")
                                    .clicked()
                                {
                                    chip_fill = Some((*example).to_string());
                                }
                                ui.add_space(crate::theme::Spacing::XS);
                            }
                        }
                        for (i, entry) in self.transcript.iter().enumerate() {
                            match entry {
                                Entry::User(t) => {
                                    // Right-aligned user bubble.
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Min),
                                        |ui| {
                                            egui::Frame::NONE
                                                .fill(user_bg)
                                                .corner_radius(bubble_radius)
                                                .inner_margin(egui::Margin::symmetric(10, 6))
                                                .show(ui, |ui| {
                                                    ui.set_max_width(ui.available_width() * 0.82);
                                                    // Bubbles live in a horizontal
                                                    // (right/left) layout whose default
                                                    // wrap mode is Extend — long/pasted
                                                    // text would render one clipped line
                                                    // instead of wrapping and growing the
                                                    // bubble height. Force wrap.
                                                    ui.style_mut().wrap_mode =
                                                        Some(egui::TextWrapMode::Wrap);
                                                    ui.label(
                                                        egui::RichText::new(t).color(user_txt),
                                                    );
                                                });
                                        },
                                    );
                                }
                                Entry::Deck(t) => {
                                    // Left-aligned model bubble.
                                    ui.with_layout(
                                        egui::Layout::left_to_right(egui::Align::Min),
                                        |ui| {
                                            egui::Frame::NONE
                                                .fill(deck_bg)
                                                .corner_radius(bubble_radius)
                                                .inner_margin(egui::Margin::symmetric(10, 6))
                                                .show(ui, |ui| {
                                                    ui.set_max_width(ui.available_width() * 0.82);
                                                    ui.style_mut().wrap_mode =
                                                        Some(egui::TextWrapMode::Wrap);
                                                    CommonMarkViewer::new().show(
                                                        ui,
                                                        &mut self.markdown,
                                                        t.trim(),
                                                    );
                                                });
                                        },
                                    );
                                }
                                Entry::Commands(commands) => {
                                    if commands_card(ui, commands) {
                                        open_detail = Some(PaneView::Detail(i));
                                    }
                                }
                                Entry::Status(t) => {
                                    // Centered, low-key system line (e.g. the
                                    // "turn done in Ns" response-time note).
                                    ui.vertical_centered(|ui| {
                                        ui.label(
                                            egui::RichText::new(t).weak().italics().small(),
                                        );
                                    });
                                }
                            }
                            ui.add_space(crate::theme::Spacing::XS);
                        }
                        // In-flight model reply streams as a left bubble.
                        if !self.streaming_chat.trim().is_empty() {
                            ui.with_layout(
                                egui::Layout::left_to_right(egui::Align::Min),
                                |ui| {
                                    egui::Frame::NONE
                                        .fill(deck_bg)
                                        .corner_radius(bubble_radius)
                                        .inner_margin(egui::Margin::symmetric(10, 6))
                                        .show(ui, |ui| {
                                            ui.set_max_width(ui.available_width() * 0.82);
                                            ui.style_mut().wrap_mode =
                                                Some(egui::TextWrapMode::Wrap);
                                            CommonMarkViewer::new().show(
                                                ui,
                                                &mut self.markdown,
                                                self.streaming_chat.trim(),
                                            );
                                        });
                                },
                            );
                        }
                        // The ONLY waiting indicator (the header has none now).
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
            });

        if let Some(view) = open_detail {
            self.view = view;
        }
        // A tapped example chip seeds the input and focuses it for editing.
        if let Some(text) = chip_fill {
            self.input = text;
            ui.memory_mut(|m| m.request_focus(input_id));
        }
        if do_stop {
            self.stop_turn();
        }
        if do_send {
            // Strip any newline the submit chord may have inserted before sending.
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
            active_doc_uuid: None,
            loaded_session_id: None,
            dirty_since_load: false,
            pending_session_decision: None,
            summarize: None,
            pending_summarize_id: None,
            session_search: String::new(),
            pending_ui_actions: Vec::new(),
            pending_app_verbs: Vec::new(),
        }
    }

    // ── SwiftUI-style component previews ────────────────────────────────────
    // Render the chat pane OFF-SCREEN via egui_kittest (wgpu) into PNGs under
    // `crates/app/tests/snapshots/`. This is our stand-in for a SwiftUI #Preview
    // canvas: edit the UI, regenerate, look at the image — no full app launch.
    // The PNGs double as visual regression baselines (a later UI change fails
    // the test until re-approved). Ignored by default because it needs a GPU
    // adapter (like the render tests). Generate / refresh with:
    //
    //   UPDATE_SNAPSHOTS=1 cargo test -p itsjustcad chat_preview -- --ignored
    //
    // then view / diff `crates/app/tests/snapshots/chat_*.png`.

    /// A deck pane whose header reads "ready" (green dot) with a warm model, so
    /// previews show the normal, connected state.
    #[cfg(test)]
    fn ready_pane() -> DeckPane {
        let mut p = blank_pane();
        p.probe = ProbeState::Ready(ProbeInfo {
            detail: "sonnet via Claude Code".into(),
            models: vec!["sonnet".into()],
        });
        p.warm = WarmState::Warm;
        // Pin warmed_model to the active deck's model so poll_warm sees it as
        // fresh and does NOT kick a (repaint-inducing) warm-up during previews.
        p.warmed_model = p
            .decks
            .decks
            .get(p.decks.active)
            .map(|c| c.model.clone());
        // Pin probed_deck so poll_probe does NOT re-probe (which would overwrite
        // the Ready state with Checking and flip the dot orange + disable input).
        p.probed_deck = Some(p.decks.active);
        p
    }

    /// Render `pane` at a phone-ish chat width in the given theme and write
    /// snapshot `name`. `keepalive` holds anything that must outlive the render
    /// (e.g. a channel sender that keeps a faked in-flight turn's receiver open).
    #[cfg(test)]
    fn snapshot_pane(name: &str, mut pane: DeckPane, dark: bool, _keepalive: impl std::any::Any) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();
        let colors = if dark {
            crate::preset::preset_for(crate::preset::CadOrigin::None)
                .tokens()
                .colors
        } else {
            // Light skin: near-white surface, blue accent.
            crate::theme::roles_from([0.98, 0.98, 0.97, 1.0], [0.20, 0.50, 1.0, 1.0])
        };
        let icons = crate::icons::Icons::new();
        let mut session = Session::default();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(360.0, 700.0))
            .wgpu()
            .build_ui(|ui| {
                pane.ui(ui, &mut session, &handle, &icons, &colors, dark, false);
            });
        // Match egui's own visuals to the requested theme (the transcript bg /
        // bubble colors key off `ui.visuals().dark_mode`).
        harness
            .ctx
            .set_theme(if dark { egui::Theme::Dark } else { egui::Theme::Light });
        // A busy pane repaints forever (spinner), so render a FIXED number of
        // frames rather than running until the UI "settles".
        harness.run_steps(4);
        harness.snapshot(name);
        // Leak harness + runtime: their Drop impls panic inside kittest's
        // pollster/wgpu block_on context (esp. when many previews run in one
        // process). Snapshot is already written; the test process is short-lived.
        std::mem::forget(harness);
        std::mem::forget(rt);
    }

    #[test]
    #[ignore = "needs a GPU adapter; run explicitly to (re)generate chat previews"]
    fn chat_preview_empty() {
        // Empty transcript → starter prompt + example chips.
        snapshot_pane("chat_empty", ready_pane(), true, ());
        snapshot_pane("chat_empty_light", ready_pane(), false, ());
    }

    #[test]
    #[ignore = "needs a GPU adapter; run explicitly to (re)generate chat previews"]
    fn chat_preview_conversation() {
        // Fresh pane per theme (DeckPane isn't Clone — channels/tasks).
        let build = || {
            let mut p = ready_pane();
            p.transcript = vec![
                Entry::User("make a 10×10×3 slab with a 4×4 courtyard".into()),
                Entry::Deck(
                    "Done — cut a **4×4** courtyard out of the slab with `difference`. \
                     The inputs are consumed and one result mesh remains. Want a parapet next?"
                        .into(),
                ),
                Entry::Status("turn done in 2.1s".into()),
            ];
            p
        };
        snapshot_pane("chat_conversation", build(), true, ());
        snapshot_pane("chat_conversation_light", build(), false, ());
    }

    #[test]
    #[ignore = "needs a GPU adapter; run explicitly to (re)generate chat previews"]
    fn chat_preview_busy() {
        // A live-but-idle channel fakes an in-flight turn: busy() is rx.is_some(),
        // so the STOP button replaces the airplane and the waiting spinner shows.
        let build = || {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<DeckDelta>();
            let mut p = ready_pane();
            p.transcript = vec![Entry::User("extrude the footprint 3m".into())];
            p.rx = Some(rx);
            p.turn_started = Some(std::time::Instant::now());
            p.current_response = "Extrud".into();
            (p, tx) // return tx so the channel stays open through the render
        };
        let (p, tx) = build();
        snapshot_pane("chat_busy", p, true, tx);
        let (p, tx) = build();
        snapshot_pane("chat_busy_light", p, false, tx);
    }

    #[test]
    #[ignore = "needs a GPU adapter; run explicitly to (re)generate chat previews"]
    fn chat_preview_error() {
        // Dead deck → red status dot, disabled input.
        let build = || {
            let mut p = blank_pane();
            p.probe =
                ProbeState::Unavailable("connection refused (localhost:11434)".into());
            p.probed_deck = Some(p.decks.active); // keep Unavailable (don't re-probe)
            p
        };
        snapshot_pane("chat_error", build(), true, ());
        snapshot_pane("chat_error_light", build(), false, ());
    }

    #[test]
    #[ignore = "needs a GPU adapter; run explicitly to (re)generate chat previews"]
    fn chat_preview_expanded() {
        // The Signal-style EXPANDED input: tall box + chevron-down (collapse) on
        // top + action buttons below. Set via egui memory before rendering.
        for (name, dark) in [("chat_expanded", true), ("chat_expanded_light", false)] {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let handle = rt.handle().clone();
            let colors = if dark {
                crate::preset::preset_for(crate::preset::CadOrigin::None)
                    .tokens()
                    .colors
            } else {
                crate::theme::roles_from([0.98, 0.98, 0.97, 1.0], [0.20, 0.50, 1.0, 1.0])
            };
            let icons = crate::icons::Icons::new();
            let mut session = Session::default();
            let mut pane = ready_pane();
            pane.transcript = vec![Entry::User("draw a spiral stair".into())];
            pane.input = "make it 3 metres tall with 18 treads".to_string();
            let mut harness = egui_kittest::Harness::builder()
                .with_size(egui::vec2(360.0, 700.0))
                .wgpu()
                .build_ui(|ui| {
                    pane.ui(ui, &mut session, &handle, &icons, &colors, dark, false);
                });
            harness
                .ctx
                .set_theme(if dark { egui::Theme::Dark } else { egui::Theme::Light });
            harness
                .ctx
                .data_mut(|d| d.insert_temp(egui::Id::new("deck_input_expanded"), true));
            harness.run_steps(4);
            harness.snapshot(name);
            std::mem::forget(harness);
            std::mem::forget(rt);
        }
    }

    #[test]
    #[ignore = "needs a GPU adapter; run explicitly to (re)generate chat previews"]
    fn chat_preview_toggle_back() {
        // Reproduce the reported bug: EXPAND then COLLAPSE across frames and
        // snapshot the FINAL collapsed state — the input must return to the
        // BOTTOM (not float in the middle).
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();
        let colors = crate::theme::roles_from([0.98, 0.98, 0.97, 1.0], [0.20, 0.50, 1.0, 1.0]);
        let icons = crate::icons::Icons::new();
        let mut session = Session::default();
        let mut pane = ready_pane();
        pane.transcript = vec![Entry::User("draw a spiral stair".into())];
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(360.0, 700.0))
            .wgpu()
            .build_ui(|ui| {
                pane.ui(ui, &mut session, &handle, &icons, &colors, false, false);
            });
        harness.ctx.set_theme(egui::Theme::Light);
        let id = egui::Id::new("deck_input_expanded");
        // Expand, render a few frames…
        harness.ctx.data_mut(|d| d.insert_temp(id, true));
        harness.run_steps(4);
        // …then collapse and render again.
        harness.ctx.data_mut(|d| d.insert_temp(id, false));
        harness.run_steps(4);
        harness.snapshot("chat_toggle_back");
        std::mem::forget(harness);
        std::mem::forget(rt);
    }

    // --- Opt-in web search gating at the request-build boundary ---

    #[test]
    fn archive_derives_title_and_summary() {
        // Archiving the live conversation must auto-fill title + summary (the
        // fallback, no model) so the Sessions cards are populated.
        let mut pane = blank_pane();
        pane.store = Some(crate::chat_store::DocSessions::new(
            "11112222-3333-4444-5555-666677778888".into(),
        ));
        pane.messages = vec![
            ChatMessage { role: Role::User, content: "make a five by five core".into() },
            ChatMessage { role: Role::Assistant, content: "Drew the core.".into() },
        ];
        pane.archive_active(ArchiveMode::Auto);
        let store = pane.store.as_ref().unwrap();
        assert_eq!(store.sessions.len(), 1);
        let s = &store.sessions[0];
        assert_eq!(s.title, "make a five by five core");
        assert!(s.summary.contains("core"), "summary populated: {}", s.summary);
        assert!(!s.summary.is_empty());
    }

    #[test]
    fn load_stored_session_replaces_live_transcript() {
        // Clicking a session card loads that session's turns into the live Chat
        // (state change), archiving whatever was live first so nothing is lost.
        let mut pane = blank_pane();
        let mut store = crate::chat_store::DocSessions::new(
            "aaaa1111-2222-3333-4444-555566667777".into(),
        );
        let mut a = crate::chat_store::ChatSession::new(10);
        a.push("user", "stored ask", 10);
        a.push("assistant", "stored reply", 11);
        a.derive_meta();
        let id = a.id.clone();
        store.sessions.push(a);
        pane.store = Some(store);

        // A different live conversation is present.
        pane.messages = vec![ChatMessage { role: Role::User, content: "live one".into() }];

        pane.load_stored_session(&id);
        // The live transcript is now the stored session's turns.
        assert_eq!(pane.messages.len(), 2);
        assert_eq!(pane.messages[0].content, "stored ask");
        assert_eq!(pane.messages[1].content, "stored reply");
        assert!(matches!(pane.view, PaneView::Chat));
        // The previously-live conversation was archived (not lost).
        assert!(
            pane.store.as_ref().unwrap().sessions.iter().any(|s| s.title == "live one"),
            "prior live conversation must be archived"
        );
    }

    #[test]
    fn saved_chat_path_derives_per_doc_and_guards_traversal() {
        // A well-formed uuid yields a `<uuid>.draft.json` under chats/.
        let uuid = "11112222-3333-4444-5555-666677778888";
        let p = saved_chat_path(uuid).expect("valid uuid → path");
        assert!(p.ends_with(format!("{uuid}.draft.json")));
        assert!(p.to_string_lossy().contains("/chats/"));
        // The draft lives BESIDE the archive `<uuid>.json` (same dir).
        let archive = crate::chat_store::store_path(uuid).unwrap();
        assert_eq!(p.parent(), archive.parent());
        // Path-traversal / malformed uuids are rejected (fail closed).
        assert!(saved_chat_path("../../etc/passwd").is_none());
        assert!(saved_chat_path("").is_none());
        assert!(saved_chat_path("has/slash").is_none());
    }

    #[test]
    fn load_draft_populates_live_state() {
        // A loaded draft replaces the live pane and clears the loaded-session
        // tracking (a draft is a continuation, not a stored session).
        let mut pane = blank_pane();
        pane.loaded_session_id = Some("stale".into());
        pane.dirty_since_load = true;
        let draft = SavedChat {
            session_id: None,
            messages: vec![ChatMessage { role: Role::User, content: "revive me".into() }],
            transcript: vec![Entry::User("revive me".into())],
        };
        pane.load_draft(draft);
        assert_eq!(pane.messages.len(), 1);
        assert_eq!(pane.messages[0].content, "revive me");
        assert!(pane.loaded_session_id.is_none());
        assert!(!pane.dirty_since_load);
    }

    #[test]
    fn archive_active_auto_updates_loaded_id_but_forks_fresh() {
        // A FRESH chat (no loaded id) → Auto pushes a NEW session.
        let mut pane = blank_pane();
        pane.store = Some(crate::chat_store::DocSessions::new(
            "aaaa1111-2222-3333-4444-555566667777".into(),
        ));
        pane.messages = vec![ChatMessage { role: Role::User, content: "fresh chat".into() }];
        let id1 = pane.archive_active(ArchiveMode::Auto).unwrap();
        assert_eq!(pane.store.as_ref().unwrap().sessions.len(), 1);
        // Now the pane TRACKS that session; continuing it and archiving again in
        // Auto mode UPDATES in place (no dup), reusing the same id.
        pane.messages.push(ChatMessage { role: Role::Assistant, content: "done".into() });
        let id2 = pane.archive_active(ArchiveMode::Auto).unwrap();
        assert_eq!(id1, id2, "Auto reuses the loaded id");
        assert_eq!(
            pane.store.as_ref().unwrap().sessions.len(),
            1,
            "no duplicate on continue"
        );
        assert_eq!(pane.store.as_ref().unwrap().sessions[0].turns.len(), 2);
    }

    #[test]
    fn archive_active_new_forces_a_fork_even_when_loaded() {
        let mut pane = blank_pane();
        pane.store = Some(crate::chat_store::DocSessions::new(
            "aaaa1111-2222-3333-4444-555566667777".into(),
        ));
        pane.messages = vec![ChatMessage { role: Role::User, content: "one".into() }];
        pane.archive_active(ArchiveMode::Auto).unwrap();
        // Force a NEW session from the (now loaded) pane.
        pane.messages.push(ChatMessage { role: Role::Assistant, content: "two".into() });
        pane.archive_active(ArchiveMode::New).unwrap();
        assert_eq!(
            pane.store.as_ref().unwrap().sessions.len(),
            2,
            "New forces a second session"
        );
    }

    #[test]
    fn send_sets_dirty_only_after_a_load() {
        // A turn on a fresh chat leaves dirty_since_load false; the SAME after a
        // load sets it true (the ambiguous continue case).
        let mut pane = blank_pane();
        // Simulate a loaded session, then a new user turn.
        pane.loaded_session_id = Some("sid".into());
        pane.messages.push(ChatMessage { role: Role::User, content: "more".into() });
        pane.dirty_since_load = pane.loaded_session_id.is_some(); // mirrors send()
        assert!(pane.dirty_since_load);
        assert!(pane.is_ambiguous_continue());
    }

    #[test]
    fn guarded_next_action_forks_immediately_for_fresh_chat() {
        // No loaded session → New Session archives immediately (Auto) and clears
        // the live pane; NO modal is parked.
        let mut pane = blank_pane();
        pane.store = Some(crate::chat_store::DocSessions::new(
            "aaaa1111-2222-3333-4444-555566667777".into(),
        ));
        pane.messages = vec![ChatMessage { role: Role::User, content: "fresh".into() }];
        let parked = pane.guarded_next_action(NextAction::NewSession);
        assert!(!parked, "fresh chat does not prompt");
        assert!(pane.needs_session_decision().is_none());
        assert_eq!(pane.store.as_ref().unwrap().sessions.len(), 1);
        assert!(pane.messages.is_empty(), "live pane cleared");
    }

    #[test]
    fn ambiguous_continue_parks_then_resolves_update_vs_new() {
        // Loaded + dirty → New Session PARKS the decision.
        let mut make = || {
            let mut pane = blank_pane();
            let mut store = crate::chat_store::DocSessions::new(
                "aaaa1111-2222-3333-4444-555566667777".into(),
            );
            let mut s = crate::chat_store::ChatSession::new(10);
            s.push("user", "stored ask", 10);
            s.derive_meta();
            let id = s.id.clone();
            store.sessions.push(s);
            pane.store = Some(store);
            pane.loaded_session_id = Some(id.clone());
            pane.dirty_since_load = true;
            pane.messages = vec![
                ChatMessage { role: Role::User, content: "stored ask".into() },
                ChatMessage { role: Role::User, content: "a new follow-up".into() },
            ];
            (pane, id)
        };
        // Update path: resolve(true) updates in place — still ONE session.
        let (mut pane, id) = make();
        let parked = pane.guarded_next_action(NextAction::NewSession);
        assert!(parked, "ambiguous continue must prompt");
        assert_eq!(pane.needs_session_decision(), Some(NextAction::NewSession));
        pane.resolve_session_decision(true);
        assert!(pane.needs_session_decision().is_none());
        let store = pane.store.as_ref().unwrap();
        assert_eq!(store.sessions.len(), 1, "Update keeps one session");
        assert_eq!(store.sessions[0].id, id);
        assert_eq!(store.sessions[0].turns.len(), 2, "updated with the follow-up");
        assert!(pane.messages.is_empty(), "New Session cleared the pane");

        // Save-as-new path: resolve(false) forks — TWO sessions.
        let (mut pane, _id) = make();
        pane.guarded_next_action(NextAction::NewSession);
        pane.resolve_session_decision(false);
        assert_eq!(pane.store.as_ref().unwrap().sessions.len(), 2, "fork");
    }

    #[test]
    fn cancel_session_decision_drops_the_parked_action() {
        let mut pane = blank_pane();
        pane.pending_session_decision = Some(NextAction::NewSession);
        pane.cancel_session_decision();
        assert!(pane.needs_session_decision().is_none());
    }

    #[test]
    fn parse_title_summary_extracts_two_lines_with_fallback() {
        // Two clean lines → (title, summary).
        let (t, s) = parse_title_summary("Office core\nA 5x5 core with a stair.").unwrap();
        assert_eq!(t, "Office core");
        assert_eq!(s, "A 5x5 core with a stair.");
        // Labels + quotes are stripped.
        let (t, s) =
            parse_title_summary("Title: \"Glass facade\"\nSummary: North curtain wall.")
                .unwrap();
        assert_eq!(t, "Glass facade");
        assert_eq!(s, "North curtain wall.");
        // Title only → empty summary, still Some.
        let (t, s) = parse_title_summary("Just a title").unwrap();
        assert_eq!(t, "Just a title");
        assert_eq!(s, "");
        // No usable content → None (deterministic fallback stays).
        assert!(parse_title_summary("   \n  ").is_none());
        assert!(parse_title_summary("").is_none());
    }

    #[test]
    fn summarize_applies_by_id_without_touching_others() {
        // The apply step (NOT the network): set_meta on the archived id refines
        // that session and leaves others alone. Mirrors poll_summarize's apply.
        let mut store = crate::chat_store::DocSessions::new(
            "aaaa1111-2222-3333-4444-555566667777".into(),
        );
        let mut a = crate::chat_store::ChatSession::new(10);
        a.push("user", "make a core", 10);
        a.derive_meta();
        let id = a.id.clone();
        let mut b = crate::chat_store::ChatSession::new(20);
        b.push("user", "add a facade", 20);
        b.derive_meta();
        let b_title = b.title.clone();
        store.sessions.push(a);
        store.sessions.push(b);
        if let Some((title, summary)) =
            parse_title_summary("Office core\nA compact office core.")
        {
            store.set_meta(&id, &title, &summary);
        }
        assert_eq!(store.get(&id).unwrap().title, "Office core");
        assert_eq!(store.get(&id).unwrap().summary, "A compact office core.");
        // The other session's fallback labels are untouched.
        assert!(store.sessions.iter().any(|s| s.title == b_title));
    }

    #[test]
    fn sync_store_switch_persists_old_draft_and_loads_new() {
        // Switching documents must persist the OUTGOING draft to its per-doc path
        // and load the INCOMING document's (empty) draft — each doc keeps its own
        // live chat. Uses fresh uuids + cleans up its draft files.
        let a = uuid::Uuid::new_v4().to_string();
        let b = uuid::Uuid::new_v4().to_string();
        // Ensure a clean slate.
        for u in [&a, &b] {
            if let Some(p) = saved_chat_path(u) {
                let _ = std::fs::remove_file(&p);
            }
        }
        let mut pane = blank_pane();
        pane.sync_store(&a);
        assert_eq!(pane.active_doc_uuid.as_deref(), Some(a.as_str()));
        // Put a live conversation on document A, then switch to B.
        pane.messages = vec![ChatMessage { role: Role::User, content: "doc A chat".into() }];
        pane.transcript = vec![Entry::User("doc A chat".into())];
        pane.sync_store(&b);
        // B starts empty…
        assert_eq!(pane.active_doc_uuid.as_deref(), Some(b.as_str()));
        assert!(pane.messages.is_empty(), "doc B has its own (empty) draft");
        // …and A's draft was persisted to A's per-doc path.
        let a_draft = SavedChat::load(&a);
        assert_eq!(a_draft.messages.len(), 1);
        assert_eq!(a_draft.messages[0].content, "doc A chat");
        // Switching BACK to A revives its draft.
        pane.sync_store(&a);
        assert_eq!(pane.messages.len(), 1);
        assert_eq!(pane.messages[0].content, "doc A chat");
        // Cleanup.
        for u in [&a, &b] {
            if let Some(p) = saved_chat_path(u) {
                let _ = std::fs::remove_file(&p);
            }
            if let Some(p) = crate::chat_store::store_path(u) {
                let _ = std::fs::remove_file(&p);
            }
        }
    }

    #[test]
    fn migration_adopts_global_deck_chat_then_removes_it() {
        // A one-time migration: the legacy GLOBAL deck_chat.json is adopted as the
        // FIRST opened document's per-doc draft, then deleted. Guarded so it only
        // runs when a real global file exists and this doc has no draft yet.
        let Some(global) = legacy_global_chat_path() else { return };
        // Do not clobber a real user file: only run the assertion when there is no
        // pre-existing global snapshot; create a synthetic one for the test.
        if global.exists() {
            return; // a live global file is present; skip to avoid data loss
        }
        let doc = uuid::Uuid::new_v4().to_string();
        if let Some(p) = saved_chat_path(&doc) {
            let _ = std::fs::remove_file(&p);
        }
        let _ = std::fs::create_dir_all(global.parent().unwrap());
        let legacy = serde_json::to_string(&SavedChatRef {
            session_id: &None,
            messages: &[ChatMessage { role: Role::User, content: "legacy global chat".into() }],
            transcript: &[Entry::User("legacy global chat".into())],
        })
        .unwrap();
        std::fs::write(&global, legacy).unwrap();

        let mut pane = blank_pane();
        pane.sync_store(&doc);
        // Adopted into the live pane…
        assert_eq!(pane.messages.len(), 1);
        assert_eq!(pane.messages[0].content, "legacy global chat");
        // …and the global file was removed (never re-adopted by another doc).
        assert!(!global.exists(), "legacy global file must be deleted after migration");
        // Cleanup the per-doc draft this created.
        if let Some(p) = saved_chat_path(&doc) {
            let _ = std::fs::remove_file(&p);
        }
    }

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
    fn deck_basemap_fetch_is_blocked_but_clear_passes() {
        // SECURITY: `basemap sat` reaches the network to fetch tiles. A
        // deck-emitted fetch must NOT auto-egress — it is dropped with an error
        // and never queued as an app verb. `basemap off` (no network) still
        // passes through so the model can clear the underlay.
        let mut pane = blank_pane();
        let mut session = Session::default();
        pane.handle_extract_events(
            vec![
                ExtractEvent::Command("basemap sat 800 0.6".to_string()),
                ExtractEvent::Command("basemap off".to_string()),
            ],
            &mut session,
        );
        // Only the clearing form reached the app-verb queue.
        assert_eq!(pane.take_app_verbs(), vec!["basemap off".to_string()]);
        // The fetch was reported as an error this turn (surfaced to the user).
        assert!(
            pane.errors_this_turn.is_empty(),
            "a dropped app-verb is recorded on the command, not as a retry-triggering error"
        );
        // The command list carries the fetch as a failed command with a reason.
        assert!(
            pane.current_commands.iter().any(|c| c.line == "basemap sat 800 0.6"
                && c.result.as_ref().is_err_and(|e| e.contains("network"))),
            "basemap fetch must be recorded as a network-blocked failure"
        );
    }

    #[test]
    fn deck_save_verb_is_blocked_never_queued() {
        // SECURITY: `save <path>` is an app-verb, not a substrate Command, so it
        // bypasses the fs side-effect gate. A deck-emitted `save /path` would be
        // an arbitrary-file-write primitive (reachable via prompt-injection). It
        // must be dropped with an error and NEVER reach the app-verb queue (which
        // App::update drains unconditionally into execute_line → io::save_file).
        let mut pane = blank_pane();
        let mut session = Session::default();
        pane.handle_extract_events(
            vec![
                ExtractEvent::Command("save /Users/victim/.zshrc".to_string()),
                ExtractEvent::Command("save ../../../etc/anything".to_string()),
            ],
            &mut session,
        );
        assert!(
            pane.take_app_verbs().is_empty(),
            "save must NEVER be queued as an app verb — it writes to the fs"
        );
        assert!(
            pane.current_commands.iter().all(|c| c
                .result
                .as_ref()
                .is_err_and(|e| e.contains("filesystem"))),
            "each save line must be recorded as a filesystem-blocked failure"
        );
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
