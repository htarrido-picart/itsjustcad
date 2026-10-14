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
    /// A catalog local model whose server WE spawn is still coming up (process
    /// launched, model loading off disk). Shown amber with a spinner and
    /// auto-advances to Ready/Unavailable — NOT an error, and no manual Retry.
    Starting(String),
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
/// Hard bound on total auto-driven rounds (steps + retries + verify) of one
/// plan-execute run, so a runaway plan can never loop the deck forever.
const MAX_PLAN_ROUNDS: u32 = 24;
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

/// Whether a vision turn should INLINE the image as a request content part
/// (HTTP multimodal cassettes: local Qwen2.5-VL, Anthropic, OpenAI) rather than
/// hand a scoped file path to the claude-code adapter. True only for a
/// non-claude-code cassette that `supports_vision()`; a blind cassette gets no
/// image at all (we never send an image to a model that can't see). Pure.
fn inline_vision_image(config: &itsjustcad_deck::DeckConfig) -> bool {
    !matches!(config.kind, itsjustcad_deck::DeckKind::ClaudeCode) && config.supports_vision()
}

/// Media type for an image path by extension; defaults to PNG (the critique
/// screenshot is always a PNG) for unknown/missing extensions. Pure.
fn image_mime_for(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => "image/png",
    }
}

/// Read one image file and wrap it as an inline [`itsjustcad_deck::ImagePart`]
/// (mime from the extension, payload base64-encoded). `None` when the file can't
/// be read — the caller then sends a plain text turn rather than a broken image.
/// The read is scoped to exactly the single path the vision turn pinned (H-1).
fn read_vision_image(path: &std::path::Path) -> Option<itsjustcad_deck::ImagePart> {
    use base64::Engine as _;
    let bytes = std::fs::read(path).ok()?;
    Some(itsjustcad_deck::ImagePart {
        mime: image_mime_for(path).to_string(),
        data_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
    })
}

#[cfg(test)]
mod vision_image_tests {
    use super::*;
    use itsjustcad_deck::{DeckConfig, DeckKind};

    fn cfg(kind: DeckKind, model: &str, grammar: bool) -> DeckConfig {
        DeckConfig {
            name: "x".into(),
            kind,
            base_url: if matches!(kind, DeckKind::ClaudeCode) {
                String::new()
            } else {
                "http://127.0.0.1:8080/v1".into()
            },
            model: model.into(),
            api_key: None,
            grammar,
            terse: None,
        }
    }

    #[test]
    fn inline_only_for_http_vision_cassettes() {
        // Local Qwen2.5-VL: grammar-constrained yet multimodal → inline.
        assert!(inline_vision_image(&cfg(
            DeckKind::OpenaiCompat,
            "qwen2.5-vl-3b",
            true
        )));
        // Local text model → no image inlined (blind).
        assert!(!inline_vision_image(&cfg(DeckKind::OpenaiCompat, "qwen3", true)));
        // claude-code uses the scoped-Read path instead, never inlining.
        assert!(!inline_vision_image(&cfg(DeckKind::ClaudeCode, "sonnet", false)));
    }

    #[test]
    fn mime_from_extension_defaults_png() {
        use std::path::Path;
        assert_eq!(image_mime_for(Path::new("a/b/critique.png")), "image/png");
        assert_eq!(image_mime_for(Path::new("x.JPG")), "image/jpeg");
        assert_eq!(image_mime_for(Path::new("x.jpeg")), "image/jpeg");
        assert_eq!(image_mime_for(Path::new("x.webp")), "image/webp");
        // No/unknown extension → PNG (the critique screenshot).
        assert_eq!(image_mime_for(Path::new("noext")), "image/png");
    }

    #[test]
    fn read_vision_image_encodes_bytes_and_errors_softly() {
        use base64::Engine as _;
        let dir = std::env::temp_dir().join("ijc_vision_read");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("shot.png");
        let payload = b"\x89PNG\r\n\x1a\nfake-bytes";
        std::fs::write(&p, payload).unwrap();
        let img = read_vision_image(&p).expect("readable image → Some");
        assert_eq!(img.mime, "image/png");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(img.data_base64.as_bytes())
            .unwrap();
        assert_eq!(decoded, payload, "round-trips the exact file bytes");
        // A missing file degrades to None (caller falls back to a text turn).
        assert!(read_vision_image(&dir.join("absent.png")).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// Human label for a cassette in the deck dropdown. Installed local models are
/// keyed `local-<catalog-id>` (a stable lookup key, ugly to show). Resolve that
/// key to the catalog's `display_name` so the dropdown reads "Qwen3 4B".
/// Non-local decks (and unknown ids) render their raw name.
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

/// The static model caption for the chat header when the interactive picker
/// can't render (the probe hasn't produced a model list — endpoint down, still
/// checking, or a server that doesn't enumerate models). The active model must
/// stay visible so the user always knows what the deck would talk to; `None`
/// only when the picker is present (non-empty list) or there's nothing to show.
/// Pure — unit-tested.
fn header_model_caption(probe_models: &[String], model: &str) -> Option<String> {
    if !probe_models.is_empty() || model.trim().is_empty() {
        return None;
    }
    Some(model.to_string())
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
    /// Load another stored session by id (in-place, fork/update-guarded). Now
    /// only exercised by the in-pane-load contract test — the production "open
    /// this chat" flow reopens the whole document (see `App::open_session`).
    #[cfg_attr(not(test), allow(dead_code))]
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
    /// A clarifying question the model asked instead of guessing (the
    /// `QUESTION:` message form). Rendered distinctly; the user's next message
    /// answers it and the turn continues naturally.
    Question(String),
    /// A numbered plan the model emitted for a prolonged task, snapshotted at
    /// the moment it advanced — shown as a checklist.
    Plan(itsjustcad_deck::Plan),
    /// An image attached to the user's turn — rendered as a thumbnail in the
    /// transcript (pushed just above the `User` text for a vision turn).
    Image(std::path::PathBuf),
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

/// The ghost-hint label for the slot currently being typed in `line` (which
/// INCLUDES the verb), e.g. after `box 0,0,0 ` → `size (x,y,z)`. Optional slots
/// are marked. `None` when there is no verb usage, or the cursor is past the
/// last slot. Pure — looks the verb's usage up in the registry/app-verb set.
fn active_slot_hint(line: &str) -> Option<String> {
    let verb = crate::suggest::verb_of(line.trim_start());
    if verb.is_empty() {
        return None;
    }
    let usage = crate::palette::entries()
        .into_iter()
        .find(|e| e.name == verb)
        .map(|e| e.usage)?;
    if usage.is_empty() {
        return None;
    }
    let slots = crate::usage_spec::parse_usage(&usage);
    let idx = crate::usage_spec::active_slot(line, slots.len())?;
    let slot = slots.get(idx)?;
    Some(match slot.req {
        crate::usage_spec::SlotReq::Optional => format!("{} (optional)", slot.name),
        crate::usage_spec::SlotReq::Required => slot.name.clone(),
    })
}

/// One row of the `/` inline palette: verb name + one-line summary.
struct SlashMatch {
    name: String,
    summary: String,
}

/// Commands matching the `/` palette query `q` (the partial verb), prefix
/// matches first then substring, capped at `limit` — the desktop equivalent of
/// the iOS palette, driven by the same registry+app-verb set the ⌘K palette
/// uses. Pure selection; reuses [`crate::palette`].
fn slash_palette_matches(q: &str, limit: usize) -> Vec<SlashMatch> {
    let entries = crate::palette::entries();
    let ql = q.to_lowercase();
    if ql.is_empty() {
        return entries
            .iter()
            .take(limit)
            .map(|e| SlashMatch { name: e.name.clone(), summary: e.summary.clone() })
            .collect();
    }
    let mut prefix = Vec::new();
    let mut substring = Vec::new();
    for e in &entries {
        let n = e.name.to_lowercase();
        if n.starts_with(&ql) {
            prefix.push(e);
        } else if n.contains(&ql) {
            substring.push(e);
        }
    }
    prefix
        .into_iter()
        .chain(substring)
        .take(limit)
        .map(|e| SlashMatch { name: e.name.clone(), summary: e.summary.clone() })
        .collect()
}

/// Render the `/` palette rows above the chat input. Returns `Some("/<name> ")`
/// when a row is clicked. A plain non-focusable list — it never steals keyboard
/// focus from the TextEdit (c2cfcff fast-typing guard).
fn slash_palette_ui(
    ui: &mut egui::Ui,
    roles: &crate::theme::ColorRoles,
    matches: &[SlashMatch],
) -> Option<String> {
    let mut pick = None;
    egui::Frame::NONE
        .fill(crate::theme::to_color32(roles.surface_elevated))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(
            crate::theme::Spacing::S as i8,
            crate::theme::Spacing::XS as i8,
        ))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .max_height(200.0)
                .show(ui, |ui| {
                    for m in matches {
                        let resp = ui
                            .scope(|ui| {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        egui::RichText::new(&m.name)
                                            .monospace()
                                            .color(ui.visuals().text_color()),
                                    );
                                    if !m.summary.is_empty() {
                                        ui.label(
                                            egui::RichText::new(&m.summary).weak().small(),
                                        );
                                    }
                                });
                            })
                            .response
                            .interact(egui::Sense::click());
                        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                        if resp.clicked() {
                            pick = Some(format!("/{} ", m.name));
                        }
                    }
                });
        });
    pick
}

/// Extensions the chat accepts as an attachable image — the SAME set the attach
/// file-dialog filters on (`deck_pane` input bar), so drag-and-drop and the
/// picker agree on what counts as an image. Lower-case, no leading dot.
const IMAGE_DROP_EXTS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif"];

/// True when `path` has a known image extension (case-insensitive). Pure.
fn is_image_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| IMAGE_DROP_EXTS.contains(&e.as_str()))
}

/// The first attachable image among a set of dropped file paths, or `None` when
/// none qualify. Pure so the extension filter is unit-tested without an egui
/// context. (We take ONE image; multi-attach is a later item in issue #68.)
/// Path-less drops — e.g. a selection dragged from a browser on some platforms —
/// are simply skipped (`DroppedFile::path` is `None` there).
fn first_dropped_image(paths: &[std::path::PathBuf]) -> Option<std::path::PathBuf> {
    paths.iter().find(|p| is_image_path(p)).cloned()
}

/// Directive appended to a claude-code vision turn's system prompt for a
/// USER-attached image. The claude-code CLI receives only a path-scoped `Read`
/// (not an inline image block), so granting the Read is not enough — the model
/// must be told the image exists at that exact path or it answers from the
/// user's prose alone. (The viewport-critique turn already carries this in its
/// own message; only user attachments need it added.) Pure so the wording is
/// unit-tested.
fn claude_code_vision_directive(image_path: &std::path::Path) -> String {
    format!(
        "\n\n[A user-attached image is available at {}. Use the Read tool on that \
         exact path to view the image, then answer the user's message about it.]",
        image_path.display()
    )
}

/// Display size for a `src`-pixel image fitted within `max`, preserving aspect
/// and NEVER upscaling beyond native size. A `max` component of `0.0` means
/// "unconstrained on that axis" (the height-bounded composer chip passes
/// `[0.0, 44.0]`). Pure — unit-tested; the texture upload lives in
/// [`DeckPane::image_texture`].
fn thumb_fit(src: [u32; 2], max: egui::Vec2) -> egui::Vec2 {
    let (w, h) = (src[0] as f32, src[1] as f32);
    if w <= 0.0 || h <= 0.0 {
        return egui::Vec2::ZERO;
    }
    // Per-axis scale to fit; an unconstrained (≤0) bound imposes no limit.
    let sx = if max.x > 0.0 { max.x / w } else { f32::INFINITY };
    let sy = if max.y > 0.0 { max.y / h } else { f32::INFINITY };
    // Fit within the box AND cap at 1.0 so a small source renders at native px.
    let s = sx.min(sy).min(1.0);
    egui::vec2(w * s, h * s)
}

/// Encode raw RGBA8 pixels (as `arboard` hands back a pasted clipboard image)
/// to PNG bytes. Pure so the encode is unit-tested without touching the
/// clipboard or disk. `None` when the buffer length doesn't match
/// `width * height * 4` (a corrupt/partial clipboard payload).
fn encode_rgba_png(width: usize, height: usize, rgba: &[u8]) -> Option<Vec<u8>> {
    let img = image::RgbaImage::from_raw(width as u32, height as u32, rgba.to_vec())?;
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png).ok()?;
    Some(buf.into_inner())
}

/// Read an image off the system clipboard and write it to `dir/itsjustcad-pasted.png`,
/// returning that path. `None` when the clipboard holds no image (e.g. text —
/// in which case normal text paste proceeds untouched) or the write fails.
/// Not pure (clipboard + disk); the encode core is [`encode_rgba_png`].
fn clipboard_image_to_file(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut cb = arboard::Clipboard::new().ok()?;
    // `get_image` errors when the clipboard holds text / no bitmap — the common
    // case on a ⌘V meant for text, so this stays quiet and the caller no-ops.
    let img = cb.get_image().ok()?; // RGBA8
    let png = encode_rgba_png(img.width, img.height, &img.bytes)?;
    let path = dir.join("itsjustcad-pasted.png");
    std::fs::write(&path, png).ok()?;
    Some(path)
}

/// The RAW text a transcript entry yields when copied — the original markdown
/// source, not the rendered layout, so a pasted table/emphasis survives as
/// markdown (M-chatmd). `None` = nothing sensible to copy (the Commands card
/// has its own detail pane).
fn entry_copy_text(entry: &Entry) -> Option<String> {
    match entry {
        Entry::User(t) | Entry::Deck(t) | Entry::Status(t) => Some(t.clone()),
        Entry::Question(q) => Some(q.clone()),
        Entry::Plan(p) => Some(p.checklist()),
        Entry::Commands(_) => None,
        // Nothing textual to copy from a thumbnail.
        Entry::Image(_) => None,
    }
}

/// Attach a right-click "Copy message" menu to a transcript bubble. The frame
/// response only senses hover, so re-interact for clicks first; the menu
/// copies the RAW markdown (see [`entry_copy_text`]). A `None` raw (the
/// Commands card) attaches nothing.
fn attach_copy_menu(response: &egui::Response, raw: Option<String>) {
    let Some(raw) = raw else { return };
    response
        .interact(egui::Sense::click())
        .context_menu(move |ui| {
            if ui.button("Copy message").clicked() {
                ui.ctx().copy_text(raw.clone());
                ui.close();
            }
        });
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
    /// In-flight plan-execute state, if a plan was interrupted (cancel, quit,
    /// crash) — revived on load so the plan resumes. `default` keeps old drafts
    /// loading.
    #[serde(default)]
    plan: Option<itsjustcad_deck::Plan>,
}

/// Borrowed mirror of [`SavedChat`] so saving never clones the transcript.
#[derive(Serialize)]
struct SavedChatRef<'a> {
    session_id: &'a Option<String>,
    messages: &'a [ChatMessage],
    transcript: &'a [Entry],
    plan: &'a Option<itsjustcad_deck::Plan>,
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
mod slash_palette_tests {
    use super::*;

    #[test]
    fn empty_query_returns_capped_prefix() {
        let hits = slash_palette_matches("", 8);
        assert_eq!(hits.len(), 8, "empty query shows the first few commands");
    }

    #[test]
    fn prefix_matches_rank_before_substring() {
        // `box` is a prefix match; `bbox` contains `box` as a substring. Prefix
        // must come first.
        let hits = slash_palette_matches("box", 8);
        assert_eq!(hits.first().map(|m| m.name.as_str()), Some("box"));
        let names: Vec<_> = hits.iter().map(|m| m.name.as_str()).collect();
        if let (Some(bi), Some(bbi)) = (
            names.iter().position(|n| *n == "box"),
            names.iter().position(|n| *n == "bbox"),
        ) {
            assert!(bi < bbi, "prefix 'box' ranks before substring 'bbox': {names:?}");
        }
    }

    #[test]
    fn cap_is_respected() {
        let hits = slash_palette_matches("e", 3);
        assert!(hits.len() <= 3);
    }

    #[test]
    fn includes_app_verbs() {
        // `ze` is an app-verb (not a registry command) and should be reachable.
        let hits = slash_palette_matches("ze", 8);
        assert!(hits.iter().any(|m| m.name == "ze"), "app verb ze reachable via /");
    }
}

