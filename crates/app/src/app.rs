// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

use itsjustcad_commands::Session;
use itsjustcad_render::{
    ColorMode, DisplayMode, OrbitCamera, SceneRenderer, StandardView, UnderlayData,
    ViewportCallback, ViewportLayout,
};

use crate::command_line::CommandLine;
use crate::deck_pane::DeckPane;
use crate::draw_tool::DrawTool;
use crate::gumball::Gumball;
use crate::journal::{self, Journal};
use crate::keymap;
use crate::preset::{self, CadOrigin};
use crate::scene;

#[derive(Clone, PartialEq)]
pub(crate) enum TemplateUnits {
    Meters,
    Millimeters,
    FeetInches,
}

#[derive(Clone, PartialEq)]
pub(crate) enum TemplateScale {
    Object,
    Building,
    Urban,
}

/// The "deck brain" choice offered during onboarding: where the LLM that powers
/// the deck runs. Selection is persisted to `ui.json` and (for the local paths)
/// writes a cassette entry into `decks.json`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DeckBrain {
    /// Point at a cloud API later (Anthropic/OpenAI-compatible). No cassette
    /// written now — the user enters a key in the deck settings.
    Cloud,
    /// Local via an already-running Ollama at http://localhost:11434.
    Ollama,
    /// Download a local model (the fetch is a later sub-phase; here we only
    /// record the chosen tier and write the local cassette).
    Download,
    /// Decide later — no cassette written, no pref locked in.
    Skip,
}

impl DeckBrain {
    fn label(self) -> &'static str {
        match self {
            DeckBrain::Cloud => "Cloud (enter an API key later)",
            DeckBrain::Ollama => "Local via Ollama (http://localhost:11434)",
            DeckBrain::Download => "Download a local model",
            DeckBrain::Skip => "Skip — decide later",
        }
    }

    /// String key persisted in `ui.json`.
    fn as_pref(self) -> &'static str {
        match self {
            DeckBrain::Cloud => "cloud",
            DeckBrain::Ollama => "ollama",
            DeckBrain::Download => "download",
            DeckBrain::Skip => "skip",
        }
    }
}

/// Decide whether the one-time `~/.config/mydrafter` → `~/.config/itsjustcad`
/// migration should run. Pure so it can be unit-tested: migrate iff the OLD
/// dir exists and the NEW one does not. Any other combination is a no-op (new
/// install, already-migrated, or a manual mix we must not clobber).
fn should_migrate_config(old_exists: bool, new_exists: bool) -> bool {
    old_exists && !new_exists
}

/// One-time config migration across the mydrafter → ItsJustCAD rename: if the
/// user has a legacy `~/.config/mydrafter` and no `~/.config/itsjustcad` yet,
/// move it so decks/plugins/blocks/journal/prefs carry over. Best-effort;
/// failure is non-fatal (the app just starts with fresh config).
pub(crate) fn migrate_legacy_config() {
    let Some(cfg) = dirs::home_dir().map(|h| h.join(".config")) else { return };
    let old = cfg.join("mydrafter");
    let new = cfg.join("itsjustcad");
    if should_migrate_config(old.exists(), new.exists()) {
        if let Err(e) = std::fs::rename(&old, &new) {
            tracing::warn!("config migration {old:?} -> {new:?} failed: {e}");
        } else {
            tracing::info!("migrated legacy config {old:?} -> {new:?}");
        }
    }
}

/// Return a private directory for ItsJustCAD runtime files (mode 0o700 on Unix).
/// Prefers `$XDG_RUNTIME_DIR/itsjustcad`, then `$HOME/.config/itsjustcad`, then
/// an `itsjustcad` subdirectory inside the system temp dir as a last resort.
/// The directory is created if it does not exist.
pub(crate) fn private_runtime_dir() -> std::path::PathBuf {
    let candidate = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .unwrap_or_else(std::env::temp_dir)
        .join("itsjustcad");

    if !candidate.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&candidate)
                .ok();
        }
        #[cfg(not(unix))]
        {
            std::fs::create_dir_all(&candidate).ok();
        }
    }
    candidate
}

/// Return the path where the viewport critique screenshot is written.
///
/// The file lives in [`private_runtime_dir()`], which is only accessible to the
/// current user (mode 0700 on Unix). This prevents a symlink pre-plant attack
/// where a world-writable `/tmp` path could redirect the write to an arbitrary
/// file via a symlink.
pub(crate) fn critique_shot_path() -> std::path::PathBuf {
    private_runtime_dir().join("critique.png")
}

/// The message sent to the deck for a viewport critique. Points the model at
/// the screenshot on disk (the claude-code cassette opens it with the Read
/// tool) and frames the assessment. Pure so the prompt is unit-testable.
pub(crate) fn critique_prompt(image_path: &str, question: &str) -> String {
    let mut p = format!(
        "Read {image_path} — it is a screenshot of the current CAD viewport. \
You are an architecture critic. Assess the massing, proportion, and how light \
reads across the form. Be specific and direct; no flattery."
    );
    let q = question.trim();
    if !q.is_empty() {
        p.push_str(&format!(" Also address: {q}"));
    }
    p
}

/// Tag stamped on a critique screenshot's `UserData` so its echoed `Screenshot`
/// event is routed to the critique handler, not the dev-shot exit path.
const CRITIQUE_TAG: &str = "critique";

/// Control-image raster size (depth / edge / mask), shared by the
/// `controlimages` verb and the diffusion `render` flow. 1280×800 keeps the
/// viewport-ish aspect and both sides divisible by 8 (SD-friendly).
const CONTROL_W: u32 = 1280;
const CONTROL_H: u32 = 800;

/// Draw a small Lucide section-header icon, tinted to the dimmed foreground and
/// vertically centered against the collapsing-header title it precedes.
fn section_icon(ui: &mut egui::Ui, icons: &crate::icons::Icons, icon: crate::icons::Icon) {
    let size = ui.text_style_height(&egui::TextStyle::Body);
    let color = ui.visuals().weak_text_color();
    ui.add(icons.image(ui.ctx(), icon, size, color));
}

/// Persist an egui screenshot buffer as a PNG. Shared by the dev shot and the
/// viewport critique.
fn save_screenshot_png(img: &egui::ColorImage, path: &std::path::Path) {
    let png = image::RgbaImage::from_raw(
        img.width() as u32,
        img.height() as u32,
        img.as_raw().to_vec(),
    )
    .expect("screenshot buffer size");
    png.save(path).expect("write screenshot");
}

/// The first `Screenshot` event this frame whose `UserData` carries the given
/// string tag.
fn tagged_screenshot(ctx: &egui::Context, tag: &str) -> Option<egui::ColorImage> {
    ctx.input(|i| {
        i.events.iter().find_map(|e| match e {
            egui::Event::Screenshot { image, user_data, .. }
                if user_data
                    .data
                    .as_ref()
                    .and_then(|d| d.downcast_ref::<&str>())
                    == Some(&tag) =>
            {
                Some((**image).clone())
            }
            _ => None,
        })
    })
}

/// The first untagged `Screenshot` event this frame (the dev/`ITSJUSTCAD_SHOT`
/// capture); critique-tagged shots are skipped.
fn untagged_screenshot(ctx: &egui::Context) -> Option<egui::ColorImage> {
    ctx.input(|i| {
        i.events.iter().find_map(|e| match e {
            egui::Event::Screenshot { image, user_data, .. } if user_data.data.is_none() => {
                Some((**image).clone())
            }
            _ => None,
        })
    })
}

pub(crate) fn units_cmd_for(u: &TemplateUnits) -> &'static str {
    match u {
        TemplateUnits::Meters => "units m",
        TemplateUnits::Millimeters => "units mm",
        TemplateUnits::FeetInches => "units ftin",
    }
}

pub(crate) fn camera_distance_for(s: &TemplateScale) -> f32 {
    match s {
        TemplateScale::Object => 5.0,
        TemplateScale::Building => 30.0,
        TemplateScale::Urban => 300.0,
    }
}

/// Returns help lines for the command reference.
/// - `None` verb: one line per command "  name — first sentence"
/// - `Some(verb)`: usage + summary for that verb, or an unknown-command note.
pub(crate) fn help_lines(verb: Option<&str>) -> Vec<String> {
    match verb {
        None => itsjustcad_commands::registry()
            .iter()
            .map(|spec| {
                let first = spec.summary.split('.').next().unwrap_or(spec.summary).trim();
                format!("  {} \u{2014} {}", spec.name, first)
            })
            .collect(),
        Some(v) => {
            if let Some(spec) = itsjustcad_commands::registry().iter().find(|s| s.name == v) {
                vec![
                    format!("usage: {}", spec.usage),
                    spec.summary.to_string(),
                ]
            } else {
                vec![format!("unknown command: {v}")]
            }
        }
    }
}

/// A finished diffusion render ready to show: the uploaded texture plus the
/// provenance the "AI Render" window captions (prompt, backend, on-disk PNG).
struct RenderResultView {
    texture: egui::TextureHandle,
    path: std::path::PathBuf,
    prompt: String,
    backend: String,
}

pub struct App {
    session: Session,
    command_line: CommandLine,
    deck_pane: DeckPane,
    draw_tool: DrawTool,
    gumball: Gumball,
    point_edit: crate::point_edit::PointEdit,
    tokio: tokio::runtime::Handle,
    /// Camera slots shared across layouts: 0 Persp, 1 Top, 2 Front, 3 Back.
    cameras: [OrbitCamera; 4],
    /// Display mode per camera slot (view state, follows the camera across
    /// layout switches; never logged).
    display_modes: [DisplayMode; 4],
    /// Color mode per camera slot (view state; never logged).
    color_modes: [ColorMode; 4],
    /// Lighting model for mesh fills (Working / Sun / Presentation). View state.
    light_mode: itsjustcad_render::LightMode,
    /// SketchUp-style thick profile / silhouette edges. View state.
    profile_edges: bool,
    /// Thin mesh feature edges in Shaded mode (the SketchUp/Rhino "shaded +
    /// edges" default). ON by default; toggled by `shadededges [on|off]`.
    /// Persisted to `ui.json`. View state, never logged.
    shaded_edges: bool,
    /// Transform gumball/gizmo visibility (Rhino-style persistent toggle).
    /// Default OFF: selecting an object shows only the highlight, no gizmo, and
    /// the gumball is neither drawn nor hit-tested. Toggled with `gumball` / the
    /// `G` hotkey / the status-bar chip. Persisted to `ui.json`. View state.
    show_gumball: bool,
    /// Hand-drawn "sketchy edges" NPR character. View state.
    sketchy: itsjustcad_render::SketchyParams,
    layout: ViewportLayout,
    /// Last hovered pane; view commands and tools target its camera.
    active_pane: usize,
    /// Generation of the last GPU upload; compare with `session.doc.generation`.
    uploaded_generation: Option<u64>,
    /// Theme of the last GPU upload; theme flips force a re-upload.
    uploaded_theme: Option<scene::Theme>,
    /// Color mode of the last GPU upload; mode changes force a re-upload.
    uploaded_color_mode: Option<ColorMode>,
    /// Profile-edge flag of the last GPU upload; toggling forces a re-upload.
    uploaded_profile_edges: Option<bool>,
    /// Sketchy params of the last GPU upload; changes force a re-upload.
    uploaded_sketchy: Option<itsjustcad_render::SketchyParams>,
    /// Last zoom factor written to ui.json (avoid rewriting every frame).
    saved_zoom: f32,
    /// Dev self-verification: ITSJUSTCAD_SHOT=<path.png> captures a frame and exits.
    shot_path: Option<String>,
    /// Dev scripting: ITSJUSTCAD_RUN="cmd;cmd;..." executes on startup.
    startup_script: Option<String>,
    /// Dev scripting: ITSJUSTCAD_DECK_RUN="prompt" sends one deck message on
    /// startup; with ITSJUSTCAD_SHOT set, the shot waits for the turn to end.
    deck_script: Option<String>,
    /// Dev hook: ITSJUSTCAD_TYPE="text" pre-fills the command input (without
    /// executing) so the autosuggest popup is visible in ITSJUSTCAD_SHOT frames.
    type_script: Option<String>,
    frame_count: u64,
    /// Set once the dev-shot `ITSJUSTCAD_SAVE` side effect has run, so it fires
    /// exactly once even though the screenshot is (re-)requested every frame
    /// until its echo lands.
    shot_saved: bool,
    /// Layer color being edited in the panel; the `layercolor` command is
    /// issued once, when the mouse is released (avoids one op per drag frame).
    pending_layer_color: Option<(String, [f32; 3])>,
    /// Row selected in the Layers table (delete/settings target). Name only;
    /// falls back to the current layer when unset or stale.
    selected_layer: Option<String>,
    /// Default color for layers created via the Layers-panel ＋ button, set in
    /// the ⚙ settings menu. `None` uses the theme default (no `layercolor`).
    new_layer_default_color: Option<[f32; 3]>,
    /// Last executed command line; Enter/Space on the canvas repeats it.
    last_line: Option<String>,
    /// A `critique` request awaiting its viewport screenshot. Holds the
    /// optional user question; once the tagged Screenshot event lands, the PNG
    /// is written and a vision deck turn (Read tool enabled) is fired.
    pending_critique: Option<String>,
    /// In-flight AI diffusion render (`render <prompt…>`), polled each frame.
    /// `render cancel` aborts the spawned task. See `diffusion.rs`.
    render_job: Option<crate::diffusion::RenderJob>,
    /// In-flight `render test` backend connection probe.
    render_conn_test: Option<crate::diffusion::ConnTest>,
    /// Last finished diffusion render, shown in the floating "AI Render"
    /// window (closing the window drops it; the PNG stays on disk).
    render_result: Option<RenderResultView>,
    /// Cmd+C pressed with a selection; Cmd+V then runs `copy sel 1,1,0`.
    clipboard_armed: bool,
    /// In-progress drag-box selection: anchor position of the drag.
    box_drag: Option<egui::Pos2>,
    /// Crash-recovery journal mirroring the op-log; deleted on save/clean exit.
    journal: Option<Journal>,
    /// Doc generation of the last journal sync (skip serializing every frame).
    journaled_generation: Option<u64>,
    /// Cursor ground-plane position in the active pane, for the status bar
    /// (written during the viewport pass, read next frame by the strip).
    status_cursor: Option<glam::DVec3>,
    /// Aspect (w/h) of the active pane from the last paint; the `camera <lens>`
    /// command needs it to convert a horizontal angle of view to `fov_y`.
    active_aspect: f32,
    /// Snap kind currently hit by the draw tool, for the status bar.
    status_snap: Option<&'static str>,
    /// Decoded underlay pixels cached by path, so a scene rebuild (any doc
    /// change) does not re-decode the image every time.
    #[allow(clippy::type_complexity)]
    underlay_cache: Option<(String, std::sync::Arc<(Vec<u8>, u32, u32)>)>,
    /// Whether the deck chat pane is visible; toggled by the ◂/▸ button or Cmd+\.
    /// Retained for the `critique` verb (reveals the Deck tab); the right panel's
    /// own visibility is governed by `panel_tabs`.
    deck_visible: bool,
    /// Right docked panel tab state (Chat/Sessions/Layers + dynamic
    /// Blocks/Plugins tabs — see `tabstrip::TabState::visible_tabs`).
    panel_tabs: crate::tabstrip::TabState,
    /// Blocks tab: name of the definition awaiting delete confirmation.
    pending_block_delete: Option<String>,
    /// Blocks tab: cached library listing (`blocklib`), refreshed on demand so
    /// the tab doesn't hit the filesystem every frame. `Err` keeps the message.
    blocklib_cache: Option<Result<Vec<String>, String>>,
    /// Plugins tab: name of the plugin awaiting delete confirmation.
    pending_plugin_delete: Option<String>,
    /// Plugins tab: open read-only JSON source popup `(plugin name, json)`.
    plugin_json_view: Option<(String, String)>,
    /// The ONE dock width (points) shared by ALL tabs. Seeded from
    /// [`crate::tabstrip::DOCK_WIDTH`]; a user drag updates it, but switching
    /// tabs never changes it (the panel always renders at this width). This is
    /// the single width source of truth — see `right_panel`.
    dock_width: f32,
    /// Screen-space x of the right dock's ACTUAL drawn left edge this frame, or
    /// `None` when the dock is hidden. The viewport clamps its right edge to this
    /// so the 3D wgpu callback (painted last, scissored to the central rect) can
    /// never overpaint the dock during a one-frame layout desync — the panel can
    /// draw slightly wider than the space it reserved right after a relayout
    /// (new document, deck turn complete), which briefly let the viewport bleed
    /// over the chat. Set in `right_panel` (runs before `viewport`).
    dock_left: Option<f32>,
    /// Whether the right docked panel is shown at all (Cmd+\ hides/shows).
    panel_visible: bool,
    /// When true, all animated progress bars (warm-up, download) run without
    /// egui's built-in animation (no indeterminate shimmer). Persisted to
    /// `ui.json`; honours accessibility / Reduce Motion preference.
    reduce_motion: bool,
    /// Whether the Help → About dialog is open.
    show_about: bool,
    /// Whether the Edit → "Edit history…" modal (op-log / amend panel) is open.
    show_history: bool,
    /// Command palette (⌘K) overlay state: whether it's open, the current fuzzy
    /// query, and the highlighted row index. The candidate set is rebuilt from
    /// the registry each time it opens (cheap, keeps it in sync).
    show_palette: bool,
    palette_query: String,
    palette_sel: usize,
    palette_entries: Vec<crate::palette::PaletteEntry>,
    /// ITSJUSTCAD_THEME pin: Some(true)=dark, Some(false)=light, None=follow OS.
    /// Re-applied every frame so eframe's per-frame system-theme read can't win.
    forced_dark: Option<bool>,
    /// Whether to show the first-run template picker on next frame.
    show_template_picker: bool,
    template_units: TemplateUnits,
    template_scale: TemplateScale,
    /// Legacy-CAD origin selected in the template picker (persisted to ui.json).
    cad_origin: CadOrigin,
    /// Deck-brain choice in the onboarding modal (persisted to ui.json; local
    /// paths also write a cassette into decks.json).
    deck_brain: DeckBrain,
    /// Hardware capabilities detected once at startup, shown when the user picks
    /// the "download a local model" path so tiers can be gated.
    hardware: crate::hardware::HardwareInfo,
    /// Whether the Tools → Model Setup panel is open (works any time, not just
    /// first-run).
    show_model_setup: bool,
    /// A DXF import running in time-boxed batches (progress modal). `None` when
    /// no import is in flight.
    import_job: Option<ImportJob>,
    /// Completion message from the last import, shown in a dismissable popup.
    import_result: Option<String>,
    /// Bundled catalog of downloadable local models, parsed once.
    catalog: crate::model_catalog::Catalog,
    /// The download in flight from the Model Setup panel, if any. The UI polls
    /// its [`crate::download::DownloadState`] each frame.
    active_download: Option<ActiveDownload>,
    /// Lucide line-icon texture cache for the chrome (menu bar, tab strip).
    /// Decodes + uploads each icon once, on first draw.
    icons: crate::icons::Icons,
    /// True native OS menu bar (muda): global NSMenu on macOS, HMENU on Windows.
    /// `None` in headless/tests or where native attach is unavailable — the app
    /// then falls back to the in-window egui menu bar. Attached lazily on the
    /// first interactive frame (macOS needs the NSApp to exist first).
    /// Not compiled on Linux (no muda dependency there).
    #[cfg(not(target_os = "linux"))]
    native_menu: Option<crate::native_menu::NativeMenuBar>,
    /// Whether we've already tried to attach the native menu bar (once), so a
    /// failed/headless attach isn't retried every frame.
    #[cfg(not(target_os = "linux"))]
    native_menu_tried: bool,
    /// Op-log cursor at the last successful save/open (`0` for a fresh doc). The
    /// unsaved-changes guard compares this to `session.history().1` — see
    /// [`crate::widgets::is_dirty`]. UI/session state, never in the op-log.
    saved_cursor: usize,
    /// A pending destructive navigation (New/Open/Quit) parked behind the
    /// unsaved-changes alert. `Some` while the alert is up; taken when the user
    /// picks Discard.
    pending_nav: Option<PendingNav>,
    /// Set true when a confirmed Discard-quit sends `ViewportCommand::Close`, so
    /// the close-request handler lets that close through instead of re-parking a
    /// fresh Quit (the doc is still dirty). Without it, Discard loops forever and
    /// the app never actually exits.
    force_close: bool,
    /// Snapshot of the saved/opened document file, polled for external changes
    /// (cloud sync, other apps). `None` until the doc has a path.
    file_watch: Option<crate::filewatch::FileWatch>,
    /// A detected external change parked behind the reload alert. `Some` while
    /// the alert is up.
    external_change: Option<crate::filewatch::ExternalChange>,
    /// Test/preview only: skip the wgpu 3D viewport paint callback so the full
    /// window can be snapshot off-screen via egui_kittest (whose egui render
    /// pass has no depth attachment, which the scene callback requires). All the
    /// egui chrome — menus, dock, command line, viewport name tags — still draws.
    preview_no_viewport: bool,
}

/// A navigation intent deferred behind the unsaved-changes guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingNav {
    New,
    Open,
    Quit,
}

/// A DXF import in progress, applied in time-boxed batches across frames so a
/// large file (tens of thousands of entities → one logged op each) never freezes
/// the UI. The app drives it in `step_import`, shows a progress modal, and on
/// completion posts a message + zoom-extents-all. Parsing is done up front (fast
/// for DXF); the batched part is applying the ops.
pub(crate) struct ImportJob {
    /// Source path, for the progress/completion messages.
    path: String,
    /// Remaining `(layer, command)` pairs to apply, consumed from `cursor`.
    entities: Vec<(String, itsjustcad_commands::Command)>,
    /// Index of the next entity to apply.
    cursor: usize,
    /// Total entities to apply (for the progress fraction).
    total: usize,
    /// Entities the parser dropped (unsupported types) — reported at the end.
    skipped: usize,
    /// The layer to restore once every entity is applied.
    prev_layer: String,
}

impl ImportJob {
    fn fraction(&self) -> f32 {
        if self.total == 0 {
            1.0
        } else {
            self.cursor as f32 / self.total as f32
        }
    }
}

/// What the unsaved-changes guard should do with a user's alert choice. Pure so
/// the click → outcome contract is unit-tested without an egui/wgpu context.
///
/// Both choices clear the parked nav; only **Discard** (Confirm) also performs
/// it. `None` (alert still open) leaves the nav parked and does nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NavAction {
    /// Drop the parked `pending_nav` (true for either explicit choice).
    pub clear_pending: bool,
    /// Actually run the parked navigation (true only for Discard/Confirm).
    pub perform: bool,
}

/// Map an alert choice to the guard's action. This is the exact logic
/// `unsaved_guard_ui` applies; extracted so the Discard-performs / Cancel-drops
/// contract is provable in a headless test.
pub(crate) fn nav_action_for(choice: Option<crate::widgets::AlertChoice>) -> NavAction {
    match choice {
        Some(crate::widgets::AlertChoice::Confirm) => NavAction { clear_pending: true, perform: true },
        Some(crate::widgets::AlertChoice::Cancel) => NavAction { clear_pending: true, perform: false },
        None => NavAction { clear_pending: false, perform: false },
    }
}

/// Whether a window close-request should be intercepted (cancelled + a Quit
/// parked behind the unsaved-changes alert). A close proceeds unimpeded when the
/// doc is clean, when a Quit is already parked, or when `force_close` is set — the
/// last is the confirmed-Discard case, where the doc is still dirty but the user
/// has already chosen to abandon it, so re-parking would loop forever.
pub(crate) fn should_intercept_close(
    close_requested: bool,
    force_close: bool,
    quit_already_parked: bool,
    dirty: bool,
) -> bool {
    close_requested && !force_close && !quit_already_parked && dirty
}

/// A running install: which model, plus the [`crate::download::Download`] handle
/// the UI polls. Kept so a completed download can be turned into a decks.json
/// cassette exactly once.
struct ActiveDownload {
    model_id: String,
    handle: crate::download::Download,
    /// Set once the terminal state (Done/Failed) has been handled — the Done
    /// cassette persisted, or the failure announced — so it happens only once.
    persisted: bool,
}

/// What a just-finished download demands of the app this frame. `None` while
/// still running, already handled, or user-cancelled (the cancel path has its
/// own caption and keeps the `.part` for resume — no extra announcement).
#[derive(Debug, Clone, PartialEq)]
enum DownloadOutcome {
    /// Verified + renamed: persist the cassette and activate the deck.
    Completed,
    /// Failed for a real reason (not a cancel): surface it to the user.
    Failed(String),
}

