// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! `render <prompt…>` — the app side of the diffusion render deck.
//!
//! The CAD owns the control images (`itsjustcad_render::render_control_images`
//! from the active viewport); this module owns the async job plumbing: parse
//! the verb, spawn the active [`itsjustcad_deck::RenderDeck`] cassette on the
//! tokio runtime, poll the oneshot each frame, and surface the diffused PNG in
//! a small result window. `render cancel` aborts the spawned task, which drops
//! the backend future mid-poll — real cancellation, not a flag.
//!
//! Ships with NO backend configured: the unconfigured cassette's guidance
//! message flows straight to the command line (never silent), mirroring the
//! LLM deck.

use itsjustcad_deck::{make_render_deck, RenderConfig, RenderDecksFile, RenderRequest, RenderedImage};
use tokio::sync::oneshot;

/// What a `render …` line asks for. Pure parse — unit-testable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenderAction {
    /// No words: print usage.
    Usage,
    /// `render cancel` — abort the in-flight job.
    Cancel,
    /// `render backends` / `render list` — list the configured cassettes.
    Backends,
    /// `render use <name>` — make the named cassette the active one.
    Use(String),
    /// `render test` — probe the active backend's reachability.
    Test,
    /// `render <prompt…>` — diffuse the current view with this prompt.
    Prompt(String),
}

/// Parse the words after the `render` verb. The setup subcommands are exact
/// short forms (`cancel` / `backends` / `list` / `test` / `use <name>`);
/// anything else — including a longer line that merely starts with one of
/// those words — is a diffusion prompt.
pub fn parse_render_words(words: &[String]) -> RenderAction {
    match words {
        [] => RenderAction::Usage,
        [w] if w == "cancel" => RenderAction::Cancel,
        [w] if w == "backends" || w == "list" => RenderAction::Backends,
        [w] if w == "test" => RenderAction::Test,
        [u, name] if u == "use" => RenderAction::Use(name.clone()),
        _ => RenderAction::Prompt(words.join(" ")),
    }
}

/// The `render` usage lines (shown for a bare `render`).
pub const USAGE: &[&str] = &[
    "usage: render <prompt...>   AI-diffuse the current view (needs a configured backend)",
    "       render cancel        abort the in-flight render",
    "       render backends      list the diffusion cassettes in render_decks.json",
    "       render use <name>    pick the active cassette",
    "       render test          check the active backend is reachable",
];

/// One command-line row per cassette in `decks`: active marker, name, kind,
/// endpoint, and whether it is ready to render. Pure — unit-testable.
pub fn backends_lines(decks: &RenderDecksFile) -> Vec<String> {
    decks
        .decks
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let marker = if i == decks.active { "▶" } else { " " };
            let kind = match d.kind {
                itsjustcad_deck::RenderKind::None => "off",
                itsjustcad_deck::RenderKind::Comfy => "comfyui",
                itsjustcad_deck::RenderKind::Automatic1111 => "a1111-api",
                itsjustcad_deck::RenderKind::Cloud => "cloud",
                itsjustcad_deck::RenderKind::LocalSd => "local-sd",
            };
            let ready = if d.is_configured() { "ready" } else { "not configured" };
            // LocalSd has no URL; show the model file so the row is still useful.
            let endpoint = if d.kind == itsjustcad_deck::RenderKind::LocalSd {
                d.model.as_str()
            } else {
                d.base_url.as_str()
            };
            let url = if endpoint.is_empty() { "-" } else { endpoint };
            format!("{marker} {:<12} {kind:<10} {url:<28} {ready}", d.name)
        })
        .collect()
}

/// `render use <name>`: point `decks.active` at the named cassette. Returns a
/// confirmation message, or an error that lists the available names — the
/// caller persists on `Ok`. Pure over `decks` — unit-testable.
pub fn select_backend(decks: &mut RenderDecksFile, name: &str) -> Result<String, String> {
    match decks.decks.iter().position(|d| d.name == name) {
        Some(i) => {
            decks.active = i;
            let d = &decks.decks[i];
            let note = if d.is_configured() {
                "— `render test` checks it"
            } else {
                "— NOT yet configured (edit render_decks.json, then `render test`)"
            };
            Ok(format!("active render backend: '{name}' {note}"))
        }
        None => {
            let names: Vec<&str> = decks.decks.iter().map(|d| d.name.as_str()).collect();
            Err(format!(
                "no render backend named '{name}' — available: {}",
                names.join(", ")
            ))
        }
    }
}

/// An in-flight `render test` connection probe (one cheap GET, spawned on the
/// tokio runtime; the UI thread only polls).
pub struct ConnTest {
    rx: oneshot::Receiver<Result<String, String>>,
}

