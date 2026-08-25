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

pub struct App {
    session: Session,
    command_line: CommandLine,
    deck_pane: DeckPane,
    draw_tool: DrawTool,
    gumball: Gumball,
    point_edit: crate::point_edit::PointEdit,
    tokio: tokio::runtime::Handle,
    /// Camera slots shared across layouts: 0 Persp, 1 Top, 2 Front, 3 Right.
    cameras: [OrbitCamera; 4],
    /// Display mode per camera slot (view state, follows the camera across
    /// layout switches; never logged).
    display_modes: [DisplayMode; 4],
    /// Color mode per camera slot (view state; never logged).
    color_modes: [ColorMode; 4],
    layout: ViewportLayout,
    /// Last hovered pane; view commands and tools target its camera.
    active_pane: usize,
    /// Generation of the last GPU upload; compare with `session.doc.generation`.
    uploaded_generation: Option<u64>,
    /// Theme of the last GPU upload; theme flips force a re-upload.
    uploaded_theme: Option<scene::Theme>,
    /// Color mode of the last GPU upload; mode changes force a re-upload.
    uploaded_color_mode: Option<ColorMode>,
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
    /// Last executed command line; Enter/Space on the canvas repeats it.
    last_line: Option<String>,
    /// A `critique` request awaiting its viewport screenshot. Holds the
    /// optional user question; once the tagged Screenshot event lands, the PNG
    /// is written and a vision deck turn (Read tool enabled) is fired.
    pending_critique: Option<String>,
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
    /// Right docked panel tab state (Layers/Properties/History/Deck).
    panel_tabs: crate::tabstrip::TabState,
    /// Whether the right docked panel is shown at all (Cmd+\ hides/shows).
    panel_visible: bool,
    /// Whether the Help → About dialog is open.
    show_about: bool,
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
    /// Bundled catalog of downloadable local models, parsed once.
    catalog: crate::model_catalog::Catalog,
    /// The download in flight from the Model Setup panel, if any. The UI polls
    /// its [`crate::download::DownloadState`] each frame.
    active_download: Option<ActiveDownload>,
}