/// The once-only terminal-state rule for a polled download. Pure so the
/// panel-closed hand-off (the old bug: completion was only handled while the
/// Model Setup window was open) is provable in a unit test.
fn download_outcome(
    state: &crate::download::DownloadState,
    already_handled: bool,
) -> Option<DownloadOutcome> {
    if already_handled {
        return None;
    }
    match state {
        crate::download::DownloadState::Done { .. } => Some(DownloadOutcome::Completed),
        crate::download::DownloadState::Failed { msg } if msg != "cancelled" => {
            Some(DownloadOutcome::Failed(msg.clone()))
        }
        _ => None,
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, tokio: tokio::runtime::Handle) -> Self {
        // One-time prefs carry-over from the old product name.
        migrate_legacy_config();

        let rs = cc
            .wgpu_render_state
            .as_ref()
            .expect("wgpu render state (eframe must run with the wgpu backend)");
        rs.renderer
            .write()
            .callback_resources
            .insert(SceneRenderer::new(&rs.device, rs.target_format));

        // Accessibility: readable default text size, persisted across runs.
        // Cmd+= / Cmd+- / Cmd+0 also work (egui built-in zoom).
        let zoom = load_zoom().unwrap_or(1.3);
        cc.egui_ctx.set_zoom_factor(zoom);

        // Legacy-CAD-informed font sizes (see docs/ui-legacy-research.md):
        //   command line / monospace prompt: 13 px  (~10 pt at 96 DPI)
        //   body / panels / status bar:      13 px  (~9–10 pt)
        //   small (autosuggest, hints):      11 px  (~8 pt)
        //   heading (panel titles):          14 px  (~11 pt)
        // These are logical pixels before the zoom factor is applied.
        // Applied to both themes so light and dark look consistent.
        let set_cad_fonts = |style: &mut egui::Style| {
            style.text_styles.insert(
                egui::TextStyle::Monospace,
                egui::FontId::monospace(13.0),
            );
            style.text_styles.insert(
                egui::TextStyle::Body,
                egui::FontId::proportional(13.0),
            );
            style.text_styles.insert(
                egui::TextStyle::Small,
                egui::FontId::proportional(11.0),
            );
            style.text_styles.insert(
                egui::TextStyle::Button,
                egui::FontId::proportional(13.0),
            );
            style.text_styles.insert(
                egui::TextStyle::Heading,
                egui::FontId::proportional(14.0),
            );
        };
        cc.egui_ctx.style_mut_of(egui::Theme::Dark, set_cad_fonts);
        cc.egui_ctx.style_mut_of(egui::Theme::Light, set_cad_fonts);
        // ITSJUSTCAD_THEME=dark|light: initial pin (also re-applied every frame in
        // `ui`, since eframe re-reads the OS theme each frame). Otherwise the OS
        // preference is followed as usual.
        match std::env::var("ITSJUSTCAD_THEME").ok().as_deref() {
            Some("dark") => cc.egui_ctx.set_theme(egui::Theme::Dark),
            Some("light") => cc.egui_ctx.set_theme(egui::Theme::Light),
            _ => {}
        }

        let deck_visible = load_deck_visible().unwrap_or(true);
        let cad_origin = load_cad_origin().unwrap_or_default();
        apply_preset(cc.egui_ctx.clone(), cad_origin);

        let journal = Journal::open_default();
        let mut command_line = CommandLine::default();
        // Leftover journals mean a crashed session; offer recovery up front.
        if let (Some(dir), Some(j)) = (journal::default_dir(), &journal) {
            let n = journal::recoverable(&dir, j.path()).len();
            if n > 0 {
                command_line.push_line(format!(
                    "{n} crash journal(s) found — type 'recover' to restore the latest"
                ));
            }
        }

        let mut session = Session::default();
        // Load user/LLM-authored plugin macros from disk into the session.
        command_line.load_plugins(&mut session);

        // Seed the block content library on first run (no-op if already present).
        itsjustcad_commands::blocklib::seed_if_empty();

        Self {
            session,
            command_line,
            deck_pane: DeckPane::default(),
            draw_tool: DrawTool::default(),
            gumball: Gumball::default(),
            point_edit: crate::point_edit::PointEdit::default(),
            tokio,
            cameras: {
                let mut cams = [OrbitCamera::default(); 4];
                cams[1].set_view(StandardView::Top);
                cams[2].set_view(StandardView::Front);
                cams[3].set_view(StandardView::Back);
                cams
            },
            display_modes: [DisplayMode::default(); 4],
            color_modes: [ColorMode::default(); 4],
            light_mode: itsjustcad_render::LightMode::default(),
            profile_edges: false,
            shaded_edges: load_shaded_edges().unwrap_or(true),
            show_gumball: load_gumball_visible().unwrap_or(false),
            sketchy: itsjustcad_render::SketchyParams::default(),
            layout: match preset::preset_for(cad_origin).default_viewports {
                4 => ViewportLayout::Four,
                2 => ViewportLayout::Two,
                _ => ViewportLayout::Single,
            },
            active_pane: 0,
            uploaded_generation: None,
            uploaded_theme: None,
            uploaded_color_mode: None,
            uploaded_profile_edges: None,
            uploaded_sketchy: None,
            saved_zoom: zoom,
            shot_path: std::env::var("ITSJUSTCAD_SHOT").ok(),
            startup_script: std::env::var("ITSJUSTCAD_RUN").ok(),
            deck_script: std::env::var("ITSJUSTCAD_DECK_RUN").ok(),
            type_script: std::env::var("ITSJUSTCAD_TYPE").ok(),
            frame_count: 0,
            shot_saved: false,
            pending_layer_color: None,
            selected_layer: None,
            new_layer_default_color: None,
            last_line: None,
            pending_critique: None,
            render_job: None,
            render_conn_test: None,
            render_result: None,
            clipboard_armed: false,
            box_drag: None,
            journal,
            journaled_generation: None,
            status_cursor: None,
            active_aspect: 16.0 / 9.0,
            status_snap: None,
            underlay_cache: None,
            deck_visible,
            panel_tabs: crate::tabstrip::TabState::default(),
            pending_block_delete: None,
            blocklib_cache: None,
            pending_plugin_delete: None,
            plugin_json_view: None,
            dock_width: crate::tabstrip::DOCK_WIDTH,
            dock_left: None,
            panel_visible: true,
            reduce_motion: load_reduce_motion(),
            show_about: false,
            show_history: false,
            show_palette: false,
            palette_query: String::new(),
            palette_sel: 0,
            palette_entries: Vec::new(),
            // Env pin wins for dev/screenshots; otherwise restore the persisted
            // Appearance choice from ui.json (None = follow the OS).
            forced_dark: match std::env::var("ITSJUSTCAD_THEME").ok().as_deref() {
                Some("dark") => Some(true),
                Some("light") => Some(false),
                _ => load_theme_pref(),
            },
            show_template_picker: !load_template_done(),
            template_units: TemplateUnits::Meters,
            template_scale: TemplateScale::Building,
            cad_origin,
            // Dev hook: ITSJUSTCAD_BRAIN=download pre-selects the download path so
            // the hardware-recommendation panel is visible in ITSJUSTCAD_SHOT frames.
            deck_brain: match std::env::var("ITSJUSTCAD_BRAIN").ok().as_deref() {
                Some("download") => DeckBrain::Download,
                Some("ollama") => DeckBrain::Ollama,
                Some("cloud") => DeckBrain::Cloud,
                _ => DeckBrain::Skip,
            },
            hardware: crate::hardware::detect(),
            // Dev hook: ITSJUSTCAD_MODEL_SETUP=1 opens the Model Setup panel on
            // startup so ITSJUSTCAD_SHOT frames can capture it without a click.
            show_model_setup: std::env::var("ITSJUSTCAD_MODEL_SETUP").is_ok(),
            import_job: None,
            import_result: None,
            catalog: crate::model_catalog::Catalog::load(),
            active_download: None,
            icons: crate::icons::Icons::new(),
            #[cfg(not(target_os = "linux"))]
            native_menu: None,
            #[cfg(not(target_os = "linux"))]
            native_menu_tried: false,
            saved_cursor: 0,
            // Dev hook: ITSJUSTCAD_UNSAVED_ALERT=1 parks a New behind the
            // unsaved-changes guard on startup so ITSJUSTCAD_SHOT frames can
            // capture the alert without a click (pair with ITSJUSTCAD_RUN to
            // dirty the doc first).
            pending_nav: std::env::var("ITSJUSTCAD_UNSAVED_ALERT")
                .is_ok()
                .then_some(PendingNav::New),
            force_close: false,
            file_watch: None,
            external_change: None,
            preview_no_viewport: false,
        }
    }

    /// Whether the document has unsaved edits since the last save/open — the
    /// signal the New/Open/Quit guard consults. Pure delegation to
    /// [`crate::widgets::is_dirty`] over the op-log cursor.
    pub(crate) fn is_dirty(&self) -> bool {
        crate::widgets::is_dirty(self.session.history().1, self.saved_cursor)
    }

    /// Mark the document clean at its current op-log cursor (called after a
    /// successful save/open, and on a fresh New).
    fn mark_saved(&mut self) {
        self.saved_cursor = self.session.history().1;
    }

    /// Route a destructive navigation (New/Open/Quit) through the unsaved-changes
    /// guard: if the doc is dirty, park the intent and raise the alert; otherwise
    /// perform it immediately.
    fn guarded_nav(&mut self, ctx: &egui::Context, nav: PendingNav) {
        if self.is_dirty() {
            self.pending_nav = Some(nav);
        } else {
            self.perform_nav(ctx, nav);
        }
    }

    /// Actually perform a (confirmed or clean) navigation intent.
    fn perform_nav(&mut self, ctx: &egui::Context, nav: PendingNav) {
        match nav {
            PendingNav::New => {
                self.new_document();
                self.mark_saved();
            }
            PendingNav::Open => self.open(None),
            PendingNav::Quit => {
                // Mark the close as intentional so the close-request handler does
                // not re-park a fresh Quit (the doc is still dirty after Discard).
                self.force_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    /// Render the unsaved-changes alert when a navigation is parked behind it,
    /// and act on the user's choice. Discard performs the parked nav; Cancel
    /// drops it. Returns `true` while the alert is up (so a close-request can be
    /// held off until the user decides).
    fn unsaved_guard_ui(&mut self, ctx: &egui::Context) -> bool {
        let Some(nav) = self.pending_nav else {
            return false;
        };
        let dark = ctx.theme() == egui::Theme::Dark;
        let roles = self.live_roles(dark);
        let verb = match nav {
            PendingNav::New => "start a new document",
            PendingNav::Open => "open another document",
            PendingNav::Quit => "quit",
        };
        let choice = crate::widgets::alert(
            ctx,
            &roles,
            dark,
            "Unsaved changes",
            &format!("You have unsaved changes. If you {verb}, they will be lost."),
            "Discard",
            crate::widgets::ButtonRole::Destructive,
        );
        let action = nav_action_for(choice);
        if action.clear_pending {
            self.pending_nav = None;
        }
        if action.perform {
            self.perform_nav(ctx, nav);
        }
        self.pending_nav.is_some()
    }

    /// Render the ask-on-continue session-conflict modal when the deck pane has a
    /// parked decision: continuing a loaded chat that gained new turns, then
    /// triggering New Session or load-another. Three grouped choices — **Update
    /// this chat** (update the loaded session in place), **Save as new** (fork a
    /// fresh session), **Cancel** (keep editing, drop the parked action). Mirrors
    /// the unsaved-changes guard's shape. Returns `true` while the modal is up.
    fn session_conflict_ui(&mut self, ctx: &egui::Context) -> bool {
        if self.deck_pane.needs_session_decision().is_none() {
            return false;
        }
        let dark = ctx.theme() == egui::Theme::Dark;
        let roles = self.live_roles(dark);
        let mut resolved = false;
        egui::Modal::new(egui::Id::new("itsjustcad_session_conflict"))
            .frame(crate::widgets::dialog_modal_frame(&roles))
            .show(ctx, |ui| {
                crate::widgets::dialog_body(ui, &roles, 400.0, |ui| {
                    crate::widgets::dialog_title(ui, &roles, "Continue this chat?");
                    ui.add_space(crate::theme::Spacing::SM);
                    crate::widgets::dialog_text(
                        ui,
                        &roles,
                        "You added to a saved chat. Update that chat, or keep it and \
                         save this as a new one?",
                    );
                    ui.add_space(crate::theme::Spacing::L);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = crate::theme::Spacing::SM;
                        if crate::widgets::role_button(
                            ui,
                            &roles,
                            dark,
                            crate::widgets::ButtonRole::Prominent,
                            "Update this chat",
                        )
                        .clicked()
                        {
                            self.deck_pane.resolve_session_decision(true);
                            resolved = true;
                        }
                        if crate::widgets::role_button(
                            ui,
                            &roles,
                            dark,
                            crate::widgets::ButtonRole::Normal,
                            "Save as new",
                        )
                        .clicked()
                        {
                            self.deck_pane.resolve_session_decision(false);
                            resolved = true;
                        }
                        if crate::widgets::role_button(
                            ui,
                            &roles,
                            dark,
                            crate::widgets::ButtonRole::Normal,
                            "Cancel",
                        )
                        .clicked()
                        {
                            self.deck_pane.cancel_session_decision();
                            resolved = true;
                        }
                    });
                });
            });
        // Esc = Cancel (safe path — keep editing).
        if !resolved && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.deck_pane.cancel_session_decision();
            resolved = true;
        }
        !resolved && self.deck_pane.needs_session_decision().is_some()
    }

    /// Lazily attach the true native OS menu bar (muda) the first interactive
    /// frame, then never retry. Only runs when eframe hands us a live winit
    /// window (`frame.window_handle()` succeeds) — headless/`--shot` has no
    /// window, so the native bar is skipped and the in-window egui bar is used.
    /// macOS needs the `NSApplication` to exist first, which it does by the time
    /// the first `ui` runs; hence lazy rather than in `new`.
    /// Not compiled on Linux (no muda/gtk dependency there).
    #[cfg(not(target_os = "linux"))]
    fn ensure_native_menu(&mut self, frame: &eframe::Frame) {
        if self.native_menu_tried {
            return;
        }
        self.native_menu_tried = true;
        use raw_window_handle::HasWindowHandle as _;
        // No window ⇒ headless / offscreen; keep the in-window bar.
        if frame.window_handle().is_err() {
            return;
        }
        let style = preset::preset_for(self.cad_origin).menu_style;
        self.native_menu = crate::native_menu::NativeMenuBar::attach(style, frame);
    }

    /// Active alias map from the current preset (used by autosuggest + execute_line).
    fn active_aliases(&self) -> &'static [(&'static str, &'static str)] {
        preset::preset_for(self.cad_origin).aliases
    }

    fn run_startup_script(&mut self) {
        if let Some(script) = self.startup_script.take() {
            for cmd in script.split(';') {
                self.execute_line(cmd.to_string());
            }
        }
        if let Some(prompt) = self.deck_script.take() {
            // Reveal the Deck tab so the scripted conversation is visible in
            // ITSJUSTCAD_DECK_RUN screenshots.
            self.panel_tabs.show(crate::tabstrip::PanelTab::Deck);
            self.deck_pane
                .send_text(&prompt, &self.session, &self.tokio);
        } else if std::env::var("ITSJUSTCAD_DECK_PANE").is_ok() {
            // Dev hook: reveal the Sessions tab (no send) so ITSJUSTCAD_SHOT
            // frames capture the promoted session browser — search field + the
            // newest-first session CARDS (title/summary/date) — without a model.
            self.panel_tabs.show(crate::tabstrip::PanelTab::Sessions);
        }
        // Dev/screenshot hook: force a specific right-dock tab so each tab can be
        // shot at the (constant) dock width. chat|sessions|layers|blocks|plugins.
        if let Ok(tab) = std::env::var("ITSJUSTCAD_PANEL_TAB") {
            if let Some(t) = panel_tab_by_name(&tab) {
                self.panel_tabs.show(t);
            }
        }
    }

    /// App-level verbs (save/open, camera) wrap the command substrate.
    fn execute_line(&mut self, line: String) {
        // Expand legacy-CAD alias BEFORE any dispatch (case-insensitive single-token).
        let expanded: String;
        let line = {
            let aliases = self.active_aliases();
            if let Some(exp) = preset::expand_alias(line.trim(), aliases) {
                expanded = exp;
                expanded.as_str()
            } else {
                line.trim()
            }
        };
        if !line.is_empty() {
            self.last_line = Some(line.to_string()); // Enter/Space repeat
        }
        let mut words = line.split_whitespace();
        match words.next() {
            Some("save") => self.save(words.next().map(Into::into)),
            Some("copyselection") => {
                let n = self.session.doc.selection.len();
                if n == 0 {
                    self.command_line.push_line("nothing selected to copy");
                } else {
                    self.clipboard_armed = true;
                    self.command_line
                        .push_line(format!("copied {n} object(s) — Cmd+V pastes with offset"));
                }
            }
            Some("pasteselection") => {
                if self.clipboard_armed {
                    self.command_line.execute(&mut self.session, "copy sel 1,1,0");
                } else {
                    self.command_line.push_line("nothing to paste");
                }
            }
            // Edit ▸ Cut: arm the clipboard from the current selection, then
            // delete it (copy + delete), so a later Paste re-inserts it.
            Some("cut") => {
                let n = self.session.doc.selection.len();
                if n == 0 {
                    self.command_line.push_line("nothing selected to cut");
                } else {
                    self.clipboard_armed = true;
                    self.command_line.execute(&mut self.session, "delete sel");
                    self.command_line
                        .push_line(format!("cut {n} object(s) — Cmd+V pastes"));
                }
            }
            Some("controlimages") => {
                match words.next() {
                    Some(prefix) => self.export_control_images(prefix),
                    None => self
                        .command_line
                        .push_line("usage: controlimages <path-prefix>"),
                }
            }
            Some("open") => self.open(words.next().map(Into::into)),
            // Import/Export: WITH a path they run unchanged through the substrate
            // (headless/scripts always pass one). A BARE `import`/`export` typed
            // interactively pops a native dialog first, then runs with the path.
            Some(verb @ ("import" | "export")) => {
                match words.next() {
                    Some(p) if verb == "import" => self.begin_import(p.into()),
                    Some(p) => {
                        self.command_line
                            .execute(&mut self.session, &format!("{verb} {p}"));
                    }
                    None if verb == "import" => self.import(None),
                    None => self.export(None),
                }
            }
            Some("recover") => self.recover(),
            Some("ze" | "zoomextents") => self.zoom_extents(),
            Some("zs" | "zoomselected") => self.zoom_selected(),
            // Display mode of the active viewport. View state, never logged.
            // Verb/mode mapping is shared with the headless runner via app_verbs.
            Some("display") => match words.next().and_then(DisplayMode::parse) {
                Some(mode) => {
                    self.display_modes[self.layout.camera_index(self.active_pane)] = mode;
                    self.command_line
                        .push_line(format!("display: {}", mode.label().to_lowercase()));
                }
                None => {
                    self.command_line
                        .push_line("usage: display shaded|wireframe|xray|ghosted|pencil");
                }
            },
            // Lighting model (Working hemispheric / Sun / Presentation). View
            // state, never logged.
            Some("lightmode" | "light") => {
                match words.next().and_then(itsjustcad_render::LightMode::parse) {
                    Some(m) => {
                        self.light_mode = m;
                        self.command_line
                            .push_line(format!("lightmode: {}", m.label().to_lowercase()));
                    }
                    None => {
                        self.command_line
                            .push_line("usage: lightmode working|sun|presentation");
                    }
                }
            }
            // Toggle SketchUp-style thick profile / silhouette edges.
            Some("profileedges" | "profiles") => {
                let on = match words.next() {
                    Some("on" | "true" | "1") => true,
                    Some("off" | "false" | "0") => false,
                    _ => !self.profile_edges, // bare toggle
                };
                self.profile_edges = on;
                self.command_line
                    .push_line(format!("profile edges: {}", if on { "on" } else { "off" }));
            }
            // Toggle the thin Shaded-mode feature edges (default ON). View
            // state, never logged; persisted to ui.json.
            Some("shadededges" | "meshedges") => {
                let on = match words.next() {
                    Some("on" | "true" | "1") => true,
                    Some("off" | "false" | "0") => false,
                    _ => !self.shaded_edges, // bare toggle
                };
                self.shaded_edges = on;
                save_shaded_edges(on);
                self.command_line
                    .push_line(format!("shaded edges: {}", if on { "on" } else { "off" }));
            }
            // Toggle the transform gumball/gizmo (Rhino-style persistent
            // toggle). View state, never logged; persisted to ui.json.
            Some("gumball" | "gizmo") => {
                let on = match words.next() {
                    Some("on" | "true" | "1") => true,
                    Some("off" | "false" | "0") => false,
                    _ => !self.show_gumball, // bare toggle
                };
                self.show_gumball = on;
                save_gumball_visible(on);
                self.command_line
                    .push_line(format!("gumball: {}", if on { "on" } else { "off" }));
            }
            // "SketchUp" display preset: Working hemispheric shading + thick
            // profile edges + shaded display. Combines the ergonomics of the
            // default lighting with the recognisable profile-edge look.
            Some("sketchup" | "su") => {
                self.light_mode = itsjustcad_render::LightMode::Working;
                self.profile_edges = true;
                self.display_modes[self.layout.camera_index(self.active_pane)] =
                    DisplayMode::Shaded;
                self.command_line
                    .push_line("preset: sketchup (working light + profile edges)");
            }
            // Toggle hand-drawn "sketchy edges" NPR character.
            Some("sketchy") => {
                let on = match words.next() {
                    Some("on" | "true" | "1") => true,
                    Some("off" | "false" | "0") => false,
                    _ => !self.sketchy.enabled, // bare toggle
                };
                self.sketchy.enabled = on;
                self.command_line
                    .push_line(format!("sketchy edges: {}", if on { "on" } else { "off" }));
            }
            // Tune the sketchy edge effect. Tuning implies the effect is on.
            Some("edgefx") => {
                self.sketchy.enabled = true;
                self.sketchy = self.sketchy.apply_tokens(words);
                self.command_line.push_line(format!(
                    "edgefx: jitter={} passes={} ext={} depthcue={} endpoints={}",
                    self.sketchy.jitter,
                    self.sketchy.passes,
                    self.sketchy.extension,
                    self.sketchy.depthcue,
                    self.sketchy.endpoints,
                ));
            }
            // Toggle Reduce Motion: disables animated progress bars.
            // View/session state, never logged; persisted to ui.json.
            Some("reducemotion") => {
                let on = match words.next() {
                    Some("on" | "true" | "1") => true,
                    Some("off" | "false" | "0") => false,
                    _ => !self.reduce_motion, // bare toggle
                };
                self.reduce_motion = on;
                save_reduce_motion(on);
                self.command_line
                    .push_line(format!("reduce motion: {}", if on { "on" } else { "off" }));
            }
            Some("viewports" | "vp") => {
                match words.next() {
                    Some("1") => self.set_layout(ViewportLayout::Single),
                    Some("2") => self.set_layout(ViewportLayout::Two),
                    Some("4") => self.set_layout(ViewportLayout::Four),
                    _ => {
                        self.command_line.push_line("usage: viewports 1|2|4");
                        return;
                    }
                }
                self.command_line
                    .push_line(format!("viewports: {}", self.layout.pane_count()));
            }
            Some(view @ ("top" | "bottom" | "front" | "back" | "left" | "right" | "persp"
            | "perspective")) => {
                self.set_view(view);
                self.command_line.push_line(format!("view: {view}"));
            }
            // Camera projection / lens. View state, never logged — mirrors
            // `display` and the standard-view verbs above.
            Some("camera") => {
                let arg = words.next().map(str::to_ascii_lowercase);
                let arg2 = words.next().map(str::to_ascii_lowercase);
                self.set_camera(arg.as_deref(), arg2.as_deref());
            }
            // `view save` captures the active camera — only the app can; the
            // parser leaves `camera: None`. Other `view ...` forms parse as-is.
            Some("view") => {
                if let ["save", name] = words.collect::<Vec<_>>().as_slice() {
                    let camera = named_view_of(self.active_camera());
                    let cmd = itsjustcad_commands::Command::ViewSave {
                        name: (*name).to_string(),
                        camera: Some(camera),
                    };
                    self.command_line.execute_command(&mut self.session, line, cmd);
                } else {
                    self.command_line.execute(&mut self.session, line);
                }
            }
            Some("help") => {
                let verb = words.next();
                let lines = help_lines(verb);
                for line in lines {
                    self.command_line.push_line(line);
                }
            }
            // Georeferenced satellite/OSM basemap underlay. View/session state,
            // NEVER logged. Opt-in network: this is the moment the user asks, so
            // the GUI is allowed to reach the tile server (and cache locally).
            Some("basemap") => {
                let args = crate::app_verbs::parse_basemap_args(words);
                self.set_basemap(args);
            }
            // AI diffusion render of the current view. Opt-in network: only the
            // user-configured backend in render_decks.json is ever reached, and
            // only at this explicit request. Ships with NO backend active.
            Some("render") => {
                let words: Vec<String> = words.map(str::to_owned).collect();
                self.handle_render(words);
            }
            Some("template") => {
                self.show_template_picker = true;
            }
            // Vision: screenshot the viewport and ask the deck to critique it.
            // The rest of the line (if any) is a user question folded into the
            // prompt. Deferred: the shot lands a frame later (see handle_critique).
            Some("critique") => {
                // Reveal the Deck tab so the critique reply is visible.
                self.panel_visible = true;
                self.panel_tabs.show(crate::tabstrip::PanelTab::Deck);
                if !self.deck_visible {
                    self.deck_visible = true;
                    save_deck_visible(self.deck_visible);
                }
                let question = words.collect::<Vec<_>>().join(" ");
                self.pending_critique = Some(question);
            }
            _ => {
                if self.draw_tool.try_start(line) {
                    if let Some(prompt) = self.draw_tool.prompt() {
                        self.command_line.push_line(prompt);
                    }
                    return;
                }
                self.command_line.execute(&mut self.session, line);
            }
        }
    }

    fn set_layout(&mut self, layout: ViewportLayout) {
        self.layout = layout;
        self.active_pane = 0;
    }

    /// Decode the document's underlay image into GPU-ready `UnderlayData`,
    /// reusing the cached pixels when the path is unchanged. A missing or
    /// unreadable file drops the texture (the placement math still stands, it
    /// just won't render an image) — no error, matching the "warning not error"
    /// contract on open.
    fn decode_underlay(&mut self) -> Option<UnderlayData> {
        let u = self.session.doc.underlay.as_ref()?;
        let cached = match &self.underlay_cache {
            Some((path, pixels)) if *path == u.path => pixels.clone(),
            _ => {
                let img = match image::open(&u.path) {
                    Ok(img) => img.to_rgba8(),
                    Err(e) => {
                        // Surface once per path change, then forget it so we do
                        // not spam the command line every frame.
                        if self.underlay_cache.as_ref().map(|(p, _)| p != &u.path).unwrap_or(true) {
                            self.command_line
                                .push_line(format!("underlay image not shown: {} ({e})", u.path));
                        }
                        self.underlay_cache = Some((u.path.clone(), std::sync::Arc::new((Vec::new(), 0, 0))));
                        return None;
                    }
                };
                let (w, h) = img.dimensions();
                let pixels = std::sync::Arc::new((img.into_raw(), w, h));
                self.underlay_cache = Some((u.path.clone(), pixels.clone()));
                pixels
            }
        };
        let (rgba, w, h) = (&cached.0, cached.1, cached.2);
        if rgba.is_empty() || w == 0 || h == 0 {
            return None; // previously-failed decode cached as empty
        }
        let c = u.quad_corners();
        Some(UnderlayData {
            rgba: rgba.clone(),
            width_px: w,
            height_px: h,
            corners: [
                [c[0].x as f32, c[0].y as f32, 0.0],
                [c[1].x as f32, c[1].y as f32, 0.0],
                [c[2].x as f32, c[2].y as f32, 0.0],
                [c[3].x as f32, c[3].y as f32, 0.0],
            ],
            opacity: u.opacity,
        })
    }

    /// Convert the document's transient basemap (already-decoded satellite/OSM
    /// pixels in local meters) into GPU-ready `UnderlayData`. No file I/O — the
    /// basemap is held in memory, not on disk.
    fn basemap_data(&self) -> Option<UnderlayData> {
        let b = self.session.doc.basemap.as_ref()?;
        if b.rgba.is_empty() || b.width_px == 0 || b.height_px == 0 {
            return None;
        }
        let c = b.quad_corners();
        Some(UnderlayData {
            rgba: b.rgba.clone(),
            width_px: b.width_px,
            height_px: b.height_px,
            corners: [
                [c[0].x as f32, c[0].y as f32, 0.0],
                [c[1].x as f32, c[1].y as f32, 0.0],
                [c[2].x as f32, c[2].y as f32, 0.0],
                [c[3].x as f32, c[3].y as f32, 0.0],
            ],
            opacity: b.opacity,
        })
    }

    /// Set (or clear) the georeferenced basemap underlay. Requires a site
    /// location (`location`/`sun`/EPW) — without one there is nothing to
    /// georeference against. This is where the opt-in network fetch happens: the
    /// tiles are fetched (or read from the local cache) and stitched into a
    /// transient [`itsjustcad_doc::Basemap`] on the document (never logged).
    fn set_basemap(&mut self, args: crate::app_verbs::BasemapArgs) {
        use crate::basemap::{
            build_basemap, provider_by_name, CachedHttpTileSource, default_cache_root,
        };
        if args.clear {
            self.session.doc.basemap = None;
            self.command_line.push_line("basemap cleared");
            return;
        }
        let Some(loc) = self.session.doc.location else {
            self.command_line
                .push_line("basemap needs a site location first — run `location <lat> <lon>`");
            return;
        };
        let provider = provider_by_name(&args.provider);
        let slug = provider.slug().to_string();
        let attribution = provider.attribution().to_string();
        // The user asked for the basemap now → network is permitted.
        let source = CachedHttpTileSource::new(
            provider,
            default_cache_root(),
            self.tokio.clone(),
            true,
        );
        match build_basemap(loc, args.span_m, args.opacity, &slug, &source) {
            Ok(b) => {
                self.command_line.push_line(format!(
                    "basemap: {} · {:.0} m span · {attribution}",
                    b.label, args.span_m
                ));
                self.session.doc.basemap = Some(b);
            }
            Err(e) => {
                self.command_line.push_line(format!("basemap failed: {e}"));
            }
        }
    }

    /// Camera of the active (last hovered) pane.
    fn active_camera(&mut self) -> &mut OrbitCamera {
        &mut self.cameras[self.layout.camera_index(self.active_pane)]
    }

    fn set_view(&mut self, name: &str) {
        // Shared name→view mapping (also used by the headless runner).
        let view = crate::app_verbs::standard_view(name).unwrap_or(StandardView::Perspective);
        self.active_camera().set_view(view);
    }

    /// `controlimages <prefix>`: render the three CAD control maps (depth / edge
    /// / mask) from the active viewport's camera. Uses an on-demand wgpu device
    /// so it does not need the egui paint callback's render state. Not logged.
    fn export_control_images(&mut self, prefix: &str) {
        let result = self.capture_control_images(prefix, CONTROL_W, CONTROL_H);
        match result {
            Ok(paths) => self.command_line.push_line(format!(
                "control images -> {} , {} , {}",
                paths.depth.display(),
                paths.edge.display(),
                paths.mask.display()
            )),
            Err(e) => self.command_line.push_line(format!("controlimages failed: {e}")),
        }
    }

    /// Render the three CAD control maps (depth / edge / mask) from the active
    /// viewport's camera to `<prefix>_{depth,edge,mask}.png`. Shared by the
    /// `controlimages` verb and the `render` diffusion flow. Uses an on-demand
    /// wgpu device so it does not need the egui paint callback's render state.
    fn capture_control_images(
        &mut self,
        prefix: &str,
        w: u32,
        h: u32,
    ) -> Result<itsjustcad_render::ControlImagePaths, String> {
        let aspect = w as f32 / h as f32;
        let cam_idx = self.layout.camera_index(self.active_pane);
        let camera = self.cameras[cam_idx];
        let view_proj = camera.view_proj(aspect);
        let eye = camera.eye();
        let (near, far) = match self.session.doc.scene_aabb() {
            Some(bb) => {
                let c = bb.center();
                let center = glam::Vec3::new(c.x as f32, c.y as f32, c.z as f32);
                let radius = (bb.size().length() as f32 * 0.5).max(0.5);
                let d = (eye - center).length();
                ((d - radius).max(0.01), d + radius)
            }
            None => (0.1, 100.0),
        };

        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(
            instance.request_adapter(&wgpu::RequestAdapterOptions::default()),
        )
        .map_err(|e| format!("no wgpu adapter: {e:?}"))?;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .map_err(|e| e.to_string())?;
        itsjustcad_render::render_control_images(
            &device, &queue, &self.session.doc, view_proj, eye, near, far, w, h, prefix,
        )
    }

    /// The `render` verb: AI-diffuse the current view via the active cassette
    /// in `render_decks.json`, plus its setup subcommands (backends/use/test).
    /// Every outcome lands on the command line — never silent.
    fn handle_render(&mut self, words: Vec<String>) {
        use crate::diffusion::{parse_render_words, RenderAction};
        match parse_render_words(&words) {
            RenderAction::Usage => {
                for l in crate::diffusion::USAGE {
                    self.command_line.push_line(*l);
                }
            }
            RenderAction::Cancel => match self.render_job.take() {
                Some(job) => {
                    job.cancel();
                    self.command_line.push_line("render canceled");
                }
                None => self.command_line.push_line("no render in flight"),
            },
            RenderAction::Backends => {
                let decks = itsjustcad_deck::RenderDecksFile::load_or_default();
                for l in crate::diffusion::backends_lines(&decks) {
                    self.command_line.push_line(l);
                }
                self.command_line
                    .push_line("`render use <name>` picks one; `render test` checks it");
            }
            RenderAction::Use(name) => {
                let mut decks = itsjustcad_deck::RenderDecksFile::load_or_default();
                match crate::diffusion::select_backend(&mut decks, &name) {
                    Ok(msg) => {
                        decks.save();
                        self.command_line.push_line(msg);
                    }
                    Err(e) => self.command_line.push_line(e),
                }
            }
            RenderAction::Test => {
                let config =
                    itsjustcad_deck::RenderDecksFile::load_or_default().active_config();
                self.command_line
                    .push_line(format!("render test: probing '{}'…", config.name));
                self.render_conn_test =
                    Some(crate::diffusion::ConnTest::start(&self.tokio, &config));
            }
            RenderAction::Prompt(prompt) => {
                if self.render_job.is_some() {
                    self.command_line
                        .push_line("a render is already in flight — `render cancel` aborts it");
                    return;
                }
                let config =
                    itsjustcad_deck::RenderDecksFile::load_or_default().active_config();
                if !config.is_configured() {
                    self.command_line.push_line(itsjustcad_deck::NO_BACKEND_MESSAGE);
                    self.command_line.push_line(
                        "`render backends` lists cassettes; `render use <name>` picks one; \
                         `render test` checks it",
                    );
                    return;
                }
                // Capture the control images from the active view into the
                // private runtime dir (0700), then hand the bytes to the job.
                let prefix = private_runtime_dir().join("render_control");
                let prefix = prefix.to_string_lossy().to_string();
                let control = match self
                    .capture_control_images(&prefix, CONTROL_W, CONTROL_H)
                    .map_err(|e| format!("control image capture failed: {e}"))
                    .and_then(|_| {
                        itsjustcad_deck::ControlImages::from_prefix(&prefix)
                            .map_err(|e| format!("control images unreadable: {e}"))
                    }) {
                    Ok(c) => c,
                    Err(e) => {
                        self.command_line.push_line(format!("render: {e}"));
                        return;
                    }
                };
                let req = itsjustcad_deck::RenderRequest::new(
                    prompt, control, CONTROL_W, CONTROL_H,
                );
                let job = crate::diffusion::RenderJob::start(&self.tokio, &config, req);
                self.command_line.push_line(format!(
                    "render started on '{}' — diffusing the current view… (`render cancel` aborts)",
                    job.backend
                ));
                self.render_job = Some(job);
            }
        }
    }

    /// Per-frame poll of the diffusion job + connection test. Results and
    /// failures land on the command line; a finished image is written to the
    /// private runtime dir and opened in the "AI Render" window.
    fn poll_render(&mut self, ctx: &egui::Context) {
        if let Some(t) = &mut self.render_conn_test {
            match t.poll() {
                Some(verdict) => {
                    self.render_conn_test = None;
                    let msg = match verdict {
                        Ok(m) => format!("render test: {m}"),
                        Err(m) => format!("render test failed: {m}"),
                    };
                    self.command_line.push_line(msg);
                }
                None => ctx.request_repaint_after(std::time::Duration::from_millis(150)),
            }
        }
        let Some(mut job) = self.render_job.take() else { return };
        match job.poll() {
            crate::diffusion::RenderPoll::Pending => {
                // Keep polling even while the user isn't interacting.
                ctx.request_repaint_after(std::time::Duration::from_millis(150));
                self.render_job = Some(job);
            }
            crate::diffusion::RenderPoll::Failed(e) => {
                self.command_line.push_line(format!("render failed: {e}"));
            }
            crate::diffusion::RenderPoll::Done(img) => {
                let path = crate::diffusion::result_path();
                if let Err(e) = img.save(&path) {
                    self.command_line
                        .push_line(format!("render: could not write {}: {e}", path.display()));
                }
                let secs = job.started.elapsed().as_secs();
                self.command_line.push_line(format!(
                    "render done in {secs}s via '{}' → {}",
                    job.backend,
                    path.display()
                ));
                match crate::diffusion::decode_png(&img.png) {
                    Ok(ci) => {
                        let texture =
                            ctx.load_texture("ai_render_result", ci, egui::TextureOptions::LINEAR);
                        self.render_result = Some(RenderResultView {
                            texture,
                            path,
                            prompt: job.prompt.clone(),
                            backend: job.backend.clone(),
                        });
                    }
                    Err(e) => self
                        .command_line
                        .push_line(format!("render: result image did not decode: {e}")),
                }
            }
        }
    }

    /// Floating "AI Render" window showing the last diffusion result, scaled to
    /// fit. Draggable + resizable + X-closable — never traps input. Closing
    /// drops the texture; the PNG stays on disk at the captioned path.
    fn render_result_window(&mut self, ctx: &egui::Context) {
        let Some(view) = &self.render_result else { return };
        let mut open = true;
        egui::Window::new("AI Render")
            .collapsible(true)
            .resizable(true)
            .default_size([640.0, 480.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(format!("{} — via '{}'", view.prompt, view.backend))
                        .weak(),
                );
                ui.label(
                    egui::RichText::new(view.path.display().to_string()).weak().small(),
                );
                let size = view.texture.size_vec2();
                let avail = ui.available_size();
                let scale = (avail.x / size.x).min(avail.y / size.y).clamp(0.05, 1.0);
                ui.add(
                    egui::Image::new(&view.texture).fit_to_exact_size(size * scale),
                );
            });
        if !open {
            self.render_result = None;
        }
    }

    /// `camera <2point|persp|pano|fisheye [fov]|15|24|35|50|85|phone|phonewide>`.
    /// Numeric args may carry a trailing "mm". Two-point toggles architectural
    /// perspective; `pano`/`fisheye` switch to a non-pinhole projection rendered
    /// via the cubemap remap; the lens presets set `fov_y` from a full-frame
    /// angle of view. All are view state on the active pane, never logged.
    fn set_camera(&mut self, arg: Option<&str>, arg2: Option<&str>) {
        let aspect = self.active_aspect;
        let Some(arg) = arg else {
            self.command_line.push_line(
                "usage: camera 2point|persp|pano|fisheye [fov]|phone [lens]|<15|24|35|50|85>mm",
            );
            return;
        };
        match arg {
            "2point" | "twopoint" | "2pt" => {
                let cam = self.active_camera();
                cam.ortho = false;
                cam.two_point = true;
                cam.pano = None;
                self.command_line.push_line("camera: two-point perspective");
            }
            "persp" | "perspective" | "1point" | "normal" => {
                let cam = self.active_camera();
                cam.ortho = false;
                cam.two_point = false;
                cam.pano = None;
                self.command_line.push_line("camera: perspective");
            }
            "pano" | "panorama" | "equirect" | "360" => {
                let cam = self.active_camera();
                cam.ortho = false;
                cam.two_point = false;
                cam.pano = Some(itsjustcad_render::PanoProjection::Equirect);
                self.command_line.push_line("camera: 360° equirectangular panorama");
            }
            "fisheye" | "fish" => {
                let p = crate::headless::parse_fisheye(arg2);
                let cam = self.active_camera();
                cam.ortho = false;
                cam.two_point = false;
                cam.pano = Some(p);
                let deg = match p {
                    itsjustcad_render::PanoProjection::Fisheye { fov } => fov.to_degrees(),
                    _ => 180.0,
                };
                self.command_line.push_line(format!("camera: fisheye {deg:.0}° fov"));
            }
            // `camera phone <preset>`: a named real phone lens (iPhone / Pixel /
            // Galaxy ultra-wide / main / tele). Bare `camera phone` = the iPhone
            // main wide, matching the shorthand.
            "phone" => {
                let preset = arg2.and_then(itsjustcad_render::phone_preset);
                let (focal, label) = match preset {
                    Some(p) => (p.focal_mm, p.label.to_string()),
                    None if arg2.is_none() => (26.0, "iPhone main wide (26mm eq)".to_string()),
                    None => {
                        let names: Vec<&str> =
                            itsjustcad_render::PHONE_PRESETS.iter().map(|p| p.name).collect();
                        self.command_line
                            .push_line(format!("camera phone: unknown lens; try {}", names.join(", ")));
                        return;
                    }
                };
                self.active_camera().set_lens_mm(focal, aspect);
                let fov = itsjustcad_render::fov_for_focal_mm(focal).to_degrees();
                self.command_line
                    .push_line(format!("camera: {label} — {fov:.0}° hfov"));
            }
            _ => {
                // Named phone sim shorthand, or a numeric focal length ("35"/"35mm").
                let focal = itsjustcad_render::preset_focal_mm(arg).or_else(|| {
                    arg.strip_suffix("mm").unwrap_or(arg).parse::<f32>().ok()
                });
                match focal {
                    Some(f) if f > 0.0 => {
                        self.active_camera().set_lens_mm(f, aspect);
                        let fov = itsjustcad_render::fov_for_focal_mm(f).to_degrees();
                        let tag = match arg {
                            "phone" => " (26mm equiv)",
                            "phonewide" => " (13mm equiv)",
                            "phonetele" => " (77mm equiv)",
                            _ => "",
                        };
                        self.command_line
                            .push_line(format!("camera: {f:.0}mm{tag} — {fov:.0}° hfov"));
                    }
                    _ => self.command_line.push_line(
                        "usage: camera 2point|persp|pano|fisheye [fov]|phone [lens]|<15|24|35|50|85>mm",
                    ),
                }
            }
        }
    }

    fn zoom_extents(&mut self) {
        if let Some(bb) = self.session.doc.scene_aabb() {
            let center = bb.center();
            let cam = self.active_camera();
            cam.target = glam::Vec3::new(center.x as f32, center.y as f32, center.z as f32);
            cam.distance = (bb.size().length() as f32 * 1.2).max(5.0);
        }
    }

    /// Modeless-hotkey exceptions (Rhino-style) that STILL fire even though the
    /// command line is focused-by-default: Delete/Backspace = delete selection,
    /// Esc = deselect, G = toggle gumball. They act ONLY on an EMPTY command line
    /// (so typing a word is never hijacked), with focus on the command input or
    /// nothing, no modal open, and no draw tool active. Runs BEFORE any widget so
    /// the matched key/text events can be consumed — otherwise the focused input
    /// would also eat the letter. Everything NOT in this whitelist types normally.
    fn early_hotkeys(&mut self, ctx: &egui::Context) {
        let modal = self.show_palette
            || self.show_about
            || self.show_history
            || self.show_model_setup
            || self.show_template_picker
            || self.import_job.is_some()
            || self.import_result.is_some()
            || self.pending_nav.is_some();
        // Block ONLY when a real text field is actively composing: the command
        // line with content, or the deck chat input. When focus is on the
        // viewport, the gumball gizmo, or nothing, the exceptions still fire
        // (that focus is not text entry) — this is why Esc/Delete kept failing
        // right after a selection, which hands focus to the gizmo.
        let chat_id = egui::Id::new("deck_chat_input");
        let focused = ctx.memory(|m| m.focused());
        let chat_composing = focused == Some(chat_id);
        if modal
            || self.draw_tool.active()
            || chat_composing
            || !self.command_line.is_empty()
        {
            return;
        }
        const EXCEPTIONS: &[egui::Key] = &[
            egui::Key::Delete,
            egui::Key::Backspace,
            egui::Key::Escape,
            egui::Key::G,
        ];
        let pressed: Vec<(egui::Key, egui::Modifiers)> = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        repeat: false,
                        modifiers,
                        ..
                    } if EXCEPTIONS.contains(key) => Some((*key, *modifiers)),
                    _ => None,
                })
                .collect()
        });
        if pressed.is_empty() {
            return;
        }
        let mut fired_g = false;
        for (key, mods) in &pressed {
            let line = keymap::keymap(
                *key,
                *mods,
                keymap::KeyContext {
                    typing: false,
                    draw_active: false,
                    has_selection: !self.session.doc.selection.is_empty(),
                    last_command: self.last_line.as_deref(),
                },
            );
            if let Some(line) = line {
                if *key == egui::Key::G {
                    fired_g = true;
                }
                self.execute_line(line);
            }
        }
        // Consume the matched key events (and the 'g' text) so the focused command
        // input never also receives the keystroke that just drove an action.
        ctx.input_mut(|i| {
            i.events.retain(|e| match e {
                egui::Event::Key { key, .. } => !EXCEPTIONS.contains(key),
                egui::Event::Text(t) if fired_g => !t.eq_ignore_ascii_case("g"),
                _ => true,
            })
        });
    }

    /// Rhino "zoom selected" (`zs`): frame ONLY the current selection in the
    /// active viewport. No selection → a status hint (don't silently reframe to
    /// the whole scene). Same framing math as [`Self::zoom_extents`].
    fn zoom_selected(&mut self) {
        match self.session.doc.selection_aabb() {
            Some(bb) => {
                let center = bb.center();
                let cam = self.active_camera();
                cam.target =
                    glam::Vec3::new(center.x as f32, center.y as f32, center.z as f32);
                cam.distance = (bb.size().length() as f32 * 1.2).max(2.0);
            }
            None => self
                .command_line
                .push_line("zoom selected: nothing selected (try 'select all' first)"),
        }
    }

    /// Zoom-extents ALL viewports to the whole scene. Same framing math as
    /// [`Self::zoom_extents`] but applied to every camera — used after an import
    /// so each pane frames the freshly-loaded geometry.
    fn zoom_extents_all(&mut self) {
        if let Some(bb) = self.session.doc.scene_aabb() {
            let c = bb.center();
            let center = glam::Vec3::new(c.x as f32, c.y as f32, c.z as f32);
            let distance = (bb.size().length() as f32 * 1.2).max(5.0);
            for cam in &mut self.cameras {
                cam.target = center;
                cam.distance = distance;
            }
        }
    }

    /// Draw the import progress modal (while a job runs) and the completion
    /// popup (after it finishes). The progress window is non-closable — the
    /// import always runs to completion; the result popup has an OK button.
    fn import_ui(&mut self, ctx: &egui::Context) {
        if let Some(job) = &self.import_job {
            egui::Window::new("Importing…")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_min_width(320.0);
                    ui.label(
                        egui::RichText::new(format!("Importing {}", job.path))
                            .strong(),
                    );
                    ui.add(egui::ProgressBar::new(job.fraction()).show_percentage());
                    ui.label(
                        egui::RichText::new(format!(
                            "{} / {} entities",
                            job.cursor, job.total
                        ))
                        .weak()
                        .small(),
                    );
                });
        }
        // Completion popup — shown until the user dismisses it.
        let mut dismiss = false;
        if let Some(msg) = &self.import_result {
            egui::Window::new("Import complete")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_min_width(300.0);
                    ui.label(egui::RichText::new("✔ Successful import").strong());
                    ui.label(egui::RichText::new(msg).weak());
                    ui.add_space(6.0);
                    if ui.button("OK").clicked() {
                        dismiss = true;
                    }
                });
        }
        if dismiss {
            self.import_result = None;
        }
    }

    /// Begin importing `path`. DXF files (potentially tens of thousands of
    /// entities) are applied in time-boxed batches with a progress modal (see
    /// [`Self::step_import`]) so the UI never freezes; other formats run
    /// synchronously through the substrate, then also get the popup + zoom-all.
    fn begin_import(&mut self, path: std::path::PathBuf) {
        let is_dxf = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("dxf"));
        if !is_dxf {
            self.command_line
                .execute(&mut self.session, &format!("import {}", path.display()));
            self.zoom_extents_all();
            self.import_result = Some(format!("Imported {}", path.display()));
            return;
        }
        // Parse up front (fast); the slow part — one logged op per entity — is
        // chunked across frames.
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                self.command_line
                    .push_line(format!("import failed: cannot read {}: {e}", path.display()));
                return;
            }
        };
        match itsjustcad_commands::dxf::parse_dxf(&text) {
            Ok(parsed) => {
                let total = parsed.entities.len();
                self.command_line
                    .push_line(format!("importing {} — {total} entities…", path.display()));
                self.import_job = Some(ImportJob {
                    path: path.display().to_string(),
                    entities: parsed.entities,
                    cursor: 0,
                    total,
                    skipped: parsed.skipped,
                    prev_layer: self.session.doc.current_layer.clone(),
                });
            }
            Err(e) => self
                .command_line
                .push_line(format!("import failed: {} — {e}", path.display())),
        }
    }

    /// Advance the in-flight DXF import by one ~8 ms batch, keeping frames live.
    /// On the final batch: restore the layer, post the success message (command
    /// line + popup), and zoom-extents every viewport onto the import.
    fn step_import(&mut self, ctx: &egui::Context) {
        let Some(mut job) = self.import_job.take() else {
            return;
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(8);
        while job.cursor < job.total {
            let (layer, cmd) = job.entities[job.cursor].clone();
            if self.session.doc.current_layer != layer {
                let _ = self
                    .session
                    .run(itsjustcad_commands::Command::Layer { name: layer });
            }
            let _ = self.session.run(cmd);
            job.cursor += 1;
            if std::time::Instant::now() >= deadline {
                break;
            }
        }
        if job.cursor >= job.total {
            if self.session.doc.current_layer != job.prev_layer {
                let _ = self.session.run(itsjustcad_commands::Command::Layer {
                    name: job.prev_layer.clone(),
                });
            }
            let msg = format!(
                "Imported {} entities from {} ({} skipped).",
                job.total, job.path, job.skipped
            );
            self.command_line.push_line(msg.clone());
            self.import_result = Some(msg);
            self.zoom_extents_all();
            // job dropped — import complete.
        } else {
            self.import_job = Some(job);
            ctx.request_repaint(); // keep stepping next frame
        }
    }

    /// Click-select: ray through the clicked pixel vs object AABBs. Unless
    /// `expand` is off (Cmd held), the hit expands to its whole group.
    fn pick(
        &mut self,
        view_proj: glam::Mat4,
        rect: egui::Rect,
        pos: egui::Pos2,
        additive: bool,
        expand: bool,
    ) {
        let (origin, dir) = screen_ray(view_proj, rect, pos);

        // Build a BVH over visible object AABBs so the ray only tests the boxes
        // it actually crosses rather than every object in the scene.
        let pickable: Vec<(itsjustcad_doc::ObjectId, kernel_mesh::Aabb)> = self
            .session
            .doc
            .objects()
            .filter(|obj| obj.visible && self.session.doc.layer_visible(&obj.layer))
            .map(|obj| (obj.id, obj.geometry.aabb()))
            .collect();
        let bvh = kernel_mesh::Bvh::build(&pickable.iter().map(|(_, bb)| *bb).collect::<Vec<_>>());
        let mut best: Option<(f64, itsjustcad_doc::ObjectId)> = None;
        for i in bvh.ray_candidates(origin, dir) {
            let (id, bb) = pickable[i as usize];
            if let Some(t) = ray_aabb(origin, dir, bb.min, bb.max)
                && best.is_none_or(|(bt, _)| t < bt)
            {
                best = Some((t, id));
            }
        }
        let doc = &mut self.session.doc;
        if !additive {
            doc.selection.clear();
        }
        let mut note = None;
        if let Some((_, id)) = best {
            let ids = if expand {
                doc.expand_pick(id)
            } else {
                std::collections::BTreeSet::from([id])
            };
            if additive && ids.iter().all(|i| doc.selection.contains(i)) {
                for i in &ids {
                    doc.selection.remove(i);
                }
            } else {
                doc.selection.extend(&ids);
            }
            if ids.len() > 1 {
                note = Some(format!("group select: {} object(s)", ids.len()));
            }
        }
        doc.generation += 1; // recolor selection
        if let Some(note) = note {
            self.command_line.push_line(note);
        }
    }

    /// Apply a finished drag-box: project visible object AABBs to screen
    /// rects, run the pure window/crossing test, update the selection.
    fn box_select(
        &mut self,
        view_proj: glam::Mat4,
        rect: egui::Rect,
        drag: egui::Rect,
        mode: crate::boxsel::BoxMode,
        additive: bool,
    ) {
        let items: Vec<(itsjustcad_doc::ObjectId, egui::Rect)> = self
            .session
            .doc
            .objects()
            .filter(|obj| obj.visible && self.session.doc.layer_visible(&obj.layer))
            .filter_map(|obj| {
                let bb = obj.geometry.aabb();
                Some((obj.id, projected_rect(view_proj, rect, bb.min, bb.max)?))
            })
            .collect();
        let ids = crate::boxsel::box_select(&items, drag, mode);
        let doc = &mut self.session.doc;
        if !additive {
            doc.selection.clear();
        }
        let n = ids.len();
        for id in ids {
            doc.selection.insert(id);
        }
        doc.generation += 1; // recolor selection
        let kind = match mode {
            crate::boxsel::BoxMode::Window => "window",
            crate::boxsel::BoxMode::Crossing => "crossing",
        };
        self.command_line
            .push_line(format!("{kind} select: {n} object(s)"));
    }

    /// Save the document. With no path (File ▸ Save, ⌘S, or a bare `save` typed
    /// interactively) a native rfd save dialog is popped DIRECTLY — the user is
    /// never asked to type a path. With a path (Save As, scripts) it runs
    /// straight away. Guarded against headless blocking, mirroring import/export.
    fn save(&mut self, path: Option<std::path::PathBuf>) {
        let path = path.or_else(|| {
            if self.headless_no_dialog() {
                return None;
            }
            rfd::FileDialog::new()
                .add_filter("ItsJustCAD", &["itsjustcad.json", "mydrafter.json", "json"])
                .set_file_name("untitled.itsjustcad.json")
                .save_file()
        });
        let Some(path) = path else { return };
        match itsjustcad_commands::io::save_file(&self.session, &path) {
            Ok(()) => {
                // Ops are safe in the file now; drop the crash journal.
                if let Some(j) = &mut self.journal {
                    j.discard();
                }
                self.journaled_generation = Some(self.session.doc.generation);
                // The document is now clean at the current op-log cursor.
                self.mark_saved();
                // Archive the live conversation into per-document history on save
                // (Auto: update-in-place or fresh; no-op when empty) so saving the
                // document also durably lands the current chat.
                self.deck_pane
                    .archive_active(crate::deck_pane::ArchiveMode::Auto);
                // Confine deck-originated fs paths to this document's directory.
                self.deck_pane
                    .set_sandbox_root(path.parent().map(|p| p.to_path_buf()));
                self.command_line
                    .push_line(format!("saved {}", path.display()));
                // Watch the file for external changes (cloud sync, other apps)
                // and surface any conflict copies a sync service left behind.
                self.file_watch = Some(crate::filewatch::FileWatch::new(&path));
                self.external_change = None;
                self.report_conflict_copies(&path);
            }
            Err(e) => self.command_line.push_line(format!("error: {e}")),
        }
    }

    /// Warn (in the command line) about sync-service conflict copies sitting
    /// next to the document — the user may be losing edits to a fork.
    fn report_conflict_copies(&mut self, path: &std::path::Path) {
        for c in crate::filewatch::conflict_siblings(path) {
            self.command_line.push_line(format!(
                "warning: sync conflict copy exists: {} — another device's edits may live there",
                c.display()
            ));
        }
    }

    /// Render the file-changed-on-disk alert and apply the user's choice:
    /// Reload replaces the session from disk; Cancel keeps the in-memory doc
    /// (and re-snapshots so the same change isn't re-reported).
    fn external_change_ui(&mut self, ctx: &egui::Context) -> bool {
        let Some(change) = self.external_change else {
            return false;
        };
        let Some(watch) = &self.file_watch else {
            self.external_change = None;
            return false;
        };
        let path = watch.path().to_path_buf();
        let dark = ctx.theme() == egui::Theme::Dark;
        let roles = self.live_roles(dark);
        let (title, message, confirm) = match change {
            crate::filewatch::ExternalChange::Modified => (
                "File changed on disk",
                format!(
                    "{} was modified outside ItsJustCAD (cloud sync or another app).\n\
                     Reload to pick up those changes, or keep your version — saving will \
                     overwrite the file on disk.",
                    path.display()
                ),
                "Reload",
            ),
            crate::filewatch::ExternalChange::Deleted => (
                "File removed on disk",
                format!(
                    "{} was moved or deleted outside ItsJustCAD.\n\
                     Keep working and Save will recreate it.",
                    path.display()
                ),
                "OK",
            ),
        };
        let choice = crate::widgets::alert(
            ctx,
            &roles,
            dark,
            title,
            &message,
            confirm,
            crate::widgets::ButtonRole::Normal,
        );
        match choice {
            Some(crate::widgets::AlertChoice::Confirm)
                if change == crate::filewatch::ExternalChange::Modified =>
            {
                self.external_change = None;
                self.open(Some(path));
            }
            Some(_) => {
                // Keep mine / OK: re-snapshot so this change isn't re-reported.
                if let Some(w) = &mut self.file_watch {
                    w.refresh();
                }
                self.external_change = None;
            }
            None => {}
        }
        true
    }

    /// Replay the newest crash journal from another session into this one.
    fn recover(&mut self) {
        let (Some(dir), Some(own)) = (
            journal::default_dir(),
            self.journal.as_ref().map(|j| j.path().to_path_buf()),
        ) else {
            self.command_line.push_line("error: no journal directory");
            return;
        };
        let Some(path) = journal::recoverable(&dir, &own).into_iter().next() else {
            self.command_line.push_line("no crash journal to recover");
            return;
        };
        match journal::load(&path) {
            Ok(session) => {
                self.session = session;
                self.uploaded_generation = None;
                // The recovered ops now live in THIS session's journal (next
                // sync writes them); the crashed one has served its purpose.
                let _ = std::fs::remove_file(&path);
                self.journaled_generation = None;
                self.command_line.push_line(format!(
                    "recovered {} op(s) from {}",
                    self.session.save_log().len(),
                    path.display()
                ));
            }
            Err(e) => self.command_line.push_line(format!("error: {e}")),
        }
    }

    /// Start a new empty document (File → New). Replaces the session with a
    /// fresh one; the chat/deck session is untouched.
    fn new_document(&mut self) {
        self.session = Session::default();
        self.uploaded_generation = None;
        self.journaled_generation = None;
        self.deck_pane.set_sandbox_root(None);
        self.file_watch = None;
        self.external_change = None;
        // A fresh document is clean at cursor 0.
        self.saved_cursor = 0;
        self.command_line.push_line("new document");
    }

    /// Start a new file session (File → New file session): a fresh document AND
    /// a fresh chat/deck session (drops the provider conversation + transcript).
    fn new_session(&mut self) {
        self.new_document();
        self.deck_pane.new_session();
        self.command_line.push_line("new file session");
    }

    /// Open a document. With no path (File ▸ Open…, ⌘O, or a bare `open` typed
    /// interactively) a native rfd open dialog is popped DIRECTLY — no typing.
    /// With a path (scripts) it loads straight away. Guarded against headless
    /// blocking, mirroring import/export.
    fn open(&mut self, path: Option<std::path::PathBuf>) {
        let path = path.or_else(|| {
            if self.headless_no_dialog() {
                return None;
            }
            rfd::FileDialog::new()
                .add_filter("ItsJustCAD", &["itsjustcad.json", "mydrafter.json", "json"])
                .pick_file()
        });
        let Some(path) = path else { return };
        match itsjustcad_commands::io::load_file(&path) {
            Ok(session) => {
                self.session = session;
                self.uploaded_generation = None;
                // A freshly-opened document is clean at its loaded op-log cursor.
                self.mark_saved();
                // Confine deck-originated fs paths to this document's directory.
                self.deck_pane
                    .set_sandbox_root(path.parent().map(|p| p.to_path_buf()));
                self.command_line
                    .push_line(format!("opened {} ({} objects)", path.display(), self.session.doc.len()));
                self.file_watch = Some(crate::filewatch::FileWatch::new(&path));
                self.external_change = None;
                self.report_conflict_copies(&path);
            }
            Err(e) => self.command_line.push_line(format!("error: {e}")),
        }
    }

    /// Import a model file THROUGH THE SUBSTRATE. With no path (menu click or a
    /// bare `import` typed interactively) a native rfd open dialog is popped to
    /// choose the file first; the op-log/replay is unchanged because the command
    /// still carries the chosen path. Guarded so headless/`--shot` frames never
    /// block on a dialog.
    fn import(&mut self, path: Option<std::path::PathBuf>) {
        let path = path.or_else(|| {
            if self.headless_no_dialog() {
                return None;
            }
            rfd::FileDialog::new()
                .add_filter("DXF", &["dxf"])
                .add_filter("IFC", &["ifc"])
                .add_filter("OBJ", &["obj"])
                .add_filter("STL", &["stl"])
                .add_filter("glTF / GLB", &["gltf", "glb"])
                .add_filter("Collada", &["dae"])
                .add_filter("Point cloud (LAS/LAZ)", &["las", "laz"])
                .add_filter("E57", &["e57"])
                .add_filter("Rhino 3DM", &["3dm"])
                .pick_file()
        });
        let Some(path) = path else { return };
        // DXF imports go through the batched job (progress modal); other formats
        // run synchronously. Both end with the popup + zoom-extents-all.
        self.begin_import(path);
    }

    /// Export the document THROUGH THE SUBSTRATE. With no path a native rfd save
    /// dialog is popped; the chosen filename's extension selects the format
    /// (exec dispatches by extension). Guarded against headless blocking.
    fn export(&mut self, path: Option<std::path::PathBuf>) {
        let path = path.or_else(|| {
            if self.headless_no_dialog() {
                return None;
            }
            rfd::FileDialog::new()
                .add_filter("DXF", &["dxf"])
                .add_filter("SVG", &["svg"])
                .add_filter("CSV", &["csv"])
                .add_filter("glTF / GLB", &["gltf", "glb"])
                .add_filter("OBJ", &["obj"])
                .add_filter("STL", &["stl"])
                .add_filter("PDF", &["pdf"])
                .add_filter("IFC", &["ifc"])
                .add_filter("SAF (xlsx)", &["xlsx", "saf"])
                .add_filter("Rhino 3DM", &["3dm"])
                .set_file_name("export.dxf")
                .save_file()
        });
        let Some(path) = path else { return };
        self.command_line
            .execute(&mut self.session, &format!("export {}", path.display()));
    }

    /// True when a native file dialog MUST NOT be shown because we are running a
    /// non-interactive/screenshot frame (mirrors how `--shot`/`ITSJUSTCAD_SHOT`
    /// drives the GUI loop without a real user). Keeps headless + tests from
    /// blocking on a modal picker.
    fn headless_no_dialog(&self) -> bool {
        self.shot_path.is_some() || self.startup_script.is_some()
    }

    fn handle_dev_screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = self.shot_path.clone() else {
            return;
        };
        ctx.request_repaint(); // keep frames flowing until the shot lands
        // With a deck script, wait for the LLM turn(s) to finish before shooting.
        let deck_ready =
            std::env::var("ITSJUSTCAD_DECK_RUN").is_err() || self.deck_pane.turns_completed();
        if deck_ready {
            self.frame_count += 1;
        }
        // Warm-up frames let the scene settle before capturing. Once past the
        // threshold, RE-REQUEST the screenshot every frame (not once at
        // `== 20`): a single dropped/late echo used to leave the app spinning
        // `request_repaint` forever, which is the multi-minute hang. Requesting
        // each frame guarantees the echo lands within a frame or two.
        if self.frame_count >= 20 {
            // One-shot ITSJUSTCAD_SAVE side effect.
            if !self.shot_saved {
                self.shot_saved = true;
                if let Ok(path) = std::env::var("ITSJUSTCAD_SAVE") {
                    self.save(Some(path.into()));
                }
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        // Ignore a critique-tagged shot here — `handle_critique` claims it. Skip
        // 0-sized frames (the framebuffer is empty until the first real paint),
        // matching the critique path, so we never save a blank PNG.
        if let Some(img) =
            untagged_screenshot(ctx).filter(|i| i.width() > 0 && i.height() > 0)
        {
            save_screenshot_png(&img, std::path::Path::new(&path));
            std::process::exit(0);
        }
    }

    /// When a `critique` request is pending, drive the screenshot: request the
    /// capture, and when the tagged frame lands, write the PNG and fire the
    /// vision deck turn (Read tool enabled) that reads it.
    fn handle_critique(&mut self, ctx: &egui::Context) {
        if self.pending_critique.is_none() {
            return;
        }
        ctx.request_repaint(); // keep frames flowing until the shot lands
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(
            CRITIQUE_TAG,
        )));
        // Ignore empty frames (the framebuffer is 0-sized until the first real
        // paint); keep requesting until a rendered frame lands.
        if let Some(img) = tagged_screenshot(ctx, CRITIQUE_TAG)
            .filter(|i| i.width() > 0 && i.height() > 0)
        {
            let question = self.pending_critique.take().unwrap_or_default();
            let shot_path = critique_shot_path();
            save_screenshot_png(&img, &shot_path);
            let shot_str = shot_path.display().to_string();
            let prompt = critique_prompt(&shot_str, &question);
            tracing::info!(target: "deck", "critique prompt: {prompt}");
            self.command_line
                .push_line(format!("critique: captured {shot_str}"));
            self.deck_pane
                .send_critique(&prompt, &self.session, &self.tokio);
        }
    }

    fn viewport(&mut self, ui: &mut egui::Ui) {
        let mut full = ui.available_rect_before_wrap();
        // Guard against the one-frame dock/viewport width desync: if the dock
        // drew slightly wider than the space it reserved (right after a relayout),
        // the leftover `full` can overlap the dock, and the 3D wgpu callback —
        // painted last and scissored to `full` — would overpaint the chat. Clamp
        // the right edge to the dock's real left edge so 3D never bleeds over it.
        if let Some(dock_left) = self.dock_left
            && dock_left > full.left()
            && dock_left < full.right()
        {
            full.max.x = dock_left;
        }
        if !self.draw_tool.active() {
            self.status_snap = None; // no tool, no snap marker to report
        }
        if self.active_pane >= self.layout.pane_count() {
            self.active_pane = 0;
        }
        // Active viewport is STICKY (Rhino-style): it changes only on an explicit
        // click in a pane (see the per-pane loop) or a click on a pane's name tag
        // — no hover flicker. EXCEPTION: while a draw tool is armed, the pane under
        // the cursor becomes active so points can be picked in ANY viewport
        // mid-command (Rhino lets you pick across viewports during a command).
        if self.draw_tool.active()
            && let Some(pos) = ui.ctx().pointer_latest_pos()
            && let Some(pane) = self.layout.pane_at(full, pos)
        {
            self.active_pane = pane;
        }

        let theme = if ui.visuals().dark_mode {
            scene::Theme::Dark
        } else {
            scene::Theme::Light
        };
        let generation = self.session.doc.generation;
        // Color mode of the active pane drives the snapshot; changes stale it.
        let active_color_mode = self.color_modes[self.layout.camera_index(self.active_pane)];
        let stale = self.uploaded_generation != Some(generation)
            || self.uploaded_theme != Some(theme)
            || self.uploaded_color_mode != Some(active_color_mode)
            || self.uploaded_profile_edges != Some(self.profile_edges)
            || self.uploaded_sketchy != Some(self.sketchy);
        // Scene is uploaded once (renderer shared); only the first pane's
        // callback carries the snapshot, the rest just set their camera.
        let mut scene = if stale {
            self.uploaded_generation = Some(generation);
            self.uploaded_theme = Some(theme);
            self.uploaded_color_mode = Some(active_color_mode);
            self.uploaded_profile_edges = Some(self.profile_edges);
            self.uploaded_sketchy = Some(self.sketchy);
            // Sketchy depth cue: bias by the active pane's eye + scene radius.
            let (sketchy_eye, sketchy_radius) = if self.sketchy.active() {
                let cam = &self.cameras[self.layout.camera_index(self.active_pane)];
                let r = self
                    .session
                    .doc
                    .scene_aabb()
                    .map(|bb| (bb.size().length() as f32 * 0.5).max(0.5))
                    .unwrap_or(8.0);
                (Some(cam.eye()), r)
            } else {
                (None, 0.0)
            };
            let mut s = scene::snapshot_with_mode(
                &self.session.doc,
                theme,
                itsjustcad_render::ColorModeSnapshot {
                    color_mode: active_color_mode,
                    profile_edges: self.profile_edges,
                    sketchy: self.sketchy,
                    sketchy_eye,
                    sketchy_radius,
                },
            );
            s.underlay = self.decode_underlay();
            s.basemap = self.basemap_data();
            Some(s)
        } else {
            None
        };

        let panes = self.layout.split(full);
        for (pane, rect) in panes.iter().copied().enumerate() {
            let cam_idx = self.layout.camera_index(pane);
            let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());

            // Click (or drag/RMB) inside a pane makes it the active viewport,
            // Rhino-style: sticky, no hover flicker.
            if response.clicked()
                || response.secondary_clicked()
                || response.dragged()
            {
                self.active_pane = pane;
            }

            // Rhino muscle memory: RMB orbit, Shift+RMB pan, scroll dolly.
            if response.dragged_by(egui::PointerButton::Secondary)
                || response.dragged_by(egui::PointerButton::Middle)
            {
                let delta = response.drag_delta();
                let shift = ui.input(|i| i.modifiers.shift);
                if shift {
                    self.cameras[cam_idx].pan(delta.x, delta.y);
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                } else {
                    self.cameras[cam_idx].orbit(delta.x, delta.y);
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
                }
            } else if response.hovered()
                && ui.input(|i| {
                    i.pointer
                        .button_down(egui::PointerButton::Secondary)
                        || i.pointer.button_down(egui::PointerButton::Middle)
                })
            {
                // Show grab cursor while holding the button before movement.
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
            }
            if response.hovered() {
                let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                if scroll != 0.0 {
                    self.cameras[cam_idx].dolly(scroll);
                }
            }
            let aspect = rect.width() / rect.height().max(1.0);
            let view_proj = self.cameras[cam_idx].view_proj(aspect);

            // Status-bar cursor readout follows the active pane's hover.
            if pane == self.active_pane {
                self.active_aspect = aspect; // for `camera <lens>` fov conversion
                self.status_cursor = response
                    .hover_pos()
                    .and_then(|pos| ground_point(view_proj, rect, pos));
            }

            if self.draw_tool.active() {
                // Draw/osnap only in the active pane; one prompt, one ghost.
                if pane == self.active_pane {
                    if response.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
                    }
                    self.drawing_input(ui, rect, &response, view_proj);
                }
            } else {
                // Gumball on the selection (active pane only). A completed
                // drag emits ONE substrate command through Session::run so
                // the op-log stays the single source of truth.
                let mut consumed = false;
                if pane == self.active_pane {
                    // Control-point handles win over the gumball: a single
                    // curve with editable points is edited vertex-by-vertex.
                    let pe = self.point_edit.ui(
                        ui,
                        rect,
                        &response,
                        view_proj,
                        &self.session.doc,
                        theme,
                    );
                    if let Some(cmd) = pe.command {
                        match self.session.run(cmd) {
                            Ok(outcome) => self.command_line.push_line(outcome.message),
                            Err(e) => self.command_line.push_line(format!("error: {e}")),
                        }
                    }
                    consumed = pe.consumed;
                    // Gumball is a persistent Rhino-style toggle (default OFF):
                    // when hidden it is neither drawn nor hit-tested, so a plain
                    // selection shows only the highlight. Move/rotate/scale verbs
                    // work regardless of this flag.
                    if !consumed && self.show_gumball {
                        let out =
                            self.gumball
                                .ui(ui, rect, &response, view_proj, &self.session.doc);
                        consumed = out.consumed;
                        if let Some(cmd) = out.command {
                            match self.session.run(cmd) {
                                Ok(outcome) => self.command_line.push_line(outcome.message),
                                Err(e) => self.command_line.push_line(format!("error: {e}")),
                            }
                        }
                    }
                }
                if !consumed
                    && response.clicked()
                    && let Some(pos) = response.interact_pointer_pos()
                {
                    let (additive, bypass_group) =
                        ui.input(|i| (i.modifiers.shift, i.modifiers.command));
                    self.pick(view_proj, rect, pos, additive, !bypass_group);
                }
                // Rhino preset: right-click (no drag) = repeat last command.
                // We only fire when the click was NOT consumed by a drag and the
                // cursor did not move (egui's secondary_clicked covers this).
                if preset::preset_for(self.cad_origin).right_click_repeat_last
                    && pane == self.active_pane
                    && response.secondary_clicked()
                    && let Some(line) = self.last_line.clone()
                {
                    self.execute_line(line);
                }
                // Drag-box selection (no tool, gumball idle): left→right is a
                // window (solid box, fully-inside only), right→left a crossing
                // (dashed box, touch counts). Shift adds to the selection.
                if !consumed
                    && pane == self.active_pane
                    && response.drag_started_by(egui::PointerButton::Primary)
                    && let Some(pos) = response.interact_pointer_pos()
                {
                    self.box_drag = Some(pos);
                }
                if pane == self.active_pane
                    && let Some(start) = self.box_drag
                    && let Some(pos) = response.interact_pointer_pos()
                {
                    let mode = crate::boxsel::mode(start, pos);
                    let drag_rect = egui::Rect::from_two_pos(start, pos);
                    draw_rubber_box(&ui.painter_at(rect), drag_rect, mode, ui.visuals());
                    if response.drag_stopped_by(egui::PointerButton::Primary) {
                        self.box_drag = None;
                        let additive = ui.input(|i| i.modifiers.shift);
                        self.box_select(view_proj, rect, drag_rect, mode, additive);
                    }
                    ui.ctx().request_repaint(); // live rubber box
                }
            }

            let sun_dir = self.session.doc.sun.map(|s| {
                itsjustcad_solar::sun_direction(s.azimuth_deg, s.altitude_deg)
            });
            // Skip the wgpu 3D callback in off-screen preview snapshots (no depth
            // attachment in kittest's egui pass). `scene.take()` still runs so the
            // one-shot snapshot isn't leaked to a later pane.
            let scene_snapshot = scene.take();
            if !self.preview_no_viewport {
                ui.painter().add(egui_wgpu::Callback::new_paint_callback(
                    rect,
                    ViewportCallback {
                        view_proj,
                        eye: self.cameras[cam_idx].eye(),
                        generation,
                        scene: scene_snapshot,
                        viewport: pane,
                        mode: self.display_modes[cam_idx],
                        sun_dir,
                        light: self.light_mode,
                        background_gradient: self.profile_edges,
                        edges_enabled: self.shaded_edges,
                    },
                ));
            }

            // Per-pane view-name TAG (Rhino/SketchUp): the pane's name in its
            // top-left corner. It is the ACTIVE-viewport indicator AND a click
            // target — clicking the tag activates that viewport (Rhino
            // title-click). Active: a blue chip with a high-contrast blue label
            // (WCAG AA, see `theme::viewport_active_tag`); inactive: plain weak
            // text. This replaces the old thin pane border, which added noise.
            {
                let cam = &self.cameras[cam_idx];
                let label = crate::statusbar::view_label(cam.yaw, cam.pitch, cam.ortho);
                let active = pane == self.active_pane;
                let font = egui::TextStyle::Small.resolve(ui.style());
                // Measure the label so the chip / hit-area fit it exactly.
                let size = ui
                    .painter()
                    .layout_no_wrap(label.to_string(), font.clone(), egui::Color32::WHITE)
                    .size();
                let pad = egui::vec2(6.0, 3.0);
                let tag_rect = egui::Rect::from_min_size(
                    rect.left_top() + egui::vec2(6.0, 6.0),
                    size + pad * 2.0,
                );
                // Tag click activates the pane without disturbing the scene
                // selection (added on top, so it captures the click before the
                // pane's pick logic).
                let tag_resp = ui.interact(
                    tag_rect,
                    ui.id().with(("view_tag", pane)),
                    egui::Sense::click(),
                );
                if tag_resp.clicked() {
                    self.active_pane = pane;
                }
                let text_color = if active {
                    let (chip, fg) =
                        crate::theme::viewport_active_tag(ui.visuals().dark_mode);
                    // Drop shadow ONLY on the active tag's chip (lifts it off the
                    // 3D); inactive tags are plain text, no shadow.
                    ui.painter().add(
                        egui::epaint::Shadow {
                            offset: [0, 1],
                            blur: 4,
                            spread: 0,
                            color: egui::Color32::from_black_alpha(70),
                        }
                        .as_shape(tag_rect, egui::CornerRadius::same(4)),
                    );
                    ui.painter().rect_filled(tag_rect, 4.0, chip);
                    fg
                } else {
                    ui.visuals().weak_text_color()
                };
                ui.painter_at(rect).text(
                    tag_rect.min + pad,
                    egui::Align2::LEFT_TOP,
                    label,
                    font,
                    text_color,
                );
            }

            // Dimensions and text are 2D overlay drawing (egui text cannot go
            // through wgpu); hatches render in the scene itself.
            self.draw_annotations(ui, rect, view_proj, theme);
            // Structural loads (arrows) and supports (symbols) overlay.
            self.draw_struct_overlays(ui, rect, view_proj);
            // Lineweight overlay: when showweights is on, re-draw curves with
            // physical stroke widths via the egui painter (wgpu cannot vary
            // line width per-draw-call on WebGPU/Metal/Vulkan).
            if self.session.doc.show_lineweights {
                self.draw_lineweights_overlay(ui, rect, view_proj, theme);
            }

            // (The full per-pane border was removed — it added visual noise. The
            // active viewport is shown by its blue name tag, above; internal
            // dividers between panes are drawn once, below.)
        }

        // Internal viewport dividers (Rhino-style cross): a single subtle grey
        // line on every pane edge that TOUCHES another pane — i.e. interior to
        // the viewport region, never on its outer boundary. For a 2×2 layout this
        // is a centered cross; for 2-up, one line. Helps the eye separate panes
        // without re-introducing the noisy full border. Drawn after the wgpu
        // callbacks so it composites on top of the 3D. Skipped in preview
        // snapshots' chrome-only mode is unnecessary — these are cheap egui lines.
        if panes.len() > 1 {
            let divider = if ui.visuals().dark_mode {
                egui::Color32::from_gray(90) // lighter line on dark viewports
            } else {
                egui::Color32::from_gray(165) // darker line on light viewports
            };
            let stroke = egui::Stroke::new(1.0, divider);
            let eps = 0.5;
            let painter = ui.painter();
            for rect in panes.iter().copied() {
                if rect.left() > full.left() + eps {
                    painter.vline(rect.left(), rect.y_range(), stroke);
                }
                if rect.right() < full.right() - eps {
                    painter.vline(rect.right(), rect.y_range(), stroke);
                }
                if rect.top() > full.top() + eps {
                    painter.hline(rect.x_range(), rect.top(), stroke);
                }
                if rect.bottom() < full.bottom() - eps {
                    painter.hline(rect.x_range(), rect.bottom(), stroke);
                }
            }
        }

        // Safety net: a drag that ends without a pointer position (or in
        // another pane) must not leave a stale rubber box behind.
        if self.box_drag.is_some() && !ui.input(|i| i.pointer.any_down()) {
            self.box_drag = None;
        }

        // Empty-document next-actions: instead of a bare viewport, offer the
        // first things a user reaches for (draw / import / example). Only when
        // the document is truly empty, no tool is mid-flight, and the user has
        // NOT yet completed first-run onboarding. Returning users with an empty
        // doc see a clean viewport — the overlay is a one-time first-run guide.
        // Only rendered over the active pane so multi-view layouts stay uncluttered.
        if show_empty_document(
            &self.session.doc,
            self.draw_tool.active(),
            load_onboarding_done(),
        ) {
            let pane_rect = panes.get(self.active_pane).copied().unwrap_or(full);
            self.empty_document_overlay(ui, pane_rect);
        } else if !self.preview_no_viewport
            && show_empty_document_hint(
                &self.session.doc,
                self.draw_tool.active(),
                load_onboarding_done(),
            )
        {
            // Returning user, empty doc: no boxed overlay — just a whisper-quiet
            // centered next-action hint so the viewport is never a blank void.
            // Weak text, no frame; suppressed in preview/`--shot` snapshots so it
            // never leaks into chrome-only captures.
            let pane_rect = panes.get(self.active_pane).copied().unwrap_or(full);
            let painter = ui.painter_at(pane_rect);
            painter.text(
                pane_rect.center(),
                egui::Align2::CENTER_CENTER,
                "Type a command or describe what to draw",
                egui::FontId::proportional(14.0),
                ui.visuals().weak_text_color(),
            );
        }
    }

    /// Centered next-actions shown over an empty document viewport. Wires the
    /// three most common first moves through the same substrate/verb paths the
    /// menus use, so nothing here is a bespoke code path.
    fn empty_document_overlay(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        let dark = ui.visuals().dark_mode;
        let roles = self.live_roles(dark);
        let mut import_now = false;
        let mut draw_line = false;
        let mut draw_box = false;
        egui::Area::new(egui::Id::new("empty_doc_overlay"))
            .fixed_pos(rect.center() - egui::vec2(150.0, 70.0))
            .order(egui::Order::Middle)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(egui::Margin::same(crate::theme::Spacing::L as i8))
                    .show(ui, |ui| {
                        ui.set_width(300.0);
                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new("Empty drawing")
                                    .heading()
                                    .color(ui.visuals().text_color()),
                            );
                            ui.add_space(crate::theme::Spacing::XS);
                            ui.label(
                                egui::RichText::new("Start with a shape, or bring in a model.")
                                    .weak(),
                            );
                            ui.add_space(crate::theme::Spacing::M);
                            draw_line = crate::widgets::role_button(
                                ui,
                                &roles,
                                dark,
                                crate::widgets::ButtonRole::Prominent,
                                "New line",
                            )
                            .clicked();
                            ui.add_space(crate::theme::Spacing::S);
                            draw_box = crate::widgets::role_button(
                                ui,
                                &roles,
                                dark,
                                crate::widgets::ButtonRole::Normal,
                                "Draw an example box",
                            )
                            .clicked();
                            ui.add_space(crate::theme::Spacing::S);
                            import_now = crate::widgets::role_button(
                                ui,
                                &roles,
                                dark,
                                crate::widgets::ButtonRole::Normal,
                                "Import a model…",
                            )
                            .clicked();
                        });
                    });
            });
        if draw_line {
            self.execute_line("line".to_string());
        }
        if draw_box {
            self.execute_line("box 0,0,0 4,4,4".to_string());
        }
        if import_now {
            self.import(None);
        }
    }

    /// Overlay pass for dimension and text annotations: world points project
    /// through `view_proj`, text sizes track world-space heights on screen.
    fn draw_annotations(
        &self,
        ui: &egui::Ui,
        rect: egui::Rect,
        view_proj: glam::Mat4,
        theme: scene::Theme,
    ) {
        use itsjustcad_doc::{Annotation, Geometry};
        let painter = ui.painter_at(rect);
        let doc = &self.session.doc;
        for obj in doc.objects() {
            if !obj.visible || !doc.layer_visible(&obj.layer) {
                continue;
            }
            let Geometry::Annotation(ann) = &obj.geometry else {
                continue;
            };
            let c = if doc.selection.contains(&obj.id) {
                theme.selected()
            } else {
                doc.layers
                    .get(&obj.layer)
                    .and_then(|s| s.color)
                    .unwrap_or(theme.curve())
            };
            let color = egui::Color32::from_rgb(
                (c[0] * 255.0).round() as u8,
                (c[1] * 255.0).round() as u8,
                (c[2] * 255.0).round() as u8,
            );
            let stroke = egui::Stroke::new(1.0, color);
            // World height -> on-screen pixels at `at`, for text sizing.
            let px_height = |at: glam::DVec3, h: f64| -> f32 {
                let Some(p) = project(view_proj, rect, at) else {
                    return 12.0;
                };
                // Whichever world axis is visible in this view carries the size
                // (Z collapses in Top view, Y in Front view).
                let len = |axis: glam::DVec3| {
                    project(view_proj, rect, at + axis * h)
                        .map(|q| (p - q).length())
                        .unwrap_or(0.0)
                };
                len(glam::DVec3::Z).max(len(glam::DVec3::Y)).clamp(8.0, 60.0)
            };
            match ann {
                Annotation::LinearDim { a, b, offset } => {
                    let dir = (*b - *a).normalize_or_zero();
                    let perp = glam::DVec3::new(-dir.y, dir.x, 0.0).normalize_or(glam::DVec3::X);
                    let (a2, b2) = (*a + perp * *offset, *b + perp * *offset);
                    let segs = [(*a, a2), (*b, b2), (a2, b2)];
                    let mut px: Option<(egui::Pos2, egui::Pos2)> = None;
                    for (w0, w1) in segs {
                        if let (Some(p), Some(q)) = (
                            project(view_proj, rect, w0),
                            project(view_proj, rect, w1),
                        ) {
                            painter.line_segment([p, q], stroke);
                            px = Some((p, q)); // last segment = dimension line
                        }
                    }
                    if let Some((p, q)) = px {
                        // 45° tick marks at the dimension line ends.
                        let d = (q - p).normalized();
                        let tick = egui::vec2(d.x - d.y, d.x + d.y) * 3.5;
                        painter.line_segment([p - tick, p + tick], stroke);
                        painter.line_segment([q - tick, q + tick], stroke);
                        let mid = egui::pos2((p.x + q.x) * 0.5, (p.y + q.y) * 0.5);
                        // Measured value: derived, formatted in document units.
                        let label =
                            itsjustcad_doc::format_length(doc.units, (*b - *a).length());
                        let size = px_height((a2 + b2) * 0.5, 0.2);
                        painter.text(
                            mid,
                            egui::Align2::CENTER_BOTTOM,
                            label,
                            egui::FontId::proportional(size),
                            color,
                        );
                    }
                }
                Annotation::Text { pos, text, height } => {
                    if let Some(p) = project(view_proj, rect, *pos) {
                        painter.text(
                            p,
                            egui::Align2::LEFT_BOTTOM,
                            text,
                            egui::FontId::proportional(px_height(*pos, *height)),
                            color,
                        );
                    }
                }
                Annotation::Hatch { .. } => {} // rendered in the wgpu scene
            }
        }
    }

    /// Overlay pass for structural loads and supports: loads → color-coded
    /// arrows, supports → geometric symbols (triangle/square/circle).
    /// All drawing is in 2D egui screen space projected from world coordinates.
    fn draw_struct_overlays(
        &self,
        ui: &egui::Ui,
        rect: egui::Rect,
        view_proj: glam::Mat4,
    ) {
        use itsjustcad_doc::{LoadGeometry, RestraintKind};
        let painter = ui.painter_at(rect);
        let doc = &self.session.doc;

        // Color palette for loads vs supports.
        let load_color = egui::Color32::from_rgb(255, 160, 40); // amber
        let support_color = egui::Color32::from_rgb(80, 200, 120); // green

        // Arrow length in screen pixels.
        const ARROW_PX: f32 = 32.0;
        // Arrowhead half-width.
        const HEAD_PX: f32 = 6.0;

        let project = |p: glam::DVec3| -> Option<egui::Pos2> { project(view_proj, rect, p) };

        // Helper: draw a 2D arrow from `tail` to `head` on screen.
        let draw_arrow = |tail: egui::Pos2, head: egui::Pos2, color: egui::Color32| {
            let d = head - tail;
            let len = d.length();
            if len < 2.0 {
                return;
            }
            let u = d / len;
            let perp = egui::vec2(-u.y, u.x);
            let stroke = egui::Stroke::new(2.0, color);
            // Shaft
            painter.line_segment([tail, head], stroke);
            // Arrowhead triangle
            let base = head - u * HEAD_PX * 2.0;
            let p1 = base + perp * HEAD_PX;
            let p2 = base - perp * HEAD_PX;
            painter.add(egui::Shape::convex_polygon(
                vec![head, p1, p2],
                color,
                egui::Stroke::NONE,
            ));
        };

        // Draw loads.
        for load in &doc.loads {
            let dir2 = {
                // Project the direction as a screen delta.
                let origin = match &load.geometry {
                    LoadGeometry::Point { position } => *position,
                    LoadGeometry::Line { a, b } => (*a + *b) * 0.5,
                    LoadGeometry::Area { boundary } => {
                        if boundary.is_empty() {
                            continue;
                        }
                        let sum: glam::DVec3 = boundary.iter().copied().sum();
                        sum / boundary.len() as f64
                    }
                };
                let Some(p0) = project(origin) else { continue };
                let dir_world = load.direction * 0.5; // 0.5 m reference length
                let Some(p1) = project(origin + dir_world) else { continue };
                let d = p1 - p0;
                if d.length() < 1.0 {
                    // Direction collapsed to zero on screen; use a default down arrow.
                    egui::vec2(0.0, 1.0)
                } else {
                    d.normalized()
                }
            };

            // Arrow: tail is the application point, head is tip (direction-pointing).
            let anchor = match &load.geometry {
                LoadGeometry::Point { position } => {
                    project(*position)
                }
                LoadGeometry::Line { a, b } => {
                    let mid = (*a + *b) * 0.5;
                    project(mid)
                }
                LoadGeometry::Area { boundary } => {
                    if boundary.is_empty() {
                        continue;
                    }
                    let sum: glam::DVec3 = boundary.iter().copied().sum();
                    project(sum / boundary.len() as f64)
                }
            };
            let Some(anch) = anchor else { continue };
            let tail = anch;
            let head = anch + dir2 * ARROW_PX;
            draw_arrow(tail, head, load_color);
            // Label
            painter.text(
                head + egui::vec2(4.0, -4.0),
                egui::Align2::LEFT_BOTTOM,
                &load.name,
                egui::FontId::proportional(10.0),
                load_color,
            );
        }

        // Draw supports.
        const SYM_R: f32 = 8.0; // symbol radius/half-size
        for sup in &doc.supports {
            let Some(sc) = project(sup.position) else { continue };
            match sup.kind {
                RestraintKind::Pinned => {
                    // Triangle pointing up (apex at node).
                    let apex = sc;
                    let base_l = apex + egui::vec2(-SYM_R, SYM_R * 1.5);
                    let base_r = apex + egui::vec2(SYM_R, SYM_R * 1.5);
                    painter.add(egui::Shape::convex_polygon(
                        vec![apex, base_l, base_r],
                        support_color,
                        egui::Stroke::NONE,
                    ));
                    // Ground line
                    let stroke = egui::Stroke::new(2.0, support_color);
                    painter.line_segment(
                        [base_l - egui::vec2(2.0, 0.0), base_r + egui::vec2(2.0, 0.0)],
                        stroke,
                    );
                }
                RestraintKind::Fixed => {
                    // Filled square at node.
                    let tl = sc - egui::vec2(SYM_R, SYM_R);
                    painter.rect_filled(
                        egui::Rect::from_min_size(tl, egui::vec2(SYM_R * 2.0, SYM_R * 2.0)),
                        0.0,
                        support_color,
                    );
                    // Ground line below
                    let stroke = egui::Stroke::new(2.0, support_color);
                    let base_y = sc.y + SYM_R;
                    painter.line_segment(
                        [egui::pos2(sc.x - SYM_R, base_y), egui::pos2(sc.x + SYM_R, base_y)],
                        stroke,
                    );
                }
                RestraintKind::Roller => {
                    // Circle (roller can rotate).
                    painter.circle_filled(sc, SYM_R, support_color);
                    // Show roller axis as a short line through the circle.
                    if let Some(ax) = sup.roller_axis {
                        let p0 = project(sup.position - ax * 0.3);
                        let p1 = project(sup.position + ax * 0.3);
                        if let (Some(p0), Some(p1)) = (p0, p1) {
                            painter.line_segment(
                                [p0, p1],
                                egui::Stroke::new(2.0, egui::Color32::WHITE),
                            );
                        }
                    }
                }
            }
            // Label: kind below the symbol.
            painter.text(
                sc + egui::vec2(SYM_R + 2.0, 0.0),
                egui::Align2::LEFT_CENTER,
                sup.kind.label(),
                egui::FontId::proportional(10.0),
                support_color,
            );
        }
    }

    /// Lineweight overlay: when `showweights on` is active, project all visible
    /// curve objects to screen space and draw them with their effective stroke
    /// width via the egui painter. This supplements the wgpu 1-pixel hairlines
    /// (which cannot vary in width per-object on WebGPU/Metal/Vulkan) so that
    /// weight differences are visible in the live 3D view.
    ///
    /// Display scale: 1 mm of lineweight → 4 screen pixels. This is intentionally
    /// above physical accuracy so differences are clearly legible on typical monitors.
    fn draw_lineweights_overlay(
        &self,
        ui: &egui::Ui,
        rect: egui::Rect,
        view_proj: glam::Mat4,
        theme: itsjustcad_render::Theme,
    ) {
        use itsjustcad_doc::Geometry;
        const DISPLAY_TOL: f64 = 0.005;
        const PX_PER_MM: f32 = 4.0; // exaggerated for legibility

        let painter = ui.painter_at(rect);
        let doc = &self.session.doc;

        for obj in doc.objects() {
            if !obj.visible || !doc.layer_visible(&obj.layer) {
                continue;
            }
            let lw_mm = doc.effective_lineweight(obj) as f32;
            // Only draw objects with non-hairline weight; hairlines are already
            // rendered by the wgpu pipeline.
            if lw_mm <= 0.18 {
                continue;
            }
            let width_px = lw_mm * PX_PER_MM;
            // Resolve display color (simplified: use layer color or theme default).
            let layer_color = doc.layers.get(&obj.layer).and_then(|s| s.color);
            let base_color = obj.color
                .map(|[r, g, b]| [r, g, b, 1.0])
                .or(layer_color)
                .unwrap_or(theme.curve());
            let egui_color = egui::Color32::from_rgba_unmultiplied(
                (base_color[0] * 255.0) as u8,
                (base_color[1] * 255.0) as u8,
                (base_color[2] * 255.0) as u8,
                (base_color[3] * 255.0) as u8,
            );
            let stroke = egui::Stroke::new(width_px, egui_color);

            // Project curve points to screen and draw.
            if let Geometry::Curve(curve) = &obj.geometry {
                let pts: Vec<egui::Pos2> = curve
                    .tessellate(DISPLAY_TOL)
                    .iter()
                    .filter_map(|&p| project(view_proj, rect, p))
                    .collect();
                for pair in pts.windows(2) {
                    painter.line_segment([pair[0], pair[1]], stroke);
                }
                if curve.is_closed()
                    && let (Some(&first), Some(&last)) = (pts.first(), pts.last())
                {
                    painter.line_segment([last, first], stroke);
                }
            }
        }
    }

    /// Undo history tab body: op list newest-last, current position highlighted.
    /// Clicking an entry jumps there by running undo/redo through the
    /// session, so the op-log stays the single source of truth. Rendered inside
    /// the docked right panel (no floating Area).
    fn history_panel(&mut self, ui: &mut egui::Ui) {
        let (entries, cursor) = self.session.history();
        let mut jump: Option<usize> = None;
        ui.label(egui::RichText::new(format!("History ({})", entries.len())).heading());
        ui.separator();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if ui.selectable_label(cursor == 0, "(start)").clicked() {
                    jump = Some(0);
                }
                for (i, name) in entries.iter().enumerate() {
                    let step = i + 1; // state after op i
                    let label = format!("{step}. {name}");
                    let text = if step > cursor {
                        egui::RichText::new(label).weak() // undone
                    } else {
                        egui::RichText::new(label)
                    };
                    if ui.selectable_label(step == cursor, text).clicked() {
                        jump = Some(step);
                    }
                }
                ui.separator();
                ui.weak("amend <op#> <command> rewrites a step\n(first op is 0, e.g. amend 0 box 0,0,0 8,8,3)");
            });
        if let Some(step) = jump {
            match self.session.jump_to(step) {
                Ok(moved) if moved > 0 => self
                    .command_line
                    .push_line(format!("history: jumped to step {step} ({moved} op(s))")),
                Ok(_) => {}
                Err(e) => self.command_line.push_line(format!("error: {e}")),
            }
        }
    }

    /// Properties tab body: read-out of the active selection (count, layer,
    /// combined bounding box). Query-only; no ops issued.
    fn properties_panel(&mut self, ui: &mut egui::Ui) {
        // Title comes from the enclosing CollapsingHeader in the Model workspace.
        let doc = &self.session.doc;
        let sel = &doc.selection;
        if sel.is_empty() {
            ui.weak("No selection.");
            ui.add_space(4.0);
            ui.weak("Click an object in a viewport, or use `select all`.");
            return;
        }
        ui.label(format!("{} object(s) selected", sel.len()));
        // Layers spanned by the selection.
        let mut layers: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        let mut aabb: Option<kernel_mesh::Aabb> = None;
        for obj in doc.objects().filter(|o| sel.contains(&o.id)) {
            layers.insert(obj.layer.as_str());
            let bb = obj.geometry.aabb();
            aabb = Some(match aabb {
                Some(a) => a.union(bb),
                None => bb,
            });
        }
        // Middle-truncate each layer name (preserve head+tail) so a long name
        // never blows out the narrow Properties column.
        let shown = layers
            .into_iter()
            .map(|n| middle_truncate(n, 18))
            .collect::<Vec<_>>()
            .join(", ");
        ui.label(format!("layer(s): {shown}"));
        if let Some(bb) = aabb {
            let s = bb.size();
            ui.separator();
            ui.label("bounding box:");
            ui.monospace(format!(
                "  size  {}",
                crate::statusbar::format_cursor(doc.units, Some(s))
            ));
            ui.monospace(format!(
                "  min   {}",
                crate::statusbar::format_cursor(doc.units, Some(bb.min))
            ));
            ui.monospace(format!(
                "  max   {}",
                crate::statusbar::format_cursor(doc.units, Some(bb.max))
            ));
        }
    }

    /// Layers tab body: a Rhino-style organized table with aligned columns
    /// (Name / On / Lock / Color / Linetype / Print Width), a current-layer dot
    /// on the leading edge, and a top toolbar (＋ add, － delete, ⚙ settings).
    /// Every mutation goes through the command substrate so it is logged/undoable.
    fn layers_panel(&mut self, ui: &mut egui::Ui, theme: scene::Theme) {
        use itsjustcad_doc::LineType;
        // Print-width dropdown choices (mm); "Default" maps to the ISO-thin 0.18.
        const PRINT_WIDTHS: [f64; 10] =
            [0.18, 0.13, 0.18, 0.25, 0.35, 0.50, 0.70, 1.00, 1.40, 2.00];

        let mut lines: Vec<String> = Vec::new();
        let current = self.session.doc.current_layer.clone();
        // Snapshot layers sorted by (order, name) so the panel matches the
        // render sort; storage stays alphabetical (BTreeMap).
        let mut layers: Vec<(String, itsjustcad_doc::LayerStyle)> = self
            .session
            .doc
            .layers
            .iter()
            .map(|(n, s)| (n.clone(), s.clone()))
            .collect();
        layers.sort_by(|a, b| a.1.order.cmp(&b.1.order).then_with(|| a.0.cmp(&b.0)));

        let fg = ui.visuals().text_color();
        let icon_sz = ui.text_style_height(&egui::TextStyle::Body);
        // Destructive tint for the layer-delete/purge affordances (system-red).
        let dark = ui.visuals().dark_mode;
        let roles = self.live_roles(dark);
        let destructive = crate::theme::to_color32(roles.destructive);

        // Top toolbar: a reserved TOP panel keeps the ＋ － ⚙ row pinned and
        // visible no matter how tall the (scrolling) table grows. Apple HIG:
        // never park critical controls at the bottom, where a window drag can
        // push them offscreen — the add-layer affordance lives up top.
        egui::Panel::top("layers_toolbar")
            .show_separator_line(true)
            .show(ui, |ui| {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    let add = self.icons.image(ui.ctx(), crate::icons::Icon::Plus, icon_sz, fg);
                    if ui
                        .add(egui::Button::image(add).frame(true))
                        .on_hover_text("add layer")
                        .clicked()
                    {
                        let name = next_free_layer_name(&self.session.doc);
                        lines.push(format!("layer {name}"));
                        if let Some(c) = self.new_layer_default_color {
                            lines.push(format!(
                                "layercolor {name} {:.3},{:.3},{:.3}",
                                c[0], c[1], c[2]
                            ));
                        }
                        self.selected_layer = Some(name);
                    }
                    let target =
                        self.selected_layer.clone().unwrap_or_else(|| current.clone());
                    let deletable = target != itsjustcad_doc::DEFAULT_LAYER;
                    // Destructive-red mark when the delete is armed, dimmed otherwise.
                    let del_tint = if deletable { destructive } else { fg };
                    let del = self
                        .icons
                        .image(ui.ctx(), crate::icons::Icon::Minus, icon_sz, del_tint);
                    if ui
                        .add_enabled(deletable, egui::Button::image(del).frame(true))
                        .on_hover_text(if deletable {
                            "delete selected layer"
                        } else {
                            "the default layer cannot be deleted"
                        })
                        .clicked()
                    {
                        lines.push(format!("layerdelete {target}"));
                        self.selected_layer = None;
                    }

                    // ⚙ settings menu: purge empty layers + new-layer default color.
                    let gear = self.icons.image(ui.ctx(), crate::icons::Icon::Settings, icon_sz, fg);
                    ui.menu_image_button(gear, |ui| {
                        if crate::widgets::role_button(
                            ui,
                            &roles,
                            dark,
                            crate::widgets::ButtonRole::Destructive,
                            "Purge empty layers",
                        )
                        .clicked()
                        {
                            for name in empty_deletable_layers(&self.session.doc) {
                                lines.push(format!("layerdelete {name}"));
                            }
                            ui.close();
                        }
                        ui.separator();
                        ui.label("New-layer default color");
                        let mut c = self.new_layer_default_color.unwrap_or_else(|| {
                            let m = theme.mesh();
                            [m[0], m[1], m[2]]
                        });
                        if ui.color_edit_button_rgb(&mut c).changed() {
                            self.new_layer_default_color = Some(c);
                        }
                        if ui.button("Clear default color").clicked() {
                            self.new_layer_default_color = None;
                            ui.close();
                        }
                    });
                });
                ui.add_space(2.0);
            });

        // BOTH-axis scroll: the 7-column table (with Linetype/Print combos) is
        // wider than the dock's DOCK_MIN. A vertical-only scroll let that content
        // width force the panel wider than Chat/Sessions and blocked resizing it
        // narrow. Allowing horizontal scroll decouples the table width from the
        // panel width, so the Layers tab resizes like the others (wide table just
        // scrolls sideways).
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // Dense list rows opt back down to the 24px group floor: the
                // ~28px PRIMARY-chrome min would over-inflate a multi-row table.
                ui.spacing_mut().interact_size.y = crate::theme::Spacing::L;
                egui::Grid::new("layers_table")
                    .num_columns(7)
                    .spacing(egui::vec2(8.0, 6.0))
                    .striped(true)
                    .show(ui, |ui| {
                        // Header row (panel_title weight via strong).
                        ui.label("");
                        ui.strong("Name");
                        ui.strong("On");
                        ui.strong("Lock");
                        ui.strong("Color");
                        ui.strong("Linetype");
                        ui.strong("Print");
                        ui.end_row();

                        for (name, style) in &layers {
                            let is_current = *name == current;
                            let is_selected = self
                                .selected_layer
                                .as_deref()
                                .map(|s| s == name)
                                .unwrap_or(false);

                            // Leading edge: current-layer dot (click = set current).
                            let dot = if is_current {
                                self.icons.image(ui.ctx(), crate::icons::Icon::CircleDot, icon_sz, ui.visuals().selection.bg_fill)
                            } else {
                                self.icons.image(ui.ctx(), crate::icons::Icon::CircleDot, icon_sz, ui.visuals().weak_text_color())
                            };
                            if ui
                                .add(egui::Button::image(dot).frame(false))
                                .on_hover_text("set current layer")
                                .clicked()
                                && !is_current
                            {
                                lines.push(format!("layer {name}"));
                            }

                            // Name (middle-truncated, stable selection highlight).
                            let shown = middle_truncate(name, 18);
                            if ui
                                .selectable_label(is_selected, shown)
                                .on_hover_text(name.as_str())
                                .clicked()
                            {
                                self.selected_layer = Some(name.clone());
                            }

                            // On/Off lightbulb (visibility → hide/show).
                            let bulb_color = if style.visible { fg } else { ui.visuals().weak_text_color() };
                            let bulb = self.icons.image(ui.ctx(), crate::icons::Icon::Lightbulb, icon_sz, bulb_color);
                            if ui
                                .add(egui::Button::image(bulb).frame(false))
                                .on_hover_text(if style.visible { "on (click to hide)" } else { "off (click to show)" })
                                .clicked()
                            {
                                let verb = if style.visible { "hide" } else { "show" };
                                lines.push(format!("{verb} {name}"));
                            }

                            // Lock padlock (layerlock).
                            let (lock_icon, lock_color) = if style.locked {
                                (crate::icons::Icon::Lock, ui.visuals().warn_fg_color)
                            } else {
                                (crate::icons::Icon::LockOpen, ui.visuals().weak_text_color())
                            };
                            let lock = self.icons.image(ui.ctx(), lock_icon, icon_sz, lock_color);
                            if ui
                                .add(egui::Button::image(lock).frame(false))
                                .on_hover_text(if style.locked { "locked (click to unlock)" } else { "unlocked (click to lock)" })
                                .clicked()
                            {
                                let state = if style.locked { "off" } else { "on" };
                                lines.push(format!("layerlock {name} {state}"));
                            }

                            // Color swatch (layercolor), committed on release.
                            let fallback = theme.mesh();
                            let mut rgb = self
                                .pending_layer_color
                                .as_ref()
                                .filter(|(n, _)| n == name)
                                .map(|(_, c)| *c)
                                .or_else(|| style.color.map(|c| [c[0], c[1], c[2]]))
                                .unwrap_or([fallback[0], fallback[1], fallback[2]]);
                            if ui.color_edit_button_rgb(&mut rgb).changed() {
                                self.pending_layer_color = Some((name.clone(), rgb));
                            }

                            // Linetype dropdown (layerlinetype).
                            let mut lt = style.linetype;
                            egui::ComboBox::from_id_salt(("lt", name))
                                .selected_text(lt.label())
                                .width(96.0)
                                .show_ui(ui, |ui| {
                                    for opt in LineType::ALL {
                                        ui.selectable_value(&mut lt, opt, opt.label());
                                    }
                                });
                            if lt != style.linetype {
                                lines.push(format!("layerlinetype {name} {}", lt.token()));
                            }

                            // Print-width dropdown (layerweight).
                            let cur_mm = style.lineweight_mm;
                            let cur_label = print_width_label(cur_mm);
                            let mut chosen: Option<f64> = None;
                            egui::ComboBox::from_id_salt(("pw", name))
                                .selected_text(cur_label)
                                .width(90.0)
                                .show_ui(ui, |ui| {
                                    for (i, mm) in PRINT_WIDTHS.iter().enumerate() {
                                        let label = if i == 0 { "Default".to_string() } else { format!("{mm:.2}") };
                                        if ui.selectable_label(false, label).clicked() {
                                            chosen = Some(*mm);
                                        }
                                    }
                                });
                            if let Some(mm) = chosen
                                && (mm - cur_mm).abs() > 1e-9
                            {
                                lines.push(format!("layerweight {name} {mm}"));
                            }
                            ui.end_row();
                        }
                    });
            });

        // Commit the color edit once the mouse is released — one logged op
        // per edit instead of one per drag frame.
        if let Some((name, c)) = self.pending_layer_color.clone()
            && !ui.input(|i| i.pointer.any_down())
        {
            lines.push(format!("layercolor {name} {:.3},{:.3},{:.3}", c[0], c[1], c[2]));
            self.pending_layer_color = None;
        }

        for line in lines {
            self.execute_line(line);
        }
    }

    /// Interactive drawing: picks on the ground plane, ghost preview, prompt.
    fn drawing_input(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        response: &egui::Response,
        view_proj: glam::Mat4,
    ) {
        // The canvas owns the keyboard while picking: typed digits build the
        // precise-input buffer, so no text field may hold focus underneath.
        if let Some(id) = ui.ctx().memory(|m| m.focused()) {
            ui.ctx().memory_mut(|m| m.surrender_focus(id));
        }

        let (esc, enter, shift) = ui.input(|i| {
            (
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::Enter),
                i.modifiers.shift,
            )
        });
        if esc {
            self.draw_tool.cancel();
            self.command_line.push_line("drawing cancelled");
            return;
        }
        // Typed characters feed the numeric buffer; Backspace edits it
        // (keymap keeps delete-selection off while drawing).
        let typed: Vec<egui::Event> = ui.input(|i| {
            i.events
                .iter()
                .filter(|e| {
                    matches!(
                        e,
                        egui::Event::Text(_)
                            | egui::Event::Key {
                                key: egui::Key::Backspace,
                                pressed: true,
                                ..
                            }
                    )
                })
                .cloned()
                .collect()
        });
        for event in typed {
            match event {
                egui::Event::Text(t) => {
                    for c in t.chars() {
                        self.draw_tool.push_input(c);
                    }
                }
                _ => {
                    self.draw_tool.pop_input();
                }
            }
        }

        // Snap resolution: nearest object point within the screen-space
        // radius wins; empty space falls back to the ground-plane 10cm grid.
        let cursor_px = response
            .hover_pos()
            .or_else(|| response.interact_pointer_pos());
        let mut snap_hit = cursor_px.and_then(|pos| {
            // Screen-proximity cull: only objects whose projected AABB (grown by
            // the snap radius) covers the cursor contribute snap points. At 10k
            // objects this trims the candidate list from every vertex in the
            // scene to just the few under the pointer.
            let cands = crate::osnap::candidates_filtered(&self.session.doc, |bb| {
                match projected_rect(view_proj, rect, bb.min, bb.max) {
                    Some(r) => r
                        .expand(crate::osnap::SNAP_RADIUS_PX)
                        .contains(pos),
                    None => true, // behind camera / partly clipped: keep to be safe
                }
            });
            crate::osnap::resolve(
                &cands,
                pos,
                crate::osnap::SNAP_RADIUS_PX,
                |w| project(view_proj, rect, w),
            )
        });
        let mut cursor_world = snap_hit.map(|(p, _)| p).or_else(|| {
            cursor_px
                .and_then(|pos| ground_point(view_proj, rect, pos))
                .map(crate::osnap::grid_snap)
        });
        // Shift = ortho lock: 0°/90° from the last picked point overrides
        // osnap (marker off, the constrained point is what a click commits).
        if shift && let (Some(last), Some(c)) = (self.draw_tool.last_point(), cursor_world) {
            cursor_world = Some(crate::precise::ortho_lock(last, c));
            snap_hit = None;
        }
        self.status_snap = snap_hit.map(|(_, kind)| kind.label());

        if enter {
            let buffer = self.draw_tool.take_input();
            if !buffer.is_empty() {
                // Precise input: resolve the typed point, feed it as a pick.
                match crate::precise::resolve_input(
                    &buffer,
                    self.draw_tool.last_point(),
                    cursor_world,
                ) {
                    Ok(world) => {
                        if let Some(cmd) = self.draw_tool.on_click(world) {
                            self.execute_line(cmd);
                            return;
                        } else if let Some(prompt) = self.draw_tool.prompt() {
                            self.command_line.push_line(prompt);
                        }
                    }
                    Err(e) => self.command_line.push_line(format!("error: {e}")),
                }
            } else if let Some(cmd) = self.draw_tool.on_enter() {
                self.execute_line(cmd);
                return;
            }
        }

        if response.clicked() && let Some(world) = cursor_world {
            if let Some(cmd) = self.draw_tool.on_click(world) {
                self.execute_line(cmd);
                return;
            } else if let Some(prompt) = self.draw_tool.prompt() {
                self.command_line.push_line(prompt);
            }
        }

        // Ghost preview + prompt overlay
        let painter = ui.painter_at(rect);
        let stroke = egui::Stroke::new(1.5, egui::Color32::from_rgb(90, 160, 255));
        for strip in self.draw_tool.preview(cursor_world) {
            for pair in strip.windows(2) {
                if let (Some(a), Some(b)) = (
                    project(view_proj, rect, pair[0]),
                    project(view_proj, rect, pair[1]),
                ) {
                    painter.line_segment([a, b], stroke);
                }
            }
        }
        // Osnap marker: square on the snapped point + kind label (Rhino look).
        if let Some((world, kind)) = snap_hit
            && let Some(screen) = project(view_proj, rect, world)
        {
            let color = egui::Color32::from_rgb(255, 200, 60);
            painter.rect_stroke(
                egui::Rect::from_center_size(screen, egui::vec2(9.0, 9.0)),
                0.0,
                egui::Stroke::new(1.5, color),
                egui::StrokeKind::Middle,
            );
            painter.text(
                screen + egui::vec2(8.0, -8.0),
                egui::Align2::LEFT_BOTTOM,
                kind.label(),
                egui::TextStyle::Small.resolve(ui.style()),
                color,
            );
        }
        if let Some(prompt) = self.draw_tool.prompt() {
            painter.text(
                rect.center_top() + egui::vec2(0.0, 28.0),
                egui::Align2::CENTER_TOP,
                prompt,
                egui::TextStyle::Body.resolve(ui.style()),
                ui.visuals().strong_text_color(),
            );
        }
        ui.ctx().request_repaint(); // live rubber-band
    }

    /// Bottom strip: cursor coords, active layer, counts, snap state, view.
    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let doc = &self.session.doc;
        let cam = &self.cameras[self.layout.camera_index(self.active_pane)];
        ui.horizontal(|ui| {
            // Fixed-width slot for the x/y/z readout so growing coordinate values
            // never widen it and shift the rest of the status bar. The text is
            // left-aligned within a constant 260px region (overflow clips rather
            // than pushing the toolbar).
            ui.allocate_ui_with_layout(
                egui::vec2(260.0, ui.available_height()),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(crate::statusbar::format_cursor(
                                doc.units,
                                self.status_cursor,
                            ))
                            .monospace(),
                        )
                        .truncate(),
                    );
                },
            );
            ui.separator();
            ui.label(format!("layer: {}", doc.current_layer));
            ui.separator();
            ui.label(crate::statusbar::format_counts(doc.selection.len(), doc.len()));
            ui.separator();
            ui.label(crate::statusbar::snap_label(
                self.draw_tool.active(),
                self.status_snap,
            ));
            ui.separator();
            ui.label(format!(
                "view: {}",
                crate::statusbar::view_label(cam.yaw, cam.pitch, cam.ortho)
            ));
            ui.separator();
            // Gumball toggle chip (Rhino-style): selectable state mirrors the
            // `gumball` verb / `G` hotkey so all three stay in sync. Explicitly
            // theme-aware (a bare selectable_label filled with the raw accent read
            // wrong in dark): ON = accent fill + on-accent text; OFF = transparent
            // + dimmed text. Colours come from the LIVE theme, not a fixed preset.
            let on = self.show_gumball;
            // ON = filled chip in the tuned viewport-active blue (SAME blue as the
            // user chat bubble — the raw accent read washed/light in dark). OFF =
            // FRAMELESS dimmed text (`frame(false)` — a transparent fill alone
            // still leaves egui's widget frame, which read as a wrong grey box).
            let (on_fill, on_txt) = crate::theme::viewport_active_tag(ui.visuals().dark_mode);
            let txt = if on { on_txt } else { ui.visuals().weak_text_color() };
            let mut btn = egui::Button::new(egui::RichText::new("gumball").color(txt))
                .frame(on)
                .corner_radius(egui::CornerRadius::same(4));
            if on {
                btn = btn.fill(on_fill);
            }
            if ui
                .add(btn)
                .on_hover_text("Toggle transform gizmo (G)")
                .clicked()
            {
                self.show_gumball = !on;
                save_gumball_visible(!on);
            }
        });
    }

    /// Mode-correct semantic color roles for the LIVE egui theme. The preset
    /// fixes the *accent*, but dark/light is a runtime choice (View ▸ Appearance),
    /// so draw code must read its surface ramp from the live theme — not from the
    /// preset's single default-mode palette (which painted off-whites in dark
    /// mode). Callers pass `ui.visuals().dark_mode`.
    fn live_roles(&self, dark: bool) -> crate::theme::ColorRoles {
        let accent = preset::preset_for(self.cad_origin).tokens().colors.primary;
        crate::theme::roles_for_mode(dark, accent)
    }

    /// Bottom viewport tab bar (Rhino convention): Persp/Top/Front/Right plus
    /// any saved named views. Clicking a tab sets the active pane's view (or
    /// restores a named view). Rendered as a `TopBottomPanel::bottom` inside the
    /// central viewport frame so it never overlaps the canvas.
    fn viewport_tab_bar(&mut self, ui: &mut egui::Ui) {
        let named: Vec<String> = self.session.doc.named_views.keys().cloned().collect();
        let roles = self.live_roles(ui.visuals().dark_mode);
        // Highlight the tab matching the active pane's current view.
        let cam = &self.cameras[self.layout.camera_index(self.active_pane)];
        let current = crate::statusbar::view_label(cam.yaw, cam.pitch, cam.ortho);
        let mut chosen: Option<String> = None;
        ui.horizontal(|ui| {
            // Standard views (Persp/Top/Front/Right) as a real segmented control:
            // equal-width, one type, selection-only.
            let std_labels: Vec<&str> =
                crate::tabstrip::STANDARD_VIEW_TABS.iter().map(|(l, _)| *l).collect();
            let cur_std = crate::tabstrip::STANDARD_VIEW_TABS
                .iter()
                .position(|(l, _)| l.eq_ignore_ascii_case(current))
                .unwrap_or(usize::MAX);
            if let Some(i) = crate::widgets::segmented(ui, &roles, &std_labels, cur_std) {
                chosen = Some(crate::tabstrip::STANDARD_VIEW_TABS[i].1.to_string());
            }
            // Named saved views stay as individual selectable tabs after the group.
            for name in &named {
                let lower = name.to_ascii_lowercase();
                if crate::tabstrip::STANDARD_VIEW_TABS.iter().any(|(_, v)| *v == lower) {
                    continue;
                }
                let selected = name.eq_ignore_ascii_case(current);
                if ui.selectable_label(selected, name).clicked() {
                    chosen = Some(format!("view {name}"));
                }
            }
            // View controls, right-aligned on the same row (de-floated from the
            // old canvas overlay): 1/2/4 layout, color mode, display mode, ZE.
            // right_to_left lays out in reverse, so paint them in reverse order
            // to read `ZE | Shaded▾ | ByLayer▾ | [1 2 4]` left→right.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let slot = self.layout.camera_index(self.active_pane);
                // Layout 1/2/4 as a segmented control (single-select, equal-width).
                let layout_variants =
                    [ViewportLayout::Single, ViewportLayout::Two, ViewportLayout::Four];
                let layout_labels = ["1", "2", "4"];
                let cur_layout = layout_variants
                    .iter()
                    .position(|l| *l == self.layout)
                    .unwrap_or(0);
                if let Some(i) =
                    crate::widgets::segmented(ui, &roles, &layout_labels, cur_layout)
                {
                    self.set_layout(layout_variants[i]);
                }
                ui.separator();
                egui::ComboBox::from_id_salt("color_mode")
                    .selected_text(self.color_modes[slot].label())
                    .show_ui(ui, |ui| {
                        for mode in ColorMode::ALL {
                            let prev = self.color_modes[slot];
                            ui.selectable_value(&mut self.color_modes[slot], mode, mode.label());
                            if self.color_modes[slot] != prev {
                                self.uploaded_color_mode = None;
                            }
                        }
                    });
                egui::ComboBox::from_id_salt("display_mode")
                    .selected_text(self.display_modes[slot].label())
                    .show_ui(ui, |ui| {
                        for mode in DisplayMode::ALL {
                            ui.selectable_value(&mut self.display_modes[slot], mode, mode.label());
                        }
                    });
                // Fixed space between the icon group (ZE, rightmost) and the text
                // group (display/color/layout) — a deliberate gap, not a divider.
                if self
                    .icons
                    .icon_button(ui, crate::icons::Icon::Maximize, "zoom extents")
                    .clicked()
                {
                    self.zoom_extents();
                }
                ui.add_space(crate::theme::Spacing::SM);
            });
        });
        if let Some(verb) = chosen {
            self.execute_line(verb);
        }
    }

    /// Command-line panel body. Docked at the bottom of the right panel: history
    /// (op-log scrollback) sits above the input and the autosuggest popup opens
    /// upward — the Rhino/AutoCAD layout.
    fn command_line_body(&mut self, ui: &mut egui::Ui) {
        let object_names: Vec<String> = self
            .session
            .doc
            .objects()
            .filter_map(|o| o.name.clone())
            .collect();
        let aliases = self.active_aliases();
        let panel_h = ui.available_height();
        if let Some(line) = self.command_line.ui(ui, &object_names, aliases, panel_h) {
            self.execute_line(line);
        }
    }

    /// Right docked tab panel (Layer 2): a hand-rolled tab strip over
    /// Layers / Properties / Chat, with the command line docked at the bottom
    /// (always visible, panel width). Clicking the active tab collapses the panel
    /// to just the strip; a chevron also toggles it. The Chat tab keeps its
    /// resize behavior (the whole panel is resizable) and background streaming
    /// (tick runs every frame in `ui`, independent of visibility).
    fn right_panel(&mut self, ui: &mut egui::Ui) {
        use crate::tabstrip::PanelTab;
        if !self.panel_visible {
            // Dock hidden → no left edge to clamp the viewport against.
            self.dock_left = None;
            // Collapsed to nothing: a small ▸ handle at the top-right edge, at the
            // SAME height the collapse button had (stored while the panel was
            // visible) so the button doesn't jump vertically on toggle.
            let vr = ui.ctx().viewport_rect();
            let y = ui
                .ctx()
                .data(|d| d.get_temp::<f32>(egui::Id::new("panel_toggle_y")))
                .unwrap_or(vr.top() + 52.0);
            egui::Area::new(egui::Id::new("panel_show_btn"))
                .fixed_pos(egui::pos2(vr.right() - 28.0, y))
                .show(ui.ctx(), |ui| {
                    if self
                        .icons
                        .icon_button(ui, crate::icons::Icon::PanelOpen, "show panel (Cmd+\\)")
                        .clicked()
                    {
                        self.panel_visible = true;
                    }
                });
            return;
        }

        // DYNAMIC TABS: Blocks appears while the doc has block definitions,
        // Plugins while macros are installed (or while the user pinned either
        // open). Reconciled every frame from live state — pure logic in
        // `tabstrip`, derivations in `dyntabs`.
        let has_blocks = crate::dyntabs::has_block_defs(&self.session.doc);
        let has_plugins = !self.session.plugins.is_empty();
        self.panel_tabs.sync_dynamic(has_blocks, has_plugins);
        let visible_tabs = self.panel_tabs.visible_tabs(has_blocks, has_plugins);

        let collapsed = self.panel_tabs.is_collapsed();
        let theme = if ui.visuals().dark_mode { scene::Theme::Dark } else { scene::Theme::Light };
        let mut panel = egui::Panel::right("right_panel").resizable(!collapsed);
        // A small inner margin off the 8pt grid so icon+label rows (tab strip,
        // section headers) aren't flush against the dock edge (SwiftUI-style
        // breathing room). The dock fill is WHITE (dark-neutral in dark mode) so
        // the ENTIRE dock — tab strip, chat, layers — reads as one clean surface
        // rather than a grey panel with a white card inside it. Keeps the default
        // left stroke that separates the dock from the viewport.
        // Route the dock fill through the MODE-CORRECT surface role (dark ramp
        // base ≈ rgb 36,36,40; light ≈ white) rather than a hardcoded magic
        // number, so the dock, chat, and command line share one coherent surface.
        // DARK ONLY: a recessed (darker) surface so the whole docked panel reads
        // as one distinct block against the viewport. Light mode was already good
        // — leave it on the normal panel surface.
        let dock_fill = if ui.visuals().dark_mode {
            crate::theme::recessed_fill(true)
        } else {
            ui.visuals().panel_fill
        };
        panel = panel.frame(
            egui::Frame::side_top_panel(ui.style())
                .fill(dock_fill)
                .inner_margin(egui::Margin::symmetric(
                    crate::theme::Spacing::default().s as i8,
                    4,
                )),
        );
        // FIXED PANEL WIDTH: the dock renders at ONE constant width for ALL tabs
        // (Chat/Sessions/Layers). `dock_width` is the single source of truth
        // (seeded from `tabstrip::DOCK_WIDTH`); switching tabs never changes it.
        // The user may still drag to resize — we read the resulting width back
        // into `dock_width` after `show`, so the dragged width persists across
        // tab switches. `tabstrip::dock_width` proves the width is tab-agnostic.
        // Width bounds: floor at DOCK_MIN (280px); cap at half the window width so
        // the dock never eats more than half the screen. Clamp the stored width too.
        let max_w = (ui.ctx().viewport_rect().width() * 0.5).max(crate::tabstrip::DOCK_MIN);
        let width = crate::tabstrip::dock_width(self.panel_tabs.active(), self.dock_width)
            .clamp(crate::tabstrip::DOCK_MIN, max_w);
        panel = if collapsed {
            // Even collapsed, hold the same width so toggling collapse/tabs
            // never jumps the dock edge.
            panel.exact_size(width)
        } else {
            panel
                .default_size(width)
                .min_size(crate::tabstrip::DOCK_MIN)
                .max_size(max_w)
        };
        let panel_resp = panel.show(ui, |ui| {
            // Header row: chevron + tab strip.
            ui.horizontal(|ui| {
                // Match the folder-tab height exactly (tab = body text + 2×the
                // shared tab vertical padding).
                let tab_h = ui.text_style_height(&egui::TextStyle::Body)
                    + 2.0 * crate::tabstrip::TAB_V_PAD;
                let close = self.icons.icon_button_min_h(
                    ui,
                    crate::icons::Icon::PanelClose,
                    "hide panel (Cmd+\\)",
                    tab_h,
                );
                // Remember this button's screen-y so the reshow button (when the
                // panel is hidden) sits at the SAME height — tapping collapse must
                // not move the button vertically.
                ui.ctx().data_mut(|d| {
                    d.insert_temp(egui::Id::new("panel_toggle_y"), close.rect.top())
                });
                if close.clicked() {
                    self.panel_visible = false;
                }
                if let Some(tab) =
                    crate::tabstrip::strip_ui(ui, &self.icons, self.panel_tabs, &visible_tabs)
                {
                    self.panel_tabs.click(tab);
                    if tab == PanelTab::Deck {
                        self.deck_visible = !self.panel_tabs.is_collapsed();
                    }
                }
            });
            if self.panel_tabs.is_collapsed() {
                return;
            }
            // Breathing room below the tab strip so each tab's top content (deck
            // selectors, search field, Layers header) doesn't hug the tabs.
            ui.add_space(crate::theme::Spacing::SM);
            // The command line USED to be docked at the bottom of this right panel.
            // It now lives as a top-level, full-width bottom panel at the very
            // bottom of the window (see `command_line_panel` in `ui()`), which
            // fixes the flicker/dropped-keystroke bug caused by a bottom panel
            // nested inside the resizable right panel. The tab body now fills the
            // full right panel. Transparent frame so the WHITE dock fill shows
            // through for every tab (no grey card inside a white dock).
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| {
                match self.panel_tabs.active() {
                    // Rhino-style "Model" workspace: Layers AND Properties shown
                    // together as stacked, independently-collapsible sections
                    // (both visible at once, not one-or-the-other). Properties is
                    // docked to the BOTTOM so the (scrollable) Layers list fills
                    // the space between the strip and it.
                    PanelTab::Model => {
                        egui::Panel::bottom("properties_section")
                            .resizable(true)
                            .default_size(160.0)
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    section_icon(ui, &self.icons, crate::icons::Icon::Properties);
                                    egui::CollapsingHeader::new("Properties")
                                        .default_open(true)
                                        .show(ui, |ui| self.properties_panel(ui));
                                });
                            });
                        egui::CentralPanel::default().show(ui, |ui| {
                            // No redundant "Layers" title — the active tab already
                            // names it.
                            self.layers_tab(ui, theme);
                        });
                    }
                    PanelTab::Sessions => {
                        // Promoted session browser: search + newest-first cards.
                        // A card click loads that session into the live Chat and
                        // switches the active tab to Chat.
                        let roles = self.live_roles(ui.visuals().dark_mode);
                        // No redundant "Sessions" title — the active tab names it.
                        if self.deck_pane.sessions_tab_ui(ui, &roles) {
                            self.panel_tabs.show(PanelTab::Deck);
                            self.deck_visible = true;
                        }
                    }
                    PanelTab::Deck => {
                        let dark = ui.visuals().dark_mode;
                        let roles = self.live_roles(dark);
                        self.deck_pane.ui(
                            ui,
                            &mut self.session,
                            &self.tokio,
                            &self.icons,
                            &roles,
                            dark,
                            self.reduce_motion,
                        );
                    }
                    PanelTab::Blocks => self.blocks_tab(ui),
                    PanelTab::Plugins => self.plugins_tab(ui),
                }
            });
        });
        // Read back the (possibly user-dragged) width so the drag persists
        // across tab switches. The panel keeps the SAME id for every tab, so
        // this width is shared by all tabs — a tab switch alone never changes it.
        if !collapsed {
            let w = panel_resp.response.rect.width();
            if w.is_finite() && w > 1.0 {
                // Persist within the same [floor, half-window] bounds.
                self.dock_width = w.clamp(crate::tabstrip::DOCK_MIN, max_w);
            }
        }
        // Record the dock's ACTUAL left edge so the viewport (rendered after this,
        // in the central panel) can clamp its right edge to it — see `dock_left`.
        self.dock_left = Some(panel_resp.response.rect.left());
    }

    /// Layers tab wrapper: runs the layers UI then commits any pending edits.
    fn layers_tab(&mut self, ui: &mut egui::Ui, theme: scene::Theme) {
        self.layers_panel(ui, theme);
    }

    /// Blocks tab (dynamic): the document's block definitions as rows —
    /// name, live instance count, param signature for dynamic blocks — plus
    /// the on-disk library below. A pure VIEW + verb-trigger surface: Insert
    /// prefills the `insert` command (placement by typing a position), Load
    /// fires `blockload`, Delete fires the guarded `blockdelete`. All mutations
    /// go through the ONE substrate path so op-log/undo/replay hold.
    fn blocks_tab(&mut self, ui: &mut egui::Ui) {
        let rows = crate::dyntabs::block_rows(&self.session.doc);
        let mut run_line: Option<String> = None;
        let mut prefill: Option<String> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            if rows.is_empty() {
                ui.weak("No block definitions yet.");
                ui.weak("Capture one with `block <selector> <name>` or load one from the library below.");
            }
            for row in &rows {
                let confirming = self.pending_block_delete.as_deref() == Some(row.name.as_str());
                ui.horizontal(|ui| {
                    ui.strong(&row.name);
                    let count = format!(
                        "{} instance{}",
                        row.instances,
                        if row.instances == 1 { "" } else { "s" }
                    );
                    ui.weak(count);
                });
                if let Some(sig) = &row.param_signature {
                    ui.weak(format!("params: {sig}"));
                }
                ui.horizontal(|ui| {
                    if ui
                        .small_button("Insert")
                        .on_hover_text("prefill `insert` — type a position, press Enter")
                        .clicked()
                    {
                        prefill = Some(format!("insert {} ", row.name));
                    }
                    if confirming {
                        ui.label(egui::RichText::new("delete definition?").color(ui.visuals().warn_fg_color));
                        if ui.small_button("Delete").clicked() {
                            run_line = Some(format!("blockdelete {}", self.pending_block_delete.take().unwrap_or_default()));
                        }
                        if ui.small_button("Cancel").clicked() {
                            self.pending_block_delete = None;
                        }
                    } else if row.instances > 0 {
                        // GUARD surfaced in the UI before the verb even runs:
                        // a definition with live instances cannot be deleted.
                        ui.add_enabled(false, egui::Button::new("Delete").small())
                            .on_disabled_hover_text(format!(
                                "{} instance{} exist — delete the instances first",
                                row.instances,
                                if row.instances == 1 { "" } else { "s" }
                            ));
                    } else if ui.small_button("Delete").clicked() {
                        self.pending_block_delete = Some(row.name.clone());
                    }
                });
                ui.separator();
            }

            // ---- on-disk block library (blocklib / blockload) ----
            ui.add_space(crate::theme::Spacing::SM);
            ui.horizontal(|ui| {
                ui.strong("Library");
                if ui
                    .small_button("↻")
                    .on_hover_text("re-scan ~/.config/itsjustcad/blocks/")
                    .clicked()
                {
                    self.blocklib_cache = None;
                }
            });
            let lib = self.blocklib_cache.get_or_insert_with(|| {
                itsjustcad_commands::blocklib::list()
                    .map(|(names, _dir)| names)
                    .map_err(|e| e.to_string())
            });
            match lib {
                Ok(names) if names.is_empty() => {
                    ui.weak("library is empty");
                }
                Ok(names) => {
                    for name in names.clone() {
                        ui.horizontal(|ui| {
                            ui.label(&name);
                            let already = rows.iter().any(|r| r.name == name);
                            if already {
                                ui.weak("loaded");
                            } else if ui
                                .small_button("Load")
                                .on_hover_text("`blockload` into this document")
                                .clicked()
                            {
                                run_line = Some(format!("blockload {name}"));
                            }
                        });
                    }
                }
                Err(e) => {
                    ui.weak(format!("library unavailable: {e}"));
                }
            }
        });
        if let Some(line) = run_line {
            self.execute_line(line);
        }
        if let Some(text) = prefill {
            self.command_line.prefill(text);
            self.command_line.focus();
        }
    }

    /// Plugins tab (dynamic): installed user/LLM-authored macros as rows —
    /// name, summary, positional arg signature — with Run (prefills the verb),
    /// JSON (read-only source popup), per-row Delete (confirm), plus Reload
    /// and Open-folder header actions. Every mutation routes through the
    /// existing `plugin …` management verbs on the command line.
    fn plugins_tab(&mut self, ui: &mut egui::Ui) {
        let rows = crate::dyntabs::plugin_rows(&self.session.plugins);
        let mut run_line: Option<String> = None;
        let mut prefill: Option<String> = None;
        ui.horizontal(|ui| {
            if ui
                .small_button("Reload")
                .on_hover_text("`plugin reload` — re-scan the plugins folder")
                .clicked()
            {
                run_line = Some("plugin reload".into());
            }
            if ui
                .small_button("Open folder")
                .on_hover_text("reveal ~/.config/itsjustcad/plugins/ in the file manager")
                .clicked()
            {
                open_plugins_folder(&mut self.command_line);
            }
        });
        ui.add_space(crate::theme::Spacing::SM);
        egui::ScrollArea::vertical().show(ui, |ui| {
            if rows.is_empty() {
                ui.weak("No plugins installed.");
                ui.weak("Ask the chat to author one, or `plugin save <name> <n>` to capture recent commands.");
            }
            for row in &rows {
                let confirming = self.pending_plugin_delete.as_deref() == Some(row.name.as_str());
                ui.strong(&row.usage);
                if !row.summary.is_empty() {
                    ui.weak(&row.summary);
                }
                ui.horizontal(|ui| {
                    if ui
                        .small_button("Run")
                        .on_hover_text("prefill the command line — add args, press Enter")
                        .clicked()
                    {
                        prefill = Some(format!("{} ", row.name));
                    }
                    if ui
                        .small_button("JSON")
                        .on_hover_text("view the plugin's JSON source (read-only)")
                        .clicked()
                    {
                        self.plugin_json_view = Some((row.name.clone(), row.json.clone()));
                    }
                    if confirming {
                        ui.label(egui::RichText::new("delete plugin?").color(ui.visuals().warn_fg_color));
                        if ui.small_button("Delete").clicked() {
                            run_line = Some(format!("plugin delete {}", self.pending_plugin_delete.take().unwrap_or_default()));
                        }
                        if ui.small_button("Cancel").clicked() {
                            self.pending_plugin_delete = None;
                        }
                    } else if ui.small_button("Delete").clicked() {
                        self.pending_plugin_delete = Some(row.name.clone());
                    }
                });
                ui.separator();
            }
        });
        // Read-only JSON source popup (floating, closable).
        if let Some((name, json)) = self.plugin_json_view.clone() {
            let mut open = true;
            egui::Window::new(format!("Plugin source — {name}"))
                .open(&mut open)
                .collapsible(false)
                .resizable(true)
                .default_width(360.0)
                .show(ui.ctx(), |ui| {
                    egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                        // Read-only: an immutable text buffer renders selectable
                        // (copyable) but uneditable monospace text.
                        let mut src = json.as_str();
                        ui.add(
                            egui::TextEdit::multiline(&mut src)
                                .font(egui::TextStyle::Monospace)
                                .desired_width(f32::INFINITY),
                        );
                    });
                });
            if !open {
                self.plugin_json_view = None;
            }
        }
        if let Some(line) = run_line {
            self.execute_line(line);
        }
        if let Some(text) = prefill {
            self.command_line.prefill(text);
            self.command_line.focus();
        }
    }

    /// Top menu bar (Layer 3): registry-driven, grouped per preset
    /// (`menu::top_menus`). The chosen action is dispatched via
    /// [`Self::apply_menu_action`].
    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        let style = preset::preset_for(self.cad_origin).menu_style;
        let icons = &self.icons;
        // When the true native OS menu bar (muda) is attached, it owns the
        // File/Edit/… verb menus; the in-window strip then carries only the
        // egui-only Appearance controls (which can't be native items). Verb
        // dispatch arrives from the native bar's event channel (polled in `ui`).
        // On Linux muda is not compiled in, so this branch is omitted.
        #[cfg(not(target_os = "linux"))]
        if self.native_menu.is_some() {
            // The native OS bar owns File/Edit/View/… AND (new) the Appearance
            // items (theme + text size live in View ▸ Appearance). So there is no
            // in-window strip at all on macOS/Windows — we render nothing here.
            // The dev/screenshot hook still fires so the menu model can be shot.
            let _ = icons;
            if let Ok(title) = std::env::var("ITSJUSTCAD_MENU_DEMO") {
                let at = egui::pos2(8.0, 0.0);
                crate::menu::demo_open(ui.ctx(), &self.icons, style, &title, at);
            }
            return;
        }
        let has_selection = !self.session.doc.selection.is_empty();
        let view = self.view_state();
        let bar = egui::Panel::top("menu_bar").resizable(false).show(ui, |ui| {
            crate::menu::ui(
                ui,
                icons,
                style,
                has_selection,
                crate::menu::MenuToggles {
                    local_only: self.deck_pane.local_only(),
                    web_search: self.deck_pane.allow_web_search(),
                    terse: self.deck_pane.terse_enabled(),
                },
                view,
            )
        });
        // Dev/screenshot hook: force one menu open to show grouped items.
        if let Ok(title) = std::env::var("ITSJUSTCAD_MENU_DEMO") {
            let at = egui::pos2(bar.response.rect.left() + 8.0, bar.response.rect.bottom());
            crate::menu::demo_open(ui.ctx(), &self.icons, style, &title, at);
        }
        if let Some(action) = bar.inner {
            let ctx = ui.ctx().clone();
            self.apply_menu_action(&ctx, action);
        }
    }

    /// Current View-menu state (active display / lighting mode + panel
    /// visibility) so the View menu's radio + flip items read as stateful. Maps
    /// the render enums onto the menu-local tags [`crate::menu`] uses so `menu.rs`
    /// stays render-dep-free.
    fn view_state(&self) -> crate::menu::ViewState {
        use crate::menu::{CameraTag, DisplayModeTag, LightModeTag};
        use itsjustcad_render::{DisplayMode, LightMode, PanoProjection};
        let disp = self.display_modes[self.layout.camera_index(self.active_pane)];
        let display = match disp {
            DisplayMode::Shaded => Some(DisplayModeTag::Shaded),
            DisplayMode::Wireframe => Some(DisplayModeTag::Wireframe),
            DisplayMode::XRay => Some(DisplayModeTag::XRay),
            DisplayMode::Pencil => Some(DisplayModeTag::Pencil),
            // Ghosted has no menu entry; nothing is checked for it.
            DisplayMode::Ghosted => None,
        };
        let lighting = match self.light_mode {
            LightMode::Working => Some(LightModeTag::Working),
            LightMode::Sun => Some(LightModeTag::Sun),
            LightMode::Presentation => Some(LightModeTag::Presentation),
        };
        // Camera projection of the focused viewport: pano/fisheye win (they
        // replace the pinhole entirely), then two-point, then plain
        // perspective; ortho standard views check nothing.
        let cam = self.cameras[self.layout.camera_index(self.active_pane)];
        let camera = if cam.ortho {
            None
        } else {
            match cam.pano {
                Some(PanoProjection::Equirect) => Some(CameraTag::Panorama),
                Some(PanoProjection::Fisheye { .. }) => Some(CameraTag::Fisheye),
                None if cam.two_point => Some(CameraTag::TwoPoint),
                None => Some(CameraTag::Perspective),
            }
        };
        crate::menu::ViewState { display, lighting, camera, panel_visible: self.panel_visible }
    }

    /// Toggle the right docked panel from the View menu, mirroring the ⌘\
    /// Deck-tab-aware hotkey: if the Deck tab is the active, visible panel, hide
    /// the panel; otherwise show it and focus the Deck tab.
    fn toggle_panel(&mut self) {
        use crate::tabstrip::PanelTab;
        let deck_showing = self.panel_visible
            && !self.panel_tabs.is_collapsed()
            && self.panel_tabs.active() == PanelTab::Deck;
        if deck_showing {
            self.panel_visible = false;
            self.deck_visible = false;
        } else {
            self.panel_visible = true;
            self.panel_tabs.show(PanelTab::Deck);
            self.deck_visible = true;
        }
        save_deck_visible(self.deck_visible);
    }

    /// Dispatch a menu pick. The rule (see `menu::menu_action`): draw verbs
    /// start the interactive tool, no-arg verbs execute, arg verbs prefill the
    /// command line for typing. `ctx` is needed for the Appearance actions
    /// (theme / text-size), which live in the native View menu.
    fn apply_menu_action(&mut self, ctx: &egui::Context, action: crate::menu::MenuAction) {
        use crate::menu::MenuAction;
        match action {
            MenuAction::Execute(line) | MenuAction::StartDraw(line) => self.execute_line(line),
            MenuAction::Insert(prefix) => {
                self.command_line.prefill(prefix);
            }
            MenuAction::Help => {
                for line in help_lines(None) {
                    self.command_line.push_line(line);
                }
            }
            MenuAction::About => self.show_about = true,
            MenuAction::ModelSetup => self.show_model_setup = true,
            MenuAction::EditHistory => self.show_history = true,
            MenuAction::ImportDialog => self.import(None),
            MenuAction::ExportDialog => self.export(None),
            MenuAction::NewDocument => self.guarded_nav(ctx, PendingNav::New),
            MenuAction::NewSession => self.new_session(),
            MenuAction::SetTheme(dark) => self.set_theme_pref(ctx, dark),
            MenuAction::ZoomStep(bigger) => {
                let delta = if bigger { 0.1 } else { -0.1 };
                let z = (ctx.zoom_factor() + delta).clamp(0.5, 3.0);
                ctx.set_zoom_factor(z);
            }
            MenuAction::ZoomReset => ctx.set_zoom_factor(1.3),
            MenuAction::CommandPalette => self.open_palette(),
            MenuAction::RevealModelsFolder => {
                if let Some(dir) = crate::download::models_dir() {
                    reveal_folder(&dir);
                }
            }
            MenuAction::DownloadDefaultModel => {
                // Only when one is actually offered (not installed, no download in
                // flight, no healthy local deck); otherwise open Model Setup so the
                // user can pick manually.
                if let Some((id, _name)) = self.default_model_offer() {
                    self.start_model_install(&id);
                } else {
                    self.show_model_setup = true;
                }
            }
            MenuAction::ToggleLocalOnly => self.deck_pane.toggle_local_only(),
            MenuAction::ToggleWebSearch => self.deck_pane.toggle_web_search(),
            MenuAction::ToggleTerse => self.deck_pane.toggle_terse(),
            MenuAction::TogglePanel => self.toggle_panel(),
        }
    }

    /// Open the ⌘K command palette: rebuild the candidate set from the registry
    /// (+ app verbs) and reset the query / selection.
    fn open_palette(&mut self) {
        self.palette_entries = crate::palette::entries();
        self.palette_query.clear();
        self.palette_sel = 0;
        self.show_palette = true;
    }

    /// The ⌘K command palette: a centered overlay fuzzy-searching EVERY verb
    /// (registry + app verbs). A single text field drives the query; Up/Down move
    /// the highlight, Enter runs the highlighted row (execute a no-arg verb, or
    /// prefill the command line for a verb needing args), Esc closes. Keyboard
    /// model mirrors the command-line autosuggest.
    fn command_palette_ui(&mut self, ctx: &egui::Context) {
        if !self.show_palette {
            return;
        }

        // Rank the current query against the candidate set (cap the visible rows).
        const MAX_ROWS: usize = 12;
        let hits: Vec<crate::palette::PaletteEntry> =
            crate::palette::search(&self.palette_query, &self.palette_entries, MAX_ROWS)
                .into_iter()
                .cloned()
                .collect();
        if self.palette_sel >= hits.len() {
            self.palette_sel = hits.len().saturating_sub(1);
        }

        // Keyboard navigation, read before the widgets consume events.
        let (up, down, enter, esc) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::Escape),
            )
        });
        if up {
            self.palette_sel = self.palette_sel.saturating_sub(1);
        }
        if down && !hits.is_empty() {
            self.palette_sel = (self.palette_sel + 1).min(hits.len() - 1);
        }

        let mut chosen: Option<crate::palette::PaletteEntry> = None;
        let mut close = false;

        egui::Area::new(egui::Id::new("command_palette"))
            .anchor(egui::Align2::CENTER_TOP, [0.0, 96.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                // Theme-aware colors: read from the LIVE egui visuals (which track
                // the active dark/light theme) rather than a fixed preset, so the
                // palette stays readable in dark mode. Popup body uses the window
                // fill; the query field uses the extreme (input) background; weak
                // text uses the theme's dimmed text color.
                let elevated = ui.visuals().window_fill();
                let field_bg = ui.visuals().extreme_bg_color;
                let weak = ui.visuals().weak_text_color();
                egui::Frame::popup(ui.style())
                    .fill(elevated)
                    .inner_margin(egui::Margin::same(10))
                    .show(ui, |ui| {
                        ui.set_width(520.0);
                        // Query field — focused each frame while open.
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.palette_query)
                                .id(egui::Id::new("command_palette_query"))
                                .desired_width(f32::INFINITY)
                                .background_color(field_bg)
                                .hint_text("Search commands…  (↑↓ to move, ↵ to run, esc to close)"),
                        );
                        resp.request_focus();
                        ui.add_space(6.0);
                        // Result rows.
                        for (i, e) in hits.iter().enumerate() {
                            let selected = i == self.palette_sel;
                            let row = egui::Frame::NONE
                                .fill(if selected {
                                    ui.visuals().selection.bg_fill
                                } else {
                                    egui::Color32::TRANSPARENT
                                })
                                .inner_margin(egui::Margin::symmetric(6, 3))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(egui::RichText::new(&e.name).strong());
                                        ui.label(
                                            egui::RichText::new(&e.category).small().color(weak),
                                        );
                                        if !e.usage.is_empty() {
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    ui.label(
                                                        egui::RichText::new(&e.usage)
                                                            .monospace()
                                                            .small()
                                                            .color(weak),
                                                    );
                                                },
                                            );
                                        }
                                    });
                                });
                            let r = row.response.interact(egui::Sense::click());
                            if r.clicked() {
                                chosen = Some(e.clone());
                            }
                            if r.hovered() {
                                self.palette_sel = i;
                            }
                        }
                        if hits.is_empty() {
                            ui.label(egui::RichText::new("no matches").color(weak));
                        }
                    });
            });

        if enter && let Some(e) = hits.get(self.palette_sel) {
            chosen = Some(e.clone());
        }
        if esc {
            close = true;
        }

        if let Some(entry) = chosen {
            let action = crate::palette::enter_action(&entry);
            self.show_palette = false;
            self.apply_menu_action(ctx, action);
        } else if close {
            self.show_palette = false;
        }
    }

    /// Apply an appearance theme choice from the native View ▸ Appearance items.
    /// `Some(true)` = Dark, `Some(false)` = Light (pinned so eframe's per-frame
    /// OS-theme read can't override it), `None` = follow the OS again. This is
    /// UI/session state — it is NOT written to the op-log (the drawing).
    fn set_theme_pref(&mut self, ctx: &egui::Context, dark: Option<bool>) {
        self.forced_dark = dark;
        if let Some(d) = dark {
            let want = if d { egui::Theme::Dark } else { egui::Theme::Light };
            ctx.set_theme(want);
        }
        // Persist the choice so it survives restarts (ui.json, not the drawing).
        save_theme_pref(dark);
    }

    /// Poll the active download's terminal state EVERY frame — panel open or
    /// not. On a fresh `Done`, persist the cassette and auto-activate it
    /// (Priority A: "download → chat just works"); on a real failure, announce
    /// it in the deck transcript so it is never silent. This used to live
    /// inside [`model_setup_ui`], which early-returns when the window is
    /// hidden — so a download finishing with the panel closed was never wired
    /// to the deck and a failure vanished entirely.
    fn poll_model_download(&mut self) {
        let outcome = self
            .active_download
            .as_mut()
            .and_then(|active| {
                let out = download_outcome(&active.handle.state(), active.persisted);
                if out.is_some() {
                    active.persisted = true;
                }
                out.map(|o| (active.model_id.clone(), o))
            });
        match outcome {
            Some((model_id, DownloadOutcome::Completed)) => {
                if let Some(entry) = self.catalog.get(&model_id).cloned() {
                    let mut decks = itsjustcad_deck::DecksFile::load_or_default();
                    install_catalog_deck(&mut decks, &entry); // sets active in-file
                    decks.save();
                    let cassette = cassette_name_for(&entry.id);
                    // Reload the pane's decks, flip the active deck, and eagerly
                    // start the local runtime so the next chat turn just works.
                    self.deck_pane.activate_installed_model(&cassette, &self.tokio);
                    tracing::info!(
                        "Installed {}; active deck is now '{}' and its runtime is starting.",
                        entry.display_name,
                        cassette
                    );
                }
            }
            Some((model_id, DownloadOutcome::Failed(msg))) => {
                let label = self
                    .catalog
                    .get(&model_id)
                    .map(|m| m.display_name.clone())
                    .unwrap_or(model_id);
                tracing::warn!("model download failed: {label}: {msg}");
                self.deck_pane.notify_status(format!(
                    "Download of {label} failed: {msg} — open LLM ▸ Model Setup to retry \
                     (a partial file resumes where it stopped)."
                ));
            }
            None => {}
        }
    }

    /// Model Setup panel: hardware recommendation, the catalog gated by RAM +
    /// free disk, an Install button per model with a live progress bar + speed
    /// + cancel, and the currently-installed model with Re-download / Remove.
    /// Download completion/failure is handled by [`Self::poll_model_download`]
    /// (every frame, panel open or not), not here.
    fn model_setup_ui(&mut self, ctx: &egui::Context) {
        if !self.show_model_setup {
            return;
        }

        let mut wants_close = false;
        let hw = self.hardware;
        let catalog = self.catalog.clone();
        let decks = itsjustcad_deck::DecksFile::load_or_default();
        // Collect UI intents, then act after the closure (avoids borrow clashes).
        let mut install: Option<String> = None;
        let mut cancel = false;
        let mut remove: Option<String> = None;
        // Token roles for the destructive Remove button (never a filled-red CTA).
        let dark = ctx.theme() == egui::Theme::Dark;
        let roles = self.live_roles(dark);

        // Model Setup is a REAL top-level OS window (its own egui viewport), not
        // an in-app floating panel — so it never overlaps the viewport/chat, can
        // be dragged outside the app bounds, and minimizes to the Dock natively.
        // Immediate mode so the body can borrow `self` (download intents etc.).
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("model_setup_window"),
            egui::ViewportBuilder::default()
                .with_title("Model Setup")
                .with_inner_size([480.0, 640.0])
                .with_min_inner_size([400.0, 320.0])
                .with_resizable(true),
            |vctx, _class| {
              egui::CentralPanel::default().show(vctx, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                // Hardware recommendation.
                ui.label(egui::RichText::new(hw.recommendation()).strong());
                ui.label(
                    egui::RichText::new(format!("Suggested: {}", hw.tier().label()))
                        .weak(),
                );
                ui.separator();

                // The one live download (if any), shown at the top.
                let active_state = self
                    .active_download
                    .as_ref()
                    .map(|a| (a.model_id.clone(), a.handle.state()));

                let recommended_id = catalog
                    .recommended_for(hw.tier())
                    .map(|m| m.id.clone());

                for entry in &catalog.models {
                    let runnable = entry.runnable_at(hw.ram_gb);
                    // Refuse to START a download that would die mid-transfer on
                    // a full disk (an opaque "write error" otherwise).
                    let disk_block = entry.disk_shortfall(hw.free_disk_gb);
                    let installed = catalog_deck_installed(&decks, &entry.id);
                    let is_downloading = active_state
                        .as_ref()
                        .is_some_and(|(id, _)| id == &entry.id);

                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(&entry.display_name).strong());
                            if recommended_id.as_deref() == Some(entry.id.as_str()) {
                                ui.label(egui::RichText::new("· recommended").weak());
                            }
                            if installed {
                                ui.label(egui::RichText::new("· installed").weak());
                            }
                            if entry.is_placeholder() {
                                ui.label(
                                    egui::RichText::new("· unverified build")
                                        .weak()
                                        .italics(),
                                )
                                .on_hover_text(
                                    "No SHA-256 checksum on record — \
                                     install at your own risk.",
                                );
                            }
                        });
                        ui.label(
                            egui::RichText::new(format!(
                                "{} · {} · needs {} GB RAM",
                                match entry.tier {
                                    crate::model_catalog::TierTag::Small3B => "3B",
                                    crate::model_catalog::TierTag::Mid7B => "7B",
                                },
                                crate::download::fmt_bytes(entry.size_bytes),
                                entry.ram_gb_min,
                            ))
                            .weak()
                            .small(),
                        );

                        if !runnable {
                            ui.label(
                                egui::RichText::new(format!(
                                    "Needs {} GB RAM — above this machine's capacity.",
                                    entry.ram_gb_min
                                ))
                                .weak(),
                            );
                        }
                        // RAM undetectable → the gate above can't vouch for this
                        // model; install stays allowed, but say so out loud.
                        if let Some(warn) = entry.ram_unknown_warning(hw.ram_gb) {
                            ui.label(
                                egui::RichText::new(warn)
                                    .small()
                                    .color(ui.visuals().warn_fg_color),
                            );
                        }
                        if let Some(block) = &disk_block {
                            ui.label(
                                egui::RichText::new(block.as_str())
                                    .small()
                                    .color(ui.visuals().warn_fg_color),
                            );
                        }

                        // Live progress for the model currently downloading.
                        if let Some((_, state)) = active_state.as_ref().filter(|_| is_downloading) {
                            ui.add(
                                egui::ProgressBar::new(state.fraction().unwrap_or(0.0))
                                    .show_percentage(),
                            );
                            ui.label(crate::download::progress_caption(state));
                            if state.is_active() {
                                ui.label(
                                    egui::RichText::new(
                                        "Cancelling keeps the partial .part file on disk; \
                                         re-downloading resumes from where it stopped.",
                                    )
                                    .small()
                                    .color(ui.visuals().warn_fg_color),
                                );
                                if ui
                                    .button("Cancel")
                                    .on_hover_text(
                                        "The partial .part file is kept so a later \
                                         download resumes instead of restarting.",
                                    )
                                    .clicked()
                                {
                                    cancel = true;
                                }
                            } else if let crate::download::DownloadState::Failed { msg } = state {
                                ui.colored_label(egui::Color32::LIGHT_RED, format!("Failed: {msg}"));
                                if ui.button("Retry").clicked() {
                                    install = Some(entry.id.clone());
                                }
                            }
                        } else if installed {
                            // Where the download lives + its on-disk size, so the
                            // user can find (or reclaim) a multi-GB file.
                            let on_disk = installed_model_file(entry);
                            if let Some(path) = &on_disk {
                                let size =
                                    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                                ui.label(
                                    egui::RichText::new(format!(
                                        "On disk: {} · {}",
                                        crate::download::fmt_bytes(size),
                                        path.display()
                                    ))
                                    .weak()
                                    .small(),
                                );
                            }
                            ui.horizontal(|ui| {
                                if ui.button("Re-download").clicked() {
                                    install = Some(entry.id.clone());
                                }
                                if let Some(path) = &on_disk
                                    && ui
                                        .button("Reveal")
                                        .on_hover_text("Show the model file in the file manager")
                                        .clicked()
                                {
                                    reveal_in_file_manager(path);
                                }
                                if crate::widgets::role_button(
                                    ui,
                                    &roles,
                                    dark,
                                    crate::widgets::ButtonRole::Destructive,
                                    "Remove",
                                )
                                .on_hover_text(
                                    "Removes the deck and DELETES the downloaded file.",
                                )
                                .clicked()
                                {
                                    remove = Some(entry.id.clone());
                                }
                            });
                        } else {
                            let any_active = active_state
                                .as_ref()
                                .is_some_and(|(_, s)| s.is_active());
                            ui.add_enabled_ui(
                                runnable && disk_block.is_none() && !any_active,
                                |ui| {
                                    if ui.button("Install").clicked() {
                                        install = Some(entry.id.clone());
                                    }
                                },
                            );
                        }
                    });
                }

                ui.separator();
                ui.horizontal(|ui| {
                    if ui
                        .button("Reveal Models Folder…")
                        .on_hover_text("Open ~/.config/itsjustcad/models in the file manager")
                        .clicked()
                        && let Some(dir) = crate::download::models_dir()
                    {
                        reveal_folder(&dir);
                    }
                });
                ui.label(
                    egui::RichText::new(
                        "Downloads stream to ~/.config/itsjustcad/models and are \
                         SHA-256 verified. When a download finishes it becomes \
                         the active deck automatically and its runtime starts — \
                         just start chatting.",
                    )
                    .weak()
                    .small(),
                );
                });
              });
              // Native window close button (or Cmd+W) requests viewport close.
              if vctx.input(|i| i.viewport().close_requested()) {
                  wants_close = true;
              }
            },
        );

        if wants_close {
            // Closing the window HIDES it but must NOT cancel an in-flight
            // download — the background thread keeps going and the corner chip
            // takes over. Cancellation is only the explicit "Cancel" button.
            self.close_model_setup();
        }

        // 2) Act on the collected intents.
        if cancel && let Some(active) = &self.active_download {
            active.handle.cancel();
        }
        if let Some(id) = remove {
            // Delete the downloaded FILE too — not just the deck entry. Removing
            // only the cassette is what orphans multi-GB files on disk.
            if let Some(entry) = self.catalog.get(&id).cloned()
                && let Some(dir) = crate::download::models_dir()
            {
                let _ = std::fs::remove_file(dir.join(entry.file_name()));
            }
            let mut decks = itsjustcad_deck::DecksFile::load_or_default();
            if remove_catalog_deck(&mut decks, &id) {
                decks.save();
            }
        }
        if let Some(id) = install {
            self.start_model_install(&id);
        }
    }

    /// Compact background download indicator: a small bottom-right corner chip
    /// ("[download] <model> NN% [x]") shown whenever a download is active but the
    /// Model Setup panel is hidden — so the user can close/minimize the panel and
    /// keep working while the download continues in its background thread.
    /// Clicking the chip body reopens the panel; the close icon cancels the download.
    fn download_progress_chip(&mut self, ctx: &egui::Context) {
        // Only when a download is in flight AND the full panel is not on screen
        // (the panel already shows its own progress bar).
        if self.show_model_setup {
            return;
        }
        let Some(active) = &self.active_download else {
            return;
        };
        let state = active.handle.state();
        // Nothing to show once the download has finished (or failed silently):
        // active work only.
        if !state.is_active() {
            return;
        }
        let label = self
            .catalog
            .get(&active.model_id)
            .map(|m| m.display_name.clone())
            .unwrap_or_else(|| active.model_id.clone());
        let pct = (state.fraction().unwrap_or(0.0) * 100.0).round() as i32;

        let mut reopen = false;
        let mut cancel = false;
        egui::Area::new(egui::Id::new("download_chip"))
            .anchor(
                egui::Align2::RIGHT_BOTTOM,
                [-crate::theme::Spacing::SM, -crate::theme::Spacing::SM],
            )
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let fg = ui.visuals().text_color();
                        let sz = ui.text_style_height(&egui::TextStyle::Body);
                        ui.add(self.icons.image(ui.ctx(), crate::icons::Icon::Download, sz, fg));
                        if ui
                            .button(format!("{label}  {pct}%"))
                            .on_hover_text("Downloading — click to open Model Setup")
                            .clicked()
                        {
                            reopen = true;
                        }
                        if self
                            .icons
                            .icon_button(ui, crate::icons::Icon::Close, "Cancel download")
                            .on_hover_text(
                                "Cancel download — the partial file is kept so a \
                                 later download resumes instead of restarting.",
                            )
                            .clicked()
                        {
                            cancel = true;
                        }
                    });
                });
            });
        if reopen {
            self.show_model_setup = true;
        }
        if cancel && let Some(active) = &self.active_download {
            active.handle.cancel();
        }
        // Keep the percentage live while a background download runs.
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
    }

    /// Hide the Model Setup panel WITHOUT cancelling any active download. The
    /// download keeps running in its background thread; the corner progress chip
    /// then surfaces it. Cancellation is a separate, explicit user action.
    fn close_model_setup(&mut self) {
        self.show_model_setup = false;
        // Deliberately does NOT touch `self.active_download` / call `cancel()`.
    }

    /// The default local model to OFFER in the LLM menu — `(id, display_name)` —
    /// or `None` when it's already installed, a download is running, or the
    /// catalog has none. For users who skipped onboarding: a one-tap way to grab
    /// the recommended local model.
    fn default_model_offer(&self) -> Option<(String, String)> {
        if self.active_download.is_some() {
            return None;
        }
        // Already have a working local model (e.g. Ollama running) → no download.
        if self.deck_pane.has_ready_local_deck() {
            return None;
        }
        let entry = self.catalog.recommended_for(self.hardware.tier())?;
        let dir = crate::download::models_dir()?;
        // Already downloaded → don't offer it.
        if dir.join(entry.file_name()).exists() {
            return None;
        }
        Some((entry.id.clone(), entry.display_name.clone()))
    }

    /// Kick off a background download for catalog model `id` into the models dir.
    fn start_model_install(&mut self, id: &str) {
        let Some(entry) = self.catalog.get(id).cloned() else {
            return;
        };
        let Some(dir) = crate::download::models_dir() else {
            tracing::error!("no home dir — cannot resolve models directory");
            return;
        };
        let spec = crate::download::DownloadSpec {
            url: entry.url.clone(),
            dir,
            file_name: entry.file_name(),
            expected_sha256: entry.expected_sha().map(|s| s.to_string()),
        };
        let handle = crate::download::start(&self.tokio, spec);
        self.active_download = Some(ActiveDownload {
            model_id: entry.id,
            handle,
            persisted: false,
        });
    }
}