impl ConnTest {
    pub fn start(handle: &tokio::runtime::Handle, config: &RenderConfig) -> Self {
        let config = config.clone();
        let (tx, rx) = oneshot::channel();
        handle.spawn(async move {
            let _ = tx.send(itsjustcad_deck::test_connection(&config).await);
        });
        Self { rx }
    }

    /// Non-blocking poll: `None` while pending, then the human verdict
    /// (Ok = reachable, Err = guidance). Never silent: a dropped task
    /// resolves to an error message.
    pub fn poll(&mut self) -> Option<Result<String, String>> {
        match self.rx.try_recv() {
            Ok(v) => Some(v),
            Err(oneshot::error::TryRecvError::Empty) => None,
            Err(oneshot::error::TryRecvError::Closed) => {
                Some(Err("connection test ended without a verdict".into()))
            }
        }
    }
}

/// Decode a PNG byte buffer into an [`egui::ColorImage`] for the result
/// window's texture upload. Pure — unit-testable.
pub fn decode_png(png: &[u8]) -> Result<egui::ColorImage, String> {
    let img = image::load_from_memory(png).map_err(|e| e.to_string())?.to_rgba8();
    let (w, h) = img.dimensions();
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [w as usize, h as usize],
        img.as_raw(),
    ))
}

/// An in-flight diffusion render. Dropping/aborting `task` cancels it.
pub struct RenderJob {
    rx: oneshot::Receiver<Result<RenderedImage, String>>,
    task: tokio::task::JoinHandle<()>,
    pub prompt: String,
    pub backend: String,
    pub started: std::time::Instant,
}

/// One poll step of an in-flight job.
pub enum RenderPoll {
    Pending,
    Done(RenderedImage),
    Failed(String),
}

impl RenderJob {
    /// Spawn the active cassette's render on the tokio runtime. All network
    /// happens inside the spawned task; the UI thread only polls.
    pub fn start(
        handle: &tokio::runtime::Handle,
        config: &RenderConfig,
        req: RenderRequest,
    ) -> Self {
        let prompt = req.prompt.clone();
        let backend = config.name.clone();
        let deck = make_render_deck(config);
        let (tx, rx) = oneshot::channel();
        let task = handle.spawn(async move {
            let result = deck.render(req).await.map_err(|e| e.to_string());
            let _ = tx.send(result);
        });
        Self { rx, task, prompt, backend, started: std::time::Instant::now() }
    }

    /// Non-blocking poll; call once per frame while the job exists.
    pub fn poll(&mut self) -> RenderPoll {
        match self.rx.try_recv() {
            Ok(Ok(img)) => RenderPoll::Done(img),
            Ok(Err(e)) => RenderPoll::Failed(e),
            Err(oneshot::error::TryRecvError::Empty) => RenderPoll::Pending,
            Err(oneshot::error::TryRecvError::Closed) => {
                RenderPoll::Failed("render task ended without a result".into())
            }
        }
    }

    /// Abort the spawned task (drops the backend future mid-await).
    pub fn cancel(self) {
        self.task.abort();
    }
}