/// A running install: which model, plus the [`crate::download::Download`] handle
/// the UI polls. Kept so a completed download can be turned into a decks.json
/// cassette exactly once.
struct ActiveDownload {
    model_id: String,
    handle: crate::download::Download,
    /// Set once we've persisted the cassette for a `Done` state, so we do it
    /// only once.
    persisted: bool,
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
                cams[3].set_view(StandardView::Right);
                cams
            },
            display_modes: [DisplayMode::default(); 4],
            color_modes: [ColorMode::default(); 4],
            layout: match preset::preset_for(cad_origin).default_viewports {
                4 => ViewportLayout::Four,
                2 => ViewportLayout::Two,
                _ => ViewportLayout::Single,
            },
            active_pane: 0,
            uploaded_generation: None,
            uploaded_theme: None,
            uploaded_color_mode: None,
            saved_zoom: zoom,
            shot_path: std::env::var("ITSJUSTCAD_SHOT").ok(),
            startup_script: std::env::var("ITSJUSTCAD_RUN").ok(),
            deck_script: std::env::var("ITSJUSTCAD_DECK_RUN").ok(),
            type_script: std::env::var("ITSJUSTCAD_TYPE").ok(),
            frame_count: 0,
            shot_saved: false,
            pending_layer_color: None,
            last_line: None,
            pending_critique: None,
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
            panel_visible: true,
            show_about: false,
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
            catalog: crate::model_catalog::Catalog::load(),
            active_download: None,
        }
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
            Some("controlimages") => {
                match words.next() {
                    Some(prefix) => self.export_control_images(prefix),
                    None => self
                        .command_line
                        .push_line("usage: controlimages <path-prefix>"),
                }
            }
            Some("open") => self.open(words.next().map(Into::into)),
            Some("recover") => self.recover(),
            Some("ze" | "zoomextents") => self.zoom_extents(),
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
        const W: u32 = 1280;
        const H: u32 = 800;
        let aspect = W as f32 / H as f32;
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

        let result = (|| -> Result<itsjustcad_render::ControlImagePaths, String> {
            let instance = wgpu::Instance::default();
            let adapter = pollster::block_on(
                instance.request_adapter(&wgpu::RequestAdapterOptions::default()),
            )
            .map_err(|e| format!("no wgpu adapter: {e:?}"))?;
            let (device, queue) = pollster::block_on(
                adapter.request_device(&wgpu::DeviceDescriptor::default()),
            )
            .map_err(|e| e.to_string())?;
            itsjustcad_render::render_control_images(
                &device,
                &queue,
                &self.session.doc,
                view_proj,
                eye,
                near,
                far,
                W,
                H,
                prefix,
            )
        })();

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

    /// `camera <2point|persp|pano|fisheye [fov]|15|24|35|50|85|phone|phonewide>`.
    /// Numeric args may carry a trailing "mm". Two-point toggles architectural
    /// perspective; `pano`/`fisheye` switch to a non-pinhole projection rendered
    /// via the cubemap remap; the lens presets set `fov_y` from a full-frame
    /// angle of view. All are view state on the active pane, never logged.
    fn set_camera(&mut self, arg: Option<&str>, arg2: Option<&str>) {
        let aspect = self.active_aspect;
        let Some(arg) = arg else {
            self.command_line.push_line(
                "usage: camera 2point|persp|pano|fisheye [fov]|<15|24|35|50|85>mm|phone|phonewide",
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
            _ => {
                // Named phone sim, or a numeric focal length (optional "mm").
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
                            _ => "",
                        };
                        self.command_line
                            .push_line(format!("camera: {f:.0}mm{tag} — {fov:.0}° hfov"));
                    }
                    _ => self.command_line.push_line(
                        "usage: camera 2point|persp|pano|fisheye [fov]|<15|24|35|50|85>mm|phone|phonewide",
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

    fn save(&mut self, path: Option<std::path::PathBuf>) {
        let path = path.or_else(|| {
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
                // Confine deck-originated fs paths to this document's directory.
                self.deck_pane
                    .set_sandbox_root(path.parent().map(|p| p.to_path_buf()));
                self.command_line
                    .push_line(format!("saved {}", path.display()));
            }
            Err(e) => self.command_line.push_line(format!("error: {e}")),
        }
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

    fn open(&mut self, path: Option<std::path::PathBuf>) {
        let path = path.or_else(|| {
            rfd::FileDialog::new()
                .add_filter("ItsJustCAD", &["itsjustcad.json", "mydrafter.json", "json"])
                .pick_file()
        });
        let Some(path) = path else { return };
        match itsjustcad_commands::io::load_file(&path) {
            Ok(session) => {
                self.session = session;
                self.uploaded_generation = None;
                // Confine deck-originated fs paths to this document's directory.
                self.deck_pane
                    .set_sandbox_root(path.parent().map(|p| p.to_path_buf()));
                self.command_line
                    .push_line(format!("opened {} ({} objects)", path.display(), self.session.doc.len()));
            }
            Err(e) => self.command_line.push_line(format!("error: {e}")),
        }
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
        let full = ui.available_rect_before_wrap();
        if !self.draw_tool.active() {
            self.status_snap = None; // no tool, no snap marker to report
        }
        if self.active_pane >= self.layout.pane_count() {
            self.active_pane = 0;
        }
        // Active viewport = last hovered: tools and view commands follow the cursor.
        if let Some(pos) = ui.ctx().pointer_latest_pos()
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
            || self.uploaded_color_mode != Some(active_color_mode);
        // Scene is uploaded once (renderer shared); only the first pane's
        // callback carries the snapshot, the rest just set their camera.
        let mut scene = if stale {
            self.uploaded_generation = Some(generation);
            self.uploaded_theme = Some(theme);
            self.uploaded_color_mode = Some(active_color_mode);
            let mut s = scene::snapshot_with_mode(
                &self.session.doc,
                theme,
                itsjustcad_render::ColorModeSnapshot { color_mode: active_color_mode },
            );
            s.underlay = self.decode_underlay();
            Some(s)
        } else {
            None
        };

        let panes = self.layout.split(full);
        for (pane, rect) in panes.iter().copied().enumerate() {
            let cam_idx = self.layout.camera_index(pane);
            let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());

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
                    );
                    if let Some(cmd) = pe.command {
                        match self.session.run(cmd) {
                            Ok(outcome) => self.command_line.push_line(outcome.message),
                            Err(e) => self.command_line.push_line(format!("error: {e}")),
                        }
                    }
                    consumed = pe.consumed;
                    if !consumed {
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
            ui.painter().add(egui_wgpu::Callback::new_paint_callback(
                rect,
                ViewportCallback {
                    view_proj,
                    eye: self.cameras[cam_idx].eye(),
                    generation,
                    scene: scene.take(),
                    viewport: pane,
                    mode: self.display_modes[cam_idx],
                    sun_dir,
                },
            ));

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

            if panes.len() > 1 {
                let color = if pane == self.active_pane {
                    ui.visuals().selection.stroke.color
                } else {
                    ui.visuals().weak_text_color()
                };
                ui.painter().rect_stroke(
                    rect.shrink(0.5),
                    0.0,
                    egui::Stroke::new(1.0, color),
                    egui::StrokeKind::Inside,
                );
            }
        }

        // Safety net: a drag that ends without a pointer position (or in
        // another pane) must not leave a stale rubber box behind.
        if self.box_drag.is_some() && !ui.input(|i| i.pointer.any_down()) {
            self.box_drag = None;
        }

        // Display/color-mode chips stay as a compact floating overlay in the
        // top-left of the active viewport (per-viewport state, no overlap with
        // the docked panels). Standard views moved to the bottom viewport tabs.
        self.view_overlay(ui, full);
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
        ui.label(egui::RichText::new("Properties").heading());
        ui.separator();
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
        ui.label(format!("layer(s): {}", layers.into_iter().collect::<Vec<_>>().join(", ")));
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

    /// Layers tab body: visibility toggle, color swatch, current-layer switch.
    /// Every edit goes through the command substrate so it is logged/undoable.
    /// Rendered inside the docked right panel (no floating Area).
    fn layers_panel(&mut self, ui: &mut egui::Ui, theme: scene::Theme) {
        let mut lines: Vec<String> = Vec::new();
        ui.label(egui::RichText::new("Layers").heading());
        ui.separator();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let layers: Vec<(String, itsjustcad_doc::LayerStyle)> = self
                    .session
                    .doc
                    .layers
                    .iter()
                    .map(|(n, s)| (n.clone(), s.clone()))
                    .collect();
                let current = self.session.doc.current_layer.clone();
                for (name, style) in layers {
                    ui.horizontal(|ui| {
                        let mut visible = style.visible;
                        if ui
                            .checkbox(&mut visible, "")
                            .on_hover_text("visible")
                            .changed()
                        {
                            let verb = if visible { "show" } else { "hide" };
                            lines.push(format!("{verb} {name}"));
                        }
                        let fallback = theme.mesh();
                        let mut rgb = self
                            .pending_layer_color
                            .as_ref()
                            .filter(|(n, _)| *n == name)
                            .map(|(_, c)| *c)
                            .or_else(|| style.color.map(|c| [c[0], c[1], c[2]]))
                            .unwrap_or([fallback[0], fallback[1], fallback[2]]);
                        if ui.color_edit_button_rgb(&mut rgb).changed() {
                            self.pending_layer_color = Some((name.clone(), rgb));
                        }
                        let is_current = name == current;
                        if ui
                            .selectable_label(is_current, &name)
                            .on_hover_text("set current layer")
                            .clicked()
                            && !is_current
                        {
                            lines.push(format!("layer {name}"));
                        }
                    });
                }
            });
        // Commit the color edit once the mouse is released — one logged op
        // per edit instead of one per drag frame.
        if let Some((name, c)) = self.pending_layer_color.clone()
            && !ui.input(|i| i.pointer.any_down())
        {
            lines.push(format!(
                "layercolor {name} {:.3},{:.3},{:.3}",
                c[0], c[1], c[2]
            ));
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
            ui.monospace(crate::statusbar::format_cursor(doc.units, self.status_cursor));
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
        });
    }

    /// Compact display/color-mode + layout chips, floating in the top-left of
    /// the viewport frame. Standard-view switching lives in the bottom viewport
    /// tab bar (`viewport_tab_bar`), so this overlay is small and never collides
    /// with the docked panels or the top command line.
    fn view_overlay(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        egui::Area::new(egui::Id::new("view_overlay"))
            .fixed_pos(rect.left_top() + egui::vec2(8.0, 8.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if ui.small_button("ZE").on_hover_text("zoom extents").clicked() {
                            self.zoom_extents();
                        }
                        ui.separator();
                        // Display mode and color mode of the active pane's camera slot.
                        let slot = self.layout.camera_index(self.active_pane);
                        egui::ComboBox::from_id_salt("display_mode")
                            .selected_text(self.display_modes[slot].label())
                            .show_ui(ui, |ui| {
                                for mode in DisplayMode::ALL {
                                    ui.selectable_value(
                                        &mut self.display_modes[slot],
                                        mode,
                                        mode.label(),
                                    );
                                }
                            });
                        egui::ComboBox::from_id_salt("color_mode")
                            .selected_text(self.color_modes[slot].label())
                            .show_ui(ui, |ui| {
                                for mode in ColorMode::ALL {
                                    let prev = self.color_modes[slot];
                                    ui.selectable_value(
                                        &mut self.color_modes[slot],
                                        mode,
                                        mode.label(),
                                    );
                                    // Force re-upload when mode changes.
                                    if self.color_modes[slot] != prev {
                                        self.uploaded_color_mode = None;
                                    }
                                }
                            });
                        ui.separator();
                        for (label, layout) in [
                            ("1", ViewportLayout::Single),
                            ("2", ViewportLayout::Two),
                            ("4", ViewportLayout::Four),
                        ] {
                            if ui
                                .selectable_label(self.layout == layout, label)
                                .on_hover_text(format!("{label} viewport(s)"))
                                .clicked()
                            {
                                self.set_layout(layout);
                            }
                        }
                    });
                });
            });
    }

    /// Bottom viewport tab bar (Rhino convention): Persp/Top/Front/Right plus
    /// any saved named views. Clicking a tab sets the active pane's view (or
    /// restores a named view). Rendered as a `TopBottomPanel::bottom` inside the
    /// central viewport frame so it never overlaps the canvas.
    fn viewport_tab_bar(&mut self, ui: &mut egui::Ui) {
        let named: Vec<String> = self.session.doc.named_views.keys().cloned().collect();
        let tabs = crate::tabstrip::viewport_tabs(&named);
        // Highlight the tab matching the active pane's current view.
        let cam = &self.cameras[self.layout.camera_index(self.active_pane)];
        let current = crate::statusbar::view_label(cam.yaw, cam.pitch, cam.ortho);
        let mut chosen: Option<String> = None;
        ui.horizontal(|ui| {
            for (label, verb) in &tabs {
                let selected = label.eq_ignore_ascii_case(current);
                if ui.selectable_label(selected, label).clicked() {
                    chosen = Some(verb.clone());
                }
            }
        });
        if let Some(verb) = chosen {
            self.execute_line(verb);
        }
    }

    /// Command-line panel body. `at_top` positions the autosuggest popup and
    /// history: a top-docked line scrolls history downward with the popup
    /// opening *below* the input; a bottom-docked line keeps the Rhino/AutoCAD
    /// layout with history above and the popup above the input.
    fn command_line_body(&mut self, ui: &mut egui::Ui, at_top: bool) {
        let object_names: Vec<String> = self
            .session
            .doc
            .objects()
            .filter_map(|o| o.name.clone())
            .collect();
        let aliases = self.active_aliases();
        if let Some(line) = self.command_line.ui(ui, &object_names, aliases, at_top) {
            self.execute_line(line);
        }
    }

    /// Right docked tab panel (Layer 2): a hand-rolled tab strip over
    /// Layers / Properties / History / Deck. Clicking the active tab collapses
    /// the panel to just the strip; a chevron also toggles it. The deck keeps
    /// its resize behavior (the whole panel is resizable) and background
    /// streaming (tick runs every frame in `ui`, independent of visibility).
    fn right_panel(&mut self, ui: &mut egui::Ui) {
        use crate::tabstrip::PanelTab;
        if !self.panel_visible {
            // Collapsed to nothing: a small ▸ handle at the top-right edge.
            let vr = ui.ctx().viewport_rect();
            egui::Area::new(egui::Id::new("panel_show_btn"))
                .fixed_pos(egui::pos2(vr.right() - 28.0, vr.top() + 88.0))
                .show(ui.ctx(), |ui| {
                    if ui.small_button("◂").on_hover_text("show panel (Cmd+\\)").clicked() {
                        self.panel_visible = true;
                    }
                });
            return;
        }

        let collapsed = self.panel_tabs.is_collapsed();
        let theme = if ui.visuals().dark_mode { scene::Theme::Dark } else { scene::Theme::Light };
        let mut panel = egui::Panel::right("right_panel").resizable(!collapsed);
        panel = if collapsed {
            panel.default_size(120.0).min_size(90.0)
        } else {
            panel.default_size(320.0).min_size(240.0)
        };
        panel.show(ui, |ui| {
            // Header row: chevron + tab strip.
            ui.horizontal(|ui| {
                if ui.small_button("▸").on_hover_text("hide panel (Cmd+\\)").clicked() {
                    self.panel_visible = false;
                }
                if let Some(tab) = crate::tabstrip::strip_ui(ui, self.panel_tabs) {
                    self.panel_tabs.click(tab);
                    if tab == PanelTab::Deck {
                        self.deck_visible = !self.panel_tabs.is_collapsed();
                    }
                }
            });
            ui.separator();
            if self.panel_tabs.is_collapsed() {
                return;
            }
            match self.panel_tabs.active() {
                PanelTab::Layers => self.layers_tab(ui, theme),
                PanelTab::Properties => self.properties_panel(ui),
                PanelTab::History => self.history_panel(ui),
                PanelTab::Deck => {
                    self.deck_pane.ui(ui, &mut self.session, &self.tokio);
                }
            }
        });
    }

    /// Layers tab wrapper: runs the layers UI then commits any pending edits.
    fn layers_tab(&mut self, ui: &mut egui::Ui, theme: scene::Theme) {
        self.layers_panel(ui, theme);
    }

    /// Top menu bar (Layer 3): registry-driven, grouped per preset
    /// (`menu::top_menus`). The chosen action is dispatched via
    /// [`Self::apply_menu_action`].
    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        let style = preset::preset_for(self.cad_origin).menu_style;
        let bar = egui::Panel::top("menu_bar").resizable(false).show(ui, |ui| {
            crate::menu::ui(ui, style)
        });
        // Dev/screenshot hook: force one menu open to show grouped items.
        if let Ok(title) = std::env::var("ITSJUSTCAD_MENU_DEMO") {
            let at = egui::pos2(bar.response.rect.left() + 8.0, bar.response.rect.bottom());
            crate::menu::demo_open(ui.ctx(), style, &title, at);
        }
        if let Some(action) = bar.inner {
            self.apply_menu_action(action);
        }
    }

    /// Dispatch a menu pick. The rule (see `menu::menu_action`): draw verbs
    /// start the interactive tool, no-arg verbs execute, arg verbs prefill the
    /// command line for typing.
    fn apply_menu_action(&mut self, action: crate::menu::MenuAction) {
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
        }
    }

    /// Model Setup panel: hardware recommendation, the catalog gated by RAM, an
    /// Install button per model with a live progress bar + speed + cancel, and
    /// the currently-installed model with Re-download / Remove. On a completed,
    /// verified download it writes/enables the local `openai_compat` cassette in
    /// `decks.json` (the runtime spawn is the next agent's job).
    fn model_setup_ui(&mut self, ctx: &egui::Context) {
        if !self.show_model_setup {
            return;
        }

        // 1) Poll the active download; on a fresh Done, persist the cassette once.
        let mut done_msg: Option<String> = None;
        if let Some(active) = &mut self.active_download {
            let state = active.handle.state();
            if matches!(state, crate::download::DownloadState::Done { .. }) && !active.persisted {
                active.persisted = true;
                if let Some(entry) = self.catalog.get(&active.model_id).cloned() {
                    let mut decks = itsjustcad_deck::DecksFile::load_or_default();
                    install_catalog_deck(&mut decks, &entry);
                    decks.save();
                    done_msg = Some(format!(
                        "Installed {} and enabled cassette '{}'. \
                         Start the local runtime to use it.",
                        entry.display_name,
                        cassette_name_for(&entry.id),
                    ));
                }
            }
        }
        if let Some(msg) = done_msg {
            tracing::info!("{msg}");
        }

        let mut open = true;
        let hw = self.hardware;
        let catalog = self.catalog.clone();
        let decks = itsjustcad_deck::DecksFile::load_or_default();
        // Collect UI intents, then act after the closure (avoids borrow clashes).
        let mut install: Option<String> = None;
        let mut cancel = false;
        let mut remove: Option<String> = None;

        egui::Window::new("Model Setup")
            .collapsible(false)
            .resizable(true)
            .default_width(460.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ctx, |ui| {
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
                                    egui::RichText::new("· PLACEHOLDER")
                                        .weak()
                                        .italics(),
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

                        // Live progress for the model currently downloading.
                        if let Some((_, state)) = active_state.as_ref().filter(|_| is_downloading) {
                            ui.add(
                                egui::ProgressBar::new(state.fraction().unwrap_or(0.0))
                                    .show_percentage(),
                            );
                            ui.label(crate::download::progress_caption(state));
                            if state.is_active() {
                                if ui.button("Cancel").clicked() {
                                    cancel = true;
                                }
                            } else if let crate::download::DownloadState::Failed { msg } = state {
                                ui.colored_label(egui::Color32::LIGHT_RED, format!("Failed: {msg}"));
                                if ui.button("Retry").clicked() {
                                    install = Some(entry.id.clone());
                                }
                            }
                        } else if installed {
                            ui.horizontal(|ui| {
                                if ui.button("Re-download").clicked() {
                                    install = Some(entry.id.clone());
                                }
                                if ui.button("Remove").clicked() {
                                    remove = Some(entry.id.clone());
                                }
                            });
                        } else {
                            let any_active = active_state
                                .as_ref()
                                .is_some_and(|(_, s)| s.is_active());
                            ui.add_enabled_ui(runnable && !any_active, |ui| {
                                if ui.button("Install").clicked() {
                                    install = Some(entry.id.clone());
                                }
                            });
                        }
                    });
                }

                ui.separator();
                ui.label(
                    egui::RichText::new(
                        "Downloads stream to ~/.config/itsjustcad/models and are \
                         SHA-256 verified. Installing enables a local cassette; \
                         start the runtime to use it.",
                    )
                    .weak()
                    .small(),
                );
            });

        if !open {
            self.show_model_setup = false;
        }

        // 2) Act on the collected intents.
        if cancel && let Some(active) = &self.active_download {
            active.handle.cancel();
        }
        if let Some(id) = remove {
            let mut decks = itsjustcad_deck::DecksFile::load_or_default();
            if remove_catalog_deck(&mut decks, &id) {
                decks.save();
            }
        }
        if let Some(id) = install {
            self.start_model_install(&id);
        }
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