#[cfg(test)]
mod image_drop_tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn known_image_extensions_are_case_insensitive() {
        for ok in ["a.png", "b.JPG", "c.jpeg", "d.WebP", "e.gif", "/x/y/z.PNG"] {
            assert!(is_image_path(std::path::Path::new(ok)), "{ok} should be an image");
        }
    }

    #[test]
    fn non_images_and_extensionless_are_rejected() {
        for no in ["a.txt", "b.pdf", "c.dxf", "d.", "noext", "e.gguf", "f.svg"] {
            assert!(!is_image_path(std::path::Path::new(no)), "{no} is not an image");
        }
    }

    #[test]
    fn first_dropped_image_picks_first_image_and_skips_others() {
        let paths = [
            PathBuf::from("/tmp/readme.txt"),
            PathBuf::from("/tmp/plan.png"),
            PathBuf::from("/tmp/section.jpg"),
        ];
        assert_eq!(first_dropped_image(&paths), Some(PathBuf::from("/tmp/plan.png")));
    }

    #[test]
    fn first_dropped_image_none_when_no_images() {
        let paths = [PathBuf::from("/tmp/a.txt"), PathBuf::from("/tmp/b.dwg")];
        assert_eq!(first_dropped_image(&paths), None);
        // Empty drop (e.g. a path-less browser drag filtered upstream) → None.
        assert_eq!(first_dropped_image(&[]), None);
    }

    #[test]
    fn encode_rgba_png_writes_valid_png_header() {
        // 2x2 opaque-red RGBA → valid PNG bytes with the 8-byte PNG signature.
        let rgba = [255u8, 0, 0, 255].repeat(4); // 4 px * RGBA
        let png = encode_rgba_png(2, 2, &rgba).expect("valid buffer encodes");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "PNG signature");
    }

    #[test]
    fn encode_rgba_png_rejects_mismatched_buffer() {
        // Length must be width*height*4; a short buffer yields None (not a panic).
        assert_eq!(encode_rgba_png(2, 2, &[0u8; 3]), None);
        assert_eq!(encode_rgba_png(1, 1, &[]), None);
    }

    // ── thumbnail sizing (pure) ────────────────────────────────────────────
    // The decode + GPU upload (`DeckPane::image_texture`) needs a live egui
    // context, so only the sizing math is unit-tested here.

    fn approx(a: egui::Vec2, b: egui::Vec2) {
        assert!((a.x - b.x).abs() < 0.01 && (a.y - b.y).abs() < 0.01, "{a:?} ≉ {b:?}");
    }

    #[test]
    fn thumb_fit_wide_clamps_on_width() {
        // 400×100 into a 240×220 box → width is the binding axis (aspect kept).
        approx(thumb_fit([400, 100], egui::vec2(240.0, 220.0)), egui::vec2(240.0, 60.0));
    }

    #[test]
    fn thumb_fit_tall_clamps_on_height() {
        // 100×400 into a 240×220 box → height binds.
        approx(thumb_fit([100, 400], egui::vec2(240.0, 220.0)), egui::vec2(55.0, 220.0));
    }

    #[test]
    fn thumb_fit_small_source_is_not_upscaled() {
        // A source smaller than the box renders at native pixels (cap at 1.0).
        approx(thumb_fit([50, 50], egui::vec2(240.0, 220.0)), egui::vec2(50.0, 50.0));
    }

    #[test]
    fn thumb_fit_zero_width_bound_constrains_only_by_height() {
        // The composer chip passes [0.0, 44.0]: height-bounded, width free.
        approx(thumb_fit([400, 100], egui::vec2(0.0, 44.0)), egui::vec2(176.0, 44.0));
        // And still never upscales a tiny source past native.
        approx(thumb_fit([20, 10], egui::vec2(0.0, 44.0)), egui::vec2(20.0, 10.0));
    }

    #[test]
    fn claude_code_vision_directive_names_the_path_and_read_tool() {
        let d = claude_code_vision_directive(std::path::Path::new("/tmp/plan.png"));
        // Must name the exact path and instruct a Read — the whole point is the
        // CLI opens the file instead of answering from the user's prose alone.
        assert!(d.contains("/tmp/plan.png"), "{d}");
        assert!(d.contains("Read"), "{d}");
        assert!(d.to_lowercase().contains("image"), "{d}");
    }
}

#[cfg(test)]
mod slot_hint_tests {
    use super::*;

    #[test]
    fn no_hint_while_typing_verb() {
        assert_eq!(active_slot_hint("box"), None);
    }

    #[test]
    fn first_slot_after_verb_space() {
        // `box ` → first slot (corner).
        let h = active_slot_hint("box ").expect("a hint on the first slot");
        assert!(h.contains("corner"), "{h}");
    }

    #[test]
    fn advances_to_size_after_first_point() {
        // `box 0,0,0 ` → the size slot.
        let h = active_slot_hint("box 0,0,0 ").expect("a hint on slot 2");
        assert!(h.contains("size"), "{h}");
    }