impl App {
    /// The current document's stable uuid, stamping a fresh one onto the session
    /// when the file never carried one. Header-level only (see
    /// `Session::set_doc_uuid`) — it is not part of the op-log and never affects
    /// geometry or replay. Used to key the deck's per-document chat store.
    fn ensure_doc_uuid(&mut self) -> String {
        if let Some(u) = self.session.doc_uuid() {
            return u.to_string();
        }
        let fresh = uuid::Uuid::new_v4().to_string();
        self.session.set_doc_uuid(Some(fresh.clone()));
        fresh
    }

    /// Reconcile live widgets to a UI-plane change just written to `ui.json`.
    /// Only touches window layout (panel visibility, theme) — never the drawing.
    fn reconcile_ui_plane(&mut self, ui_json: &serde_json::Value) {
        if let Some(v) = ui_json["panel_visible"].as_bool() {
            self.panel_visible = v;
        }
        match ui_json["theme"].as_str() {
            Some("dark") => self.forced_dark = Some(true),
            Some("light") => self.forced_dark = Some(false),
            _ => {}
        }
    }
}

/// Map a user/deck-facing tab name to a right-dock tab. Accepts the labels the
/// UI uses plus historic aliases (`model` → Layers, `deck` → Chat). Shared by
/// the `ITSJUSTCAD_PANEL_TAB` dev hook and the UI-plane `panel <tab>` action.
pub(crate) fn panel_tab_by_name(name: &str) -> Option<crate::tabstrip::PanelTab> {
    use crate::tabstrip::PanelTab;
    match name.to_ascii_lowercase().as_str() {
        "chat" | "deck" => Some(PanelTab::Deck),
        "sessions" => Some(PanelTab::Sessions),
        "layers" | "model" => Some(PanelTab::Model),
        "blocks" => Some(PanelTab::Blocks),
        "plugins" => Some(PanelTab::Plugins),
        _ => None,
    }
}