/// Where a finished render is written: `render_<unix-secs>.png` in the private
/// runtime dir (0700), timestamped so successive renders never clobber.
pub fn result_path() -> std::path::PathBuf {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    crate::app::private_runtime_dir().join(format!("render_{secs}.png"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use itsjustcad_deck::ControlImages;

    fn words(s: &[&str]) -> Vec<String> {
        s.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn parses_usage_cancel_and_prompt() {
        assert_eq!(parse_render_words(&words(&[])), RenderAction::Usage);
        assert_eq!(parse_render_words(&words(&["cancel"])), RenderAction::Cancel);
        assert_eq!(
            parse_render_words(&words(&["glass", "pavilion", "at", "dusk"])),
            RenderAction::Prompt("glass pavilion at dusk".into())
        );
        // "cancel" as part of a longer prompt is a prompt, not an abort.
        assert_eq!(
            parse_render_words(&words(&["cancel", "culture", "museum"])),
            RenderAction::Prompt("cancel culture museum".into())
        );
    }

    #[test]
    fn parses_setup_subcommands() {
        assert_eq!(parse_render_words(&words(&["backends"])), RenderAction::Backends);
        assert_eq!(parse_render_words(&words(&["list"])), RenderAction::Backends);
        assert_eq!(parse_render_words(&words(&["test"])), RenderAction::Test);
        assert_eq!(
            parse_render_words(&words(&["use", "comfyui"])),
            RenderAction::Use("comfyui".into())
        );
        // Longer lines that merely start with a subcommand word are prompts.
        assert_eq!(
            parse_render_words(&words(&["test", "of", "time"])),
            RenderAction::Prompt("test of time".into())
        );
        assert_eq!(
            parse_render_words(&words(&["use", "of", "brick"])),
            RenderAction::Prompt("use of brick".into())
        );
    }

    #[test]
    fn backends_lines_mark_active_and_readiness() {
        let decks = itsjustcad_deck::RenderDecksFile::default();
        let lines = backends_lines(&decks);
        assert_eq!(lines.len(), decks.decks.len());
        // Ship default: 'none' active and not configured.
        assert!(lines[0].starts_with('▶'), "{}", lines[0]);
        assert!(lines[0].contains("not configured"), "{}", lines[0]);
        // The comfyui preset is ready (URL present) but not active.
        let comfy = lines.iter().find(|l| l.contains("comfyui ")).unwrap();
        assert!(!comfy.starts_with('▶'), "{comfy}");
        assert!(comfy.contains("http://localhost:8188"), "{comfy}");
        assert!(comfy.ends_with("ready"), "{comfy}");
    }

    #[test]
    fn select_backend_switches_or_lists_names() {
        let mut decks = itsjustcad_deck::RenderDecksFile::default();
        let msg = select_backend(&mut decks, "comfyui").expect("preset exists");
        assert!(msg.contains("'comfyui'"), "{msg}");
        assert_eq!(decks.active_config().name, "comfyui");
        // Switching to a preset that lacks config says so instead of `test`.
        let msg = select_backend(&mut decks, "none").unwrap();
        assert!(msg.contains("NOT yet configured"), "{msg}");
        // Unknown name: error lists what IS available.
        let err = select_backend(&mut decks, "sdxl-magic").unwrap_err();
        assert!(err.contains("sdxl-magic"), "{err}");
        assert!(err.contains("comfyui") && err.contains("a1111"), "{err}");
    }

    #[test]
    fn conn_test_unconfigured_resolves_with_guidance() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut t = ConnTest::start(rt.handle(), &RenderConfig::none());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let verdict = loop {
            if let Some(v) = t.poll() {
                break v;
            }
            assert!(std::time::Instant::now() < deadline, "conn test never resolved");
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        let err = verdict.unwrap_err();
        assert!(err.contains("no render backend configured"), "{err}");
    }

    #[test]
    fn decode_png_roundtrips_pixels() {
        // Encode a 2×1 RGBA PNG via the image crate, then decode through ours.
        let img = image::RgbaImage::from_raw(2, 1, vec![255, 0, 0, 255, 0, 255, 0, 255]).unwrap();
        let mut png = std::io::Cursor::new(Vec::new());
        img.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let ci = decode_png(png.get_ref()).expect("decode");
        assert_eq!(ci.size, [2, 1]);
        assert_eq!(ci.pixels[0], egui::Color32::from_rgba_unmultiplied(255, 0, 0, 255));
        assert_eq!(ci.pixels[1], egui::Color32::from_rgba_unmultiplied(0, 255, 0, 255));
        // Garbage bytes are an error, not a panic.
        assert!(decode_png(b"not a png").is_err());
    }

    #[test]
    fn unconfigured_job_fails_with_guidance_not_silence() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let control = ControlImages { depth: vec![1], edge: vec![2], mask: vec![3] };
        let req = RenderRequest::new("anything", control, 512, 512);
        let mut job = RenderJob::start(rt.handle(), &RenderConfig::none(), req);
        // The unconfigured deck resolves immediately; wait for the oneshot.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match job.poll() {
                RenderPoll::Failed(msg) => {
                    assert!(msg.contains("no render backend configured"), "{msg}");
                    break;
                }
                RenderPoll::Done(_) => panic!("unconfigured deck must not render"),
                RenderPoll::Pending => {
                    assert!(std::time::Instant::now() < deadline, "job never resolved");
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }
    }

    #[test]
    fn cancel_aborts_the_task() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let control = ControlImages { depth: vec![1], edge: vec![2], mask: vec![3] };
        // Unreachable local backend: the job would sit in connect/retry; abort
        // must kill it regardless.
        let cfg = RenderConfig {
            name: "comfy".into(),
            kind: itsjustcad_deck::RenderKind::Comfy,
            base_url: "http://127.0.0.1:1".into(),
            model: "m".into(),
            api_key: None,
            ..RenderConfig::none()
        };
        let job = RenderJob::start(rt.handle(), &cfg, RenderRequest::new("x", control, 64, 64));
        job.cancel(); // must not panic; the spawned future is dropped.
    }

    #[test]
    fn result_path_is_private_and_timestamped() {
        let p = result_path();
        assert!(p.starts_with(crate::app::private_runtime_dir()));
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("render_") && name.ends_with(".png"), "{name}");
    }
}