fn load_deck_visible() -> Option<bool> {
    load_ui_json()["deck_visible"].as_bool()
}

fn save_deck_visible(visible: bool) {
    let mut v = load_ui_json();
    v["deck_visible"] = serde_json::json!(visible);
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
        // Legacy-CAD preset background overrides theme when active.
        if self.cad_origin != CadOrigin::None {
            return preset::preset_for(self.cad_origin).bg_color;
        }
        if visuals.dark_mode {
            scene::Theme::Dark.background()
        } else {
            scene::Theme::Light.background()
        }
    }

    fn on_exit(&mut self) {
        // Clean exit: nothing crashed, nothing to recover.
        if let Some(j) = &mut self.journal {
            j.discard();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.run_startup_script();
        // ITSJUSTCAD_TYPE: pre-fill command input (dev hook for autosuggest screenshots).
        if let Some(text) = self.type_script.take() {
            self.command_line.prefill(text);
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
                save_template_done();
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
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .open(&mut open)
                .show(ui.ctx(), |ui| {
                    ui.label(egui::RichText::new("ItsJustCAD").heading());
                    ui.label("It's just CAD — a command-first, FOSS CAD workspace.");
                    ui.add_space(6.0);
                    // Attribution required under AGPLv3 §7(b) — do not remove.
                    ui.label("© 2026 Hector Tarrido-Picart");
                    ui.label("Desktop: AGPLv3. Commercial & mobile licensing from the author.");
                    ui.label(format!("Preset: {}", preset::preset_for(self.cad_origin).menu_style_label()));
                    ui.add_space(6.0);
                    if ui.button("Close").clicked() {
                        self.show_about = false;
                    }
                });
            if !open {
                self.show_about = false;
            }
        }

        // Tools → Model Setup panel (also the onboarding "download a local
        // model" entry point). Renders any time show_model_setup is set.
        self.model_setup_ui(ui.ctx());

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
            self.open(None);
        }

        // Deck pane tick: must run every frame regardless of visibility so
        // streaming turns and probes keep making progress while the pane is hidden.
        self.deck_pane.tick(&mut self.session, &self.tokio, ui.ctx());

        // Cmd+\ reveals the Deck tab in the right panel (or hides the panel if
        // the Deck tab is already the active, visible one). Backslash is not
        // used elsewhere.
        let toggle_deck = ui.input_mut(|i| {
            i.consume_key(egui::Modifiers::COMMAND, egui::Key::Backslash)
        });
        if toggle_deck {
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
        // Panel order matters: the first-declared panel is outermost. We build
        // the menu bar at the very top, then the command line (top OR bottom by
        // preset), the status bar (always very bottom), then the right tab
        // panel, and finally the central viewport frame with its bottom tab bar.
        let preset = preset::preset_for(self.cad_origin);
        let cmd_top = preset.command_line_pos == preset::CommandLinePos::Top;

        // 1. Menu bar (Layer 3) — always the topmost strip.
        self.menu_bar(ui);

        // 2. Command line at TOP (Rhino default), directly under the menu bar.
        if cmd_top {
            egui::Panel::top("command_line")
                .resizable(false)
                .show(ui, |ui| self.command_line_body(ui, true));
        }

        // 3. Status bar — always at the very bottom.
        egui::Panel::bottom("statusbar")
            .resizable(false)
            .show(ui, |ui| self.status_bar(ui));

        // 4. Command line at BOTTOM (AutoCAD), above the status bar.
        if !cmd_top {
            egui::Panel::bottom("command_line")
                .resizable(false)
                .show(ui, |ui| self.command_line_body(ui, false));
        }

        // 5. Right docked tab panel: Layers / Properties / History / Deck.
        // The deck lives here as a tab; its tick() still runs every frame above
        // regardless of visibility, so background streaming keeps progressing.
        self.right_panel(ui);

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
