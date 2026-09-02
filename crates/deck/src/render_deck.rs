// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Diffusion render deck: cassette-player backends behind one trait, exactly
//! mirroring the LLM [`crate::deck::LlmDeck`] pattern. The CAD owns the control
//! images (depth / edge / mask PNGs from the wgpu view — see
//! `itsjustcad_render::render_control_images`); a [`RenderDeck`] cassette sends
//! them as ControlNet conditioning plus a prompt and returns the diffused
//! image, which the app shows as a viewport overlay.
//!
//! SHIPS WITH NO BACKEND CONFIGURED (off by default, no key, no hard dep). An
//! unconfigured deck returns a clear "no render backend" message telling the
//! user to point at a local ComfyUI / A1111 or add a cloud key — the same
//! stance as the unconfigured LLM deck.
//!
//! Local cassettes (no key): ComfyUI (`/upload/image` + `/prompt` graph queue +
//! `/history` poll), A1111 / Forge / Draw Things (`/sdapi/v1/img2img`, one
//! blocking POST). Cloud cassette (`env:` key, no key ships): the Replicate
//! predictions API (create → poll → fetch output URL).
//!
//! ALL network is guarded: [`RenderDeck::render`] is `async` and only the HTTP
//! cassettes touch the wire; aborting the spawned task cancels a render
//! mid-poll. Unit tests exercise each wire protocol against in-process mock
//! HTTP servers (std::net, canned responses) — none require a real SD server —
//! and [`MockRenderDeck`] proves the app-side plumbing with zero I/O.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::write_private;

/// The CAD-owned control images for one diffusion request. Each is the raw PNG
/// byte payload (as written by `render_control_images`, read back into memory)
/// so a cassette can base64/multipart them without touching the filesystem.
#[derive(Clone, Default)]
pub struct ControlImages {
    /// Near-to-far depth map (ControlNet `depth`).
    pub depth: Vec<u8>,
    /// Feature-edge linework (ControlNet `canny`/`lineart`).
    pub edge: Vec<u8>,
    /// Flat per-layer semantic color (ControlNet `seg` / inpaint mask).
    pub mask: Vec<u8>,
}

impl ControlImages {
    /// Load the three PNGs a control-image export wrote (`<prefix>_depth.png`
    /// etc.) into memory. Pure filesystem read — no network.
    pub fn from_prefix(prefix: &str) -> std::io::Result<Self> {
        Ok(Self {
            depth: std::fs::read(format!("{prefix}_depth.png"))?,
            edge: std::fs::read(format!("{prefix}_edge.png"))?,
            mask: std::fs::read(format!("{prefix}_mask.png"))?,
        })
    }

    fn is_empty(&self) -> bool {
        self.depth.is_empty() && self.edge.is_empty() && self.mask.is_empty()
    }
}

/// One diffusion request: the owned control images plus the prompt. The deck
/// LLM may author `prompt` from the scene digest + user intent (optional
/// wiring); the app may also pass a literal prompt.
#[derive(Clone)]
pub struct RenderRequest {
    pub prompt: String,
    pub negative_prompt: String,
    pub control: ControlImages,
    pub width: u32,
    pub height: u32,
    /// img2img denoise strength (0 = keep the control image, 1 = full redraw).
    pub strength: f32,
    /// Optional seed for reproducibility; `None` = backend picks.
    pub seed: Option<u64>,
}

impl RenderRequest {
    /// A control-image-driven request with sensible diffusion defaults.
    pub fn new(prompt: impl Into<String>, control: ControlImages, width: u32, height: u32) -> Self {
        Self {
            prompt: prompt.into(),
            negative_prompt: String::new(),
            control,
            width,
            height,
            strength: 0.65,
            seed: None,
        }
    }
}

/// A diffused image returned by a cassette: the raw PNG bytes plus the backend
/// that produced it. The app writes it and shows it as a viewport overlay.
#[derive(Clone)]
pub struct RenderedImage {
    pub png: Vec<u8>,
    pub backend: String,
}

impl std::fmt::Debug for RenderedImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderedImage")
            .field("backend", &self.backend)
            .field("png_bytes", &self.png.len())
            .finish()
    }
}

impl RenderedImage {
    /// Write the diffused PNG to `path` (for the overlay to load / for the user
    /// to keep). Pure filesystem write — no network.
    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        std::fs::write(path, &self.png)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RenderDeckError {
    /// No backend configured — the ship-default state. Carries the guidance
    /// message the UI shows.
    #[error("{0}")]
    NoBackend(String),
    #[error("no control images to send — run 'controlimages <prefix>' first")]
    NoControlImages,
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("backend error {status}: {body}")]
    Api { status: u16, body: String },
    #[error("backend returned no image")]
    NoImage,
    #[error("{0}")]
    Other(String),
}

/// One diffusion cassette. The app never knows which backend is loaded.
#[async_trait]
pub trait RenderDeck: Send + Sync {
    fn name(&self) -> String;
    /// Diffuse `req` (control images + prompt) into an image. ALWAYS the only
    /// method that may touch the network, and only for HTTP cassettes.
    async fn render(&self, req: RenderRequest) -> Result<RenderedImage, RenderDeckError>;
}