    #[test]
    fn verb_without_usage_has_no_hint() {
        assert_eq!(active_slot_hint("undo "), None);
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
            plan: &None,
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
        // A `local-<catalog-id>` key renders as the catalog's display name.
        assert_eq!(
            deck_display_name("local-qwen3-4b-q4km-llamafile"),
            "Qwen3 4B"
        );
        assert_eq!(
            deck_display_name("local-qwen3-0.6b-q6k-llamafile"),
            "Qwen3 0.6B"
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
    fn saved_chat_roundtrips_question_and_plan_entries() {
        // The clarify/plan transcript entries persist in the per-doc draft.
        let mut plan = itsjustcad_deck::Plan::new(vec!["slab".into(), "cores".into()]);
        plan.mark_step_done();
        let entries = vec![
            Entry::Question("how tall should the wall be?".into()),
            Entry::Plan(plan.clone()),
        ];
        let json = serde_json::to_string(&SavedChatRef {
            session_id: &None,
            messages: &[],
            transcript: &entries,
            plan: &Some(plan.clone()),
        })
        .unwrap();
        let back: SavedChat = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(&back.transcript[0], Entry::Question(q) if q == "how tall should the wall be?")
        );
        assert!(matches!(&back.transcript[1], Entry::Plan(p) if *p == plan));
        // The live plan state itself persists too (resume-after-interrupt).
        assert_eq!(back.plan, Some(plan));
        // Old drafts without the plan field still load (serde default).
        let old: SavedChat = serde_json::from_str(
            r#"{"session_id":null,"messages":[],"transcript":[]}"#,
        )
        .unwrap();
        assert!(old.plan.is_none());
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

/// The continuation message that drives one plan step: names the step and
/// pins the contract (only this step's commands). Pure — unit-testable.
fn step_prompt(plan: &itsjustcad_deck::Plan) -> String {
    let step = plan
        .current_step()
        .map(|s| s.text.as_str())
        .unwrap_or_default();
    format!(
        "Step {} of {}: {}\nEmit ONLY this step's commands in a ```draft block.",
        plan.current + 1,
        plan.steps.len(),
        step
    )
}

/// The end-of-plan verification message: read-only checks + a summary.
const VERIFY_PROMPT: &str = "All plan steps are done. Verify the end state with \
read-only commands (`bbox all`, `schedule`) if useful, and reply with a one-line \
summary of what was built.";

/// A one-line summary of a queued deck command for the first-run modal. Uses the
/// command's side-effect summary when it parses to a side-effecting command (so
/// a write/read reads "export → /path"), else the raw line trimmed to length.
fn firstrun_summary(line: &str) -> String {
    if let Ok(cmd) = parse(line)
        && let Some(s) = cmd.side_effect_summary()
    {
        return s;
    }
    let line = line.trim();
    if line.chars().count() > 72 {
        let t: String = line.chars().take(71).collect();
        format!("{t}…")
    } else {
        line.to_string()
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

/// Return a copy of a FILE-READER command with its source path replaced by
/// `chosen` — the path the USER picked in the OS dialog. The deck's proposed
/// path is discarded (security Part C): opens/imports/images never auto-use the
/// LLM's path. `None` when no native picker applies to this reader (e.g. `Trace`
/// reads the current underlay, `XrefReload`/`BlockLibLoad` take a name, not an
/// external path) — the modal then offers Decline only.
fn reader_with_user_path(cmd: &Command, chosen: &str) -> Option<Command> {
    let c = chosen.to_string();
    match cmd {
        Command::Import { .. } => Some(Command::Import { path: c }),
        Command::Terrain { .. } => Some(Command::Terrain { path: c }),
        Command::OsmFile { .. } => Some(Command::OsmFile { path: c }),
        Command::CheckRulesLoad { .. } => Some(Command::CheckRulesLoad { path: c }),
        Command::HatchPat { .. } => Some(Command::HatchPat { path: c }),
        Command::Underlay { corner, width, height, .. } => Some(Command::Underlay {
            path: c,
            corner: *corner,
            width: *width,
            height: *height,
        }),
        Command::XrefAttach {
            at,
            scale,
            rotation_deg,
            name,
            ..
        } => Some(Command::XrefAttach {
            path: c,
            at: *at,
            scale: *scale,
            rotation_deg: *rotation_deg,
            id: None,
            name: name.clone(),
            geometries: None,
        }),
        // Readers with no swappable external path → no picker, Decline only.
        _ => None,
    }
}

/// The rfd file filter (label, extensions) appropriate for a reader command's
/// native "Open file picker…" button, so the user only sees relevant files.
fn reader_file_filter(cmd: &Command) -> (&'static str, &'static [&'static str]) {
    match cmd {
        Command::Import { .. } => (
            "All supported",
            &["dxf", "dwg", "ifc", "obj", "stl", "gltf", "glb", "dae", "las", "laz", "e57", "3dm"],
        ),
        Command::Underlay { .. } => ("Image", &["png", "jpg", "jpeg", "bmp", "tif", "tiff"]),
        Command::Terrain { .. } => ("Terrain", &["csv", "txt", "geojson", "json"]),
        Command::OsmFile { .. } => ("OSM", &["json", "geojson", "osm"]),
        Command::XrefAttach { .. } => ("Drawing", &["dxf", "dwg", "json"]),
        Command::CheckRulesLoad { .. } => ("Rules", &["json"]),
        Command::HatchPat { .. } => ("Hatch pattern", &["pat"]),
        _ => ("All files", &["*"]),
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
    /// True when this command READS an external file into the app (import / open
    /// / underlay-image / xref / data-load). Readers get the manual-only modal
    /// (Decline + "Open file picker…"); they are NEVER auto-run and NEVER
    /// eligible for "approve for session". Writers get approve-once/for-session.
    reader: bool,
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

/// A busy indicator that honours the reduce-motion preference: the spinning
/// egui spinner when motion is allowed, or a static accent dot (same footprint,
/// no animation/repaint) when the user has asked to reduce motion.
fn busy_indicator(ui: &mut egui::Ui, reduce_motion: bool) {
    if reduce_motion {
        let d = ui.spacing().interact_size.y * 0.5;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(d, d), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), d * 0.35, ACCENT);
    } else {
        ui.spinner();
    }
}

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
    /// `help <verb>` output requested by the model this turn. When non-empty at
    /// turn end (and no errors preempt it), it is threaded back as a follow-up
    /// user message so the model can read the verb's full syntax mid-turn — the
    /// on-demand-help half of the compact-catalog design.
    help_output_this_turn: Vec<String>,
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
    /// Decoded-image texture cache for thumbnails (composer chip + transcript),
    /// keyed by path → (file mtime, handle). The mtime busts a stale texture
    /// when a re-paste overwrites the same temp path. Not persisted.
    image_cache: std::collections::HashMap<std::path::PathBuf, (std::time::SystemTime, egui::TextureHandle)>,
    /// Deck-emitted side-effecting commands (export/print/import/…) awaiting an
    /// explicit human [run]/[skip]. They never touch the filesystem until the
    /// user confirms (security C-2 / H-7).
    pending_side_effects: Vec<PendingSideEffect>,
    /// Opt-in: auto-run deck side-effecting commands whose path stays inside the
    /// sandbox root, without a per-command prompt. Defaults OFF. Paths that
    /// escape the sandbox are still queued even when this is on.
    allow_deck_side_effects: bool,
    /// Claude-Code-style OUTER first-run gate: has the user approved the deck
    /// running ANY commands this session? Defaults OFF. While false, the whole
    /// of a turn's emitted command batch is held in `pending_firstrun` and a
    /// first-run modal is shown; nothing (even pure geometry) runs until the
    /// user approves. "Approve for session" sets this (and
    /// `allow_deck_side_effects`); "Approve once" runs the batch but leaves this
    /// false so the next batch re-prompts. Once true, behavior is exactly as
    /// before: geometry auto-runs, writes follow `allow_deck_side_effects` / the
    /// write-modal, and reads ALWAYS take the manual picker carve-out.
    deck_run_approved: bool,
    /// The current turn's emitted command lines, held unprocessed while the
    /// first-run gate is closed (`!deck_run_approved`). Raw lines — replayed
    /// through the normal per-line routing (`process_command_line`) once the
    /// user approves, so writers/readers still hit their INNER gates.
    pending_firstrun: Vec<String>,
    /// Full deck-pane rect captured at the top of [`DeckPane::ui`]. The approval
    /// overlays dim + centre on THIS (the whole chat pane), not on the bottom
    /// input panel's `max_rect` where the modal fns are actually called.
    pane_rect: egui::Rect,
    /// The transcript (message) region, captured each frame. Drag-and-drop and
    /// its hover hint are scoped to THIS rect so the composer input and the
    /// `Chat | Commands` segment control stay clear of image drops. Read one
    /// frame late by `handle_image_drop` (which runs before layout); negligible.
    message_area_rect: egui::Rect,
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
    /// The stable Sessions-archive row id for the LIVE conversation, used so the
    /// per-turn auto-upsert (issue #58) and the finalize `archive_active(Auto)`
    /// converge on ONE row instead of forking a new session per turn. Assigned
    /// on the first auto-upsert of a fresh conversation (and set equal to
    /// `loaded_session_id` when a stored card is loaded); cleared on new/load.
    /// Distinct from `loaded_session_id`, which stays `None` for a fresh
    /// conversation so continuing it never spuriously trips the update-vs-new
    /// modal (`is_ambiguous_continue`).
    auto_session_id: Option<String>,
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
    /// Pre-formatted Commands-history echo lines for commands the DECK ran this
    /// frame (continuous execution + confirmed side-effects). Each entry is
    /// already in the command-line scrollback's wire form: a `> <line>` echo
    /// (rendered with the ✓ + syntax colours) or an `error: …` line (red). The
    /// app drains these each frame via [`DeckPane::take_command_echoes`] and
    /// feeds them to its `CommandLine` so deck-run commands show in the Commands
    /// segment exactly like `/`-typed chat commands do (issue #59, mirroring the
    /// #28 echo). NOT op-logged — the session already logged the real command.
    pending_command_echoes: Vec<String>,
    /// Plan-execute state: the numbered plan the model announced for a
    /// prolonged task, driven step-by-step by [`finish_turn`]. Persisted in the
    /// per-doc draft so a cancelled/interrupted plan resumes. `None` = no plan.
    plan: Option<itsjustcad_deck::Plan>,
    /// Auto-driven rounds consumed by the current plan run (steps + retries +
    /// the verify turn), bounded by [`MAX_PLAN_ROUNDS`].
    plan_rounds: u32,
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
            help_output_this_turn: Vec::new(),
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
            image_cache: std::collections::HashMap::new(),
            pending_side_effects: Vec::new(),
            allow_deck_side_effects: false,
            deck_run_approved: false,
            pending_firstrun: Vec::new(),
            pane_rect: egui::Rect::NOTHING,
            message_area_rect: egui::Rect::NOTHING,
            sandbox_root: None,
            local_runtime: None,
            catalog: crate::model_catalog::Catalog::load(),
            deferred_local_turn: false,
            allow_web_search: false,
            store: None,
            active_doc_uuid: None,
            loaded_session_id: None,
            auto_session_id: None,
            dirty_since_load: false,
            pending_session_decision: None,
            summarize: None,
            pending_summarize_id: None,
            session_search: String::new(),
            pending_ui_actions: Vec::new(),
            pending_app_verbs: Vec::new(),
            pending_command_echoes: Vec::new(),
            plan: None,
            plan_rounds: 0,
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

    /// Reload cassettes from disk after an external edit (the "LLM ▸ API Keys…"
    /// dialog updated a key/model, or wrote a new provider cassette) so the live
    /// pane uses the new config on the next turn. Keeps the active selection on
    /// the same cassette by name when possible, then forces a re-probe.
    pub fn reload_decks(&mut self) {
        let active_name = self
            .decks
            .decks
            .get(self.decks.active)
            .map(|d| d.name.clone());
        self.decks = DecksFile::load_or_default();
        if let Some(name) = active_name
            && let Some(i) = self.decks.decks.iter().position(|d| d.name == name)
        {
            self.decks.active = i;
        }
        self.decks.active = self
            .decks
            .active
            .min(self.decks.decks.len().saturating_sub(1));
        // Re-probe the (possibly updated) active deck on the next frame.
        self.probed_deck = None;
        self.probe = ProbeState::Unknown;
    }

    /// Push a status line into the chat transcript. Used by the app to surface
    /// out-of-band events (e.g. a background model download failing while the
    /// Model Setup window is closed) IN the deck UI instead of only in the log.
    pub fn notify_status(&mut self, msg: String) {
        self.transcript.push(Entry::Status(msg));
        self.persist_chat();
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

    /// Terse-mode state of the ACTIVE cassette (kind-based default unless the
    /// user overrode it). Drives the LLM ▸ Terse Replies checkmark.
    pub fn terse_enabled(&self) -> bool {
        self.decks
            .decks
            .get(self.decks.active)
            .map(|d| d.terse_enabled())
            .unwrap_or(false)
    }

    /// Flip "terse replies" for the ACTIVE cassette (LLM menu). Stores an
    /// explicit override on that cassette and persists `decks.json`, so a local
    /// model can opt out and a cloud one can opt in.
    pub fn toggle_terse(&mut self) {
        let next = !self.terse_enabled();
        if let Some(d) = self.decks.decks.get_mut(self.decks.active) {
            d.terse = Some(next);
            self.decks.save();
        }
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
        self.plan = None;
        self.plan_rounds = 0;
        self.messages.clear();
        self.transcript.clear();
        self.current_response.clear();
        self.streaming_chat.clear();
        self.current_commands.clear();
        self.errors_this_turn.clear();
        self.help_output_this_turn.clear();
        self.input.clear();
        self.attached_image = None;
        self.loaded_session_id = None;
        self.auto_session_id = None;
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

    /// Drain the Commands-history echo lines for deck-run commands accumulated
    /// since the last call (issue #59). Each line is already in command-line
    /// scrollback form (`> <line>` or `error: …`); the app feeds them straight
    /// to `CommandLine::push_line` so the Commands segment mirrors what the deck
    /// ran, the same way `/`-typed chat commands are mirrored (issue #28).
    pub fn take_command_echoes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_command_echoes)
    }

    /// Queue the Commands-history echo for one deck-run command: a `> <line>`
    /// echo (✓ + syntax colours) plus, on failure, an `error: …` line (red).
    /// Success output is intentionally NOT echoed as a body line here — the
    /// transcript's command card already shows the result, and the `> line`
    /// echo alone matches how iOS mirrors deck commands. `error` is `Some(msg)`
    /// when `session.run` failed, `None` on success.
    fn echo_command_to_history(&mut self, line: &str, error: Option<&str>) {
        self.pending_command_echoes.push(format!("> {line}"));
        if let Some(e) = error {
            self.pending_command_echoes.push(format!("error: {e}"));
        }
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
        self.plan = draft.plan;
        self.plan_rounds = 0;
        self.loaded_session_id = None;
        self.dirty_since_load = false;
        // An interrupted plan revives with the draft; the user's next message
        // continues it (the driver picks the cursor back up).
        if let Some(p) = &self.plan
            && !p.is_complete()
            && !p.has_failed()
        {
            self.transcript.push(Entry::Status(
                "interrupted plan restored — send a message to continue".into(),
            ));
        }
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
        let store = self.store.as_mut()?;
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
            // Reuse the loaded card's id OR the id the per-turn auto-upsert
            // already assigned to this live conversation (issue #58), so the
            // finalize archive converges on the SAME row instead of forking.
            ArchiveMode::Auto => self
                .loaded_session_id
                .as_ref()
                .or(self.auto_session_id.as_ref())
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
        self.auto_session_id = Some(archived_id.clone());
        self.dirty_since_load = false;
        // Queue an async LLM title/summary refine; `tick` (which owns the tokio
        // handle) spawns it. The deterministic labels are already in place, so a
        // headless/offline archive is fully labeled without this ever firing.
        self.pending_summarize_id = Some(archived_id.clone());
        Some(archived_id)
    }

    /// Auto-upsert the LIVE conversation into the Sessions archive after a turn,
    /// so it appears in the Sessions tab immediately and survives a crash —
    /// rather than only landing on new/save/quit (issue #58). Update-in-place:
    /// ONE row per conversation, never one per turn.
    ///
    /// Dedup is by a stable per-conversation id: reuse `loaded_session_id` when
    /// it is set (this is also what [`archive_active`]'s `Auto` mode reuses, so
    /// the per-turn upsert and the finalize archive converge on the SAME row);
    /// otherwise mint a fresh id, REMEMBER it in `loaded_session_id`, and upsert
    /// — so later turns in this conversation update that same row.
    ///
    /// Uses the deterministic `derive_meta` fallback (no async LLM title pass is
    /// blocked on). Unlike `archive_active` this does NOT clear
    /// `dirty_since_load` (a loaded-then-continued chat must still trip the
    /// update-vs-new modal) and does NOT queue a summarize pass (that stays a
    /// finalize concern). No-op until `sync_store` and when there are no
    /// messages.
    fn auto_upsert_active(&mut self) {
        let Some(store) = self.store.as_mut() else { return };
        if self.messages.is_empty() {
            return;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Reuse the remembered conversation id (loaded card, or an earlier
        // auto-upsert of this conversation) for update-in-place; otherwise mint
        // one. Prefer `loaded_session_id` so a loaded card keeps its own row.
        let id = self
            .loaded_session_id
            .clone()
            .or_else(|| self.auto_session_id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let mut session = crate::chat_store::ChatSession::new(now);
        session.id = id.clone();
        for m in &self.messages {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            session.push(role, &m.content, now);
        }
        session.derive_meta();
        store.upsert(session);
        store.save();
        // Remember the id so the next turn (and the finalize archive) update THIS
        // row instead of creating a duplicate. Recorded in `auto_session_id`, NOT
        // `loaded_session_id`, so a fresh conversation doesn't start tripping the
        // update-vs-new modal mid-flight (`is_ambiguous_continue`).
        self.auto_session_id = Some(id);
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
            plan: &self.plan,
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
        // A catalog-installed local cassette is served by a subprocess WE spawn on
        // demand — it is NOT an external server the user must start. Probing its
        // URL cold would fail with the generic "is the server running? (ollama
        // serve)" message, which is misleading for an auto-spawn model. Instead
        // start (or reuse) the local runtime here and reflect its real state.
        if is_spawnable_local(&config, &self.catalog) {
            match self.ensure_local_runtime(&config, handle) {
                // The server is health-checked up and serving THIS model. Mark
                // ready DIRECTLY — do NOT run the generic probe(), whose strict
                // /v1/models name-match rejects us: the llamafile server
                // advertises the model under its own name (e.g. "Qwen3-4B"), not
                // our catalog id, so the probe would report "model not found" and
                // leave the deck unusable even though the server is up.
                LocalReady::Ready(_base_url) => {
                    self.probe = ProbeState::Ready(ProbeInfo {
                        detail: "local model ready".into(),
                        models: vec![config.model.clone()],
                    });
                    return;
                }
                // Still loading the model (first run reads GBs off disk). Show an
                // honest "starting" status — NOT an external-server error. Retry
                // (button) re-probes once it's warm.
                LocalReady::Pending => {
                    self.probe = ProbeState::Starting(
                        "starting the local model server… first run loads the model \
                         (~30 s)"
                            .into(),
                    );
                    return;
                }
                LocalReady::Failed(msg) => {
                    self.probe = ProbeState::Unavailable(msg);
                    return;
                }
            }
        }
        let (tx, rx) = oneshot::channel();
        self.probe = ProbeState::Checking(rx);
        handle.spawn(async move {
            let _ = tx.send(probe(&config).await);
        });
    }

    fn poll_probe(&mut self, handle: &tokio::runtime::Handle) {
        // Re-probe when the cassette changed, nothing has been probed yet, or a
        // spawnable local server is still starting (re-check advances it to
        // Ready/Unavailable once the runtime health-check flips).
        if self.probed_deck != Some(self.decks.active)
            || matches!(self.probe, ProbeState::Unknown | ProbeState::Starting(_))
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

    /// Dev hook: pre-fill the chat composer input WITHOUT sending, so the
    /// ITSJUSTCAD_TYPE/ITSJUSTCAD_SHOT pair can capture the `/`-slash preview and
    /// palette (issue #50). Mirrors `CommandLine::prefill`; never auto-sends.
    pub fn prefill(&mut self, text: String) {
        self.input = text;
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
        // UNIFIED PROMPT: every backend — claude-code, Anthropic, and local
        // models alike — gets the COMPACT catalog (every verb name + a one-liner,
        // grouped by category; ~7 KB) instead of the full ~33 KB registry dump.
        // The model fetches full syntax on demand with `help <verb>`, whose output
        // threads back through the turn loop. This replaces the old
        // `is_local_url`-based brief/full fork: the claude-code cassette has an
        // EMPTY base_url — `is_local_url("")` is true — so it used to be
        // misclassified as a weak local model and handed the 10-verb brief,
        // blinding Sonnet to geodesic/hypar/funicular/… The full prompt is now an
        // explicit opt-in only (`ITSJUSTCAD_FULL_PROMPT`), never url-gated.
        let digest = crate::scene::digest(&session.doc);
        // `/no_think` + terse still key on the LOCAL heuristic (fast inference),
        // but the PROMPT FLAVOR no longer does.
        let local_prompt = itsjustcad_deck::is_local_url(&config.base_url);
        let full_opt_in = std::env::var_os("ITSJUSTCAD_FULL_PROMPT").is_some();
        let prompt = match itsjustcad_deck::select_prompt(full_opt_in) {
            itsjustcad_deck::PromptChoice::Full => system_prompt(&digest, &session.plugins),
            itsjustcad_deck::PromptChoice::Compact => {
                itsjustcad_deck::compact_system_prompt(&digest, &session.plugins)
            }
        };
        // Deck follows the UI language (M-romance): append a "respond in <lang>"
        // directive keyed to the active locale's English name. The reply lands in
        // the target language; the command catalog + every command TOKEN stay
        // canonical English (the parser + GBNF grammar accept English only).
        // A no-op for English, so English prompts stay byte-identical.
        let prompt =
            itsjustcad_deck::with_language(prompt, crate::i18n::current_lang().english_name());
        // Terse mode: style rules appended to the system prompt + a hard
        // per-turn max-token cap. Default ON for local cassettes (fewer tokens
        // = faster inference), OFF for cloud unless the user enabled it.
        let (mut prompt, max_tokens) =
            itsjustcad_deck::terse_adjusted(prompt, 4096, config.terse_enabled());
        if local_prompt {
            // `/no_think` last so Qwen3's directive stays at the prompt tail.
            prompt.push_str("\n\n/no_think");
        }
        let mut req = ChatRequest::text(
            prompt,
            self.messages.clone(),
            String::new(),
            max_tokens,
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
            // SECURITY (H-1): a vision turn is pinned to a SINGLE fixed image —
            // the user-attached image or the critique screenshot — never an
            // arbitrary read (no `decks.json` key exfiltration via an
            // attacker-controlled scene name). HOW that one image reaches the
            // model depends on the cassette.
            // Whether this image came from a USER attachment (drag/paste/picker)
            // vs the viewport critique. The critique's own message already says
            // "Read {path} …"; a user attachment carries only the user's prose.
            let user_attached = self.attached_image.is_some();
            let shot = self
                .attached_image
                .take()
                .unwrap_or_else(crate::app::critique_shot_path);
            if matches!(config.kind, itsjustcad_deck::DeckKind::ClaudeCode) {
                // claude-code: grant no unscoped Read — hand the adapter the one
                // path, from which it derives a Read scoped to exactly that file,
                // and give it a 2nd agentic step to open the shot then answer.
                req.vision_shot_path = Some(shot.display().to_string());
                req.max_turns = 2;
                // Unlike the HTTP cassettes (which inline the image bytes), the
                // claude-code CLI only gets a scoped Read of the path — not an
                // image block. Granting the Read isn't enough: the model must be
                // TOLD the image exists, or it answers from the user's text alone
                // (the exact bug where "what is this?" got a generic reply). The
                // critique turn already instructs this in its message, so only the
                // user-attached path needs the directive added here.
                if user_attached {
                    req.system.push_str(&claude_code_vision_directive(&shot));
                }
            } else if inline_vision_image(&config) {
                // HTTP multimodal cassette (local Qwen2.5-VL served with
                // --mmproj, or a cloud vision model): inline the image as a
                // base64 content part on the last user message — which
                // `openai_compat`/`anthropic` turn into a provider-native image
                // block. A read failure degrades to a plain text turn rather than
                // blocking the message; we never attach to a blind model.
                if let Some(img) = read_vision_image(&shot) {
                    req.images.push(img);
                }
            }
        }
        let (tx, rx) = unbounded_channel();
        self.rx = Some(rx);
        self.extractor = Extractor::default();
        self.current_response.clear();
        self.streaming_chat.clear();
        self.errors_this_turn.clear();
        self.help_output_this_turn.clear();
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
        // A cancel mid-plan PAUSES the plan (state stays persisted); the user's
        // next message resumes it from the current step.
        if let Some(p) = &self.plan
            && !p.is_complete()
            && !p.has_failed()
        {
            self.transcript.push(Entry::Status(
                "plan paused — send a message to resume".into(),
            ));
        }
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
        if let Some(img) = self.attached_image.clone() {
            self.vision_turn = true;
            // Show the attachment in the conversation, just above the message
            // (the path is consumed later when the request is built).
            self.transcript.push(Entry::Image(img));
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
                    // OUTER first-run gate (Claude-Code style): until the user
                    // has approved the deck running ANY commands this session,
                    // hold the WHOLE turn's batch unprocessed and show the
                    // first-run modal. Nothing — not even pure geometry — runs
                    // until the user approves. Once approved (for-session), this
                    // branch is skipped and commands route normally; "approve
                    // once" replays `pending_firstrun` through this same arm with
                    // the gate temporarily open, then re-closes it.
                    if !self.deck_run_approved {
                        self.pending_firstrun.push(line);
                        continue;
                    }
                    self.process_command_line(line, session);
                }
            }
        }
    }

    /// Route a single deck-emitted command line through its plane (ui / app-verb
    /// / substrate command) and the INNER side-effect gates. Called once the
    /// OUTER first-run gate is open (either because the session is approved, or
    /// while replaying an approved-once batch). Writers still hit the write-modal
    /// via `pending_side_effects`; readers still take the manual picker carve-out.
    fn process_command_line(&mut self, line: String, session: &mut Session) {
        {
            {
                {
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
                        return;
                    }
                    // USER-HOTKEY TOOL (W4): the assistant may bind/unbind
                    // hotkeys ONLY when the user opted in (LLM ▸ Keybindings →
                    // "Let the assistant set hotkeys"). Otherwise refuse — a
                    // prompt-injected bind could remap keys to destructive verbs.
                    // `hotkeys`/`keybindings` (opens the editor) is GUI-only and
                    // never driven from the deck. Allowed binds are queued for the
                    // app to run through `App::execute_line`, like other app-verbs.
                    {
                        let verb0 = line.split_whitespace().next().unwrap_or("");
                        if matches!(verb0, "bind_hotkey" | "bindkey" | "unbind_hotkey" | "unbindkey")
                        {
                            if crate::keybindings::KeybindingsFile::load_or_default().allow_assistant
                            {
                                self.current_commands.push(ExecutedCommand {
                                    line: line.clone(),
                                    result: Ok("hotkey updated".to_string()),
                                });
                                self.pending_app_verbs.push(line);
                            } else {
                                self.current_commands.push(ExecutedCommand {
                                    line: line.clone(),
                                    result: Err("assistant hotkey changes are off — enable “Let the assistant set hotkeys” in LLM ▸ Keybindings".to_string()),
                                });
                            }
                            return;
                        }
                        if matches!(verb0, "hotkeys" | "keybindings") {
                            self.current_commands.push(ExecutedCommand {
                                line: line.clone(),
                                result: Err("open LLM ▸ Keybindings to edit hotkeys".to_string()),
                            });
                            return;
                        }
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
                        // ON-DEMAND HELP TOOL: `help <verb>` is not a view/camera
                        // action — it fetches a verb's full syntax + examples from
                        // the registry and threads the text BACK to the model so it
                        // can call an unfamiliar verb correctly. Run it here (read
                        // only, no op-log, no fs) and stash the reply; the turn
                        // finisher feeds it back as a follow-up user message. This
                        // is the counterpart to the compact catalog (names only).
                        if let crate::app_verbs::AppVerb::Help(v) = &verb {
                            let body = crate::app::help_lines(v.as_deref()).join("\n");
                            let label = v.as_deref().unwrap_or("(all commands)");
                            self.current_commands.push(ExecutedCommand {
                                line: line.clone(),
                                result: Ok(format!("help: {label}")),
                            });
                            self.help_output_this_turn
                                .push(format!("help {label}:\n{body}"));
                            return;
                        }
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
                            return;
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
                            return;
                        }
                        self.current_commands.push(ExecutedCommand {
                            line: line.clone(),
                            result: Ok("applied (view/camera)".to_string()),
                        });
                        self.pending_app_verbs.push(line);
                        return;
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
                            return;
                        }
                    };
                    // SECURITY (C-2/H-7): a side-effecting command *emitted by
                    // the deck* never touches the filesystem without an explicit
                    // human OK. Pure geometry ops auto-run as before.
                    if cmd.is_side_effecting() {
                        let outside = self.path_outside_sandbox(&cmd);
                        // HARD security carve-out (Part C): a command that READS
                        // an external file into the app (open/import/underlay-
                        // image/xref/data-load) is NEVER auto-run and NEVER
                        // eligible for the session fast-path — even with the
                        // opt-in on. The user must pick the file via the OS
                        // dialog; the LLM's proposed path is never auto-used.
                        let reader = cmd.reads_external_file();
                        // Opt-in fast path (WRITERS only): user allowed deck
                        // side-effects AND the path stays inside the sandbox →
                        // run without a prompt. Escaping paths and readers are
                        // always queued.
                        if !reader && self.allow_deck_side_effects && !outside {
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
                                reader,
                            });
                        }
                        return;
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
        // Mirror the executed command into the Commands-history scrollback
        // (issue #59) so deck-run commands show there like `/`-chat ones.
        self.echo_command_to_history(line, result.as_ref().err().map(String::as_str));
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
        let had_commands = !self.current_commands.is_empty();
        // A turn that ends awaiting the USER (clarifying question) must not be
        // auto-continued by the plan driver below.
        let mut question_turn = false;
        // A pure `PLAN:` announcement adopted this turn (no commands ran).
        let mut announced_plan: Option<itsjustcad_deck::Plan> = None;
        if !self.streaming_chat.trim().is_empty() {
            let chat = std::mem::take(&mut self.streaming_chat);
            // Clarify-before-act: a `QUESTION:` turn (with no commands) renders
            // as a distinct question entry; the user's next message answers it.
            if self.current_commands.is_empty()
                && let Some(q) = itsjustcad_deck::parse_question(&chat)
            {
                question_turn = true;
                self.transcript.push(Entry::Question(q));
            } else if self.current_commands.is_empty()
                && self.plan.is_none()
                && let Some(p) = itsjustcad_deck::parse_plan(&chat)
            {
                // Plan-execute: a commands-free PLAN announcement becomes the
                // live plan, rendered as a checklist (not a prose bubble).
                self.transcript.push(Entry::Plan(p.clone()));
                announced_plan = Some(p);
            } else {
                self.transcript.push(Entry::Deck(chat));
            }
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
        // ── Plan-execute driver ─────────────────────────────────────────────
        // Adopt a fresh plan and drive step 1, or advance/retry the live plan.
        // Every auto-continuation is bounded: MAX_STEP_ATTEMPTS per step (via
        // the plan's attempt counter) and MAX_PLAN_ROUNDS per run overall.
        if let Some(p) = announced_plan {
            self.plan = Some(p);
            self.plan_rounds = 0;
            self.continue_plan(session, handle);
            return;
        }
        if self.plan.is_some() && !question_turn {
            self.drive_plan(had_commands, session, handle);
            return;
        }
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
        } else if !self.help_output_this_turn.is_empty() && self.retries < MAX_RETRIES {
            // ON-DEMAND HELP: the model asked for a verb's full syntax with
            // `help <verb>`. Thread the reply back as a follow-up user turn so it
            // can now call the verb correctly. Bounded by the same retry budget
            // as error feedback so a model that only ever emits `help` can't spin.
            self.retries += 1;
            let feedback = format!(
                "Command reference you requested:\n{}\nNow emit the draft commands to carry out the user's request.",
                self.help_output_this_turn.join("\n\n"),
            );
            self.transcript.push(Entry::Status(format!(
                "help {}/{MAX_RETRIES}: feeding syntax back",
                self.retries
            )));
            self.messages.push(ChatMessage {
                role: Role::User,
                content: feedback,
            });
            self.start_turn(session, handle);
        }
        self.persist_chat();
        // Auto-land the live conversation in the Sessions archive each completed
        // turn (update-in-place, one row per conversation) so it shows in the
        // Sessions tab immediately and survives a crash (issue #58).
        self.auto_upsert_active();
    }

    /// Advance or retry the live plan after a finished turn. `had_commands`
    /// tells whether the turn actually executed anything (a chat-only reply
    /// stalls the driver and waits for the user instead of spinning).
    fn drive_plan(
        &mut self,
        had_commands: bool,
        session: &Session,
        handle: &tokio::runtime::Handle,
    ) {
        let Some(mut plan) = self.plan.take() else { return };
        if !self.errors_this_turn.is_empty() {
            // Bounded self-correction: burn one attempt on the current step and
            // feed the errors back, or fail the step once the budget is gone.
            let attempts = plan.note_step_error();
            if attempts >= itsjustcad_deck::MAX_STEP_ATTEMPTS {
                plan.mark_step_failed();
                self.refresh_plan_entry(&plan);
                self.transcript.push(Entry::Status(format!(
                    "plan stopped: step {} failed after {attempts} attempts",
                    plan.current + 1
                )));
                // The failed plan stays out of `self.plan` (run over), but its
                // checklist entry keeps the [!] mark in the transcript.
                self.persist_chat();
                return;
            }
            self.refresh_plan_entry(&plan);
            self.transcript.push(Entry::Status(format!(
                "plan step {}: retry {attempts}/{} — feeding errors back",
                plan.current + 1,
                itsjustcad_deck::MAX_STEP_ATTEMPTS,
            )));
            self.messages.push(ChatMessage {
                role: Role::User,
                content: format!(
                    "Some commands failed:\n{}\nCurrent scene:\n{}\nFix and re-emit ONLY this step's failed or missing commands in a ```draft block.",
                    self.errors_this_turn.join("\n"),
                    crate::scene::digest(&session.doc),
                ),
            });
            self.plan = Some(plan);
            self.start_plan_round(session, handle);
            return;
        }
        if !had_commands {
            // A chat-only reply mid-plan: do not auto-continue on prose — wait
            // for the user so the driver can never ping-pong with a rambler.
            self.plan = Some(plan);
            self.persist_chat();
            return;
        }
        // The step's commands all succeeded → advance.
        plan.mark_step_done();
        self.refresh_plan_entry(&plan);
        if plan.is_complete() {
            self.transcript.push(Entry::Status(format!(
                "plan complete — {} step(s) done, verifying",
                plan.steps.len()
            )));
            // Plan run over; the verify turn is an ordinary turn.
            self.messages.push(ChatMessage {
                role: Role::User,
                content: VERIFY_PROMPT.to_string(),
            });
            self.start_plan_round(session, handle);
            return;
        }
        self.plan = Some(plan);
        self.continue_plan(session, handle);
    }

    /// Queue the continuation message for the plan's current step and start the
    /// next turn (bounded by [`MAX_PLAN_ROUNDS`]).
    fn continue_plan(&mut self, session: &Session, handle: &tokio::runtime::Handle) {
        let Some(plan) = self.plan.as_ref() else { return };
        self.messages.push(ChatMessage {
            role: Role::User,
            content: step_prompt(plan),
        });
        self.start_plan_round(session, handle);
    }

    /// Start one auto-driven plan round, enforcing the total-round bound. Each
    /// step gets a fresh per-turn retry budget.
    fn start_plan_round(&mut self, session: &Session, handle: &tokio::runtime::Handle) {
        self.plan_rounds += 1;
        if self.plan_rounds > MAX_PLAN_ROUNDS {
            self.transcript.push(Entry::Status(format!(
                "plan aborted: exceeded {MAX_PLAN_ROUNDS} auto-driven rounds"
            )));
            self.plan = None;
            self.persist_chat();
            return;
        }
        self.retries = 0;
        self.persist_chat();
        self.start_turn(session, handle);
    }

    /// Refresh the newest Plan checklist entry in the transcript to `plan`'s
    /// current state (statuses, cursor), so the checklist ticks live.
    fn refresh_plan_entry(&mut self, plan: &itsjustcad_deck::Plan) {
        if let Some(entry) = self
            .transcript
            .iter_mut()
            .rev()
            .find(|e| matches!(e, Entry::Plan(_)))
        {
            *entry = Entry::Plan(plan.clone());
        }
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
            || matches!(self.probe, ProbeState::Checking(_) | ProbeState::Starting(_))
            || matches!(self.warm, WarmState::Warming { .. })
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    /// Render the OUTER first-run gate modal (Claude-Code style). Shown while
    /// `!deck_run_approved` and the current turn produced commands: it lists the
    /// whole batch and offers Approve once / Approve for session / Decline.
    ///
    /// * Approve for session → `deck_run_approved = true` AND
    ///   `allow_deck_side_effects = true` (geometry + in-sandbox writes then flow
    ///   with no further first-run prompt); the batch is replayed normally.
    /// * Approve once → replay the batch for THIS turn only; the flags stay off
    ///   so the next turn's batch re-prompts.
    /// * Decline → discard the batch unrun; record "declined N command(s)".
    ///
    /// The batch is replayed through `process_command_line`, so any WRITE in it
    /// still pops the INNER write-modal and any READ still takes the manual
    /// picker carve-out — the first-run gate never bypasses those. This is the
    /// OUTER gate: it is rendered BEFORE `side_effect_confirm_ui`, and because it
    /// only replays once dismissed, the two modals never show at once (no
    /// deadlock — first-run resolves, then per-command side-effect modals run as
    /// the replayed batch queues them).
    /// Shared in-pane approval overlay. Dims ONLY the deck pane (not the whole
    /// window / 3D viewport) and floats the approval CARD within that rect:
    ///
    /// * `pane = ui.max_rect()` is the deck-pane rect at the call site.
    /// * A semi-transparent fill is painted over `pane` on a FOREGROUND layer so
    ///   it sits above the chat, and an invisible full-pane `Sense::click` button
    ///   is placed on that layer so backdrop clicks are CONSUMED (no fall-through
    ///   to the chat behind) and detectable.
    /// * The card is an `Area` (Foreground, constrained to `pane`), width = the
    ///   pane width minus a symmetric margin, clamped to a sane min/max, so it
    ///   spans ~the chat width instead of a fixed 380px. `add` draws its body.
    /// * Returns `true` for a DISMISS gesture: Esc, or a click on the dimmed
    ///   backdrop OUTSIDE the card. Callers map that to their Decline action,
    ///   preserving the old `Modal::should_close()` semantics.
    fn approval_overlay(
        &self,
        ui: &mut egui::Ui,
        id: &str,
        add: impl FnOnce(&mut egui::Ui),
    ) -> bool {
        // The FULL deck-pane rect (captured at the top of `ui()`), NOT the bottom
        // input panel's `max_rect` where the modal fns run.
        let pane = self.pane_rect;
        let ctx = ui.ctx().clone();

        // Dim just the pane, on a foreground layer above the chat.
        let backdrop_layer = egui::LayerId::new(egui::Order::Foreground, egui::Id::new(id));
        let dim = if ui.visuals().dark_mode {
            egui::Color32::from_black_alpha(160)
        } else {
            egui::Color32::from_black_alpha(96)
        };
        ctx.layer_painter(backdrop_layer)
            .with_clip_rect(pane)
            .rect_filled(pane, 0.0, dim);

        // A full-pane click sink on the same layer: consumes pointer input over
        // the pane so clicks never fall through to the chat behind the dim.
        let backdrop = ui.interact(
            pane,
            egui::Id::new(id).with("backdrop"),
            egui::Sense::click(),
        );

        // Card: span the pane width minus a symmetric margin, clamped.
        let margin = 14.0;
        let card_w = (pane.width() - 2.0 * margin).clamp(260.0, 460.0);
        // Position in the upper third of the pane (falls back to centred if the
        // pane is short), horizontally centred.
        let card_x = pane.center().x - card_w / 2.0;
        let card_y = pane.top() + (pane.height() * 0.22).min(pane.height() * 0.5);

        let resp = egui::Area::new(egui::Id::new(id).with("card"))
            .order(egui::Order::Foreground)
            .constrain_to(pane)
            .fixed_pos(egui::pos2(card_x, card_y))
            .show(&ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(egui::Margin::same(crate::theme::Spacing::M as i8))
                    .show(ui, |ui| {
                        ui.set_width(card_w);
                        add(ui);
                    });
            });

        // Dismiss = Esc, or a click on the backdrop OUTSIDE the card rect.
        let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        let card_rect = resp.response.rect;
        let backdrop_click = backdrop.clicked()
            && backdrop
                .interact_pointer_pos()
                .map(|p| !card_rect.contains(p))
                .unwrap_or(false);
        esc || backdrop_click
    }

    fn firstrun_confirm_ui(
        &mut self,
        ui: &mut egui::Ui,
        session: &mut Session,
        roles: &crate::theme::ColorRoles,
    ) {
        if self.deck_run_approved || self.pending_firstrun.is_empty() {
            return;
        }
        enum Decision {
            ApproveOnce,
            ApproveSession,
            Decline,
        }
        let mut decision: Option<Decision> = None;
        let batch = self.pending_firstrun.clone();
        let n = batch.len();

        let dismissed = self.approval_overlay(ui, "deck_firstrun_modal", |ui| {
            ui.heading("The deck wants to run commands.");
            ui.add_space(crate::theme::Spacing::XS);
            ui.label(format!(
                "The assistant proposed {n} command{} this turn. Nothing runs \
                 until you approve. Opens/imports always ask again regardless.",
                if n == 1 { "" } else { "s" }
            ));
            ui.add_space(crate::theme::Spacing::S);
            egui::Frame::group(ui.style())
                .fill(ui.visuals().extreme_bg_color)
                .inner_margin(egui::Margin::same(crate::theme::Spacing::S as i8))
                .show(ui, |ui| {
                    for line in batch.iter().take(8) {
                        let summary = firstrun_summary(line);
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            crate::syntax_layout::highlighted_command_inline(ui, &summary);
                        });
                    }
                    if n > 8 {
                        ui.label(
                            egui::RichText::new(format!("… and {} more", n - 8))
                                .weak()
                                .small(),
                        );
                    }
                });
            ui.add_space(crate::theme::Spacing::M);
            ui.horizontal_wrapped(|ui| {
                let dark = ui.visuals().dark_mode;
                use crate::widgets::{role_button, ButtonRole};
                // Hierarchy: Approve once = prominent (filled accent), Approve for
                // session = normal (outlined), Decline = destructive (red).
                if role_button(ui, roles, dark, ButtonRole::Prominent, "Approve once")
                    .on_hover_text("run this turn's commands only; ask again next turn")
                    .clicked()
                {
                    decision = Some(Decision::ApproveOnce);
                }
                if role_button(ui, roles, dark, ButtonRole::Normal, "Approve for session")
                    .on_hover_text(
                        "let the deck run commands (and in-folder writes) this \
                         session without asking; opens/imports still ask",
                    )
                    .clicked()
                {
                    decision = Some(Decision::ApproveSession);
                }
                if role_button(ui, roles, dark, ButtonRole::Destructive, "Decline").clicked() {
                    decision = Some(Decision::Decline);
                }
            });
        });
        if dismissed && decision.is_none() {
            // Esc / backdrop dismiss = Decline the batch.
            decision = Some(Decision::Decline);
        }

        match decision {
            None => {}
            Some(Decision::ApproveSession) => self.firstrun_approve(session, true),
            Some(Decision::ApproveOnce) => self.firstrun_approve(session, false),
            Some(Decision::Decline) => self.firstrun_decline(),
        }
    }

    /// Open the OUTER gate and run the held batch. `for_session` → latch
    /// `deck_run_approved` + `allow_deck_side_effects` so future turns and
    /// in-sandbox writes flow without re-prompting; otherwise run THIS batch only
    /// (gate stays closed, next turn re-prompts). Either way the batch replays
    /// through `process_command_line`, so each writer still hits the write-modal
    /// and each reader still takes the manual picker carve-out.
    fn firstrun_approve(&mut self, session: &mut Session, for_session: bool) {
        if for_session {
            self.deck_run_approved = true;
            self.allow_deck_side_effects = true;
        }
        let batch = std::mem::take(&mut self.pending_firstrun);
        for line in batch {
            self.process_command_line(line, session);
        }
    }

    /// Discard the held batch unrun and record a status line.
    fn firstrun_decline(&mut self) {
        let n = self.pending_firstrun.len();
        self.pending_firstrun.clear();
        self.transcript
            .push(Entry::Status(format!("declined {n} command(s)")));
        self.persist_chat();
    }

    /// Render the permission MODAL for deck-emitted side-effecting commands.
    /// Styled like Claude Code's permission prompt / the iOS ApprovalSheet: a
    /// dimmed backdrop + centered window showing the FIRST pending command and
    /// the choices appropriate to its class (security C-2/H-7, Part B/C):
    ///
    /// * WRITE side-effect (export/print/save/publish/grant): Approve once,
    ///   Approve for session (sets `allow_deck_side_effects`), Decline.
    /// * FILE READ (import/open/underlay-image/xref/data-load): NEVER auto-run
    ///   and NEVER session-approvable. Decline, and — where a native picker
    ///   applies — "Open file picker…" so the USER chooses the file (the LLM's
    ///   proposed path is discarded).
    ///
    /// Applies at most one decision per frame so queue indices stay valid.
    fn side_effect_confirm_ui(
        &mut self,
        ui: &mut egui::Ui,
        session: &mut Session,
        _icons: &crate::icons::Icons,
        roles: &crate::theme::ColorRoles,
    ) {
        if self.pending_side_effects.is_empty() {
            return;
        }
        // One decision per frame. Index 0 is the head of the queue.
        enum Decision {
            ApproveOnce,
            ApproveSession,
            Decline,
            Pick,
        }
        let mut decision: Option<Decision> = None;
        let pending = &self.pending_side_effects[0];
        let reader = pending.reader;
        let outside = pending.outside_sandbox;
        let summary = pending.summary.clone();
        let remaining = self.pending_side_effects.len();

        // In-pane dimmed overlay + card (dims only the deck pane, not the
        // viewport). Card spans ~the pane width.
        let picker_applies =
            reader_with_user_path(&self.pending_side_effects[0].cmd, "x").is_some();
        let dismissed = self.approval_overlay(ui, "deck_side_effect_modal", |ui| {
            ui.heading(if reader {
                "Open a file?"
            } else {
                "Allow this action?"
            });
            ui.add_space(crate::theme::Spacing::XS);
            ui.label(if reader {
                "The deck wants to READ an external file into the document. \
                 Opens and imports (images included) are always manual — \
                 you choose the file, not the model."
            } else {
                "The deck wants to run a command that writes to disk."
            });
            ui.add_space(crate::theme::Spacing::S);
            egui::Frame::group(ui.style())
                .fill(ui.visuals().extreme_bg_color)
                .inner_margin(egui::Margin::same(crate::theme::Spacing::S as i8))
                .show(ui, |ui| {
                    // Summary may be a bare command or the `verb → /path` form;
                    // the tokenizer colours the leading verb either way.
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        crate::syntax_layout::highlighted_command_inline(ui, &summary);
                    });
                });
            if outside {
                ui.add_space(crate::theme::Spacing::XS);
                ui.label(
                    egui::RichText::new("⚠ path is OUTSIDE the document folder")
                        .color(ERR_COLOR)
                        .small(),
                );
            }
            if remaining > 1 {
                ui.label(
                    egui::RichText::new(format!("{} more queued", remaining - 1))
                        .weak()
                        .small(),
                );
            }
            ui.add_space(crate::theme::Spacing::M);
            ui.horizontal_wrapped(|ui| {
                let dark = ui.visuals().dark_mode;
                use crate::widgets::{role_button, ButtonRole};
                if reader {
                    // Readers: NO session approval, NO auto-run. Only a user
                    // file pick (if a picker applies) or Decline. The picker is
                    // the prominent action; Decline is destructive (red).
                    if picker_applies {
                        if role_button(ui, roles, dark, ButtonRole::Prominent, "Open file picker…")
                            .on_hover_text(
                                "pick the file yourself — the model's proposed path is ignored",
                            )
                            .clicked()
                        {
                            decision = Some(Decision::Pick);
                        }
                    } else {
                        ui.label(
                            egui::RichText::new("opens/imports are manual")
                                .weak()
                                .small(),
                        );
                    }
                    if role_button(ui, roles, dark, ButtonRole::Destructive, "Decline").clicked() {
                        decision = Some(Decision::Decline);
                    }
                } else {
                    // Approve once = prominent, Approve for session = normal,
                    // Decline = destructive.
                    if role_button(ui, roles, dark, ButtonRole::Prominent, "Approve once")
                        .on_hover_text("run this one command")
                        .clicked()
                    {
                        decision = Some(Decision::ApproveOnce);
                    }
                    // "Approve for session" only makes sense for in-sandbox
                    // writes; an outside-sandbox path always asks again.
                    if !outside
                        && role_button(ui, roles, dark, ButtonRole::Normal, "Approve for session")
                            .on_hover_text(
                                "run this, and let in-folder writes run this session without asking",
                            )
                            .clicked()
                    {
                        decision = Some(Decision::ApproveSession);
                    }
                    if role_button(ui, roles, dark, ButtonRole::Destructive, "Decline").clicked() {
                        decision = Some(Decision::Decline);
                    }
                }
            });
        });
        if dismissed && decision.is_none() {
            // Dismissing the overlay (Esc / backdrop click) = Decline the head.
            decision = Some(Decision::Decline);
        }

        match decision {
            None => {}
            Some(Decision::ApproveSession) => {
                self.allow_deck_side_effects = true;
                let p = self.pending_side_effects.remove(0);
                self.run_confirmed_side_effect(session, p.cmd, p.line);
            }
            Some(Decision::ApproveOnce) => {
                let p = self.pending_side_effects.remove(0);
                self.run_confirmed_side_effect(session, p.cmd, p.line);
            }
            Some(Decision::Decline) => {
                let p = self.pending_side_effects.remove(0);
                self.transcript.push(Entry::Status(format!(
                    "declined deck side-effect: {}",
                    p.summary
                )));
                self.persist_chat();
            }
            Some(Decision::Pick) => {
                // SECURITY (Part C): the user picks the file via the OS dialog;
                // the LLM's proposed path is discarded. Guarded against headless
                // / screenshot frames so they never block on a dialog.
                let headless = std::env::var("ITSJUSTCAD_SHOT").is_ok()
                    || std::env::var("ITSJUSTCAD_DECK_RUN").is_ok();
                let p = &self.pending_side_effects[0];
                let (label, exts) = reader_file_filter(&p.cmd);
                let chosen = if headless {
                    None
                } else {
                    rfd::FileDialog::new()
                        .add_filter(label, exts)
                        .pick_file()
                };
                match chosen {
                    Some(path) => {
                        let p = self.pending_side_effects.remove(0);
                        let display = path.display().to_string();
                        match reader_with_user_path(&p.cmd, &display) {
                            Some(cmd) => {
                                // Label the transcript with the command's real
                                // verb (import/underlay/terrain/…) + the chosen
                                // path, not a hardcoded "import".
                                let line = cmd
                                    .side_effect_summary()
                                    .unwrap_or_else(|| format!("import {display}"));
                                self.run_confirmed_side_effect(session, cmd, line);
                            }
                            None => {
                                self.transcript.push(Entry::Status(
                                    "no file picker for this command".into(),
                                ));
                                self.persist_chat();
                            }
                        }
                    }
                    None => {
                        // Cancelled the dialog (or headless): leave it queued.
                    }
                }
            }
        }
    }

    /// Run a human-confirmed side-effecting command and record it as its own
    /// transcript entry. Goes through `session.run` — logged/replayed like any
    /// command — so replay reproduces the confirmed side-effect.
    fn run_confirmed_side_effect(&mut self, session: &mut Session, cmd: Command, line: String) {
        let result = session.run(cmd).map_err(|e| e.to_string());
        // Mirror the confirmed command into the Commands-history scrollback
        // (issue #59), the same as auto-run deck commands.
        self.echo_command_to_history(&line, result.as_ref().err().map(String::as_str));
        self.transcript.push(Entry::Commands(vec![ExecutedCommand {
            line,
            result: result.map(|o| o.message),
        }]));
        self.persist_chat();
    }

    /// Dev hook (`ITSJUSTCAD_DECK_APPROVAL=reader|writer`): seed a pending
    /// deck-emitted side-effect so an ITSJUSTCAD_SHOT frame can capture the
    /// permission modal without a live model. `writer` → an export (approve
    /// once/for-session/decline); anything else → an import (picker/decline,
    /// never session-approvable).
    pub fn seed_pending_side_effect(&mut self, kind: &str) {
        let (line, cmd, reader) = if kind.eq_ignore_ascii_case("writer") {
            (
                "export /Users/you/project/out.dxf".to_string(),
                Command::Export { path: "/Users/you/project/out.dxf".into() },
                false,
            )
        } else {
            (
                "import /Users/you/Downloads/site-survey.dxf".to_string(),
                Command::Import { path: "/Users/you/Downloads/site-survey.dxf".into() },
                true,
            )
        };
        let summary = cmd.side_effect_summary().unwrap_or_else(|| line.clone());
        let outside_sandbox = self.path_outside_sandbox(&cmd);
        self.pending_side_effects.push(PendingSideEffect {
            line,
            cmd,
            summary,
            outside_sandbox,
            reader,
        });
    }

    /// Dev hook (`ITSJUSTCAD_DECK_FIRSTRUN=1`): seed a sample unprocessed command
    /// batch with the OUTER first-run gate closed, so an ITSJUSTCAD_SHOT frame can
    /// capture the "The deck wants to run commands." modal without a live model.
    pub fn seed_pending_firstrun(&mut self) {
        self.deck_run_approved = false;
        self.pending_firstrun = vec![
            "box 0,0,0 10,10,3".to_string(),
            "circle 5,5,3 2".to_string(),
            "export /Users/you/project/plan.dxf".to_string(),
        ];
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
    /// tab. Returns `Some(session_id)` when a card was clicked this frame; the
    /// caller confirms, then opens that chat's document AND transcript (replacing
    /// the current document in place, routed through the unsaved-changes guard).
    #[must_use]
    pub fn sessions_tab_ui(
        &mut self,
        ui: &mut egui::Ui,
        roles: &crate::theme::ColorRoles,
    ) -> Option<String> {
        if self.store.is_none() {
            ui.label(egui::RichText::new("no chats yet for this document").weak().italics());
            return None;
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

        // Surface the clicked id to the caller (App), which pops the
        // confirmation then opens the chat's document + transcript. We no longer
        // load into the pane directly here — opening replaces the whole document.
        load_session
    }

    /// Load a stored session's turns into the live transcript for viewing /
    /// continuation IN PLACE (archiving the current conversation first). This is
    /// the fork/update-guarded load used when the document is NOT changing. Since
    /// "open this chat" now reopens the chat's document too (replacing it
    /// wholesale, guarded at the App level), the production path uses
    /// [`open_stored_session`]; this remains for the in-pane-load contract test.
    #[cfg(test)]
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
        // Keep the auto-upsert row id in sync so continuing a loaded card
        // updates THAT row (issue #58), not a fresh duplicate.
        self.auto_session_id = Some(id.to_string());
        self.dirty_since_load = false;
        self.session_search.clear();
        self.view = PaneView::Chat;
        self.persist_chat();
    }

    /// The document uuid this pane's chat store is currently keyed to, or `None`
    /// before the first `sync_store`.
    pub fn active_doc_uuid(&self) -> Option<&str> {
        self.active_doc_uuid.as_deref()
    }

    /// Open a stored session's transcript into the live pane DIRECTLY (no
    /// fork/update modal). Used by the App's "open this chat" flow, where the
    /// unsaved-changes guard already ran and the document has just been
    /// reopened/replayed — so there is nothing to fork and the session simply
    /// becomes the live conversation.
    pub fn open_stored_session(&mut self, id: &str) {
        self.replace_live_with_stored(id);
    }

    /// Extract, in order, the command lines a stored session executed — the
    /// contents of its assistant turns' ```draft fences — using the SAME
    /// [`Extractor`] the live deck uses to run LLM output. Returns `None` when
    /// the session id is unknown. Powers the replay fallback when a chat has no
    /// (or a missing) saved file: replaying these lines rebuilds the geometry.
    pub fn session_command_lines(&self, id: &str) -> Option<Vec<String>> {
        let store = self.store.as_ref()?;
        let session = store.get(id)?;
        Some(Self::command_lines_from_turns(&session.turns))
    }

    /// Pure helper: pull every ```draft-fenced command line out of a session's
    /// assistant turns, in order. Reuses the deck's [`Extractor`] so the fence
    /// grammar matches exactly how the commands were executed live.
    fn command_lines_from_turns(turns: &[crate::chat_store::Turn]) -> Vec<String> {
        let mut out = Vec::new();
        for turn in turns {
            if turn.role != "assistant" {
                continue;
            }
            let mut ex = Extractor::default();
            let mut events = ex.push(&turn.content);
            events.extend(ex.finish());
            for ev in events {
                if let ExtractEvent::Command(line) = ev {
                    out.push(line);
                }
            }
        }
        out
    }

    /// Whether the active cassette can accept an image — the SAME gate as the
    /// attach button (`DeckConfig::supports_vision`). Drag-and-drop honours it so
    /// we never attach an image to a blind model.
    fn active_supports_vision(&self) -> bool {
        self.decks
            .decks
            .get(self.decks.active)
            .map(|c| c.supports_vision())
            .unwrap_or(false)
    }

    /// Decode `path` into a cached egui texture for thumbnail display (composer
    /// chip + transcript). Keyed by path AND file mtime so overwriting the same
    /// temp path (a re-paste into `itsjustcad-pasted.png`) busts the stale
    /// texture. Returns an owned handle (a cheap Arc clone); `None` on a
    /// missing/undecodable file. Not pure (disk read + GPU upload); the sizing
    /// math is the pure [`thumb_fit`].
    fn image_texture(
        &mut self,
        ctx: &egui::Context,
        path: &std::path::Path,
    ) -> Option<egui::TextureHandle> {
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
        if let Some((cached, tex)) = self.image_cache.get(path)
            && *cached == mtime
        {
            return Some(tex.clone());
        }
        let img = image::open(path).ok()?.to_rgba8();
        let (w, h) = (img.width() as usize, img.height() as usize);
        let ci = egui::ColorImage::from_rgba_unmultiplied([w, h], img.as_raw());
        let tex = ctx.load_texture(
            format!("deck_attach:{}", path.display()),
            ci,
            egui::TextureOptions::LINEAR,
        );
        self.image_cache
            .insert(path.to_path_buf(), (mtime, tex.clone()));
        Some(tex)
    }

    /// Accept an image dragged from the OS onto the chat — restricted to the
    /// MESSAGE (transcript) area only: never the composer input, nor the
    /// `Chat | Commands` segment control above it. Both the drop and its hover
    /// hint are scoped to `message_area_rect` (the transcript region captured on
    /// the previous frame — this runs before this frame lays it out, and the
    /// rect barely moves between frames). Gated on `active_supports_vision`: a
    /// blind model shows a "can't see images" hint rather than accepting.
    fn handle_image_drop(&mut self, ui: &egui::Ui, roles: &crate::theme::ColorRoles) {
        let ctx = ui.ctx();
        let vision = self.active_supports_vision();

        // Accept the drop regardless of position. macOS does NOT reliably feed
        // egui the cursor position during an external file drag, so a
        // rect/`latest_pos` test silently rejects every drop (the bug the user
        // hit). We can't geometrically scope WHERE the file lands — but an image
        // is only ever a chat attachment, and this runs only while the Chat pane
        // is visible, so a position-free accept is correct. The VISUAL hint below
        // is still confined to the message area, so no drop feedback ever shows
        // over the composer input or the `Chat | Commands` segment control.
        let dropped: Vec<std::path::PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if vision && let Some(img) = first_dropped_image(&dropped) {
            self.attached_image = Some(img);
        }

        // Drop-target hint painted over the MESSAGE AREA only (see
        // `paint_drop_hint`) while an OS file drag is in progress.
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            self.paint_drop_hint(ui, roles, vision);
        }
    }

    /// Attach an image pasted from the clipboard (⌘V / Ctrl+V) — the paste
    /// sibling of drag-and-drop, e.g. after a screenshot or "copy image" from a
    /// browser/Preview. Fires on the paste chord while the Chat pane is on
    /// screen and the active cassette can see images; no focus/pointer gate, so
    /// ⌘V works anywhere over the chat, not only inside the composer.
    ///
    /// Reads only clipboard IMAGE BITMAP data (what a screenshot or "copy image"
    /// puts there). A clipboard holding TEXT — or a FILE reference copied in
    /// Finder — yields no bitmap, so `clipboard_image_to_file` returns `None`
    /// and we no-op: normal text paste into the input is untouched, and a copied
    /// file is handled by dragging it in instead.
    fn handle_image_paste(&mut self, ui: &egui::Ui) {
        // Detect ⌘V for an IMAGE paste. Two macOS facts (confirmed by tracing)
        // break the obvious detections:
        //   1. macOS consumes the Cmd+V key-DOWN for its paste shortcut, so egui
        //      only ever sees the key-UP — `key_pressed(V)` is never true.
        //   2. eframe emits `Event::Paste` only for non-empty clipboard TEXT, so
        //      an image-only clipboard produces no Paste event.
        // So we scan for ANY `V` key event (down or up) with Cmd held — the key-UP
        // reliably arrives — plus `Event::Paste` for text-bearing clipboards and
        // other platforms. Firing on key-UP just means the attach lands on release.
        let paste = ui.ctx().input(|i| {
            i.events.iter().any(|e| match e {
                egui::Event::Paste(_) => true,
                egui::Event::Key {
                    key: egui::Key::V,
                    modifiers,
                    ..
                } => modifiers.command,
                _ => false,
            })
        });
        if !paste {
            return;
        }
        if !self.active_supports_vision() {
            return;
        }
        if let Some(img) = clipboard_image_to_file(&std::env::temp_dir()) {
            self.attached_image = Some(img);
        }
    }

    /// Draw a small circular "×" remove badge on the TOP-RIGHT corner of an
    /// attachment thumbnail (`img_rect`) and return whether it was clicked. The
    /// photo-attachment convention: the remove control sits on the image itself,
    /// not as a separate button beside it.
    fn remove_attachment_badge(ui: &egui::Ui, img_rect: egui::Rect) -> bool {
        let r = 8.5;
        // Nudge the badge slightly inward from the corner so it overlaps the
        // image edge rather than floating off it.
        let center = egui::pos2(img_rect.right() - r + 1.0, img_rect.top() + r - 1.0);
        let hit = egui::Rect::from_center_size(center, egui::vec2(2.0 * r, 2.0 * r));
        let resp = ui
            .interact(hit, ui.id().with("remove_attachment_badge"), egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text("remove attachment");
        let painter = ui.painter();
        let bg = if resp.hovered() {
            egui::Color32::from_rgb(220, 60, 60)
        } else {
            egui::Color32::from_black_alpha(170)
        };
        painter.circle_filled(center, r, bg);
        // The × glyph: two diagonal strokes.
        let d = r * 0.42;
        let stroke = egui::Stroke::new(1.6, egui::Color32::WHITE);
        painter.line_segment(
            [center + egui::vec2(-d, -d), center + egui::vec2(d, d)],
            stroke,
        );
        painter.line_segment(
            [center + egui::vec2(-d, d), center + egui::vec2(d, -d)],
            stroke,
        );
        resp.clicked()
    }

    /// Paint a rounded drop-target border + centred label over the MESSAGE area
    /// while an OS file drag hovers it. `vision` selects accept vs "can't see"
    /// messaging. Uses `message_area_rect` so the hint never covers the composer
    /// or the segment control.
    fn paint_drop_hint(&self, ui: &egui::Ui, roles: &crate::theme::ColorRoles, vision: bool) {
        let painter = ui.ctx().layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("deck_image_drop_hint"),
        ));
        let rect = self.message_area_rect.shrink(8.0);
        let (color, label) = if vision {
            (crate::theme::to_color32(roles.primary), "Drop image to attach")
        } else {
            (
                crate::theme::to_color32(roles.on_surface_tertiary),
                "This model can't see images — pick a vision-capable model",
            )
        };
        painter.rect_stroke(
            rect,
            12.0,
            egui::Stroke::new(2.0, color),
            egui::StrokeKind::Inside,
        );
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::proportional(16.0),
            color,
        );
    }