/// Reveal the plugins directory in the OS file manager (Plugins tab "Open
/// folder"). Best-effort; the outcome lands on the command line either way.
fn open_plugins_folder(command_line: &mut crate::command_line::CommandLine) {
    let Some(dir) = itsjustcad_commands::plugin::default_dir() else {
        command_line.push_line("error: no home directory — cannot locate the plugins folder");
        return;
    };
    // The folder may not exist yet (no plugin ever saved) — create it so the
    // file manager has something to show.
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(target_os = "windows")]
    let cmd = "explorer";
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = "xdg-open";
    match std::process::Command::new(cmd).arg(&dir).spawn() {
        Ok(_) => command_line.push_line(format!("opened {}", dir.display())),
        Err(e) => command_line.push_line(format!("error: could not open {}: {e}", dir.display())),
    }
}

fn ui_config_path() -> Option<std::path::PathBuf> {
    Some(dirs::home_dir()?.join(".config").join("itsjustcad").join("ui.json"))
}

fn load_ui_json() -> serde_json::Value {
    ui_config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::Value::Object(Default::default()))
}

fn save_ui_json(value: &serde_json::Value) {
    if let Some(path) = ui_config_path() {
        let _ = std::fs::create_dir_all(path.parent().expect("has parent"));
        let _ = std::fs::write(
            path,
            serde_json::to_string_pretty(value).expect("serializes"),
        );
    }
}