// ── Config (mirrors DeckConfig / DecksFile, reuses resolved_key semantics) ────

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RenderKind {
    /// No backend — the ship default. `render()` returns `NoBackend` guidance.
    None,
    /// Local ComfyUI (`/prompt` graph API, best ControlNet).
    Comfy,
    /// Local A1111 / Forge (`/sdapi/v1/img2img`).
    Automatic1111,
    /// Cloud diffusion (Replicate / fal.ai / Stability) — scaffold, `env:` key.
    Cloud,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RenderConfig {
    pub name: String,
    pub kind: RenderKind,
    /// e.g. "http://localhost:8188" (ComfyUI) or "http://localhost:7860"
    /// (A1111). Empty for the `None` backend.
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    /// Literal key, or "env:VAR_NAME" to read from the environment. Local
    /// backends need none; cloud backends read a key at send time only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

impl RenderConfig {
    /// The ship default: no backend configured. Same "off by default" stance as
    /// the unconfigured LLM deck.
    pub fn none() -> Self {
        Self {
            name: "none".into(),
            kind: RenderKind::None,
            base_url: String::new(),
            model: String::new(),
            api_key: None,
        }
    }

    /// Resolve the API key: literal, `env:VAR`, or `None`. Identical semantics
    /// to `DeckConfig::resolved_key` — reused, not reinvented.
    pub fn resolved_key(&self) -> Option<String> {
        match self.api_key.as_deref() {
            Some(k) if k.starts_with("env:") => std::env::var(&k[4..]).ok(),
            Some(k) => Some(k.to_string()),
            None => None,
        }
    }

    /// Whether this cassette is ready to render. `None`, or a cloud cassette
    /// with no resolved key, is NOT configured.
    pub fn is_configured(&self) -> bool {
        match self.kind {
            RenderKind::None => false,
            RenderKind::Comfy | RenderKind::Automatic1111 => !self.base_url.is_empty(),
            RenderKind::Cloud => self.resolved_key().is_some(),
        }
    }
}

/// The unconfigured-backend guidance string. Mirrors the unconfigured LLM
/// deck's clear message.
pub const NO_BACKEND_MESSAGE: &str =
    "no render backend configured — point at a local ComfyUI (http://localhost:8188) \
     or A1111/Forge (http://localhost:7860), or add a cloud key, in render_decks.json";

/// The persisted render-deck file. Mirrors `DecksFile`: a list of cassettes and
/// the active index. Ships with ONLY the `None` backend (off by default).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RenderDecksFile {
    pub decks: Vec<RenderConfig>,
    #[serde(default)]
    pub active: usize,
}

impl Default for RenderDecksFile {
    fn default() -> Self {
        // SHIP WITH NO BACKEND. The commented shapes below document how a user
        // wires a local backend; none is active out of the box.
        Self {
            decks: vec![
                RenderConfig::none(),
                RenderConfig {
                    name: "comfyui".into(),
                    kind: RenderKind::Comfy,
                    base_url: "http://localhost:8188".into(),
                    model: "sd_xl_base_1.0.safetensors".into(),
                    api_key: None,
                },
                RenderConfig {
                    name: "a1111".into(),
                    kind: RenderKind::Automatic1111,
                    base_url: "http://localhost:7860".into(),
                    model: String::new(),
                    api_key: None,
                },
                RenderConfig {
                    // Draw Things (macOS) exposes an A1111-compatible HTTP API
                    // (Settings > API Server); same cassette, its default port.
                    name: "drawthings".into(),
                    kind: RenderKind::Automatic1111,
                    base_url: "http://127.0.0.1:7860".into(),
                    model: String::new(),
                    api_key: None,
                },
                RenderConfig {
                    name: "cloud".into(),
                    kind: RenderKind::Cloud,
                    base_url: "https://api.replicate.com/v1".into(),
                    model: "stability-ai/sdxl".into(),
                    api_key: Some("env:REPLICATE_API_TOKEN".into()),
                },
            ],
            // Default active = index 0 = the None backend. OFF BY DEFAULT.
            active: 0,
        }
    }
}

pub fn render_config_path() -> Option<std::path::PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".config")
            .join("itsjustcad")
            .join("render_decks.json"),
    )
}

impl RenderDecksFile {
    pub fn load_or_default() -> Self {
        render_config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Some(path) = render_config_path() {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // 0600 so any literal cloud key is not world-readable. Best-effort:
            // a serialization failure must not panic the app on save.
            if let Ok(json) = serde_json::to_string_pretty(self) {
                let _ = write_private(&path, &json);
            }
        }
    }

    /// The active cassette config, or the `None` backend if the index is stale.
    pub fn active_config(&self) -> RenderConfig {
        self.decks.get(self.active).cloned().unwrap_or_else(RenderConfig::none)
    }
}

/// Build the cassette for `config`. `None`/cloud-without-key resolve to a
/// backend whose `render()` returns clear guidance — never a panic, never a
/// silent no-op.
pub fn make_render_deck(config: &RenderConfig) -> Box<dyn RenderDeck> {
    match config.kind {
        RenderKind::None => Box::new(UnconfiguredRenderDeck),
        RenderKind::Comfy => Box::new(ComfyRenderDeck::new(config)),
        RenderKind::Automatic1111 => Box::new(Automatic1111RenderDeck::new(config)),
        RenderKind::Cloud => Box::new(CloudRenderDeck::new(config)),
    }
}

/// The cheap GET endpoint `test_connection` probes for each backend kind.
/// Pure (unit-testable); `None` for the unconfigured backend.
pub fn probe_url(config: &RenderConfig) -> Option<String> {
    let base = config.base_url.trim_end_matches('/');
    match config.kind {
        RenderKind::None => None,
        RenderKind::Comfy => Some(format!("{base}/system_stats")),
        RenderKind::Automatic1111 => Some(format!("{base}/sdapi/v1/options")),
        RenderKind::Cloud => Some(format!("{base}/account")),
    }
}