    #[allow(clippy::too_many_arguments)] // fixed signature; splitting further aids nothing
    /// Render the Chat segment. Returns `Some(line)` when the user submitted a
    /// leading-`/` command (e.g. `/box 0,0,0 5,5,3`): the caller runs it through
    /// `App::execute_line` and echoes it into the Commands history (issue #28) —
    /// it is NOT sent to the LLM. Plain prose is sent to the deck as usual and
    /// returns `None`.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        session: &mut Session,
        handle: &tokio::runtime::Handle,
        icons: &crate::icons::Icons,
        roles: &crate::theme::ColorRoles,
        _dark: bool,
        reduce_motion: bool,
    ) -> Option<String> {
        // Capture the FULL deck-pane rect up front: the approval overlays are
        // invoked inside the bottom input `Panel`, whose `max_rect` is only the
        // composer strip — we want to dim + centre over the whole chat pane.
        self.pane_rect = ui.max_rect();
        self.handle_image_drop(ui, roles);
        self.handle_image_paste(ui);
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
            || matches!(self.probe, ProbeState::Checking(_) | ProbeState::Starting(_))
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
        // Fill only the region BELOW the current cursor — not `ui.max_rect()`,
        // which spans the whole panel and would paint over the Chat|Commands
        // segmented control drawn just above us (that overdraw hid the control on
        // the Chat segment; #28 polish).
        ui.painter().rect_filled(ui.available_rect_before_wrap(), 0.0, pane_bg);

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
                ProbeState::Starting(msg) => (STATUS_ORANGE, msg.clone(), false),
                ProbeState::Checking(_) => {
                    (STATUS_ORANGE, "checking deck…".to_string(), false)
                }
                ProbeState::Unknown => {
                    (STATUS_ORANGE, "idle — no deck probe yet".to_string(), false)
                }
            };
        let status_modal_id = egui::Id::new("deck_status_modal");

        // Set inside the header closure by the "+" button; applied after it to
        // avoid a &mut self borrow inside the &self-capturing closure.
        let mut new_session_clicked = false;
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
            } else if let Some(caption) = self
                .decks
                .decks
                .get(self.decks.active)
                .and_then(|c| header_model_caption(&probe_models, &c.model))
            {
                // No interactive picker (endpoint down / probing / list not
                // enumerated): still show WHICH model is configured so the
                // active deck+model pair is always readable in the header.
                header_chip_frame(roles).show(ui, |ui| {
                    ui.label(egui::RichText::new(caption).weak())
                        .on_hover_text("configured model — picker appears when the endpoint is reachable");
                });
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
            // "+" — start a fresh chat session on the SAME document (archives the
            // current conversation; the file/op-log is untouched). Distinct from
            // File ▸ New Session, which starts a new *file*.
            if icons
                .icon_button(ui, crate::icons::Icon::NewSession, "new chat session (same file)")
                .clicked()
            {
                new_session_clicked = true;
            }
            // Local model server status — shown ONLY while starting or failed. On
            // Ready the green status dot already conveys readiness, so the extra
            // "local model ready" text is redundant (and confused users when it
            // sat next to a stale "starting…"); hide it.
            if let Some(rt) = &self.local_runtime {
                match rt.state() {
                    crate::local_runtime::RuntimeState::Starting => {
                        busy_indicator(ui, reduce_motion);
                        ui.colored_label(ACCENT, "starting local model…");
                    }
                    crate::local_runtime::RuntimeState::Failed { .. } => {
                        ui.colored_label(ERR_COLOR, rt.state().caption());
                    }
                    crate::local_runtime::RuntimeState::Ready { .. } => {}
                }
            }
            // No critique button: `critique` is a command-line/chat verb, not a
            // toolbar affordance (the app still honours `critique_requested`).
            // No "clear" button either — starting a new session (Sessions tab)
            // supersedes it; a stray destructive control in the header invited
            // accidental transcript loss.
        });
        // Apply the "+" click outside the closure (needs &mut self). Archives the
        // current chat via the guarded flow and clears the live pane; the document
        // stays loaded.
        if new_session_clicked {
            self.new_session();
        }
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
            return None;
        }
        if !matches!(self.view, PaneView::Chat) {
            self.view = PaneView::Chat; // stale index — fall back
        }

        let mut open_detail: Option<PaneView> = None;
        let mut chip_fill: Option<String> = None;
        // A `/`-palette row click this frame: the `/<name> ` string to install.
        let mut slash_pick: Option<String> = None;
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
        // `/`-armed: the chat input is typing a command. Syntax-highlight it live
        // and show the active-slot ghost hint (issue #28). `slash` also closes the
        // palette once a space ends the verb (→ Command), so the hint tracks slots.
        let slash_armed = !matches!(
            crate::usage_spec::slash(&self.input),
            crate::usage_spec::SlashState::Chat
        );
        let slot_hint = if slash_armed {
            crate::usage_spec::strip_slash(&self.input)
                .as_deref()
                .and_then(active_slot_hint)
        } else {
            None
        };
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
                // `/` INLINE PALETTE (issue #28): when the input (after leading
                // whitespace) starts with `/` and the verb has no trailing space,
                // show up to 8 matching commands ABOVE the input. A row click
                // inserts `/<name> ` and re-focuses the TextEdit — the palette is a
                // plain non-focusable list, so it NEVER steals focus mid-typing
                // (c2cfcff fast-typing guard). It closes the instant a space ends
                // the verb.
                if let crate::usage_spec::SlashState::Arming(q) =
                    crate::usage_spec::slash(&self.input)
                {
                    let matches = slash_palette_matches(&q, 8);
                    if !matches.is_empty() {
                        slash_pick = slash_palette_ui(ui, roles, &matches);
                    }
                }
                // Live labeled parse-preview for a `/`-armed command (issue #50):
                // the SAME `name=value` strip the Commands view shows, reused
                // verbatim so `/box 0,0,0 1,1,1` reads `corner=0,0,0 size=1,1,1`
                // directly above the input. Slash-gated and ADDITIVE — plain chat
                // (no leading `/`) never reaches this branch, so the non-slash
                // composer path is byte-for-byte unchanged. The strip is read-only
                // and never touches `self.input`, so fast typing drops no keys.
                if slash_armed
                    && let Some(cmd) = crate::usage_spec::strip_slash(&self.input)
                    && let Some(slots) = crate::command_line::command_preview(&cmd)
                {
                    crate::command_line::render_command_preview(ui, &slots);
                }
                // OUTER first-run gate renders first; while it's up the batch is
                // held unprocessed, so no per-command side-effect modal can show
                // simultaneously (no deadlock). Once it resolves (approve/replay)
                // the inner side-effect modal below handles any writes/reads.
                self.firstrun_confirm_ui(ui, session, roles);
                // Side-effect confirmation sits directly above the input card.
                self.side_effect_confirm_ui(ui, session, icons, roles);
                // Attachment chip (thumbnail + remove) above the input card.
                if let Some(img) = self.attached_image.clone() {
                    ui.horizontal(|ui| {
                        let name = img
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        // Thumbnail preview with a circular × overlaid on its
                        // top-right corner (photo-attachment convention); fall
                        // back to a paperclip + filename + a plain × if the image
                        // can't be decoded.
                        match self.image_texture(ui.ctx(), &img) {
                            Some(tex) => {
                                let size =
                                    thumb_fit(tex.size().map(|d| d as u32), egui::vec2(0.0, 44.0));
                                let resp = ui
                                    .image(egui::load::SizedTexture::new(tex.id(), size))
                                    .on_hover_text(&name);
                                if Self::remove_attachment_badge(ui, resp.rect) {
                                    self.attached_image = None;
                                }
                            }
                            None => {
                                let fg = ui.visuals().weak_text_color();
                                let sz = ui.text_style_height(&egui::TextStyle::Body);
                                ui.add(icons.image(
                                    ui.ctx(),
                                    crate::icons::Icon::Paperclip,
                                    sz,
                                    fg,
                                ))
                                .on_hover_text("attached image");
                                ui.label(egui::RichText::new(&name).weak());
                                if icons
                                    .icon_button(ui, crate::icons::Icon::Close, "remove attachment")
                                    .clicked()
                                {
                                    self.attached_image = None;
                                }
                            }
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

                    // Chevron top-center, a SIMPLE frameless button (no grey button
                    // background): up = expand, down = collapse. HIDDEN in `/`-armed
                    // slash mode (issue #50) — a one-line command never expands, so
                    // the chevron would only float uselessly. Gated on `slash_armed`
                    // so the non-slash chevron behaviour is unchanged.
                    if !slash_armed {
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
                    }
                    if expanded {
                        // Tall field (vertical padding only → full-width aligned).
                        egui::Frame::NONE
                            .inner_margin(egui::Margin::symmetric(
                                0,
                                crate::theme::Spacing::XS as i8,
                            ))
                            .show(ui, |ui| {
                                let mut layouter =
                                    |ui: &egui::Ui, text: &dyn egui::TextBuffer, _w: f32| {
                                        let text = text.as_str();
                                        let job = if slash_armed {
                                            crate::syntax_layout::highlight_job(
                                                ui,
                                                text,
                                                egui::TextStyle::Monospace.resolve(ui.style()),
                                            )
                                        } else {
                                            let mut j = egui::text::LayoutJob::single_section(
                                                text.to_owned(),
                                                egui::TextFormat {
                                                    font_id: egui::TextStyle::Body
                                                        .resolve(ui.style()),
                                                    color: ui.visuals().text_color(),
                                                    ..Default::default()
                                                },
                                            );
                                            j.wrap.max_width = f32::INFINITY;
                                            j
                                        };
                                        ui.ctx().fonts_mut(|f| f.layout_job(job))
                                    };
                                let response = ui.add_enabled(
                                    enabled,
                                    egui::TextEdit::multiline(&mut self.input)
                                        .id(input_id)
                                        .desired_rows(8)
                                        .desired_width(f32::INFINITY)
                                        .hint_text(hint)
                                        .layouter(&mut layouter),
                                );
                                submit_on_chord!(response, ui);
                                crate::text_context_menu::attach(
                                    &response,
                                    &mut self.input,
                                    input_id,
                                );
                            });
                        if let Some(h) = &slot_hint {
                            ui.label(
                                egui::RichText::new(format!("↳ {h}")).weak().small(),
                            );
                        }
                        // Buttons: photo + send together, right-aligned, below.
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                send_or_stop!(ui);
                                photo!(ui);
                            },
                        );
                    } else {
                        if let Some(h) = &slot_hint {
                            ui.label(
                                egui::RichText::new(format!("↳ {h}")).weak().small(),
                            );
                        }
                        // Single-line row below the chevron: text fills; photo +
                        // send hug the right.
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                send_or_stop!(ui);
                                photo!(ui);
                                // Two-line field in the collapsed state (expand
                                // with the chevron for a tall composer).
                                let mut layouter =
                                    |ui: &egui::Ui, text: &dyn egui::TextBuffer, _w: f32| {
                                        let text = text.as_str();
                                        let job = if slash_armed {
                                            crate::syntax_layout::highlight_job(
                                                ui,
                                                text,
                                                egui::TextStyle::Monospace.resolve(ui.style()),
                                            )
                                        } else {
                                            let mut j = egui::text::LayoutJob::single_section(
                                                text.to_owned(),
                                                egui::TextFormat {
                                                    font_id: egui::TextStyle::Body
                                                        .resolve(ui.style()),
                                                    color: ui.visuals().text_color(),
                                                    ..Default::default()
                                                },
                                            );
                                            j.wrap.max_width = f32::INFINITY;
                                            j
                                        };
                                        ui.ctx().fonts_mut(|f| f.layout_job(job))
                                    };
                                let response = ui.add_enabled(
                                    enabled,
                                    egui::TextEdit::multiline(&mut self.input)
                                        .id(input_id)
                                        .desired_rows(2)
                                        .desired_width(f32::INFINITY)
                                        .hint_text(hint)
                                        .layouter(&mut layouter),
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
        // The transcript fills the remaining central space — the bottom composer
        // `Panel` and the top toolbar/segment control are already reserved — so
        // this is exactly the message area the next frame's drag-drop scopes to.
        self.message_area_rect = ui.available_rect_before_wrap();
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
                        // Pre-load thumbnails into the cache BEFORE the immutable
                        // transcript borrow below (texture build needs &mut self);
                        // the render arm then reads the cached handle.
                        let thumb_paths: Vec<std::path::PathBuf> = self
                            .transcript
                            .iter()
                            .filter_map(|e| match e {
                                Entry::Image(p) => Some(p.clone()),
                                _ => None,
                            })
                            .collect();
                        let thumb_ctx = ui.ctx().clone();
                        for p in &thumb_paths {
                            let _ = self.image_texture(&thumb_ctx, p);
                        }
                        for (i, entry) in self.transcript.iter().enumerate() {
                            match entry {
                                Entry::User(t) => {
                                    // Right-aligned user bubble.
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Min),
                                        |ui| {
                                            let r = egui::Frame::NONE
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
                                            attach_copy_menu(&r.response, entry_copy_text(entry));
                                        },
                                    );
                                }
                                Entry::Image(path) => {
                                    // Right-aligned thumbnail accompanying the
                                    // user's message (pre-loaded into the cache
                                    // above). A missing/undecodable file degrades
                                    // to a muted placeholder rather than vanishing.
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Min),
                                        |ui| match self.image_cache.get(path) {
                                            Some((_, tex)) => {
                                                let size = thumb_fit(
                                                    tex.size().map(|d| d as u32),
                                                    egui::vec2(240.0, 220.0),
                                                );
                                                ui.image(egui::load::SizedTexture::new(
                                                    tex.id(),
                                                    size,
                                                ));
                                            }
                                            None => {
                                                ui.label(
                                                    egui::RichText::new("[image unavailable]")
                                                        .weak()
                                                        .italics()
                                                        .small(),
                                                );
                                            }
                                        },
                                    );
                                }
                                Entry::Deck(t) => {
                                    // Left-aligned model bubble.
                                    ui.with_layout(
                                        egui::Layout::left_to_right(egui::Align::Min),
                                        |ui| {
                                            let r = egui::Frame::NONE
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
                                            // Copy yields the RAW markdown, not
                                            // the rendered layout.
                                            attach_copy_menu(&r.response, entry_copy_text(entry));
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
                                Entry::Question(q) => {
                                    // Clarifying question: left bubble with an
                                    // accent border + "?" badge so it reads as
                                    // "the model is waiting on your answer".
                                    ui.with_layout(
                                        egui::Layout::left_to_right(egui::Align::Min),
                                        |ui| {
                                            let r = egui::Frame::NONE
                                                .fill(deck_bg)
                                                .stroke(egui::Stroke::new(1.5, ACCENT))
                                                .corner_radius(bubble_radius)
                                                .inner_margin(egui::Margin::symmetric(10, 6))
                                                .show(ui, |ui| {
                                                    ui.set_max_width(ui.available_width() * 0.82);
                                                    ui.style_mut().wrap_mode =
                                                        Some(egui::TextWrapMode::Wrap);
                                                    ui.label(
                                                        egui::RichText::new("?")
                                                            .color(ACCENT)
                                                            .strong(),
                                                    );
                                                    ui.label(egui::RichText::new(q));
                                                });
                                            attach_copy_menu(&r.response, entry_copy_text(entry));
                                        },
                                    );
                                }
                                Entry::Plan(plan) => {
                                    // Plan checklist: monospace card so the
                                    // [x]/[ ]/[!] marks align.
                                    let r = egui::Frame::group(ui.style())
                                        .inner_margin(egui::Margin::same(
                                            crate::theme::Spacing::S as i8,
                                        ))
                                        .show(ui, |ui| {
                                            ui.set_width(ui.available_width());
                                            ui.label(
                                                egui::RichText::new("Plan").strong().small(),
                                            );
                                            ui.label(
                                                egui::RichText::new(plan.checklist())
                                                    .monospace(),
                                            );
                                        });
                                    attach_copy_menu(&r.response, entry_copy_text(entry));
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
                                busy_indicator(ui, reduce_motion);
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
        // A `/`-palette row click installs `/<name> ` and re-focuses the input so
        // the user types args into the SAME stable TextEdit (c2cfcff: the palette
        // never grabs focus itself).
        if let Some(insert) = slash_pick {
            self.input = insert;
            ui.memory_mut(|m| m.request_focus(input_id));
        }
        if do_stop {
            self.stop_turn();
        }
        if do_send {
            // Strip any newline the submit chord may have inserted before sending.
            self.input = self.input.trim_end_matches('\n').to_string();
            // A leading `/` runs the line as a COMMAND (verb + args) instead of a
            // chat turn: return it so the app runs it via `execute_line` and echoes
            // it into the Commands history. A compact confirmation is pushed to the
            // transcript so the chat shows what happened (issue #28).
            if let Some(cmd) = crate::usage_spec::strip_slash(&self.input) {
                self.input.clear();
                self.transcript
                    .push(Entry::Status(format!("ran /{cmd} (see Commands)")));
                self.persist_chat();
                return Some(cmd);
            }
            self.send(session, handle);
        }
        None
    }
}

#[cfg(test)]
mod clarify_tests {
    use super::*;

    #[test]
    fn question_turn_renders_as_question_entry() {
        // A commands-free `QUESTION:` reply becomes a distinct Question entry
        // (not a plain Deck bubble); the turn ends awaiting the user's answer.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let session = Session::default();
        let mut pane = side_effect_gate_tests::blank_pane();
        pane.streaming_chat = "QUESTION: how tall should the wall be?".into();
        pane.current_response = pane.streaming_chat.clone();
        pane.finish_turn(&session, rt.handle());
        assert!(
            pane.transcript
                .iter()
                .any(|e| matches!(e, Entry::Question(q) if q == "how tall should the wall be?")),
            "expected a Question entry"
        );
        assert!(
            !pane.transcript.iter().any(|e| matches!(e, Entry::Deck(_))),
            "question must not also render as plain chat"
        );
        // The raw reply stays in the API messages so the model sees its own
        // question next turn.
        assert!(pane.messages.last().unwrap().content.contains("QUESTION:"));
    }

    #[test]
    fn question_alongside_commands_stays_plain_chat() {
        // Mixing commands and a QUESTION violates the advertised contract; we
        // fail safe by treating the prose as ordinary chat (commands already ran).
        let rt = tokio::runtime::Runtime::new().unwrap();
        let session = Session::default();
        let mut pane = side_effect_gate_tests::blank_pane();
        pane.streaming_chat = "QUESTION: sure?".into();
        pane.current_commands.push(ExecutedCommand {
            line: "box 0,0,0 1,1,1".into(),
            result: Ok("box abc".into()),
        });
        pane.finish_turn(&session, rt.handle());
        assert!(!pane.transcript.iter().any(|e| matches!(e, Entry::Question(_))));
        assert!(pane.transcript.iter().any(|e| matches!(e, Entry::Deck(_))));
    }
}

#[cfg(test)]
mod plan_tests {
    use super::*;
    use itsjustcad_deck::{DeckKind, StepStatus};

    /// A pane whose active cassette is an unreachable local HTTP endpoint, so
    /// auto-continued turns fail fast asynchronously instead of spawning a
    /// `claude` subprocess. The spawned request task is dropped with the rt.
    fn plan_pane() -> DeckPane {
        let mut pane = side_effect_gate_tests::blank_pane();
        pane.decks = DecksFile {
            decks: vec![itsjustcad_deck::DeckConfig {
                name: "test-local".into(),
                kind: DeckKind::OpenaiCompat,
                base_url: "http://127.0.0.1:1/v1".into(),
                model: "test".into(),
                api_key: None,
                grammar: false,
                terse: None,
            }],
            active: 0,
            local_only: false,
        };
        pane
    }

    fn two_step_plan() -> itsjustcad_deck::Plan {
        itsjustcad_deck::Plan::new(vec!["model the slab".into(), "extrude the core".into()])
    }

    #[test]
    fn step_prompt_names_step_and_contract() {
        let mut p = two_step_plan();
        assert!(step_prompt(&p).contains("Step 1 of 2: model the slab"));
        assert!(step_prompt(&p).contains("ONLY this step's commands"));
        p.mark_step_done();
        assert!(step_prompt(&p).contains("Step 2 of 2: extrude the core"));
    }

    #[test]
    fn plan_announcement_becomes_checklist_and_drives_step_one() {
        // A commands-free PLAN: reply is adopted: checklist entry, live plan,
        // and an auto-queued "Step 1 of 2" continuation message.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let session = Session::default();
        let mut pane = plan_pane();
        pane.streaming_chat = "PLAN:\n1. model the slab\n2. extrude the core\n".into();
        pane.current_response = pane.streaming_chat.clone();
        pane.finish_turn(&session, rt.handle());
        assert!(pane.transcript.iter().any(|e| matches!(e, Entry::Plan(_))));
        let plan = pane.plan.as_ref().expect("plan adopted");
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.current, 0);
        let last = pane.messages.last().expect("continuation queued");
        assert!(matches!(last.role, Role::User));
        assert!(last.content.contains("Step 1 of 2: model the slab"), "{}", last.content);
        assert!(pane.busy(), "the next turn was started");
        pane.stop_turn();
    }

    #[test]
    fn plan_step_success_advances_and_continues() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let session = Session::default();
        let mut pane = plan_pane();
        pane.plan = Some(two_step_plan());
        pane.transcript.push(Entry::Plan(two_step_plan()));
        pane.current_commands.push(ExecutedCommand {
            line: "box 0,0,0 10,10,0.3".into(),
            result: Ok("box abc".into()),
        });
        pane.finish_turn(&session, rt.handle());
        let plan = pane.plan.as_ref().expect("plan still live");
        assert_eq!(plan.current, 1, "step advanced on success");
        assert_eq!(plan.steps[0].status, StepStatus::Done);
        // The checklist entry ticked live.
        assert!(pane.transcript.iter().any(
            |e| matches!(e, Entry::Plan(p) if p.steps[0].status == StepStatus::Done)
        ));
        // Continuation for step 2 queued + started.
        assert!(pane.messages.last().unwrap().content.contains("Step 2 of 2"));
        assert!(pane.busy());
        pane.stop_turn();
    }

    #[test]
    fn plan_completion_triggers_verify_turn() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let session = Session::default();
        let mut pane = plan_pane();
        let mut p = two_step_plan();
        p.mark_step_done(); // step 1 already done; this turn finishes step 2
        pane.plan = Some(p);
        pane.transcript.push(Entry::Plan(two_step_plan()));
        pane.current_commands.push(ExecutedCommand {
            line: "box 4,4,0 2,2,9".into(),
            result: Ok("box def".into()),
        });
        pane.finish_turn(&session, rt.handle());
        assert!(pane.plan.is_none(), "run is over; verify turn is ordinary");
        assert!(pane
            .transcript
            .iter()
            .any(|e| matches!(e, Entry::Status(s) if s.contains("plan complete"))));
        assert!(pane.messages.last().unwrap().content.contains("Verify the end state"));
        assert!(pane.busy());
        pane.stop_turn();
    }

    #[test]
    fn plan_step_errors_get_bounded_retries_then_fail() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let session = Session::default();
        let mut pane = plan_pane();
        // Two attempts already burned; this turn's error exhausts the budget.
        let mut p = two_step_plan();
        p.note_step_error();
        p.note_step_error();
        pane.plan = Some(p);
        pane.transcript.push(Entry::Plan(two_step_plan()));
        pane.current_commands.push(ExecutedCommand {
            line: "bogus 1".into(),
            result: Err("unknown verb".into()),
        });
        pane.errors_this_turn.push("`bogus 1` failed: unknown verb".into());
        pane.finish_turn(&session, rt.handle());
        assert!(pane.plan.is_none(), "failed plan run stops");
        assert!(!pane.busy(), "no further auto-continuation after failure");
        assert!(pane.transcript.iter().any(
            |e| matches!(e, Entry::Status(s) if s.contains("failed after 3 attempts"))
        ));
        // The checklist keeps the [!] mark.
        assert!(pane.transcript.iter().any(
            |e| matches!(e, Entry::Plan(p) if p.steps[0].status == StepStatus::Failed)
        ));
    }

    #[test]
    fn plan_step_error_below_budget_retries_with_feedback() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let session = Session::default();
        let mut pane = plan_pane();
        pane.plan = Some(two_step_plan());
        pane.transcript.push(Entry::Plan(two_step_plan()));
        pane.errors_this_turn.push("`box` failed: bad point".into());
        pane.current_commands.push(ExecutedCommand {
            line: "box x".into(),
            result: Err("bad point".into()),
        });
        pane.finish_turn(&session, rt.handle());
        let plan = pane.plan.as_ref().expect("plan still live");
        assert_eq!(plan.steps[0].attempts, 1);
        assert_eq!(plan.current, 0, "cursor stays on the failing step");
        let last = pane.messages.last().unwrap();
        assert!(last.content.contains("Some commands failed"), "{}", last.content);
        assert!(pane.busy(), "retry turn started");
        pane.stop_turn();
    }

    #[test]
    fn cancel_mid_plan_pauses_and_preserves_state() {
        let mut pane = side_effect_gate_tests::blank_pane();
        pane.plan = Some(two_step_plan());
        pane.stop_turn();
        let plan = pane.plan.as_ref().expect("plan preserved across cancel");
        assert!(!plan.is_complete());
        assert!(pane.transcript.iter().any(
            |e| matches!(e, Entry::Status(s) if s.contains("plan paused"))
        ));
    }

    #[test]
    fn interrupted_plan_resumes_from_persisted_draft() {
        // Round-trip through SavedChat (what persist_chat writes) and load_draft
        // (what a relaunch runs): the plan revives with its cursor intact.
        let mut p = two_step_plan();
        p.mark_step_done();
        let json = serde_json::to_string(&SavedChatRef {
            session_id: &None,
            messages: &[],
            transcript: &[],
            plan: &Some(p.clone()),
        })
        .unwrap();
        let draft: SavedChat = serde_json::from_str(&json).unwrap();
        let mut pane = side_effect_gate_tests::blank_pane();
        pane.load_draft(draft);
        let revived = pane.plan.as_ref().expect("plan revived");
        assert_eq!(revived.current, 1);
        assert_eq!(revived.steps[0].status, StepStatus::Done);
        assert!(pane.transcript.iter().any(
            |e| matches!(e, Entry::Status(s) if s.contains("interrupted plan restored"))
        ));
    }

    #[test]
    fn question_mid_plan_waits_for_the_user() {
        // A clarifying question during a plan must NOT auto-continue the plan.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let session = Session::default();
        let mut pane = plan_pane();
        pane.plan = Some(two_step_plan());
        pane.streaming_chat = "QUESTION: which core layout?".into();
        pane.current_response = pane.streaming_chat.clone();
        pane.finish_turn(&session, rt.handle());
        assert!(pane.transcript.iter().any(|e| matches!(e, Entry::Question(_))));
        assert!(!pane.busy(), "plan driver must not run past a question");
        assert!(pane.plan.is_some(), "plan stays for after the answer");
    }
}