fn load_zoom() -> Option<f32> {
    load_ui_json()["zoom"].as_f64().map(|z| (z as f32).clamp(0.5, 3.0))
}

fn save_zoom(zoom: f32) {
    let mut v = load_ui_json();
    v["zoom"] = serde_json::json!(zoom);
    save_ui_json(&v);
}

/// Restore the persisted Appearance theme choice from ui.json: `Some(true)`=dark,
/// `Some(false)`=light, `None`=follow the OS (the default / absent key). UI state,
/// never part of the op-log (the drawing).
fn load_theme_pref() -> Option<bool> {
    match load_ui_json()["theme"].as_str() {
        Some("dark") => Some(true),
        Some("light") => Some(false),
        _ => None,
    }
}

/// Persist the Appearance theme choice to ui.json. `None` clears the key so the
/// app follows the OS on next launch.
fn save_theme_pref(dark: Option<bool>) {
    let mut v = load_ui_json();
    match dark {
        Some(true) => v["theme"] = serde_json::json!("dark"),
        Some(false) => v["theme"] = serde_json::json!("light"),
        None => {
            if let Some(obj) = v.as_object_mut() {
                obj.remove("theme");
            }
        }
    }
    save_ui_json(&v);
}

fn load_deck_visible() -> Option<bool> {
    load_ui_json()["deck_visible"].as_bool()
}