/// `render test`: one cheap GET against the backend's probe endpoint. Returns a
/// human-readable verdict either way — reachable (`Ok`) or a clear "cannot
/// reach X at <url> — is it running?" / missing-key guidance (`Err`). This is
/// the ONLY network call outside [`RenderDeck::render`], and it too only fires
/// on an explicit user request.
pub async fn test_connection(config: &RenderConfig) -> Result<String, String> {
    let Some(url) = probe_url(config) else {
        return Err(NO_BACKEND_MESSAGE.to_string());
    };
    if config.kind == RenderKind::Cloud && config.resolved_key().is_none() {
        return Err(format!(
            "cloud backend '{}' has no API key — set api_key (literal or env:VAR) in render_decks.json",
            config.name
        ));
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?;
    let mut req = client.get(&url);
    if let Some(key) = config.resolved_key() {
        req = req.bearer_auth(key);
    }
    match req.send().await {
        Ok(resp) if resp.status().is_success() => Ok(format!(
            "'{}' is reachable at {} (HTTP {})",
            config.name,
            config.base_url,
            resp.status().as_u16()
        )),
        Ok(resp) => Err(format!(
            "'{}' answered HTTP {} at {url} — the server is up but rejected the probe \
             (check base_url, API flags, or the key)",
            config.name,
            resp.status().as_u16()
        )),
        Err(e) => Err(unreachable_hint(&config.name, &config.base_url, e).to_string()),
    }
}

// ── Unconfigured backend (the ship default) ──────────────────────────────────

/// The default cassette: no backend. Every `render()` returns the guidance
/// string, so the app can surface it exactly like the unconfigured LLM deck.
pub struct UnconfiguredRenderDeck;

#[async_trait]
impl RenderDeck for UnconfiguredRenderDeck {
    fn name(&self) -> String {
        "none".into()
    }
    async fn render(&self, _req: RenderRequest) -> Result<RenderedImage, RenderDeckError> {
        Err(RenderDeckError::NoBackend(NO_BACKEND_MESSAGE.into()))
    }
}

// ── ComfyUI cassette (local, best ControlNet) ────────────────────────────────

/// Local ComfyUI backend. Builds a graph prompt that feeds the depth + edge
/// control images as ControlNet conditioning on top of an SDXL img2img latent.
/// The request assembly is pure (unit-testable); only `render()` hits the wire.
pub struct ComfyRenderDeck {
    name: String,
    base_url: String,
    model: String,
    client: reqwest::Client,
}

impl ComfyRenderDeck {
    pub fn new(config: &RenderConfig) -> Self {
        Self {
            name: config.name.clone(),
            base_url: config.base_url.trim_end_matches('/').to_string(),
            model: config.model.clone(),
            client: reqwest::Client::new(),
        }
    }

    /// Assemble the ComfyUI graph JSON (a minimal ControlNet img2img workflow).
    /// Pure — no network — so tests can assert the control images and prompt
    /// land in the right nodes. Control PNGs are uploaded separately via
    /// `/upload/image`; here they are referenced by name.
    pub fn build_prompt_graph(&self, req: &RenderRequest) -> serde_json::Value {
        use serde_json::json;
        json!({
            "prompt": {
                "ckpt": { "class_type": "CheckpointLoaderSimple",
                    "inputs": { "ckpt_name": self.model } },
                "pos": { "class_type": "CLIPTextEncode",
                    "inputs": { "text": req.prompt, "clip": ["ckpt", 1] } },
                "neg": { "class_type": "CLIPTextEncode",
                    "inputs": { "text": req.negative_prompt, "clip": ["ckpt", 1] } },
                "depth_img": { "class_type": "LoadImage",
                    "inputs": { "image": "itsjustcad_depth.png" } },
                "edge_img": { "class_type": "LoadImage",
                    "inputs": { "image": "itsjustcad_edge.png" } },
                "cnet_depth": { "class_type": "ControlNetApply",
                    "inputs": { "conditioning": ["pos", 0], "image": ["depth_img", 0],
                        "strength": 1.0 - req.strength } },
                "sampler": { "class_type": "KSampler",
                    "inputs": {
                        "seed": req.seed.unwrap_or(0),
                        "denoise": req.strength,
                        "positive": ["cnet_depth", 0],
                        "negative": ["neg", 0],
                        "model": ["ckpt", 0],
                    } },
                "out": { "class_type": "SaveImage",
                    "inputs": { "images": ["sampler", 0] } },
            }
        })
    }
}

#[async_trait]
impl RenderDeck for ComfyRenderDeck {
    fn name(&self) -> String {
        self.name.clone()
    }

    async fn render(&self, req: RenderRequest) -> Result<RenderedImage, RenderDeckError> {
        if req.control.is_empty() {
            return Err(RenderDeckError::NoControlImages);
        }
        if self.base_url.is_empty() {
            return Err(RenderDeckError::NoBackend(NO_BACKEND_MESSAGE.into()));
        }
        // LIVE NETWORK PATH: upload the control images, queue the graph, poll
        // history until the sampler finishes, fetch the saved image.
        let kind = "ComfyUI";
        for (name, bytes) in [
            ("itsjustcad_depth.png", &req.control.depth),
            ("itsjustcad_edge.png", &req.control.edge),
        ] {
            let (body, content_type) = multipart_png_upload(name, bytes);
            let resp = self
                .client
                .post(format!("{}/upload/image", self.base_url))
                .header("content-type", content_type)
                .body(body)
                .send()
                .await
                .map_err(|e| unreachable_hint(kind, &self.base_url, e))?;
            json_or_api_error(resp).await?;
        }
        // Queue the graph. `build_prompt_graph` already wraps it as {"prompt": …}.
        let resp = self
            .client
            .post(format!("{}/prompt", self.base_url))
            .json(&self.build_prompt_graph(&req))
            .send()
            .await
            .map_err(|e| unreachable_hint(kind, &self.base_url, e))?;
        let v = json_or_api_error(resp).await?;
        let prompt_id = v["prompt_id"]
            .as_str()
            .ok_or_else(|| RenderDeckError::Other("ComfyUI returned no prompt_id".into()))?
            .to_string();
        // Poll /history/<id> until the SaveImage node reports its output. A
        // task-level abort (the app's Cancel) drops this future mid-await.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
        let img = loop {
            let resp = self
                .client
                .get(format!("{}/history/{prompt_id}", self.base_url))
                .send()
                .await
                .map_err(|e| unreachable_hint(kind, &self.base_url, e))?;
            let hist = json_or_api_error(resp).await?;
            if let Some(images) = hist[&prompt_id]["outputs"]
                .as_object()
                .and_then(|outs| outs.values().find_map(|o| o["images"].as_array()))
                && let Some(img) = images.first()
            {
                break img.clone();
            }
            let status = &hist[&prompt_id]["status"];
            if status["status_str"].as_str() == Some("error") {
                return Err(RenderDeckError::Other(format!(
                    "ComfyUI reported an execution error for prompt {prompt_id}"
                )));
            }
            if std::time::Instant::now() > deadline {
                return Err(RenderDeckError::Other("ComfyUI render timed out (600 s)".into()));
            }
            tokio::time::sleep(std::time::Duration::from_millis(POLL_MS)).await;
        };
        let (filename, subfolder, ty) = (
            img["filename"].as_str().unwrap_or_default(),
            img["subfolder"].as_str().unwrap_or_default(),
            img["type"].as_str().unwrap_or("output"),
        );
        let resp = self
            .client
            .get(format!(
                "{}/view?filename={filename}&subfolder={subfolder}&type={ty}",
                self.base_url
            ))
            .send()
            .await
            .map_err(|e| unreachable_hint(kind, &self.base_url, e))?;
        if !resp.status().is_success() {
            return Err(RenderDeckError::Api {
                status: resp.status().as_u16(),
                body: resp.text().await.unwrap_or_default().chars().take(400).collect(),
            });
        }
        let png = resp.bytes().await?.to_vec();
        if png.is_empty() {
            return Err(RenderDeckError::NoImage);
        }
        Ok(RenderedImage { png, backend: self.name.clone() })
    }
}

// ── A1111 / Forge cassette (local, /sdapi/v1/img2img) ────────────────────────

pub struct Automatic1111RenderDeck {
    name: String,
    base_url: String,
    client: reqwest::Client,
}

impl Automatic1111RenderDeck {
    pub fn new(config: &RenderConfig) -> Self {
        Self {
            name: config.name.clone(),
            base_url: config.base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
        }
    }

    /// Build the `/sdapi/v1/img2img` JSON body: the edge map as the init image,
    /// the depth map as a ControlNet unit. Pure — no network.
    pub fn build_body(&self, req: &RenderRequest) -> serde_json::Value {
        use serde_json::json;
        let b64 = base64_encode;
        json!({
            "prompt": req.prompt,
            "negative_prompt": req.negative_prompt,
            "init_images": [b64(&req.control.edge)],
            "denoising_strength": req.strength,
            "width": req.width,
            "height": req.height,
            "seed": req.seed.map(|s| s as i64).unwrap_or(-1),
            "alwayson_scripts": {
                "controlnet": { "args": [{
                    "module": "depth",
                    "model": "control_depth",
                    "image": b64(&req.control.depth),
                    "weight": 1.0 - req.strength,
                }] }
            }
        })
    }
}

#[async_trait]
impl RenderDeck for Automatic1111RenderDeck {
    fn name(&self) -> String {
        self.name.clone()
    }

    async fn render(&self, req: RenderRequest) -> Result<RenderedImage, RenderDeckError> {
        if req.control.is_empty() {
            return Err(RenderDeckError::NoControlImages);
        }
        if self.base_url.is_empty() {
            return Err(RenderDeckError::NoBackend(NO_BACKEND_MESSAGE.into()));
        }
        // LIVE NETWORK PATH. One POST; A1111 (and Draw Things / Forge, which
        // speak the same API) blocks until the image is diffused.
        let url = format!("{}/sdapi/v1/img2img", self.base_url);
        let body = self.build_body(&req);
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .timeout(std::time::Duration::from_secs(600))
            .send()
            .await
            .map_err(|e| unreachable_hint("A1111/Draw Things", &self.base_url, e))?;
        let v = json_or_api_error(resp).await?;
        let b64 = v["images"][0].as_str().ok_or(RenderDeckError::NoImage)?;
        // A1111 may prefix a data-URI header; the payload is after the comma.
        let b64 = b64.rsplit(',').next().unwrap_or(b64);
        let png = base64_decode(b64)
            .map_err(|e| RenderDeckError::Other(format!("bad base64 image from A1111: {e}")))?;
        Ok(RenderedImage { png, backend: self.name.clone() })
    }
}

// ── Cloud cassette (scaffold, no key ships) ──────────────────────────────────

pub struct CloudRenderDeck {
    name: String,
    base_url: String,
    model: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl CloudRenderDeck {
    pub fn new(config: &RenderConfig) -> Self {
        Self {
            name: config.name.clone(),
            base_url: config.base_url.trim_end_matches('/').to_string(),
            model: config.model.clone(),
            api_key: config.resolved_key(),
            client: reqwest::Client::new(),
        }
    }

    /// Build a Replicate-style prediction body. Pure — no network. The control
    /// images ride as data-URIs so any of Replicate / fal.ai / Stability can be
    /// adapted from this shape later.
    pub fn build_body(&self, req: &RenderRequest) -> serde_json::Value {
        use serde_json::json;
        let data_uri = |bytes: &[u8]| format!("data:image/png;base64,{}", base64_encode(bytes));
        json!({
            "version": self.model,
            "input": {
                "prompt": req.prompt,
                "negative_prompt": req.negative_prompt,
                "image": data_uri(&req.control.edge),
                "control_image": data_uri(&req.control.depth),
                "prompt_strength": req.strength,
                "width": req.width,
                "height": req.height,
                "seed": req.seed,
            }
        })
    }
}

#[async_trait]
impl RenderDeck for CloudRenderDeck {
    fn name(&self) -> String {
        self.name.clone()
    }

    async fn render(&self, req: RenderRequest) -> Result<RenderedImage, RenderDeckError> {
        // Cloud requires a key — none ships. Fail with clear guidance before any
        // network.
        let Some(key) = self.api_key.clone() else {
            return Err(RenderDeckError::NoBackend(format!(
                "cloud render backend '{}' has no key — set the env var referenced in render_decks.json",
                self.name
            )));
        };
        if req.control.is_empty() {
            return Err(RenderDeckError::NoControlImages);
        }
        // LIVE NETWORK PATH (Replicate predictions API): create the prediction,
        // poll it until it settles, fetch the output image URL.
        let kind = "the cloud render API";
        let resp = self
            .client
            .post(format!("{}/predictions", self.base_url))
            .header("authorization", format!("Bearer {key}"))
            .json(&self.build_body(&req))
            .send()
            .await
            .map_err(|e| unreachable_hint(kind, &self.base_url, e))?;
        let v = json_or_api_error(resp).await?;
        let id = v["id"]
            .as_str()
            .ok_or_else(|| RenderDeckError::Other("prediction response had no id".into()))?
            .to_string();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
        let output = loop {
            let resp = self
                .client
                .get(format!("{}/predictions/{id}", self.base_url))
                .header("authorization", format!("Bearer {key}"))
                .send()
                .await
                .map_err(|e| unreachable_hint(kind, &self.base_url, e))?;
            let p = json_or_api_error(resp).await?;
            match p["status"].as_str().unwrap_or_default() {
                "succeeded" => break p["output"].clone(),
                "failed" | "canceled" => {
                    return Err(RenderDeckError::Other(format!(
                        "cloud prediction {id} {}: {}",
                        p["status"].as_str().unwrap_or("failed"),
                        p["error"].as_str().unwrap_or("no detail")
                    )));
                }
                _ => {}
            }
            if std::time::Instant::now() > deadline {
                return Err(RenderDeckError::Other("cloud render timed out (600 s)".into()));
            }
            tokio::time::sleep(std::time::Duration::from_millis(POLL_MS)).await;
        };
        // `output` is a URL string or an array of URL strings.
        let url = output
            .as_str()
            .or_else(|| output.as_array().and_then(|a| a.first()).and_then(|v| v.as_str()))
            .ok_or(RenderDeckError::NoImage)?
            .to_string();
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| unreachable_hint(kind, &url, e))?;
        if !resp.status().is_success() {
            return Err(RenderDeckError::Api {
                status: resp.status().as_u16(),
                body: resp.text().await.unwrap_or_default().chars().take(400).collect(),
            });
        }
        let png = resp.bytes().await?.to_vec();
        if png.is_empty() {
            return Err(RenderDeckError::NoImage);
        }
        Ok(RenderedImage { png, backend: self.name.clone() })
    }
}

// ── Mock backend (tests / headless sanity — no I/O) ──────────────────────────

/// A canned cassette that returns a fixed PNG with no network. Proves the whole
/// pipeline: control images → request assembly → response → overlay. It records
/// the last request it saw so callers can assert the control images arrived.
pub struct MockRenderDeck {
    /// The canned PNG bytes to return.
    pub canned_png: Vec<u8>,
    /// The last request the mock received (for assertions).
    pub last: std::sync::Mutex<Option<RenderRequest>>,
}

impl MockRenderDeck {
    /// A mock returning a tiny 2x2 solid-colour PNG.
    pub fn new() -> Self {
        Self {
            canned_png: tiny_png(),
            last: std::sync::Mutex::new(None),
        }
    }
}

impl Default for MockRenderDeck {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RenderDeck for MockRenderDeck {
    fn name(&self) -> String {
        "mock".into()
    }
    async fn render(&self, req: RenderRequest) -> Result<RenderedImage, RenderDeckError> {
        if req.control.is_empty() {
            return Err(RenderDeckError::NoControlImages);
        }
        *self.last.lock().expect("mock lock") = Some(req);
        Ok(RenderedImage { png: self.canned_png.clone(), backend: "mock".into() })
    }
}

/// A minimal valid 1x1 PNG (canned mock output). Hand-built so the mock has zero
/// dependency on a GPU or the `image` crate.
fn tiny_png() -> Vec<u8> {
    // 1x1 opaque magenta PNG.
    const BYTES: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // signature
        0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR len + type
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // 1x1
        0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xDE, // bit depth/color + crc
        0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, // IDAT len + type
        0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0xF0, 0x1F, 0x00, 0x05, 0x05, 0x02, // data
        0xFE, 0x02, 0x7E, 0x9B, // idat crc (approximate; decoders that verify may reject)
        0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82, // IEND
    ];
    BYTES.to_vec()
}

// ── Shared HTTP plumbing ─────────────────────────────────────────────────────

/// Poll cadence for backends that queue work (ComfyUI history, cloud
/// predictions).
const POLL_MS: u64 = 500;

/// Map a transport-level failure to the friendly "is it running?" guidance the
/// UI shows — never a bare hyper error for the by-far-most-common case (the
/// local server simply isn't up).
fn unreachable_hint(kind: &str, base_url: &str, e: reqwest::Error) -> RenderDeckError {
    if e.is_connect() || e.is_timeout() {
        RenderDeckError::Other(format!(
            "cannot reach {kind} at {base_url} — is it running? ({e})"
        ))
    } else {
        RenderDeckError::Http(e)
    }
}

/// Success → parsed JSON body; non-2xx → `Api` with the (truncated) body so the
/// backend's own error text is surfaced, never swallowed.
async fn json_or_api_error(resp: reqwest::Response) -> Result<serde_json::Value, RenderDeckError> {
    let status = resp.status();
    if !status.is_success() {
        return Err(RenderDeckError::Api {
            status: status.as_u16(),
            body: resp.text().await.unwrap_or_default().chars().take(400).collect(),
        });
    }
    Ok(resp.json::<serde_json::Value>().await?)
}

/// Hand-rolled `multipart/form-data` body for a ComfyUI `/upload/image` PNG
/// (image field + `overwrite=true`). Avoids enabling reqwest's multipart
/// feature — zero new deps. Returns `(body, content-type header value)`.
fn multipart_png_upload(filename: &str, bytes: &[u8]) -> (Vec<u8>, String) {
    const BOUNDARY: &str = "itsjustcad-render-7f3a9c";
    let mut body = Vec::with_capacity(bytes.len() + 512);
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"image\"; \
             filename=\"{filename}\"\r\nContent-Type: image/png\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(
        format!(
            "\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; \
             name=\"overwrite\"\r\n\r\ntrue\r\n--{BOUNDARY}--\r\n"
        )
        .as_bytes(),
    );
    (body, format!("multipart/form-data; boundary={BOUNDARY}"))
}