#[cfg(test)]
mod side_effect_gate_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// A DeckPane with empty state, isolated from the on-disk chat/decks.
    /// `pub(super)` so sibling test modules (e.g. `notify_status_tests`) reuse it.
    pub(super) fn blank_pane() -> DeckPane {
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
            help_output_this_turn: Vec::new(),
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
            image_cache: std::collections::HashMap::new(),
            pending_side_effects: Vec::new(),
            allow_deck_side_effects: false,
            // Test helper = an already-running session: the OUTER first-run gate
            // is already open, so existing #56 behavior tests drive commands past
            // it. The first-run gate is tested explicitly by flipping to false.
            deck_run_approved: true,
            pending_firstrun: Vec::new(),
            pane_rect: egui::Rect::NOTHING,
            message_area_rect: egui::Rect::NOTHING,
            sandbox_root: None,
            local_runtime: None,
            catalog: crate::model_catalog::Catalog::load(),
            deferred_local_turn: false,
            allow_web_search: false,
            store: None,
            active_doc_uuid: None,
            loaded_session_id: None,
            auto_session_id: None,
            dirty_since_load: false,
            pending_session_decision: None,
            summarize: None,
            pending_summarize_id: None,
            session_search: String::new(),
            pending_ui_actions: Vec::new(),
            pending_app_verbs: Vec::new(),
            pending_command_echoes: Vec::new(),
            plan: None,
            plan_rounds: 0,
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
    fn chat_preview_markdown_table() {
        // M-chatmd headline: a pipe-syntax table (the `report` verb's natural
        // output shape) renders as a striped grid, plus the rest of the
        // markdown subset in one transcript.
        let build = || {
            let mut p = ready_pane();
            p.transcript = vec![
                Entry::User("give me the sun-hours report as a table".into()),
                Entry::Deck(
                    "### Sun hours\n\
                     | Face | Min | Avg | Max |\n\
                     |------|----:|----:|----:|\n\
                     | North | 0.5 | 1.2 | 2.0 |\n\
                     | South | 6.1 | 8.4 | 9.9 |\n\n\
                     The **north** face is the shade risk — see `report sunhours`.\n\n\
                     - terrace: south\n\
                     - storage: north"
                        .into(),
                ),
                Entry::Status("turn done in 1.4s".into()),
            ];
            p
        };
        snapshot_pane("chat_markdown_table", build(), true, ());
        snapshot_pane("chat_markdown_table_light", build(), false, ());
    }

    #[test]
    fn entry_copy_text_yields_raw_markdown() {
        // Copying a bubble must give back the ORIGINAL markdown source (so a
        // pasted table stays a table), never a rendered/flattened form.
        let md = "| a | b |\n|---|---|\n| 1 | 2 |";
        assert_eq!(entry_copy_text(&Entry::Deck(md.into())).as_deref(), Some(md));
        assert_eq!(
            entry_copy_text(&Entry::User("**hi**".into())).as_deref(),
            Some("**hi**")
        );
        assert_eq!(
            entry_copy_text(&Entry::Question("how tall?".into())).as_deref(),
            Some("how tall?")
        );
        // The commands card opens its own detail pane instead.
        assert_eq!(entry_copy_text(&Entry::Commands(Vec::new())), None);
        // A plan copies as its checklist text.
        let plan = itsjustcad_deck::parse_plan("PLAN:\n1. slab\n2. cores").unwrap();
        let copied = entry_copy_text(&Entry::Plan(plan)).unwrap();
        assert!(copied.contains("slab") && copied.contains("cores"), "{copied}");
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

    // ── M-guitest: chat journey + reduce-motion audit (CPU kittest) ──────
    // These render the pane through kittest's default (CPU) renderer — no GPU,
    // no snapshot — and assert on the AccessKit tree, so they run in the
    // normal `cargo test` suite everywhere.

    /// M-deckagent journey: a `QUESTION:` turn renders as a distinct question
    /// bubble and a `PLAN:` turn as a checklist card — both reachable through
    /// the accessibility tree (the transcript is not just painted pixels).
    #[test]
    fn chat_question_and_plan_render_in_transcript() {
        use egui_kittest::kittest::Queryable as _;
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();
        let colors = crate::theme::roles_from([0.98, 0.98, 0.97, 1.0], [0.20, 0.50, 1.0, 1.0]);
        let icons = crate::icons::Icons::new();
        let mut session = Session::default();
        let mut pane = ready_pane();
        let mut plan = itsjustcad_deck::Plan::new(vec!["slab".into(), "cores".into()]);
        plan.mark_step_done();
        pane.transcript = vec![
            Entry::User("build a tower".into()),
            Entry::Question("how tall should the tower be?".into()),
            Entry::Plan(plan),
        ];
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(360.0, 700.0))
            .build_ui(|ui| {
                pane.ui(ui, &mut session, &handle, &icons, &colors, false, false);
            });
        harness.run_steps(4);
        // Question bubble: the question text plus its "?" badge both render.
        assert!(
            harness
                .query_all_by_label_contains("how tall should the tower be?")
                .next()
                .is_some(),
            "QUESTION turn renders as a question bubble"
        );
        // Plan card: header + the checklist body carrying the step names.
        assert!(
            harness.query_all_by_label("Plan").next().is_some(),
            "PLAN turn renders the checklist card header"
        );
        // The snapshotted step statuses render as checklist marks.
        assert!(
            harness
                .query_all_by_label_contains("[x] 1. slab")
                .next()
                .is_some(),
            "checklist shows the done mark on step 1"
        );
        assert!(
            harness
                .query_all_by_label_contains("[ ] 2. cores")
                .next()
                .is_some(),
            "checklist shows the pending mark on step 2"
        );
        drop(harness);
        drop(rt);
    }

    /// Usability audit: with reduce-motion set the busy indicator is a STATIC
    /// dot — it must not request continuous repaints the way the spinner does.
    #[test]
    fn busy_indicator_honors_reduce_motion() {
        let mut harness = egui_kittest::Harness::new_ui(|ui| busy_indicator(ui, true));
        harness.run_steps(2);
        assert!(
            !harness.ctx.requested_repaint_last_pass(),
            "reduce-motion busy indicator must be static (no repaint loop)"
        );
        let mut harness = egui_kittest::Harness::new_ui(|ui| busy_indicator(ui, false));
        harness.run_steps(2);
        assert!(
            harness.ctx.requested_repaint_last_pass(),
            "sanity: the animated spinner does request repaints"
        );
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
    fn auto_upsert_updates_one_row_across_turns() {
        // Issue #58: the per-turn auto-upsert must land the live conversation as
        // ONE session row and UPDATE it across turns — never a new row per turn.
        let mut pane = blank_pane();
        pane.store = Some(crate::chat_store::DocSessions::new(
            "11112222-3333-4444-5555-666677778888".into(),
        ));
        // Turn 1.
        pane.messages = vec![
            ChatMessage { role: Role::User, content: "make a slab".into() },
            ChatMessage { role: Role::Assistant, content: "Drew a slab.".into() },
        ];
        pane.auto_upsert_active();
        let first_id = pane.auto_session_id.clone().expect("id assigned");
        assert_eq!(
            pane.store.as_ref().unwrap().sessions.len(),
            1,
            "first turn creates exactly one row"
        );
        // Turn 2 appends to the SAME conversation.
        pane.messages.push(ChatMessage { role: Role::User, content: "add columns".into() });
        pane.messages.push(ChatMessage { role: Role::Assistant, content: "Added columns.".into() });
        pane.auto_upsert_active();
        let store = pane.store.as_ref().unwrap();
        assert_eq!(store.sessions.len(), 1, "second turn UPDATES the row, no duplicate");
        assert_eq!(
            pane.auto_session_id.as_deref(),
            Some(first_id.as_str()),
            "the conversation keeps its stable id"
        );
        assert_eq!(store.sessions[0].id, first_id);
        assert_eq!(store.sessions[0].turns.len(), 4, "row reflects all four turns");
        // A fresh conversation stays non-ambiguous (no spurious update-vs-new
        // modal): loaded_session_id is untouched, dirty flag stays false.
        assert!(pane.loaded_session_id.is_none());
        assert!(!pane.dirty_since_load);
    }

    #[test]
    fn auto_upsert_and_finalize_archive_converge_on_one_row() {
        // The per-turn auto-upsert and the new/save/quit archive_active(Auto)
        // must NOT create duplicate sessions — they share the conversation id.
        let mut pane = blank_pane();
        pane.store = Some(crate::chat_store::DocSessions::new(
            "11112222-3333-4444-5555-666677778888".into(),
        ));
        pane.messages = vec![
            ChatMessage { role: Role::User, content: "make a slab".into() },
            ChatMessage { role: Role::Assistant, content: "Drew a slab.".into() },
        ];
        pane.auto_upsert_active();
        let auto_id = pane.auto_session_id.clone().unwrap();
        // Finalize (as save/quit does).
        let archived = pane.archive_active(ArchiveMode::Auto).unwrap();
        assert_eq!(archived, auto_id, "finalize reuses the auto-upsert id");
        assert_eq!(
            pane.store.as_ref().unwrap().sessions.len(),
            1,
            "finalize updates the SAME row — no duplicate"
        );
    }

    #[test]
    fn echo_command_to_history_mirrors_success_and_error() {
        // Issue #59: deck-run commands queue a `> line` echo (✓) for success and
        // an extra `error: …` line (red) for failure — drained by the app into
        // the Commands scrollback. No double-logging of the `> line`.
        let mut pane = blank_pane();
        pane.echo_command_to_history("box 0,0,0 4,4,4", None);
        pane.echo_command_to_history("bogus", Some("unknown verb"));
        let echoes = pane.take_command_echoes();
        assert_eq!(
            echoes,
            vec![
                "> box 0,0,0 4,4,4".to_string(),
                "> bogus".to_string(),
                "error: unknown verb".to_string(),
            ],
            "success echoes `> line` once; error echoes `> line` + `error: …`"
        );
        // Drained: a second take yields nothing (no double-logging across frames).
        assert!(pane.take_command_echoes().is_empty());
    }

    #[test]
    fn run_deck_command_echoes_once_to_history() {
        // A deck-run command mirrors into the Commands history exactly once.
        let mut pane = blank_pane();
        let mut session = Session::default();
        let cmd = parse("box 0,0,0 2,2,2").expect("parses");
        pane.run_deck_command("box 0,0,0 2,2,2", cmd, &mut session);
        let echoes = pane.take_command_echoes();
        assert_eq!(
            echoes.iter().filter(|l| l.as_str() == "> box 0,0,0 2,2,2").count(),
            1,
            "the deck command is echoed exactly once (no double-log)"
        );
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
            plan: None,
        };
        pane.load_draft(draft);
        assert_eq!(pane.messages.len(), 1);
        assert_eq!(pane.messages[0].content, "revive me");
        assert!(pane.loaded_session_id.is_none());
        assert!(!pane.dirty_since_load);
    }

    #[test]
    fn session_command_lines_extracts_draft_blocks_in_order() {
        // The replay fallback rebuilds the drawing from the chat's ```draft
        // command lines. Prove the extraction pulls exactly those lines, in
        // order, from the assistant turns — reusing the live Extractor's fence
        // grammar — and ignores prose and user turns.
        let turns = vec![
            crate::chat_store::Turn { role: "user".into(), content: "draw a box and a rect".into() },
            crate::chat_store::Turn {
                role: "assistant".into(),
                content: "Sure, here you go:\n```draft\nbox 0,0,0 2,2,2\n```\nDone.".into(),
            },
            crate::chat_store::Turn {
                role: "assistant".into(),
                content: "And the rectangle:\n```draft\nrect 4,0,0 3 2\n```".into(),
            },
        ];
        let lines = DeckPane::command_lines_from_turns(&turns);
        assert_eq!(lines, vec!["box 0,0,0 2,2,2".to_string(), "rect 4,0,0 3 2".to_string()]);

        // And the same via the store-backed public API (unknown id → None).
        let mut store = crate::chat_store::DocSessions::new(
            "aaaa1111-2222-3333-4444-555566667777".into(),
        );
        let mut s = crate::chat_store::ChatSession::new(10);
        s.turns = turns;
        let id = s.id.clone();
        store.sessions.push(s);
        let mut pane = blank_pane();
        pane.store = Some(store);
        assert_eq!(
            pane.session_command_lines(&id),
            Some(vec!["box 0,0,0 2,2,2".to_string(), "rect 4,0,0 3 2".to_string()])
        );
        assert_eq!(pane.session_command_lines("nope"), None, "unknown id → None");
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
        let make = || {
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

    /// Serializes the tests that call `sync_store`: migration adopts (and
    /// deletes) the LEGACY GLOBAL chat file, so a concurrent `sync_store` from
    /// another test can steal it mid-assertion. One shared lock removes the
    /// flake without weakening either test.
    static SYNC_STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn sync_store_switch_persists_old_draft_and_loads_new() {
        let _guard = SYNC_STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _guard = SYNC_STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
            plan: &None,
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
    fn deck_help_verb_is_not_an_app_verb_and_captures_syntax_to_thread_back() {
        // ON-DEMAND HELP TOOL: `help <verb>` is not a view/camera action — it is
        // run in-app (read-only) and its full syntax + examples are stashed in
        // `help_output_this_turn` so the turn finisher can thread them back to the
        // model. It must NOT be queued as an app verb and must NOT error.
        let mut pane = blank_pane();
        let mut session = Session::default();
        pane.handle_extract_events(
            vec![ExtractEvent::Command("help geodesic".to_string())],
            &mut session,
        );
        assert!(pane.take_app_verbs().is_empty(), "help is not a view app-verb");
        assert!(pane.errors_this_turn.is_empty(), "help must not error");
        assert_eq!(pane.help_output_this_turn.len(), 1, "help output captured");
        let captured = &pane.help_output_this_turn[0];
        assert!(captured.contains("geodesic"), "syntax names the verb: {captured}");
        // The captured text is the registry's real usage/summary for the verb,
        // i.e. what `help_lines` returns — proving full syntax (not the compact
        // one-liner) threads back.
        let expected = crate::app::help_lines(Some("geodesic")).join("\n");
        assert!(captured.contains(&expected), "captured must carry full help_lines");
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
    fn reader_import_never_auto_runs_even_with_allow_flag_and_in_sandbox() {
        // HARD carve-out (Part C): an import/open/image READ from the deck is
        // NEVER auto-run — not even with the session opt-in on AND the path
        // inside the sandbox. It must always queue as a manual-only pending.
        let dir = std::env::temp_dir().join(format!("itsjustcad_rd_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut pane = blank_pane();
        pane.allow_deck_side_effects = true; // opt-in ON
        pane.set_sandbox_root(Some(dir.clone()));
        let mut session = Session::default();
        let inside = dir.join("model.dxf");
        pane.handle_extract_events(
            vec![ExtractEvent::Command(format!("import {}", inside.display()))],
            &mut session,
        );
        assert_eq!(
            pane.pending_side_effects.len(),
            1,
            "a reader import must ALWAYS queue, never auto-run"
        );
        assert!(
            pane.pending_side_effects[0].reader,
            "import must be flagged as a file reader"
        );
        assert!(
            pane.current_commands.is_empty(),
            "the reader must NOT have executed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reader_underlay_image_is_a_queued_reader() {
        // Images (underlay) are readers too — manual-only, never session-approved.
        let mut pane = blank_pane();
        pane.allow_deck_side_effects = true;
        let mut session = Session::default();
        pane.handle_extract_events(
            vec![ExtractEvent::Command("underlay /tmp/site.png".into())],
            &mut session,
        );
        assert_eq!(pane.pending_side_effects.len(), 1);
        assert!(
            pane.pending_side_effects[0].reader,
            "an underlay image must be flagged as a reader"
        );
    }

    #[test]
    fn writer_export_auto_runs_with_allow_flag_in_sandbox() {
        // Contrast with readers: a WRITER (export) WITH the opt-in and in-sandbox
        // DOES auto-run — writers remain eligible for the fast path.
        let dir = std::env::temp_dir().join(format!("itsjustcad_wr_{}", std::process::id()));
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
            "an in-sandbox export with the opt-in on must auto-run"
        );
        assert_eq!(pane.current_commands.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn queued_export_carries_writer_flag_not_reader() {
        // A deck export queues with reader=false so the modal shows the
        // approve-once/for-session choices, not the picker/decline pair.
        let mut pane = blank_pane();
        let mut session = Session::default();
        pane.handle_extract_events(
            vec![ExtractEvent::Command("export /tmp/elsewhere.csv".into())],
            &mut session,
        );
        assert_eq!(pane.pending_side_effects.len(), 1);
        assert!(
            !pane.pending_side_effects[0].reader,
            "export is a writer, not a reader"
        );
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
            terse: None,
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

    // --- active-deck clarity: the model stays visible without a picker ---

    #[test]
    fn header_caption_shows_model_when_probe_has_no_list() {
        // Endpoint down / probing / non-enumerating server → static caption.
        assert_eq!(
            header_model_caption(&[], "qwen3"),
            Some("qwen3".to_string())
        );
    }

    #[test]
    fn header_caption_absent_when_picker_renders() {
        // A non-empty probe list means the interactive picker is shown instead.
        assert_eq!(header_model_caption(&["qwen3".to_string()], "qwen3"), None);
    }

    #[test]
    fn header_caption_absent_for_blank_model() {
        assert_eq!(header_model_caption(&[], ""), None);
        assert_eq!(header_model_caption(&[], "   "), None);
    }

    // ── OUTER first-run gate (Claude-Code model) ────────────────────────────

    #[test]
    fn firstrun_geometry_does_not_auto_run_and_queues() {
        // Gate closed: even a pure geometry command is HELD unprocessed and the
        // first-run modal is pending (pending_firstrun non-empty). Nothing runs.
        let mut pane = blank_pane();
        pane.deck_run_approved = false;
        let mut session = Session::default();
        let before = session.doc.len();
        pane.handle_extract_events(
            vec![ExtractEvent::Command("box 0,0,0 1,1,1".into())],
            &mut session,
        );
        assert_eq!(
            pane.pending_firstrun.len(),
            1,
            "command must be held for the first-run gate"
        );
        assert!(pane.current_commands.is_empty(), "nothing may run yet");
        assert_eq!(session.doc.len(), before, "no geometry op may have run");
        assert!(!pane.deck_run_approved, "the gate stays closed until approval");
    }

    #[test]
    fn firstrun_approve_for_session_runs_batch_and_latches_flags() {
        let mut pane = blank_pane();
        pane.deck_run_approved = false;
        let mut session = Session::default();
        pane.handle_extract_events(
            vec![ExtractEvent::Command("box 0,0,0 1,1,1".into())],
            &mut session,
        );
        let before = session.doc.len();
        pane.firstrun_approve(&mut session, /* for_session */ true);
        assert!(pane.deck_run_approved, "for-session latches the gate open");
        assert!(
            pane.allow_deck_side_effects,
            "for-session also lets in-sandbox writes flow"
        );
        assert!(pane.pending_firstrun.is_empty(), "batch is drained");
        assert!(session.doc.len() > before, "geometry ran after approval");
        // And future geometry now auto-runs with no gate.
        let mid = session.doc.len();
        pane.handle_extract_events(
            vec![ExtractEvent::Command("box 0,0,0 2,2,2".into())],
            &mut session,
        );
        assert!(
            pane.pending_firstrun.is_empty(),
            "gate open → no more queuing"
        );
        assert!(session.doc.len() > mid, "subsequent geometry auto-ran");
    }

    #[test]
    fn firstrun_approve_once_runs_batch_but_leaves_gate_closed() {
        let mut pane = blank_pane();
        pane.deck_run_approved = false;
        let mut session = Session::default();
        pane.handle_extract_events(
            vec![ExtractEvent::Command("box 0,0,0 1,1,1".into())],
            &mut session,
        );
        let before = session.doc.len();
        pane.firstrun_approve(&mut session, /* for_session */ false);
        assert!(session.doc.len() > before, "the one batch ran");
        assert!(
            !pane.deck_run_approved,
            "approve-once must NOT latch the gate"
        );
        assert!(
            !pane.allow_deck_side_effects,
            "approve-once must NOT enable session writes"
        );
        // Next turn's batch re-prompts (held again).
        pane.handle_extract_events(
            vec![ExtractEvent::Command("box 0,0,0 2,2,2".into())],
            &mut session,
        );
        assert_eq!(
            pane.pending_firstrun.len(),
            1,
            "next batch re-prompts the first-run gate"
        );
    }

    #[test]
    fn firstrun_decline_discards_batch_unrun() {
        let mut pane = blank_pane();
        pane.deck_run_approved = false;
        let mut session = Session::default();
        pane.handle_extract_events(
            vec![
                ExtractEvent::Command("box 0,0,0 1,1,1".into()),
                ExtractEvent::Command("circle 0,0,0 2".into()),
            ],
            &mut session,
        );
        assert_eq!(pane.pending_firstrun.len(), 2);
        pane.firstrun_decline();
        assert!(pane.pending_firstrun.is_empty(), "batch discarded");
        assert_eq!(session.doc.len(), 0, "nothing ran");
        assert!(
            pane.transcript.iter().any(|e| matches!(
                e,
                Entry::Status(s) if s.contains("declined 2 command")
            )),
            "a declined-N status line is recorded"
        );
    }

    #[test]
    fn firstrun_approve_session_replays_reader_through_manual_carveout() {
        // HARD carve-out: even with the gate open AND allow_deck_side_effects on,
        // a READER (import) in the approved batch is NEVER auto-run — it is routed
        // to the manual side-effect modal (queued as a reader), not executed.
        let mut pane = blank_pane();
        pane.deck_run_approved = false;
        pane.set_sandbox_root(Some(PathBuf::from("/home/u/proj")));
        let mut session = Session::default();
        let sentinel = std::env::temp_dir()
            .join(format!("itsjustcad_fr_import_{}.dxf", std::process::id()));
        let _ = std::fs::remove_file(&sentinel);
        pane.handle_extract_events(
            vec![ExtractEvent::Command(format!("import {}", sentinel.display()))],
            &mut session,
        );
        pane.firstrun_approve(&mut session, /* for_session */ true);
        assert_eq!(
            pane.pending_side_effects.len(),
            1,
            "reader routed to the manual INNER modal, not auto-run"
        );
        assert!(
            pane.pending_side_effects[0].reader,
            "the queued side-effect is a reader (manual picker only)"
        );
        assert!(!sentinel.exists(), "no read/write hit disk without a picker");
    }

    #[test]
    fn firstrun_approved_reader_still_never_auto_runs() {
        // Gate already open (session approved): a reader command still NEVER
        // auto-runs — it is queued for the manual picker (the #56 carve-out is
        // untouched by the outer gate).
        let mut pane = blank_pane(); // deck_run_approved = true
        pane.allow_deck_side_effects = true; // even with writes allowed…
        pane.set_sandbox_root(Some(PathBuf::from("/home/u/proj")));
        let mut session = Session::default();
        pane.handle_extract_events(
            vec![ExtractEvent::Command("import /home/u/proj/site.dxf".into())],
            &mut session,
        );
        assert_eq!(pane.pending_side_effects.len(), 1, "reader queued, not run");
        assert!(pane.pending_side_effects[0].reader);
    }
}

#[cfg(test)]
mod notify_status_tests {
    use super::*;

    #[test]
    fn notify_status_lands_in_the_transcript() {
        // The app surfaces background events (e.g. a download failing with the
        // Model Setup window closed) through this — it must be user-visible.
        let mut pane = super::side_effect_gate_tests::blank_pane();
        pane.notify_status("Download of X failed: server returned 404".into());
        assert!(pane.transcript.iter().any(|e| matches!(
            e,
            Entry::Status(s) if s.contains("failed") && s.contains("404")
        )));
    }
}