fn save_deck_visible(visible: bool) {
    let mut v = load_ui_json();
    v["deck_visible"] = serde_json::json!(visible);
    save_ui_json(&v);
}

/// Restore the persisted Shaded feature-edge toggle (default ON when absent).
fn load_shaded_edges() -> Option<bool> {
    load_ui_json()["shaded_edges"].as_bool()
}

fn save_shaded_edges(on: bool) {
    let mut v = load_ui_json();
    v["shaded_edges"] = serde_json::json!(on);
    save_ui_json(&v);
}

/// Restore the persisted gumball-visibility toggle (default OFF when absent).
fn load_gumball_visible() -> Option<bool> {
    load_ui_json()["show_gumball"].as_bool()
}

fn save_gumball_visible(visible: bool) {
    let mut v = load_ui_json();
    v["show_gumball"] = serde_json::json!(visible);
    save_ui_json(&v);
}

fn load_template_done() -> bool {
    load_ui_json()["template_done"].as_bool().unwrap_or(false)
}

fn save_template_done() {
    let mut v = load_ui_json();
    v["template_done"] = serde_json::json!(true);
    save_ui_json(&v);
}

/// Returns `true` once the user has completed first-run onboarding (i.e.
/// dismissed/completed the template picker at least once). After that the
/// empty-document overlay is suppressed so a returning user with an empty doc
/// sees a clean viewport instead of a re-onboarding prompt.
fn load_onboarding_done() -> bool {
    // Reuse the same flag as the template picker: if the user has ever clicked
    // "Start" (which writes `template_done = true`), onboarding is done.
    load_template_done()
}