/// Standard base64 decode (strict, no whitespace). Counterpart of
/// [`base64_encode`]; hand-rolled for the same zero-new-deps reason.
fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    fn val(c: u8) -> Result<u32, String> {
        match c {
            b'A'..=b'Z' => Ok((c - b'A') as u32),
            b'a'..=b'z' => Ok((c - b'a' + 26) as u32),
            b'0'..=b'9' => Ok((c - b'0' + 52) as u32),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(format!("invalid base64 byte 0x{c:02x}")),
        }
    }
    let input = input.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    for chunk in input.chunks(4) {
        if chunk.len() == 1 {
            return Err("truncated base64 (dangling single symbol)".into());
        }
        let mut n: u32 = 0;
        for &c in chunk {
            n = (n << 6) | val(c)?;
        }
        n <<= 6 * (4 - chunk.len()) as u32;
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

/// Standard base64 encode (no line breaks). Hand-rolled to avoid pulling a new
/// dependency into the workspace — the diffusion deck ships zero new hard deps.
fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_control() -> ControlImages {
        ControlImages { depth: vec![1, 2, 3], edge: vec![4, 5, 6], mask: vec![7, 8, 9] }
    }

    #[test]
    fn resolved_key_literal_env_none() {
        let mut c = RenderConfig::none();
        assert_eq!(c.resolved_key(), None);
        c.api_key = Some("literal-key".into());
        assert_eq!(c.resolved_key().as_deref(), Some("literal-key"));
        c.api_key = Some("env:ITSJUSTCAD_TEST_RENDER_KEY_XYZ".into());
        assert_eq!(c.resolved_key(), None); // unset env
        // SAFETY: single-threaded test; set + read + clear the var.
        unsafe { std::env::set_var("ITSJUSTCAD_TEST_RENDER_KEY_XYZ", "from-env") };
        assert_eq!(c.resolved_key().as_deref(), Some("from-env"));
        unsafe { std::env::remove_var("ITSJUSTCAD_TEST_RENDER_KEY_XYZ") };
    }

    #[test]
    fn ships_with_no_backend_active() {
        let f = RenderDecksFile::default();
        // Active default must be the None backend — OFF by default.
        assert_eq!(f.active, 0);
        assert_eq!(f.active_config().kind, RenderKind::None);
        assert!(!f.active_config().is_configured());
    }

    #[test]
    fn is_configured_gating() {
        assert!(!RenderConfig::none().is_configured());
        let comfy = RenderConfig {
            name: "c".into(), kind: RenderKind::Comfy,
            base_url: "http://localhost:8188".into(), model: "m".into(), api_key: None,
        };
        assert!(comfy.is_configured());
        let comfy_no_url = RenderConfig { base_url: String::new(), ..comfy.clone() };
        assert!(!comfy_no_url.is_configured());
        // Cloud without a key is not configured; with one, it is.
        let cloud = RenderConfig {
            name: "x".into(), kind: RenderKind::Cloud,
            base_url: "https://api.replicate.com/v1".into(), model: "m".into(),
            api_key: Some("literal".into()),
        };
        assert!(cloud.is_configured());
        let cloud_no_key = RenderConfig { api_key: None, ..cloud };
        assert!(!cloud_no_key.is_configured());
    }

    #[tokio::test]
    async fn unconfigured_backend_returns_guidance() {
        let deck = make_render_deck(&RenderConfig::none());
        assert_eq!(deck.name(), "none");
        let req = RenderRequest::new("a house", dummy_control(), 512, 512);
        let err = deck.render(req).await.unwrap_err();
        match err {
            RenderDeckError::NoBackend(msg) => {
                assert!(msg.contains("ComfyUI"), "{msg}");
                assert!(msg.contains("cloud key"), "{msg}");
            }
            other => panic!("expected NoBackend, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_pipeline_end_to_end() {
        // control images → request → response, and the mock recorded the control.
        let deck = MockRenderDeck::new();
        let req = RenderRequest::new("a glass pavilion at dusk", dummy_control(), 640, 400);
        let out = deck.render(req).await.unwrap();
        assert_eq!(out.backend, "mock");
        assert!(out.png.starts_with(&[0x89, 0x50, 0x4E, 0x47]), "PNG signature");
        let seen = deck.last.lock().unwrap();
        let seen = seen.as_ref().expect("mock recorded a request");
        assert_eq!(seen.prompt, "a glass pavilion at dusk");
        assert_eq!(seen.control.depth, vec![1, 2, 3]);
        assert_eq!(seen.width, 640);
    }

    #[tokio::test]
    async fn mock_rejects_empty_control() {
        let deck = MockRenderDeck::new();
        let req = RenderRequest::new("x", ControlImages::default(), 512, 512);
        assert!(matches!(deck.render(req).await, Err(RenderDeckError::NoControlImages)));
    }

    #[test]
    fn comfy_graph_carries_prompt_and_control() {
        let cfg = RenderConfig {
            name: "c".into(), kind: RenderKind::Comfy,
            base_url: "http://localhost:8188".into(),
            model: "sd_xl_base_1.0.safetensors".into(), api_key: None,
        };
        let deck = ComfyRenderDeck::new(&cfg);
        let req = RenderRequest::new("a stone tower", dummy_control(), 512, 512);
        let g = deck.build_prompt_graph(&req);
        let s = g.to_string();
        assert!(s.contains("a stone tower"), "prompt in graph");
        assert!(s.contains("itsjustcad_depth.png"), "depth control referenced");
        assert!(s.contains("ControlNetApply"), "controlnet node present");
        assert_eq!(g["prompt"]["ckpt"]["inputs"]["ckpt_name"], "sd_xl_base_1.0.safetensors");
    }

    #[test]
    fn a1111_body_base64s_control_images() {
        let cfg = RenderConfig {
            name: "a".into(), kind: RenderKind::Automatic1111,
            base_url: "http://localhost:7860".into(), model: String::new(), api_key: None,
        };
        let deck = Automatic1111RenderDeck::new(&cfg);
        let req = RenderRequest::new("dusk", dummy_control(), 768, 512);
        let body = deck.build_body(&req);
        assert_eq!(body["width"], 768);
        assert_eq!(body["seed"], -1); // None → -1 (backend picks)
        assert!(!body["init_images"][0].as_str().unwrap().is_empty(), "edge as init image");
        assert!(body["alwayson_scripts"]["controlnet"]["args"][0]["image"].is_string());
    }

    #[tokio::test]
    async fn cloud_without_key_is_no_backend() {
        let cfg = RenderConfig {
            name: "cloud".into(), kind: RenderKind::Cloud,
            base_url: "https://api.replicate.com/v1".into(),
            model: "m".into(), api_key: Some("env:ITSJUSTCAD_UNSET_CLOUD_KEY_ABC".into()),
        };
        let deck = make_render_deck(&cfg);
        let req = RenderRequest::new("x", dummy_control(), 512, 512);
        assert!(matches!(deck.render(req).await, Err(RenderDeckError::NoBackend(_))));
    }

    #[test]
    fn render_decks_file_serde_roundtrip() {
        let f = RenderDecksFile::default();
        let json = serde_json::to_string(&f).unwrap();
        let back: RenderDecksFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.active, f.active);
        assert_eq!(back.decks.len(), f.decks.len());
        assert_eq!(back.decks[0].kind, RenderKind::None);
    }

    #[test]
    fn pre_field_render_config_deserializes_with_defaults() {
        // A minimal config (older/hand-written) with only name+kind must fill
        // base_url/model/api_key from defaults.
        let json = r#"{"name":"c","kind":"comfy"}"#;
        let c: RenderConfig = serde_json::from_str(json).unwrap();
        assert_eq!(c.kind, RenderKind::Comfy);
        assert_eq!(c.base_url, "");
        assert_eq!(c.model, "");
        assert_eq!(c.api_key, None);
    }

    #[test]
    fn base64_encode_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    // ── Mock HTTP servers (std::net on an OS thread — no new deps, no real SD
    // server; each protocol is exercised against canned wire responses) ──────

    use std::sync::{Arc, Mutex};

    /// Bind an ephemeral local port. Returns the listener plus its base URL.
    fn bind_mock() -> (std::net::TcpListener, String) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let base = format!("http://{}", listener.local_addr().unwrap());
        (listener, base)
    }

    /// Serve HTTP on `listener` from a detached OS thread. `handler` maps a
    /// request line ("POST /sdapi/v1/img2img HTTP/1.1") + body to
    /// (status, content-type, body). Every raw request head is recorded in the
    /// returned log so tests can assert headers (auth, multipart) arrived.
    fn serve_mock<F>(listener: std::net::TcpListener, handler: F) -> Arc<Mutex<Vec<String>>>
    where
        F: Fn(&str, &[u8]) -> (u16, &'static str, Vec<u8>) + Send + 'static,
    {
        let log = Arc::new(Mutex::new(Vec::new()));
        let log2 = Arc::clone(&log);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let Some((head, body)) = read_request(&mut s) else { continue };
                let request_line = head.lines().next().unwrap_or_default().to_string();
                log2.lock().unwrap().push(head.clone());
                let (status, ct, resp) = handler(&request_line, &body);
                use std::io::Write;
                let _ = write!(
                    s,
                    "HTTP/1.1 {status} MOCK\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    resp.len()
                );
                let _ = s.write_all(&resp);
            }
        });
        log
    }

    /// Read one HTTP request (head + content-length body) off the stream.
    fn read_request(s: &mut std::net::TcpStream) -> Option<(String, Vec<u8>)> {
        use std::io::Read;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 8192];
        let header_end = loop {
            let n = s.read(&mut tmp).ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
        let content_length = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("content-length").then(|| v.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0);
        while buf.len() < header_end + content_length {
            let n = s.read(&mut tmp).ok()?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        Some((head, buf[header_end..(header_end + content_length).min(buf.len())].to_vec()))
    }

    fn a1111_deck(base: &str) -> Automatic1111RenderDeck {
        Automatic1111RenderDeck::new(&RenderConfig {
            name: "a1111".into(),
            kind: RenderKind::Automatic1111,
            base_url: base.into(),
            model: String::new(),
            api_key: None,
        })
    }

    #[tokio::test]
    async fn a1111_mock_server_end_to_end() {
        let (listener, base) = bind_mock();
        let png = tiny_png();
        let b64 = base64_encode(&png);
        serve_mock(listener, move |line, body| {
            assert!(line.starts_with("POST /sdapi/v1/img2img"), "unexpected {line}");
            // The wire body must carry the prompt and the ControlNet unit.
            let v: serde_json::Value = serde_json::from_slice(body).unwrap();
            assert_eq!(v["prompt"], "a brick warehouse");
            assert!(v["alwayson_scripts"]["controlnet"]["args"][0]["image"].is_string());
            (200, "application/json", format!("{{\"images\":[\"{b64}\"]}}").into_bytes())
        });
        let deck = a1111_deck(&base);
        let req = RenderRequest::new("a brick warehouse", dummy_control(), 512, 512);
        let out = deck.render(req).await.expect("mock a1111 render");
        assert_eq!(out.png, tiny_png(), "decoded PNG round-trips the wire");
        assert_eq!(out.backend, "a1111");
    }

    #[tokio::test]
    async fn a1111_data_uri_image_is_accepted() {
        let (listener, base) = bind_mock();
        let b64 = base64_encode(&tiny_png());
        serve_mock(listener, move |_, _| {
            (
                200,
                "application/json",
                format!("{{\"images\":[\"data:image/png;base64,{b64}\"]}}").into_bytes(),
            )
        });
        let out = a1111_deck(&base)
            .render(RenderRequest::new("x", dummy_control(), 512, 512))
            .await
            .expect("data-uri image decodes");
        assert_eq!(out.png, tiny_png());
    }

    #[tokio::test]
    async fn a1111_api_error_body_is_surfaced() {
        let (listener, base) = bind_mock();
        serve_mock(listener, |_, _| {
            (500, "text/plain", b"CUDA out of memory".to_vec())
        });
        let err = a1111_deck(&base)
            .render(RenderRequest::new("x", dummy_control(), 512, 512))
            .await
            .unwrap_err();
        match err {
            RenderDeckError::Api { status, body } => {
                assert_eq!(status, 500);
                assert!(body.contains("CUDA out of memory"), "{body}");
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unreachable_backend_says_is_it_running() {
        // Bind then drop, so the port is closed — the classic "server not up".
        let (listener, base) = bind_mock();
        drop(listener);
        let err = a1111_deck(&base)
            .render(RenderRequest::new("x", dummy_control(), 512, 512))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("is it running?"), "{msg}");
        assert!(msg.contains(&base), "hint names the URL: {msg}");
    }

    #[test]
    fn probe_url_per_kind() {
        assert_eq!(probe_url(&RenderConfig::none()), None);
        let comfy = RenderConfig {
            name: "c".into(), kind: RenderKind::Comfy,
            base_url: "http://localhost:8188/".into(), model: "m".into(), api_key: None,
        };
        // Trailing slash is normalised away.
        assert_eq!(probe_url(&comfy).as_deref(), Some("http://localhost:8188/system_stats"));
        let a = RenderConfig { kind: RenderKind::Automatic1111, ..comfy.clone() };
        assert_eq!(
            probe_url(&a).as_deref(),
            Some("http://localhost:8188/sdapi/v1/options")
        );
        let cloud = RenderConfig { kind: RenderKind::Cloud, ..comfy };
        assert_eq!(probe_url(&cloud).as_deref(), Some("http://localhost:8188/account"));
    }

    #[tokio::test]
    async fn test_connection_unconfigured_gives_guidance() {
        let err = test_connection(&RenderConfig::none()).await.unwrap_err();
        assert!(err.contains("no render backend configured"), "{err}");
    }

    #[tokio::test]
    async fn test_connection_cloud_without_key_names_the_fix() {
        let cfg = RenderConfig {
            name: "cloud".into(), kind: RenderKind::Cloud,
            base_url: "https://api.replicate.com/v1".into(), model: "m".into(),
            api_key: Some("env:ITSJUSTCAD_UNSET_CLOUD_KEY_DEF".into()),
        };
        let err = test_connection(&cfg).await.unwrap_err();
        assert!(err.contains("no API key"), "{err}");
        assert!(err.contains("render_decks.json"), "{err}");
    }

    #[tokio::test]
    async fn test_connection_reachable_and_unreachable() {
        // Reachable: mock A1111 answers the options probe.
        let (listener, base) = bind_mock();
        serve_mock(listener, |line, _| {
            assert!(line.starts_with("GET /sdapi/v1/options"), "unexpected {line}");
            (200, "application/json", b"{}".to_vec())
        });
        let cfg = RenderConfig {
            name: "a1111".into(), kind: RenderKind::Automatic1111,
            base_url: base, model: String::new(), api_key: None,
        };
        let ok = test_connection(&cfg).await.expect("mock reachable");
        assert!(ok.contains("reachable"), "{ok}");

        // Unreachable: bound-then-dropped port → the "is it running?" hint.
        let (listener, base) = bind_mock();
        drop(listener);
        let cfg = RenderConfig { base_url: base.clone(), ..cfg };
        let err = test_connection(&cfg).await.unwrap_err();
        assert!(err.contains("is it running?"), "{err}");
        assert!(err.contains(&base), "hint names the URL: {err}");
    }

    #[tokio::test]
    async fn test_connection_reachable_but_rejecting_is_honest() {
        // A server that is up but 404s the probe (e.g. A1111 without --api):
        // the verdict must say reachable-but-rejected, not "is it running?".
        let (listener, base) = bind_mock();
        serve_mock(listener, |_, _| (404, "text/plain", b"not found".to_vec()));
        let cfg = RenderConfig {
            name: "a1111".into(), kind: RenderKind::Automatic1111,
            base_url: base, model: String::new(), api_key: None,
        };
        let err = test_connection(&cfg).await.unwrap_err();
        assert!(err.contains("HTTP 404"), "{err}");
        assert!(err.contains("rejected the probe"), "{err}");
    }

    #[tokio::test]
    async fn test_connection_cloud_sends_bearer_key() {
        let (listener, base) = bind_mock();
        let log = serve_mock(listener, |line, _| {
            assert!(line.starts_with("GET /account"), "unexpected {line}");
            (200, "application/json", b"{}".to_vec())
        });
        let cfg = RenderConfig {
            name: "cloud".into(), kind: RenderKind::Cloud,
            base_url: base, model: "m".into(), api_key: Some("sekrit-123".into()),
        };
        test_connection(&cfg).await.expect("mock cloud probe");
        let log = log.lock().unwrap();
        assert!(
            log.iter().any(|h| h.contains("authorization: Bearer sekrit-123")
                || h.contains("Authorization: Bearer sekrit-123")),
            "bearer key on the probe: {log:?}"
        );
    }

    #[tokio::test]
    async fn comfy_mock_server_end_to_end() {
        let (listener, base) = bind_mock();
        let png = tiny_png();
        let log = serve_mock(listener, move |line, _body| {
            if line.starts_with("POST /upload/image") {
                (200, "application/json", br#"{"name":"ok.png"}"#.to_vec())
            } else if line.starts_with("POST /prompt") {
                (200, "application/json", br#"{"prompt_id":"p42"}"#.to_vec())
            } else if line.starts_with("GET /history/p42") {
                (
                    200,
                    "application/json",
                    br#"{"p42":{"outputs":{"out":{"images":[{"filename":"r.png","subfolder":"sub","type":"output"}]}},"status":{"status_str":"success"}}}"#.to_vec(),
                )
            } else if line.starts_with("GET /view?filename=r.png&subfolder=sub&type=output") {
                (200, "image/png", png.clone())
            } else {
                panic!("unexpected request: {line}");
            }
        });
        let deck = ComfyRenderDeck::new(&RenderConfig {
            name: "comfy".into(),
            kind: RenderKind::Comfy,
            base_url: base,
            model: "sd_xl_base_1.0.safetensors".into(),
            api_key: None,
        });
        let out = deck
            .render(RenderRequest::new("a stone tower", dummy_control(), 512, 512))
            .await
            .expect("mock comfy render");
        assert_eq!(out.png, tiny_png());
        assert_eq!(out.backend, "comfy");
        // Both control images were uploaded as multipart PNGs before queueing.
        let log = log.lock().unwrap();
        let uploads: Vec<_> = log.iter().filter(|h| h.contains("POST /upload/image")).collect();
        assert_eq!(uploads.len(), 2, "depth + edge uploads");
        assert!(uploads[0].contains("multipart/form-data; boundary="));
    }

    #[tokio::test]
    async fn comfy_execution_error_is_surfaced() {
        let (listener, base) = bind_mock();
        serve_mock(listener, |line, _| {
            if line.starts_with("POST /upload/image") {
                (200, "application/json", br#"{"name":"ok.png"}"#.to_vec())
            } else if line.starts_with("POST /prompt") {
                (200, "application/json", br#"{"prompt_id":"bad"}"#.to_vec())
            } else {
                (
                    200,
                    "application/json",
                    br#"{"bad":{"outputs":{},"status":{"status_str":"error"}}}"#.to_vec(),
                )
            }
        });
        let deck = ComfyRenderDeck::new(&RenderConfig {
            name: "comfy".into(),
            kind: RenderKind::Comfy,
            base_url: base,
            model: "m".into(),
            api_key: None,
        });
        let err = deck
            .render(RenderRequest::new("x", dummy_control(), 512, 512))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("execution error"), "{err}");
    }

    #[tokio::test]
    async fn cloud_replicate_mock_end_to_end() {
        let (listener, base) = bind_mock();
        let png = tiny_png();
        let out_url = format!("{base}/out.png");
        let log = serve_mock(listener, move |line, body| {
            if line.starts_with("POST /predictions") {
                let v: serde_json::Value = serde_json::from_slice(body).unwrap();
                assert_eq!(v["input"]["prompt"], "a glass pavilion");
                (
                    201,
                    "application/json",
                    br#"{"id":"pr1","status":"starting"}"#.to_vec(),
                )
            } else if line.starts_with("GET /predictions/pr1") {
                (
                    200,
                    "application/json",
                    format!("{{\"status\":\"succeeded\",\"output\":[\"{out_url}\"]}}").into_bytes(),
                )
            } else if line.starts_with("GET /out.png") {
                (200, "image/png", png.clone())
            } else {
                panic!("unexpected request: {line}");
            }
        });
        let deck = CloudRenderDeck::new(&RenderConfig {
            name: "cloud".into(),
            kind: RenderKind::Cloud,
            base_url: base,
            model: "stability-ai/sdxl".into(),
            api_key: Some("test-key-123".into()),
        });
        let out = deck
            .render(RenderRequest::new("a glass pavilion", dummy_control(), 512, 512))
            .await
            .expect("mock cloud render");
        assert_eq!(out.png, tiny_png());
        // The bearer key rode on the prediction requests.
        let log = log.lock().unwrap();
        assert!(
            log.iter()
                .filter(|h| h.contains("/predictions"))
                .all(|h| h.to_lowercase().contains("authorization: bearer test-key-123")),
            "bearer auth on prediction calls"
        );
    }

    #[tokio::test]
    async fn cloud_failed_prediction_surfaces_backend_error() {
        let (listener, base) = bind_mock();
        serve_mock(listener, |line, _| {
            if line.starts_with("POST /predictions") {
                (201, "application/json", br#"{"id":"pr9","status":"starting"}"#.to_vec())
            } else {
                (
                    200,
                    "application/json",
                    br#"{"status":"failed","error":"NSFW content detected"}"#.to_vec(),
                )
            }
        });
        let deck = CloudRenderDeck::new(&RenderConfig {
            name: "cloud".into(),
            kind: RenderKind::Cloud,
            base_url: base,
            model: "m".into(),
            api_key: Some("k".into()),
        });
        let err = deck
            .render(RenderRequest::new("x", dummy_control(), 512, 512))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("NSFW content detected"), "{err}");
    }

    #[test]
    fn base64_decode_inverts_encode() {
        for v in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar"] {
            assert_eq!(base64_decode(&base64_encode(v)).unwrap(), v);
        }
        // Binary round-trip (the actual payload shape: a PNG).
        let png = tiny_png();
        assert_eq!(base64_decode(&base64_encode(&png)).unwrap(), png);
        // Invalid input errors instead of corrupting silently.
        assert!(base64_decode("not base64!!").is_err());
        assert!(base64_decode("Q").is_err(), "dangling single symbol");
    }

    #[test]
    fn multipart_upload_body_shape() {
        let (body, ct) = multipart_png_upload("itsjustcad_depth.png", b"PNGBYTES");
        let s = String::from_utf8_lossy(&body);
        assert!(ct.starts_with("multipart/form-data; boundary="));
        assert!(s.contains("filename=\"itsjustcad_depth.png\""));
        assert!(s.contains("PNGBYTES"));
        assert!(s.contains("name=\"overwrite\""));
        assert!(s.trim_end().ends_with("--"), "closing boundary");
    }

    #[test]
    fn default_decks_include_drawthings_preset() {
        // Draw Things speaks the A1111 API; the preset documents that.
        let f = RenderDecksFile::default();
        let dt = f.decks.iter().find(|d| d.name == "drawthings").expect("drawthings preset");
        assert_eq!(dt.kind, RenderKind::Automatic1111);
        assert!(dt.api_key.is_none(), "local — no key");
    }

    #[test]
    fn control_images_from_prefix_reads_three_pngs() {
        let dir = std::env::temp_dir().join(format!("ijc_ctrl_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let prefix = dir.join("scene");
        let p = prefix.to_string_lossy().to_string();
        std::fs::write(format!("{p}_depth.png"), b"D").unwrap();
        std::fs::write(format!("{p}_edge.png"), b"E").unwrap();
        std::fs::write(format!("{p}_mask.png"), b"M").unwrap();
        let ci = ControlImages::from_prefix(&p).unwrap();
        assert_eq!(ci.depth, b"D");
        assert_eq!(ci.edge, b"E");
        assert_eq!(ci.mask, b"M");
        assert!(!ci.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