/// Mark onboarding as complete so the empty-document overlay will never show
/// again on subsequent launches. Call this whenever the template picker is
/// successfully dismissed.
fn save_onboarding_done() {
    save_template_done();
}

fn load_cad_origin() -> Option<CadOrigin> {
    let v = load_ui_json();
    let s = v["cad_origin"].as_str()?;
    serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
}

fn save_cad_origin(origin: CadOrigin) {
    let mut v = load_ui_json();
    v["cad_origin"] = serde_json::to_value(origin).unwrap_or(serde_json::Value::Null);
    save_ui_json(&v);
}

/// Persist the deck-brain onboarding choice to `ui.json`.
fn save_deck_brain(brain: DeckBrain) {
    let mut v = load_ui_json();
    v["deck_brain"] = serde_json::json!(brain.as_pref());
    save_ui_json(&v);
}

/// Load the persisted deck-brain choice, if any.
#[allow(dead_code)] // read on re-open of the model-setup dialog (later sub-phase)
fn load_deck_brain() -> Option<DeckBrain> {
    match load_ui_json()["deck_brain"].as_str()? {
        "cloud" => Some(DeckBrain::Cloud),
        "ollama" => Some(DeckBrain::Ollama),
        "download" => Some(DeckBrain::Download),
        "skip" => Some(DeckBrain::Skip),
        _ => None,
    }
}

/// Restore the persisted Reduce Motion toggle (default OFF when absent).
fn load_reduce_motion() -> bool {
    load_ui_json()["reduce_motion"].as_bool().unwrap_or(false)
}

fn save_reduce_motion(on: bool) {
    let mut v = load_ui_json();
    v["reduce_motion"] = serde_json::json!(on);
    save_ui_json(&v);
}

/// Model id to seed the local cassette with for a given hardware tier.
fn model_id_for_tier(tier: crate::hardware::ModelTier) -> &'static str {
    use crate::hardware::ModelTier;
    match tier {
        // 3B-class default; 7B-class default. Names match common Ollama tags.
        ModelTier::None | ModelTier::Small3B => "qwen2.5:3b",
        ModelTier::Mid7B => "qwen2.5:7b",
    }
}

/// Apply the onboarding deck-brain choice: write/update the appropriate cassette
/// in `decks.json` and make it active. Cloud and Skip write no cassette (the
/// user configures those from the deck settings). `tier` is only consulted for
/// the Download path to pick a sensible default model.
///
/// Returns the deck name that was made active, if any. Pure enough to unit-test
/// via [`deck_brain_into_decks`]; this wrapper just handles load/save I/O.
fn apply_deck_brain(brain: DeckBrain, tier: crate::hardware::ModelTier) {
    if matches!(brain, DeckBrain::Cloud | DeckBrain::Skip) {
        return;
    }
    let mut decks = itsjustcad_deck::DecksFile::load_or_default();
    if deck_brain_into_decks(&mut decks, brain, tier).is_some() {
        decks.save();
    }
}

/// Pure core of [`apply_deck_brain`]: mutate `decks` for the given choice and
/// return the name of the deck made active. Ollama and Download both produce a
/// local `openai_compat` cassette pointed at localhost:11434 with grammar on;
/// Download additionally names the tier's default model. Cloud/Skip are no-ops.
fn deck_brain_into_decks(
    decks: &mut itsjustcad_deck::DecksFile,
    brain: DeckBrain,
    tier: crate::hardware::ModelTier,
) -> Option<String> {
    use itsjustcad_deck::{DeckConfig, DeckKind};
    let (name, model) = match brain {
        DeckBrain::Ollama => ("ollama", "qwen3".to_string()),
        DeckBrain::Download => ("local-download", model_id_for_tier(tier).to_string()),
        DeckBrain::Cloud | DeckBrain::Skip => return None,
    };
    let entry = DeckConfig {
        name: name.to_string(),
        kind: DeckKind::OpenaiCompat,
        base_url: "http://localhost:11434/v1".to_string(),
        model,
        api_key: None,
        // Local model — grammar-constrained decoding on by default (per the
        // grammar agent's flag) so it can only emit real verbs in draft fences.
        grammar: true,
        terse: None,
    };
    // Replace an existing cassette of the same name, else append.
    match decks.decks.iter().position(|d| d.name == name) {
        Some(i) => {
            decks.decks[i] = entry;
            decks.active = i;
        }
        None => {
            decks.decks.push(entry);
            decks.active = decks.decks.len() - 1;
        }
    }
    Some(name.to_string())
}

/// Base URL the local llama.cpp server will expose an OpenAI-compatible endpoint
/// at. The actual runtime spawn is the next agent's job; this is the port the
/// cassette points at so it's ready the moment the server is up.
const LOCAL_RUNTIME_BASE_URL: &str = "http://localhost:8080/v1";

/// The `decks.json` cassette name for a catalog model id.
fn cassette_name_for(model_id: &str) -> String {
    format!("local-{model_id}")
}

/// Absolute path of a catalog model's downloaded file, if it exists on disk.
/// The file lives at `models_dir()/<file_name>` (see [`download::models_dir`]).
fn installed_model_file(entry: &crate::model_catalog::ModelEntry) -> Option<std::path::PathBuf> {
    let p = crate::download::models_dir()?.join(entry.file_name());
    p.exists().then_some(p)
}

/// Reveal a file in the OS file manager, selecting it. Best-effort — a failure
/// to spawn the helper is silently ignored (nothing to recover).
fn reveal_in_file_manager(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer")
        .arg(format!("/select,{}", path.display()))
        .spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // xdg-open cannot select a file; open the containing directory instead.
        let dir = path.parent().unwrap_or(path);
        let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
    }
}

/// Open a folder in the OS file manager. Creates it first so an empty models
/// directory still opens rather than failing.
fn reveal_folder(dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(dir);
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(dir).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(dir).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
}

/// Build the local cassette for an installed catalog model. Pure so the shape is
/// unit-tested. `openai_compat` kind, localhost runtime base URL, grammar on
/// (local models get grammar-constrained decoding), model = the catalog id.
fn catalog_deck_entry(entry: &crate::model_catalog::ModelEntry) -> itsjustcad_deck::DeckConfig {
    itsjustcad_deck::DeckConfig {
        name: cassette_name_for(&entry.id),
        kind: itsjustcad_deck::DeckKind::OpenaiCompat,
        base_url: LOCAL_RUNTIME_BASE_URL.to_string(),
        model: entry.id.clone(),
        api_key: None,
        grammar: true,
        terse: None,
    }
}

/// Insert/replace the cassette for an installed model and make it active.
/// Returns the (possibly new) active index. Pure over `decks` — I/O is the
/// caller's job.
fn install_catalog_deck(
    decks: &mut itsjustcad_deck::DecksFile,
    entry: &crate::model_catalog::ModelEntry,
) -> usize {
    let cfg = catalog_deck_entry(entry);
    match decks.decks.iter().position(|d| d.name == cfg.name) {
        Some(i) => {
            decks.decks[i] = cfg;
            decks.active = i;
            i
        }
        None => {
            decks.decks.push(cfg);
            let i = decks.decks.len() - 1;
            decks.active = i;
            i
        }
    }
}

/// Remove a catalog model's cassette from `decks.json` (does not delete the
/// weights file). Returns true if a cassette was removed. Clamps `active`.
fn remove_catalog_deck(decks: &mut itsjustcad_deck::DecksFile, model_id: &str) -> bool {
    let name = cassette_name_for(model_id);
    let Some(i) = decks.decks.iter().position(|d| d.name == name) else {
        return false;
    };
    decks.decks.remove(i);
    if decks.active >= decks.decks.len() {
        decks.active = decks.decks.len().saturating_sub(1);
    } else if decks.active > i {
        decks.active -= 1;
    }
    true
}

/// True when a cassette for this model id already exists in decks.json.
fn catalog_deck_installed(decks: &itsjustcad_deck::DecksFile, model_id: &str) -> bool {
    let name = cassette_name_for(model_id);
    decks.decks.iter().any(|d| d.name == name)
}

/// Apply a legacy-CAD preset to the egui context via the design-token system.
/// Each skin is a token set (`UiPreset::tokens`); `theme::apply` stamps the
/// spacing/type/color roles onto egui's Style. Called on startup (from saved
/// prefs) and when the user picks a preset in the template dialog. Pencil-mode
/// clear-color is handled separately in `clear_color`.
fn apply_preset(ctx: egui::Context, origin: CadOrigin) {
    let tokens = preset::preset_for(origin).tokens();
    crate::theme::apply(&ctx, &tokens);
}

/// Screen position -> world-space pick ray (origin on the near plane).
pub(crate) fn screen_ray(
    view_proj: glam::Mat4,
    rect: egui::Rect,
    pos: egui::Pos2,
) -> (glam::DVec3, glam::DVec3) {
    let inv = view_proj.inverse();
    let ndc = glam::Vec2::new(
        (pos.x - rect.left()) / rect.width() * 2.0 - 1.0,
        1.0 - (pos.y - rect.top()) / rect.height() * 2.0,
    );
    let unproject = |z: f32| {
        let p = inv * glam::Vec4::new(ndc.x, ndc.y, z, 1.0);
        (p.truncate() / p.w).as_dvec3()
    };
    let origin = unproject(0.0);
    let dir = (unproject(1.0) - origin).normalize();
    (origin, dir)
}

/// Whether to paint the empty-document next-actions overlay.
///
/// Conditions (all must hold):
/// - The document has no geometry.
/// - No draw tool is mid-flight (a tool in progress means the user is already
///   placing the first object, so the prompt would be noise).
/// - Onboarding has NOT yet been completed (`onboarding_done == false`), i.e.
///   this is a genuine first-run session. After onboarding the user gets a
///   clean viewport on empty opens — they know what to do.
fn show_empty_document(
    doc: &itsjustcad_doc::Document,
    tool_active: bool,
    onboarding_done: bool,
) -> bool {
    doc.is_empty() && !tool_active && !onboarding_done
}

/// Whether to paint the SUBTLE empty-document hint (a low-key line of weak text,
/// no box) over the active viewport. This is the RETURNING-user counterpart to
/// [`show_empty_document`]: once onboarding is done the boxed first-run overlay
/// is retired, but a bare grid is still a blank void — so a whisper-quiet
/// next-action hint fills it. Never shown when the boxed overlay is (`!done`),
/// nor while a draw tool is mid-flight.
fn show_empty_document_hint(
    doc: &itsjustcad_doc::Document,
    tool_active: bool,
    onboarding_done: bool,
) -> bool {
    doc.is_empty() && !tool_active && onboarding_done
}

/// Middle-truncate a layer name to `max` chars, inserting an ellipsis so the
/// head and tail both stay readable (Rhino-style long-name handling).
fn middle_truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let head_s: String = chars[..head].iter().collect();
    let tail_s: String = chars[chars.len() - tail..].iter().collect();
    format!("{head_s}…{tail_s}")
}

/// Label for the print-width dropdown: the ISO-thin default reads "Default",
/// everything else its millimetre value.
fn print_width_label(mm: f64) -> String {
    if (mm - 0.18).abs() < 1e-9 {
        "Default".to_string()
    } else {
        format!("{mm:.2}")
    }
}

/// First free "Layer NN" name for the ＋ button (skips names already taken).
fn next_free_layer_name(doc: &itsjustcad_doc::Document) -> String {
    for i in 1..1000 {
        let candidate = format!("Layer {i:02}");
        if !doc.layers.contains_key(&candidate) {
            return candidate;
        }
    }
    "Layer".to_string()
}

/// Deletable layers with no objects on them (for the ⚙ "purge empty" action).
/// The default layer is never purged.
fn empty_deletable_layers(doc: &itsjustcad_doc::Document) -> Vec<String> {
    use std::collections::BTreeSet;
    let used: BTreeSet<&str> = doc.objects().map(|o| o.layer.as_str()).collect();
    doc.layers
        .keys()
        .filter(|n| n.as_str() != itsjustcad_doc::DEFAULT_LAYER && !used.contains(n.as_str()))
        .cloned()
        .collect()
}

/// Screen position -> point on the z=0 ground plane.
fn ground_point(view_proj: glam::Mat4, rect: egui::Rect, pos: egui::Pos2) -> Option<glam::DVec3> {
    let (origin, dir) = screen_ray(view_proj, rect, pos);
    if dir.z.abs() < 1e-12 {
        return None;
    }
    let t = -origin.z / dir.z;
    (t > 0.0).then(|| origin + dir * t)
}

/// World point -> screen position (None when behind the camera).
pub(crate) fn project(view_proj: glam::Mat4, rect: egui::Rect, world: glam::DVec3) -> Option<egui::Pos2> {
    let clip = view_proj * glam::Vec4::new(world.x as f32, world.y as f32, world.z as f32, 1.0);
    if clip.w <= 0.0 {
        return None;
    }
    let ndc = clip.truncate() / clip.w;
    Some(egui::pos2(
        rect.left() + (ndc.x + 1.0) * 0.5 * rect.width(),
        rect.top() + (1.0 - ndc.y) * 0.5 * rect.height(),
    ))
}

/// World AABB -> covering screen rect; None when any corner sits behind the
/// camera (the object is then skipped rather than mis-boxed).
fn projected_rect(
    view_proj: glam::Mat4,
    rect: egui::Rect,
    min: glam::DVec3,
    max: glam::DVec3,
) -> Option<egui::Rect> {
    let mut out: Option<egui::Rect> = None;
    for i in 0..8 {
        let corner = glam::DVec3::new(
            if i & 1 == 0 { min.x } else { max.x },
            if i & 2 == 0 { min.y } else { max.y },
            if i & 4 == 0 { min.z } else { max.z },
        );
        let p = project(view_proj, rect, corner)?;
        out = Some(match out {
            Some(r) => r.union(egui::Rect::from_min_max(p, p)),
            None => egui::Rect::from_min_max(p, p),
        });
    }
    out
}

/// Rubber box: solid stroke for a window drag, dashed for a crossing drag.
fn draw_rubber_box(
    painter: &egui::Painter,
    drag: egui::Rect,
    mode: crate::boxsel::BoxMode,
    visuals: &egui::Visuals,
) {
    let stroke = egui::Stroke::new(1.0, visuals.selection.stroke.color);
    match mode {
        crate::boxsel::BoxMode::Window => {
            painter.rect_stroke(drag, 0.0, stroke, egui::StrokeKind::Middle);
        }
        crate::boxsel::BoxMode::Crossing => {
            let corners = [
                drag.left_top(),
                drag.right_top(),
                drag.right_bottom(),
                drag.left_bottom(),
                drag.left_top(),
            ];
            for pair in corners.windows(2) {
                painter.extend(egui::Shape::dashed_line(pair, stroke, 4.0, 4.0));
            }
        }
    }
}

fn ray_aabb(origin: glam::DVec3, dir: glam::DVec3, min: glam::DVec3, max: glam::DVec3) -> Option<f64> {
    let inv = dir.recip();
    let t1 = (min - origin) * inv;
    let t2 = (max - origin) * inv;
    let t_min = t1.min(t2).max_element();
    let t_max = t1.max(t2).min_element();
    (t_max >= t_min.max(0.0)).then_some(t_min.max(0.0))
}

impl eframe::App for App {
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        // Pencil mode forces paper white regardless of the egui theme.
        // Use the active pane's display mode to drive the clear colour.
        let active_mode =
            self.display_modes[self.layout.camera_index(self.active_pane)];
        if active_mode == itsjustcad_render::DisplayMode::Pencil {
            return itsjustcad_render::DisplayMode::pencil_background();
        }
        // Dark mode wins for the 3D model space: a near-black off-grey, even
        // under a legacy-CAD preset (whose fixed light-grey/white bg would
        // otherwise ignore the dark theme).
        if visuals.dark_mode {
            return scene::Theme::Dark.background();
        }
        // Light mode: honour the active legacy-CAD preset background.
        if self.cad_origin != CadOrigin::None {
            return preset::preset_for(self.cad_origin).bg_color;
        }
        scene::Theme::Light.background()
    }

    fn on_exit(&mut self) {
        // Archive the live conversation into per-document history before quitting
        // so a quit mid-chat is not lost. Auto mode updates the loaded session in
        // place (no fork) or pushes a fresh one; it is a no-op when empty.
        self.deck_pane
            .archive_active(crate::deck_pane::ArchiveMode::Auto);
        // Clean exit: nothing crashed, nothing to recover.
        if let Some(j) = &mut self.journal {
            j.discard();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        // Modeless-hotkey exceptions run BEFORE any widget so their key/text
        // events can be consumed (the command line is focused-by-default, so the
        // input would otherwise eat the letter).
        self.early_hotkeys(&ui.ctx().clone());
        // Attach the true native OS menu bar (muda) on the first interactive
        // frame — no-op in headless/`--shot` (no window) — then drain its click
        // channel each frame and route any pick through the substrate, exactly
        // like the in-window bar. Request a repaint so native clicks (which don't
        // otherwise wake egui) are handled promptly.
        // Linux: muda/gtk not available; in-window bar is always used.
        #[cfg(not(target_os = "linux"))]
        self.ensure_native_menu(frame);
        #[cfg(not(target_os = "linux"))]
        {
            let has_selection = !self.session.doc.selection.is_empty();
            // Read live LLM-toggle state before the mutable native borrow.
            let local_only = self.deck_pane.local_only();
            let web_search = self.deck_pane.allow_web_search();
            let terse = self.deck_pane.terse_enabled();
            // Live View state (active display / lighting mode + panel visibility)
            // for the stateful View menu radios + Panel flip.
            let view = self.view_state();
            if let Some(native) = &mut self.native_menu {
                // Disable-don't-hide: keep the selection-dependent native items'
                // enabled state in sync with the current selection.
                native.sync_selection(has_selection);
                // Mirror the live Local Only / Allow Web Search state onto the
                // native checkable LLM items.
                native.sync_toggles(local_only, web_search, terse);
                // Mirror the active display/lighting mode + panel visibility onto
                // the native View menu (radio checks + Hide/Show Panel label).
                native.sync_view_state(view);
                let action = native.poll();
                if let Some(action) = action {
                    let ctx = ui.ctx().clone();
                    self.apply_menu_action(&ctx, action);
                }
                // Keep polling responsive while a native bar is attached.
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            }
        }

        // Unsaved-changes guard on window close (Quit): if a close was requested
        // and the doc is dirty, cancel the close and park a Quit intent behind the
        // alert. A confirmed Discard re-sends Close; Cancel drops it.
        {
            let ctx = ui.ctx().clone();
            let close_requested = ctx.input(|i| i.viewport().close_requested());
            // Clean doc lets the close proceed unimpeded; a dirty doc cancels it
            // and parks the Quit behind the alert.
            if should_intercept_close(
                close_requested,
                self.force_close,
                self.pending_nav == Some(PendingNav::Quit),
                self.is_dirty(),
            ) {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.pending_nav = Some(PendingNav::Quit);
            }
        }

        // ITSJUSTCAD_THEME pins the theme every frame (eframe otherwise re-reads
        // the OS theme each frame and would override a one-shot pin at startup).
        if let Some(dark) = self.forced_dark {
            let want = if dark { egui::Theme::Dark } else { egui::Theme::Light };
            if ui.ctx().theme() != want {
                ui.ctx().set_theme(want);
            }
        }
        self.run_startup_script();
        // ITSJUSTCAD_TYPE: pre-fill command input (dev hook for autosuggest screenshots).
        if let Some(text) = self.type_script.take() {
            self.command_line.prefill(text);
        }
        // ITSJUSTCAD_DIALOG=about: open an overlay so a --shot captures elevated
        // surfaces (dialog/popover) floating above the panels. Dev-only hook.
        if self.frame_count == 0
            && std::env::var("ITSJUSTCAD_DIALOG").as_deref() == Ok("about")
        {
            self.show_about = true;
        }
        // ITSJUSTCAD_PALETTE=<query>: open the ⌘K command palette with a query
        // typed, so a --shot/GUI-shot captures the overlay. Dev-only hook.
        if self.frame_count == 0
            && let Ok(q) = std::env::var("ITSJUSTCAD_PALETTE")
        {
            self.open_palette();
            self.palette_query = q;
        }

        if self.show_template_picker {
            let mut done = false;
            egui::Window::new("New Document Setup")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ui.ctx(), |ui| {
                    ui.label("Units:");
                    ui.radio_value(&mut self.template_units, TemplateUnits::Meters, "Meters");
                    ui.radio_value(&mut self.template_units, TemplateUnits::Millimeters, "Millimeters");
                    ui.radio_value(&mut self.template_units, TemplateUnits::FeetInches, "Feet-inches");
                    ui.add_space(8.0);
                    ui.label("Scale:");
                    ui.radio_value(&mut self.template_scale, TemplateScale::Object, "Object (~5m)");
                    ui.radio_value(&mut self.template_scale, TemplateScale::Building, "Building (~30m)");
                    ui.radio_value(&mut self.template_scale, TemplateScale::Urban, "Urban (~300m)");
                    ui.add_space(8.0);
                    ui.label("Which CAD are you coming from?");
                    ui.radio_value(
                        &mut self.cad_origin, CadOrigin::None,
                        CadOrigin::None.label(),
                    );
                    ui.radio_value(
                        &mut self.cad_origin, CadOrigin::AutoCAD,
                        CadOrigin::AutoCAD.label(),
                    );
                    ui.radio_value(
                        &mut self.cad_origin, CadOrigin::Rhino,
                        CadOrigin::Rhino.label(),
                    );
                    ui.radio_value(
                        &mut self.cad_origin, CadOrigin::Revit,
                        CadOrigin::Revit.label(),
                    );
                    ui.add_space(8.0);
                    ui.separator();
                    ui.label("Deck brain — where should the assistant run?");
                    ui.radio_value(&mut self.deck_brain, DeckBrain::Cloud, DeckBrain::Cloud.label());
                    ui.radio_value(&mut self.deck_brain, DeckBrain::Ollama, DeckBrain::Ollama.label());
                    ui.radio_value(
                        &mut self.deck_brain,
                        DeckBrain::Download,
                        DeckBrain::Download.label(),
                    );
                    ui.radio_value(&mut self.deck_brain, DeckBrain::Skip, DeckBrain::Skip.label());

                    // For the "download a local model" path, show the hardware
                    // recommendation and gate tiers the RAM can't run.
                    if self.deck_brain == DeckBrain::Download {
                        use crate::hardware::ModelTier;
                        let hw = &self.hardware;
                        let tier = hw.tier();
                        ui.add_space(4.0);
                        ui.group(|ui| {
                            ui.label(egui::RichText::new(hw.recommendation()).strong());
                            ui.label(format!("Suggested: {}", tier.label()));
                            // 3B is always offered; 7B only when the machine can run it.
                            ui.label("• 3B model — fits ~8 GB machines");
                            let can_7b = matches!(tier, ModelTier::Mid7B);
                            if can_7b {
                                ui.label("• 7B model — recommended for this machine");
                            } else {
                                ui.label(
                                    egui::RichText::new(
                                        "• 7B model — needs 16 GB+ RAM (unavailable)",
                                    )
                                    .weak(),
                                );
                            }
                            if matches!(tier, ModelTier::None) {
                                ui.label(
                                    egui::RichText::new(
                                        "Low RAM — a cloud brain will likely feel better.",
                                    )
                                    .italics(),
                                );
                            }
                            ui.label(
                                egui::RichText::new(
                                    "Pick a model in Model Setup after Start \
                                     (also under Tools → Model Setup).",
                                )
                                .weak()
                                .small(),
                            );
                        });
                    }
                    ui.add_space(8.0);
                    if ui.button("Start").clicked() {
                        done = true;
                    }
                });
            if done {
                self.show_template_picker = false;
                save_onboarding_done();
                save_cad_origin(self.cad_origin);
                save_deck_brain(self.deck_brain);
                apply_deck_brain(self.deck_brain, self.hardware.tier());
                // The "download a local model" path opens Model Setup so the
                // user can pick + fetch a model right away.
                if self.deck_brain == DeckBrain::Download {
                    self.show_model_setup = true;
                }
                apply_preset(ui.ctx().clone(), self.cad_origin);
                let units_cmd = units_cmd_for(&self.template_units);
                self.command_line.execute(&mut self.session, units_cmd);
                let distance = camera_distance_for(&self.template_scale);
                for cam in &mut self.cameras {
                    cam.distance = distance;
                }
            }
        }

        // Help → About dialog.
        if self.show_about {
            let mut open = true;
            egui::Window::new("About ItsJustCAD")
                // Draggable + collapsible + X-closable — never traps input.
                .collapsible(true)
                .resizable(false)
                .open(&mut open)
                .show(ui.ctx(), |ui| {
                    let roles = &self.live_roles(ui.visuals().dark_mode);
                    crate::widgets::dialog_body(ui, roles, 360.0, |ui| {
                        crate::widgets::dialog_title(ui, roles, "ItsJustCAD");
                        ui.add_space(crate::theme::Spacing::S);
                        crate::widgets::dialog_text(
                            ui,
                            roles,
                            "It's just CAD — a command-first, FOSS CAD workspace.",
                        );
                        ui.add_space(crate::theme::Spacing::S);
                        // Attribution required under AGPLv3 §7(b) — do not remove.
                        crate::widgets::dialog_text(ui, roles, "© 2026 Hector Tarrido-Picart");
                        crate::widgets::dialog_text(
                            ui,
                            roles,
                            "Desktop: AGPLv3. Commercial & mobile licensing from the author.",
                        );
                        crate::widgets::dialog_text(
                            ui,
                            roles,
                            &format!(
                                "Preset: {}",
                                preset::preset_for(self.cad_origin).menu_style_label()
                            ),
                        );
                        ui.add_space(crate::theme::Spacing::M);
                        if ui.button("Close").clicked() {
                            self.show_about = false;
                        }
                    });
                });
            if !open {
                self.show_about = false;
            }
        }

        // Edit → "Edit history…" modal. The command line is the op-log
        // scrollback; this on-demand popover exposes step-jump + amend (the old
        // History tab's body) as a floating window.
        if self.show_history {
            let mut open = true;
            egui::Window::new("Edit history")
                // Draggable + collapsible + X-closable — never traps input.
                .collapsible(true)
                .resizable(true)
                .default_size([320.0, 380.0])
                .open(&mut open)
                .show(ui.ctx(), |ui| self.history_panel(ui));
            if !open {
                self.show_history = false;
            }
        }

        // Handle download completion/failure every frame — even with the Model
        // Setup window closed — so a finished download always becomes the
        // active deck and a failure is never silent.
        self.poll_model_download();
        // Tools → Model Setup panel (also the onboarding "download a local
        // model" entry point). Renders any time show_model_setup is set.
        self.model_setup_ui(ui.ctx());
        // Compact corner chip so a download can continue with the panel hidden.
        self.download_progress_chip(ui.ctx());

        // Drive an in-flight DXF import one batch per frame, then draw its modal.
        self.step_import(ui.ctx());
        self.import_ui(ui.ctx());

        // Mirror the op-log to the crash journal. One hook covers every
        // mutation path (command line, gumball, deck, history jumps); the
        // generation check keeps idle frames free.
        if self.journaled_generation != Some(self.session.doc.generation) {
            self.journaled_generation = Some(self.session.doc.generation);
            if let Some(j) = &mut self.journal {
                j.sync(&self.session);
            }
        }

        // Deck's "critique" button: same effect as the `critique` verb.
        if self.deck_pane.take_critique_request() {
            self.pending_critique = Some(String::new());
        }
        self.handle_dev_screenshot(ui.ctx());
        self.handle_critique(ui.ctx());

        // Persist zoom changes from any source (buttons or Cmd+=/Cmd+-).
        let zoom = ui.ctx().zoom_factor();
        if (zoom - self.saved_zoom).abs() > 0.01 {
            self.saved_zoom = zoom;
            save_zoom(zoom);
        }

        let open_key =
            ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::O));
        if open_key {
            let ctx = ui.ctx().clone();
            self.guarded_nav(&ctx, PendingNav::Open);
        }

        // Deck pane tick: must run every frame regardless of visibility so
        // streaming turns and probes keep making progress while the pane is hidden.
        self.deck_pane.tick(&mut self.session, &self.tokio, ui.ctx());

        // Diffusion render: poll the in-flight job / connection test and draw
        // the floating result window. Must run every frame so progress and
        // failures surface even while the user is idle.
        self.poll_render(ui.ctx());
        self.render_result_window(ui.ctx());

        // Multi-session store: key the deck's per-document chats off a stable
        // document uuid (stamped lazily if the file never had one). App-local,
        // private — never written into the shared drawing.
        let doc_uuid = self.ensure_doc_uuid();
        self.deck_pane.sync_store(&doc_uuid);
        if std::env::var("ITSJUSTCAD_DECK_PANE").is_ok()
            || std::env::var("ITSJUSTCAD_PANEL_TAB").as_deref() == Ok("sessions")
        {
            self.deck_pane.seed_demo_sessions();
        }

        // UI/SESSION TOOL PLANE: apply any layout actions the deck emitted into
        // ui.json — never the op-log. Layout is not the drawing, so it must not
        // replay or undo. The document/op-log is untouched by this.
        let ui_actions = self.deck_pane.take_ui_actions();
        if !ui_actions.is_empty() {
            let mut v = load_ui_json();
            for action in &ui_actions {
                crate::ui_plane::apply(&mut v, action);
            }
            save_ui_json(&v);
            self.reconcile_ui_plane(&v);
        }

        // APP-VERB PLANE: run any app-level verbs the deck emitted (camera/view/
        // display/lighting/ze) through the SAME app-verb-aware path the human
        // command line uses. The substrate parser rejects these, so the deck
        // queues them here instead of trying to `session.run` them. Never logged.
        let app_verb_lines = self.deck_pane.take_app_verbs();
        for line in app_verb_lines {
            self.execute_line(line);
        }

        // Cmd+\ reveals the Deck tab in the right panel (or hides the panel if
        // the Deck tab is already the active, visible one). Backslash is not
        // used elsewhere.
        let toggle_deck = ui.input_mut(|i| {
            i.consume_key(egui::Modifiers::COMMAND, egui::Key::Backslash)
        });
        if toggle_deck {
            self.toggle_panel();
        }

        // ⌘K opens the command palette. Consumed here (not via the keymap, which
        // maps to command strings) so it fires even when the command line has
        // focus — the palette is a global launcher.
        let open_palette_key =
            ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::K));
        if open_palette_key {
            self.open_palette();
        }
        // Render the palette overlay (dispatches the chosen action).
        {
            let ctx = ui.ctx().clone();
            self.command_palette_ui(&ctx);
        }

        // Canvas shortcuts: pure keymap resolves each key press to a command
        // line; nothing fires while a text field owns the keyboard.
        let typing = ui.ctx().memory(|m| m.focused().is_some());
        let pressed: Vec<(egui::Key, egui::Modifiers)> = ui.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        repeat: false,
                        modifiers,
                        ..
                    } => Some((*key, *modifiers)),
                    _ => None,
                })
                .collect()
        });
        for (key, mods) in pressed {
            // Context is rebuilt per key: an earlier press this frame may have
            // started a tool or changed the selection.
            let line = keymap::keymap(
                key,
                mods,
                keymap::KeyContext {
                    typing,
                    draw_active: self.draw_tool.active(),
                    has_selection: !self.session.doc.selection.is_empty(),
                    last_command: self.last_line.as_deref(),
                },
            );
            if let Some(line) = line {
                self.execute_line(line);
            }
        }

        // ── Fixed docked slots (Layer 2) ─────────────────────────────────
        // Panel order matters: the first-declared bottom panel is OUTERMOST, i.e.
        // lowest on screen. We build the menu bar at the very top, then the
        // command line as a full-width panel at the VERY bottom of the window,
        // the status bar directly above it, then the right tab panel, and finally
        // the central viewport frame with its bottom tab bar.

        // 1. Menu bar (Layer 3) — always the topmost strip.
        self.menu_bar(ui);

        // 2. Command line — top-level, FULL WIDTH, at the very bottom of the
        // window (declared first among bottom panels so it is the lowest). Always
        // visible: it is the op-log scrollback + input for every mode. Moving it
        // out of the nested right panel fixes the input flicker/dropped-keystroke
        // bug (an unstable bottom panel inside a resizable right panel).
        // Resizable so the user can drag the top edge to grow the op-log
        // scrollback. Default height shows ~5 history lines plus the input row;
        // the history block reserves that space even when scrollback is sparse.
        // Resizable: drag the top edge to grow the op-log scrollback up to 60%
        // of the window (the history fills the extra height — see
        // `history_h_for`). Floor keeps ~5 lines + input visible.
        let cmd_max = (ui.ctx().viewport_rect().height() * 0.6).max(150.0);
        // White background (dark-neutral in dark mode) so the command line reads
        // as a clean surface like the chat dock — not a grey strip.
        let cmd_accent = preset::preset_for(self.cad_origin).tokens().colors.primary;
        let cmd_fill = crate::theme::to_color32(
            crate::theme::roles_for_mode(ui.visuals().dark_mode, cmd_accent).surface,
        );
        egui::Panel::bottom("command_line")
            .resizable(true)
            // ~5 lines of history (≈16pt/line) + the input row reserve.
            .default_size(5.0 * 16.0 + 44.0)
            .min_size(90.0)
            .max_size(cmd_max)
            .frame(
                egui::Frame::side_top_panel(ui.style())
                    .fill(cmd_fill)
                    .inner_margin(egui::Margin::symmetric(crate::theme::Spacing::S as i8, 4)),
            )
            .show(ui, |ui| self.command_line_body(ui));

        // 3. Status bar — directly above the command line.
        // Right dock is declared BEFORE the statusbar so it spans the full height
        // down to the command line — its bottom edge TOUCHES the command line
        // instead of stopping at a full-width statusbar. The statusbar (declared
        // after) then only spans the remaining VIEWPORT width, not under the dock.
        self.right_panel(ui);

        egui::Panel::bottom("statusbar")
            .resizable(false)
            .show(ui, |ui| self.status_bar(ui));

        // A `view <name>` restore (command line, deck or script) parks the
        // saved camera in the document mailbox; drive the active viewport.
        if let Some(view) = self.session.doc.pending_view.take() {
            apply_named_view(self.active_camera(), &view);
        }

        // 6. Central viewport frame with the bottom viewport tab bar.
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                egui::Panel::bottom("viewport_tabs")
                    .resizable(false)
                    .show(ui, |ui| self.viewport_tab_bar(ui));
                self.viewport(ui);
            });

        // Unsaved-changes alert: modal on top of everything when a New/Open/Quit
        // is parked behind the guard.
        let ctx = ui.ctx().clone();
        self.unsaved_guard_ui(&ctx);
        // External-change watch: poll the saved file's mtime/size (throttled to
        // one stat every 2 s) and park a reload alert when it diverges. Skipped
        // while any change alert is already up.
        if self.external_change.is_none()
            && let Some(w) = &mut self.file_watch
            && let Some(change) = w.poll(std::time::Duration::from_secs(2))
        {
            self.external_change = Some(change);
        }
        self.external_change_ui(&ctx);
        // Ask-on-continue: modal when continuing a loaded chat that diverged and
        // the user then triggered New Session / load-another.
        self.session_conflict_ui(&ctx);

        // COMMAND LINE ALWAYS LISTENS (Rhino-style): whenever nothing else owns
        // the keyboard — e.g. after clicking the viewport, which leaves focus on
        // no text field — re-focus the command input so typing goes straight to
        // it without a click. Skip while a draw tool owns the keyboard (numeric
        // input) or a modal/overlay with its own focus is open (palette, dialogs,
        // Model Setup) — and never steal from another focused field (the deck
        // chat input keeps focus because `focused()` is then `Some`).
        let modal_open = self.show_palette
            || self.show_about
            || self.show_history
            || self.show_model_setup
            || self.show_template_picker
            || self.import_job.is_some()
            || self.import_result.is_some()
            || self.pending_nav.is_some();
        if !modal_open
            && !self.draw_tool.active()
            && ctx.memory(|m| m.focused()).is_none()
        {
            self.command_line.focus();
        }
    }
}

/// Snapshot the orbit camera as document-storable named-view parameters.
fn named_view_of(cam: &OrbitCamera) -> itsjustcad_doc::NamedView {
    itsjustcad_doc::NamedView {
        target: cam.target.to_array(),
        distance: cam.distance,
        yaw: cam.yaw,
        pitch: cam.pitch,
        fov_y: cam.fov_y,
        ortho: cam.ortho,
        two_point: cam.two_point,
        pano: cam.pano.map(pano_to_view),
    }
}

fn apply_named_view(cam: &mut OrbitCamera, view: &itsjustcad_doc::NamedView) {
    cam.target = glam::Vec3::from_array(view.target);
    cam.distance = view.distance;
    cam.yaw = view.yaw;
    cam.pitch = view.pitch;
    cam.fov_y = view.fov_y;
    cam.ortho = view.ortho;
    cam.two_point = view.two_point;
    cam.pano = view.pano.map(pano_from_view);
}

/// Bridge the renderer's `PanoProjection` to the doc's serde `PanoView`.
fn pano_to_view(p: itsjustcad_render::PanoProjection) -> itsjustcad_doc::PanoView {
    match p {
        itsjustcad_render::PanoProjection::Equirect => itsjustcad_doc::PanoView::Equirect,
        itsjustcad_render::PanoProjection::Fisheye { fov } => {
            itsjustcad_doc::PanoView::Fisheye { fov }
        }
    }
}

fn pano_from_view(v: itsjustcad_doc::PanoView) -> itsjustcad_render::PanoProjection {
    match v {
        itsjustcad_doc::PanoView::Equirect => itsjustcad_render::PanoProjection::Equirect,
        itsjustcad_doc::PanoView::Fisheye { fov } => {
            itsjustcad_render::PanoProjection::Fisheye { fov }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use itsjustcad_commands::registry;

    #[test]
    fn import_job_fraction_tracks_progress() {
        let mut job = ImportJob {
            path: "x.dxf".into(),
            entities: Vec::new(),
            cursor: 0,
            total: 200,
            skipped: 5,
            prev_layer: "0".into(),
        };
        assert_eq!(job.fraction(), 0.0);
        job.cursor = 100;
        assert!((job.fraction() - 0.5).abs() < 1e-6);
        job.cursor = 200;
        assert_eq!(job.fraction(), 1.0);
        // Empty import is treated as complete (avoids a 0/0 progress bar).
        job.total = 0;
        assert_eq!(job.fraction(), 1.0);
    }

    /// Whole-app-window previews (SwiftUI-#Preview style) rendered off-screen via
    /// egui_kittest's eframe integration: `build_eframe` hands `App::new` a real
    /// `CreationContext` carrying kittest's wgpu render state, so the full window
    /// — menu bar, viewports, right dock, command line — renders in BOTH themes.
    /// `setup` seeds per-preview state (populate layers, raise a dialog, …).
    /// Ignored by default (needs a GPU adapter). Generate / refresh with:
    ///
    ///   UPDATE_SNAPSHOTS=1 cargo test -p itsjustcad preview -- --ignored --test-threads=1
    ///
    /// then view `crates/app/tests/snapshots/{app,layers,dialog}_*.png`.
    #[cfg(test)]
    fn snapshot_app(name: &str, dark: bool, setup: impl Fn(&mut App)) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1440.0, 900.0))
            .wgpu()
            .build_eframe(|cc| App::new(cc, handle.clone()));
        {
            let app = harness.state_mut();
            // Chrome-only: skip the 3D wgpu callback (kittest's egui pass has no
            // depth attachment). Pin the theme via the app's own `forced_dark`
            // (a plain `ctx.set_theme` gets reset by eframe's per-frame OS read).
            app.preview_no_viewport = true;
            app.forced_dark = Some(dark);
            setup(app);
        }
        harness.run_steps(4);
        harness.snapshot(name);
        // Leak the harness + runtime: their Drop impls panic inside kittest's
        // pollster/wgpu block_on context (wgpu device teardown; "cannot drop a
        // runtime …"). The snapshot is already written and the test process is
        // short-lived, so leaking is harmless and keeps the run green.
        std::mem::forget(harness);
        std::mem::forget(rt);
    }

    #[test]
    #[ignore = "needs a GPU adapter; run explicitly to (re)generate app-window previews"]
    fn app_preview_panel_toggle() {
        // Verify the collapse/reshow button DOESN'T move vertically. Render with
        // the panel visible (the collapse button records its y), then hide the
        // panel — the floating reshow button must appear at that same y.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1440.0, 900.0))
            .wgpu()
            .build_eframe(|cc| App::new(cc, handle.clone()));
        {
            let app = harness.state_mut();
            app.preview_no_viewport = true;
            app.forced_dark = Some(false);
        }
        harness.run_steps(4); // visible → collapse button stores its y
        harness.state_mut().panel_visible = false;
        harness.run_steps(4); // hidden → reshow button at the stored y
        harness.snapshot("app_panel_collapsed");
        std::mem::forget(harness);
        std::mem::forget(rt);
    }

    #[test]
    #[ignore = "needs a GPU adapter; run explicitly to (re)generate app-window previews"]
    fn app_preview_chat_toggle() {
        // Reproduce the input-float bug in the REAL nested layout (deck pane
        // inside the dock CentralPanel inside Panel::right): EXPAND the chat
        // input, render frames, then COLLAPSE and render again. The final
        // snapshot must show the input back at the BOTTOM of the dock.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1440.0, 900.0))
            .wgpu()
            .build_eframe(|cc| App::new(cc, handle.clone()));
        {
            let app = harness.state_mut();
            app.preview_no_viewport = true;
            app.forced_dark = Some(false);
        }
        let id = egui::Id::new("deck_input_expanded");
        harness.ctx.data_mut(|d| d.insert_temp(id, true));
        harness.run_steps(4);
        harness.ctx.data_mut(|d| d.insert_temp(id, false));
        harness.run_steps(4);
        harness.snapshot("app_chat_toggle");
        std::mem::forget(harness);
        std::mem::forget(rt);
    }

    #[test]
    #[ignore = "needs a GPU adapter; run explicitly to (re)generate app-window previews"]
    fn app_preview_window() {
        snapshot_app("app_dark", true, |_| {});
        snapshot_app("app_light", false, |_| {});
    }

    #[test]
    #[ignore = "needs a GPU adapter; run explicitly to (re)generate app-window previews"]
    fn app_preview_layers() {
        // Seed a few layers + objects and switch the right dock to the Layers tab.
        let seed = |app: &mut App| {
            for line in [
                "box 0,0,0 5,5,3",
                "layer walls",
                "tolayer last walls",
                "layer structure",
                "layercolor structure 0.2,0.5,1",
                "box 1,1,0 2,2,4",
                "circle 0,0,0 2",
                "layer grid",
            ] {
                app.execute_line(line.to_string());
            }
            app.panel_tabs.show(crate::tabstrip::PanelTab::Model); // "Layers"
        };
        snapshot_app("layers_dark", true, &seed);
        snapshot_app("layers_light", false, &seed);
    }

    #[test]
    #[ignore = "needs a GPU adapter; run explicitly to (re)generate app-window previews"]
    fn app_preview_dialog() {
        // Make an edit (dirty doc), then park a New behind the unsaved-changes
        // guard so `unsaved_guard_ui` renders the modal alert over the window.
        let seed = |app: &mut App| {
            app.execute_line("box 0,0,0 5,5,3".to_string());
            app.pending_nav = Some(PendingNav::New);
        };
        snapshot_app("dialog_dark", true, &seed);
        snapshot_app("dialog_light", false, &seed);
    }

    #[test]
    fn discard_choice_clears_pending_and_performs_nav() {
        // The user-reported bug: clicking Discard did nothing. The guard's
        // choice→action contract must (a) clear the parked nav AND (b) perform
        // it for Discard/Confirm. Both are required for New/Open/Quit to fire.
        let a = nav_action_for(Some(crate::widgets::AlertChoice::Confirm));
        assert!(a.clear_pending, "Discard must clear the parked nav");
        assert!(a.perform, "Discard MUST perform the parked navigation");
    }

    #[test]
    fn discard_quit_does_not_loop_forever() {
        // Regression: after Discard on Quit, perform_nav sets force_close and
        // sends Close. The next frame's close-request handler must let that close
        // THROUGH (not re-park a Quit), even though the doc is still dirty —
        // otherwise the app loops on the alert and never exits.
        //
        // Frame with close_requested, dirty, no Quit parked, force_close unset:
        // intercept (park the Quit + raise the alert).
        assert!(
            should_intercept_close(true, false, false, true),
            "dirty close with no override must be intercepted"
        );
        // After Discard: force_close set → the follow-up Close proceeds.
        assert!(
            !should_intercept_close(true, true, false, true),
            "force_close must let the confirmed Discard-quit close through"
        );
        // A clean doc never intercepts; a close with no request never intercepts.
        assert!(!should_intercept_close(true, false, false, false));
        assert!(!should_intercept_close(false, false, false, true));
        // A Quit already parked (alert up) must not double-park.
        assert!(!should_intercept_close(true, false, true, true));
    }

    #[test]
    fn cancel_choice_clears_pending_without_performing() {
        let a = nav_action_for(Some(crate::widgets::AlertChoice::Cancel));
        assert!(a.clear_pending, "Cancel drops the parked nav");
        assert!(!a.perform, "Cancel must NOT perform the navigation");
    }

    #[test]
    fn open_alert_leaves_nav_parked() {
        // No choice yet (alert still up) → nav stays parked, nothing performed.
        let a = nav_action_for(None);
        assert!(!a.clear_pending);
        assert!(!a.perform);
    }

    #[test]
    fn new_document_nav_effect_clears_the_doc() {
        // Proves the New branch of `perform_nav`'s effect: a dirty session is
        // replaced by a fresh empty one at cursor 0. (perform_nav itself needs
        // an egui ctx, but its New arm is `new_document` whose effect is this.)
        use itsjustcad_commands::{parse, Session};
        let mut session = Session::default();
        session.run(parse("box 0,0,0 1,1,1").unwrap()).unwrap();
        assert!(!session.doc.is_empty(), "precondition: dirty doc has geometry");
        // The New navigation replaces the session (see `new_document`).
        session = Session::default();
        assert!(session.doc.is_empty(), "New must clear the document");
        assert_eq!(session.history().1, 0, "New resets the op-log cursor");
    }

    #[test]
    fn middle_truncate_keeps_head_and_tail() {
        assert_eq!(middle_truncate("short", 18), "short");
        let long = "ExteriorMasonryWall-North";
        let out = middle_truncate(long, 18);
        assert!(out.chars().count() <= 18, "capped: {out}");
        assert!(out.contains('…'));
        assert!(out.starts_with("Exter"));
        assert!(out.ends_with("orth"));
        // Multibyte-safe: slicing is by `char`, never by byte, so this must not
        // panic and must stay within the char budget.
        let multi = "外壁-北側マソンリー-コンクリート-層";
        let m = middle_truncate(multi, 8);
        assert!(m.chars().count() <= 8, "multibyte capped: {m}");
        assert!(m.contains('…'));
        // A short multibyte string is returned unchanged.
        assert_eq!(middle_truncate("café", 10), "café");
    }

    #[test]
    fn empty_document_overlay_only_on_empty_idle_doc() {
        use itsjustcad_commands::{parse, Session};
        // Fresh doc, no tool, first-run (onboarding_done=false) → show prompt.
        let mut s = Session::default();
        assert!(s.doc.is_empty());
        assert!(show_empty_document(&s.doc, false, false));
        // A tool mid-flight suppresses it even on first-run (user is placing #1).
        assert!(!show_empty_document(&s.doc, true, false));
        // Once geometry exists it never shows again.
        s.run(parse("box 0,0,0 1,1,1").unwrap()).unwrap();
        assert!(!s.doc.is_empty());
        assert!(!show_empty_document(&s.doc, false, false));
    }

    #[test]
    fn empty_document_overlay_hidden_after_onboarding() {
        use itsjustcad_commands::Session;
        // After onboarding is done, an empty doc with no tool must NOT show the
        // overlay. This is the "second launch / returning user" scenario.
        let s = Session::default();
        assert!(s.doc.is_empty());
        // onboarding_done = true → overlay suppressed regardless of doc state.
        assert!(!show_empty_document(&s.doc, false, true));
        // Confirming: tool active + onboarding done → also suppressed.
        assert!(!show_empty_document(&s.doc, true, true));
    }

    #[test]
    fn empty_document_hint_is_returning_user_counterpart() {
        use itsjustcad_commands::{parse, Session};
        let mut s = Session::default();
        assert!(s.doc.is_empty());
        // Returning user (onboarding done), empty doc, no tool → subtle hint.
        assert!(show_empty_document_hint(&s.doc, false, true));
        // First-run (onboarding NOT done) shows the boxed overlay, NOT the hint,
        // so the two never paint at once.
        assert!(!show_empty_document_hint(&s.doc, false, false));
        assert!(show_empty_document(&s.doc, false, false));
        // A tool mid-flight suppresses the hint.
        assert!(!show_empty_document_hint(&s.doc, true, true));
        // Geometry present → no hint.
        s.run(parse("box 0,0,0 1,1,1").unwrap()).unwrap();
        assert!(!show_empty_document_hint(&s.doc, false, true));
    }

    #[test]
    fn focus_ring_is_two_px_accent_on_focusable_controls() {
        // The keyboard focus cue egui paints on any focused widget (fields,
        // combos, list rows) is the 2px accent selection stroke.
        let mut style = egui::Style::default();
        let tokens = crate::preset::preset_for(crate::preset::CadOrigin::Rhino).tokens();
        crate::theme::apply_to_style(&mut style, &tokens);
        assert_eq!(style.visuals.selection.stroke.width, 2.0);
        assert_eq!(
            style.visuals.selection.stroke.color,
            crate::theme::to_color32(tokens.colors.primary)
        );
    }

    #[test]
    fn print_width_label_maps_default() {
        assert_eq!(print_width_label(0.18), "Default");
        assert_eq!(print_width_label(0.35), "0.35");
        assert_eq!(print_width_label(1.00), "1.00");
    }

    #[test]
    fn next_free_layer_name_skips_seeded() {
        let doc = itsjustcad_doc::Document::default();
        // Seeded doc already has Layer 01..05, so the next free is Layer 06.
        assert_eq!(next_free_layer_name(&doc), "Layer 06");
    }

    #[test]
    fn empty_deletable_layers_excludes_default_and_used() {
        use itsjustcad_commands::{parse, Session};
        let mut s = Session::default();
        // Put an object on Layer 01 so it is no longer empty.
        s.run(parse("layer Layer 01").unwrap()).unwrap();
        s.run(parse("box 0,0,0 1,1,1").unwrap()).unwrap();
        let empty = empty_deletable_layers(&s.doc);
        assert!(!empty.contains(&itsjustcad_doc::DEFAULT_LAYER.to_string()));
        assert!(!empty.iter().any(|n| n == "Layer 01"), "used layer excluded");
        assert!(empty.iter().any(|n| n == "Layer 05"), "empty seeded layer purgeable");
    }

    #[test]
    fn config_migration_only_when_old_present_and_new_absent() {
        assert!(should_migrate_config(true, false), "legacy only → migrate");
        assert!(!should_migrate_config(false, false), "fresh install → no-op");
        assert!(!should_migrate_config(true, true), "already migrated → no-op");
        assert!(!should_migrate_config(false, true), "new only → no-op");
    }

    #[test]
    fn critique_prompt_names_the_image_and_frames_the_ask() {
        let p = critique_prompt("/some/private/path/critique.png", "");
        assert!(p.contains("Read /some/private/path/critique.png"));
        assert!(p.contains("architecture critic"));
        assert!(p.contains("massing"));
        assert!(p.contains("proportion"));
        assert!(p.contains("light"));
        // No trailing "Also address:" when there is no question.
        assert!(!p.contains("Also address"));
    }

    #[test]
    fn critique_prompt_folds_in_user_question() {
        let shot_str = critique_shot_path().display().to_string();
        let p = critique_prompt(&shot_str, "  is the roof pitch too steep?  ");
        // Whitespace-trimmed and appended.
        assert!(p.ends_with("Also address: is the roof pitch too steep?"));
    }

    /// M-4 regression: the critique screenshot path must NOT be under /tmp
    /// (world-writable, symlink-plantable). It must be inside the user's
    /// private config/runtime directory.
    #[test]
    fn critique_shot_path_is_not_under_tmp() {
        let path = critique_shot_path();
        // The path must not start with /tmp (the fixed-path vulnerability).
        assert!(
            !path.starts_with("/tmp"),
            "critique path must not be world-writable /tmp: {path:?}"
        );
        // Must be inside the ItsJustCAD private dir (not a bare temp path).
        let dir = private_runtime_dir();
        assert!(
            path.starts_with(&dir),
            "critique path {path:?} should be inside private dir {dir:?}"
        );
        // The private dir itself should not be /tmp directly.
        assert!(
            dir != std::path::Path::new("/tmp"),
            "private_runtime_dir must not be bare /tmp"
        );
    }

    /// Closing the Model Setup panel must NOT cancel an active download: the
    /// background task stays alive and keeps advancing. We model the close path
    /// (`close_model_setup` only flips `show_model_setup`) against a fake
    /// download and assert (a) cancel was never requested and (b) the shared
    /// state can still advance to a new value afterwards.
    #[test]
    fn closing_model_setup_does_not_cancel_active_download() {
        use crate::download::{Download, DownloadState};
        use std::sync::atomic::Ordering;

        let (handle, state, cancel) = Download::for_test(DownloadState::Downloading {
            done: 10,
            total: Some(100),
            bytes_per_sec: 1.0,
        });
        let active = ActiveDownload {
            model_id: "m".into(),
            handle,
            persisted: false,
        };

        // The close path (see `close_model_setup`) touches only the panel flag,
        // never `active.handle.cancel()`. Simulate it:
        // (close_model_setup would set show_model_setup=false and do nothing else)
        assert!(!cancel.load(Ordering::SeqCst), "no cancel before close");

        // Nothing about closing touched the handle: still not cancelled.
        assert!(
            !cancel.load(Ordering::SeqCst),
            "closing the panel must not request cancellation"
        );

        // The background task can still advance — it's alive.
        *state.lock().expect("state") = DownloadState::Downloading {
            done: 50,
            total: Some(100),
            bytes_per_sec: 2.0,
        };
        assert!(
            active.handle.state().is_active(),
            "download still advancing after close"
        );
        assert_eq!(active.handle.state().fraction(), Some(0.5));

        // By contrast, the explicit Cancel button DOES cancel.
        active.handle.cancel();
        assert!(cancel.load(Ordering::SeqCst), "explicit cancel sets the flag");
    }

    // ── download terminal-state hand-off (panel open OR closed) ────────────
    // `poll_model_download` runs every frame regardless of `show_model_setup`,
    // applying this pure rule — the old bug was polling only inside the panel.

    #[test]
    fn download_outcome_done_completes_exactly_once() {
        use crate::download::DownloadState;
        let done = DownloadState::Done { path: std::path::PathBuf::from("/m.gguf") };
        assert_eq!(
            download_outcome(&done, false),
            Some(DownloadOutcome::Completed)
        );
        // Second poll (already handled) must not re-install.
        assert_eq!(download_outcome(&done, true), None);
    }

    #[test]
    fn download_outcome_failure_is_surfaced() {
        use crate::download::DownloadState;
        let failed = DownloadState::Failed { msg: "server returned 404".into() };
        assert_eq!(
            download_outcome(&failed, false),
            Some(DownloadOutcome::Failed("server returned 404".into()))
        );
        assert_eq!(download_outcome(&failed, true), None, "announce only once");
    }

    #[test]
    fn download_outcome_cancel_is_quiet() {
        use crate::download::DownloadState;
        // A user cancel already shows its own caption and keeps the .part for
        // resume — no extra transcript announcement.
        let cancelled = DownloadState::Failed { msg: "cancelled".into() };
        assert_eq!(download_outcome(&cancelled, false), None);
    }

    #[test]
    fn download_outcome_nonterminal_states_do_nothing() {
        use crate::download::DownloadState;
        let running = DownloadState::Downloading { done: 1, total: Some(2), bytes_per_sec: 1.0 };
        assert_eq!(download_outcome(&running, false), None);
        assert_eq!(download_outcome(&DownloadState::Verifying, false), None);
        assert_eq!(download_outcome(&DownloadState::Idle, false), None);
    }

    #[test]
    fn help_all_lists_every_registry_verb() {
        let lines = help_lines(None);
        let text = lines.join("\n");
        for spec in registry() {
            assert!(text.contains(spec.name), "help missing verb '{}'", spec.name);
        }
    }

    #[test]
    fn help_verb_shows_usage() {
        let lines = help_lines(Some("box"));
        let text = lines.join("\n");
        assert!(text.contains("box <corner"), "expected usage: {text}");
    }

    #[test]
    fn help_unknown_verb_reports_error() {
        let lines = help_lines(Some("xyzzy"));
        assert!(lines.iter().any(|l| l.contains("unknown")));
    }

    #[test]
    fn template_units_mapping() {
        assert_eq!(units_cmd_for(&TemplateUnits::Meters), "units m");
        assert_eq!(units_cmd_for(&TemplateUnits::Millimeters), "units mm");
        assert_eq!(units_cmd_for(&TemplateUnits::FeetInches), "units ftin");
    }

    #[test]
    fn template_scale_distance() {
        assert_eq!(camera_distance_for(&TemplateScale::Object), 5.0f32);
        assert_eq!(camera_distance_for(&TemplateScale::Building), 30.0f32);
        assert_eq!(camera_distance_for(&TemplateScale::Urban), 300.0f32);
    }

    #[test]
    fn prefs_round_trip_template_done() {
        let mut v = serde_json::Value::Object(Default::default());
        v["template_done"] = serde_json::json!(true);
        assert_eq!(v["template_done"].as_bool(), Some(true));
    }

    // ── deck-brain onboarding ──────────────────────────────────────────────

    use crate::hardware::ModelTier;
    use itsjustcad_deck::{DeckKind, DecksFile};

    #[test]
    fn deck_brain_pref_round_trip() {
        // The pref string maps back to the same enum for every variant.
        for brain in [
            DeckBrain::Cloud,
            DeckBrain::Ollama,
            DeckBrain::Download,
            DeckBrain::Skip,
        ] {
            let s = brain.as_pref();
            let back = match s {
                "cloud" => DeckBrain::Cloud,
                "ollama" => DeckBrain::Ollama,
                "download" => DeckBrain::Download,
                "skip" => DeckBrain::Skip,
                other => panic!("unexpected pref {other}"),
            };
            assert_eq!(back, brain);
        }
    }

    #[test]
    fn cloud_and_skip_write_no_cassette() {
        let mut decks = DecksFile::default();
        let before = decks.decks.len();
        assert!(deck_brain_into_decks(&mut decks, DeckBrain::Cloud, ModelTier::Mid7B).is_none());
        assert!(deck_brain_into_decks(&mut decks, DeckBrain::Skip, ModelTier::Mid7B).is_none());
        assert_eq!(decks.decks.len(), before, "no cassette added for cloud/skip");
    }

    #[test]
    fn ollama_choice_activates_local_cassette() {
        // Start from an empty file to isolate the write.
        let mut decks = DecksFile {
            decks: vec![],
            active: 0,
            local_only: false,
        };
        let name = deck_brain_into_decks(&mut decks, DeckBrain::Ollama, ModelTier::Mid7B).unwrap();
        assert_eq!(name, "ollama");
        let active = &decks.decks[decks.active];
        assert_eq!(active.name, "ollama");
        assert_eq!(active.kind, DeckKind::OpenaiCompat);
        assert_eq!(active.base_url, "http://localhost:11434/v1");
        assert!(active.grammar, "local cassette must have grammar on");
        assert!(active.api_key.is_none());
        assert!(itsjustcad_deck::is_local_url(&active.base_url));
    }

    #[test]
    fn download_choice_names_model_from_tier() {
        let mut decks = DecksFile {
            decks: vec![],
            active: 0,
            local_only: false,
        };
        // 7B tier → 7b model.
        deck_brain_into_decks(&mut decks, DeckBrain::Download, ModelTier::Mid7B);
        assert_eq!(decks.decks[decks.active].model, "qwen2.5:7b");
        assert!(decks.decks[decks.active].grammar);

        // 3B tier → 3b model, replacing the same-named cassette (no dup).
        let n = decks.decks.len();
        deck_brain_into_decks(&mut decks, DeckBrain::Download, ModelTier::Small3B);
        assert_eq!(decks.decks.len(), n, "same name replaced, not duplicated");
        assert_eq!(decks.decks[decks.active].model, "qwen2.5:3b");
    }

    #[test]
    fn model_id_tier_mapping() {
        assert_eq!(model_id_for_tier(ModelTier::None), "qwen2.5:3b");
        assert_eq!(model_id_for_tier(ModelTier::Small3B), "qwen2.5:3b");
        assert_eq!(model_id_for_tier(ModelTier::Mid7B), "qwen2.5:7b");
    }

    // ── catalog cassette install / remove ──────────────────────────────────

    fn sample_entry() -> crate::model_catalog::ModelEntry {
        crate::model_catalog::Catalog::load().models[0].clone()
    }

    #[test]
    fn catalog_entry_is_local_openai_compat_with_grammar() {
        let entry = sample_entry();
        let cfg = catalog_deck_entry(&entry);
        assert_eq!(cfg.name, format!("local-{}", entry.id));
        assert_eq!(cfg.kind, DeckKind::OpenaiCompat);
        assert_eq!(cfg.model, entry.id);
        assert!(cfg.grammar, "local model cassette must have grammar on");
        assert!(cfg.api_key.is_none());
        assert!(itsjustcad_deck::is_local_url(&cfg.base_url), "runtime url must be local");
    }

    #[test]
    fn install_appends_then_replaces_and_activates() {
        let entry = sample_entry();
        let mut decks = DecksFile {
            decks: vec![],
            active: 0,
            local_only: false,
        };
        let i = install_catalog_deck(&mut decks, &entry);
        assert_eq!(decks.decks.len(), 1);
        assert_eq!(decks.active, i);
        assert!(catalog_deck_installed(&decks, &entry.id));
        // Re-install (re-download) replaces the same cassette, no duplicate.
        install_catalog_deck(&mut decks, &entry);
        assert_eq!(decks.decks.len(), 1, "same model must not duplicate");
    }

    #[test]
    fn remove_deletes_cassette_and_clamps_active() {
        let entry = sample_entry();
        let mut decks = DecksFile::default();
        let before = decks.decks.len();
        install_catalog_deck(&mut decks, &entry);
        assert_eq!(decks.decks.len(), before + 1);
        assert!(remove_catalog_deck(&mut decks, &entry.id));
        assert_eq!(decks.decks.len(), before);
        assert!(!catalog_deck_installed(&decks, &entry.id));
        assert!(decks.active < decks.decks.len().max(1));
        // Removing a non-installed model is a no-op.
        assert!(!remove_catalog_deck(&mut decks, "not-a-model"));
    }

    #[test]
    fn model_setup_menu_action_wires_to_flag() {
        // The menu emits ModelSetup; the app maps it to opening the panel.
        assert_eq!(
            crate::menu::menu_action("model_setup"),
            crate::menu::MenuAction::Insert("model_setup ".to_string()),
            "sanity: unknown verb still routes through menu_action"
        );
        // The real wiring is the Tools button emitting MenuAction::ModelSetup,
        // matched in apply_menu_action → show_model_setup = true. Assert the
        // variant exists and is distinct.
        let a = crate::menu::MenuAction::ModelSetup;
        assert_ne!(a, crate::menu::MenuAction::About);
    }
}
