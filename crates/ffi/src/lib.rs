// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! C FFI over the ItsJustCAD core for the native iOS shell.
//!
//! One opaque [`AppHandle`] owns the Session, the wgpu renderer bound to a
//! `CAMetalLayer`-backed `UIView`, a tokio runtime, and the LLM deck. The Swift
//! side only: hosts the view, ticks `ijc_render_frame` on a `CADisplayLink`,
//! forwards typed/streamed command lines, and receives deck deltas via a
//! callback. All geometry + camera + GPU mutation happens on the render thread
//! (inside `ijc_render_frame` / the `*_h` accessors called from Swift's main
//! thread); the deck runs on tokio threads and only pushes [`PendingOp`]s onto a
//! shared queue and fires the callback — so no `Session`/renderer state is ever
//! shared across threads.
//!
//! # Safety contract (C-ABI boundary)
//!
//! Every `#[no_mangle]` entry point is `unsafe` because the host passes raw
//! pointers. The Rust side upholds the following defenses at the boundary:
//!
//! * **Null / alignment.** Every incoming pointer is null-checked; `ptr`+`len`
//!   buffers are additionally alignment- and length-validated before any
//!   `from_raw_parts`.
//! * **Panic safety.** A Rust panic unwinding across `extern "C"` is undefined
//!   behavior, so the body of *every* entry point runs inside
//!   [`std::panic::catch_unwind`] and returns a safe default on panic.
//! * **UTF-8.** C strings are validated with `CStr::to_str`; invalid UTF-8 is
//!   rejected rather than assumed.
//! * **Lifecycle.** [`AppHandle`] carries a magic guard word that [`ijc_free`]
//!   poisons, so a use-after-free or use-before-init with a stale/garbage
//!   pointer is caught (best-effort) instead of dereferencing freed memory.
//!
//! The caller must still uphold the parts Rust cannot check: a non-null
//! `AppHandle` must be a pointer previously returned by [`ijc_init`] and not yet
//! freed, and buffer pointers must be valid for the given length.
#![allow(clippy::missing_safety_doc)]

use std::ffi::{c_char, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use itsjustcad_commands::{io, parse, Command, Session};
use itsjustcad_deck::{
    digest, make_deck, system_prompt, ChatMessage, ChatRequest, DeckConfig, DeckDelta, DeckKind,
    ExtractEvent, Extractor, LlmDeck, Role,
};
use itsjustcad_render::{
    camera_uniform_with_mode, snapshot, DisplayMode, OrbitCamera, SceneRenderer, StandardView,
    Theme, DEPTH_FORMAT,
};
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, UiKitDisplayHandle, UiKitWindowHandle,
};

/// Live guard word stamped into every [`AppHandle`] by [`ijc_init`]. [`ijc_free`]
/// overwrites it with [`GUARD_DEAD`] before dropping, so a later call on a stale
/// (freed) pointer is caught before we touch any owned state.
const GUARD_LIVE: u64 = 0x1CAD_A11E_C0DE_F00D;
/// Poison word written over the guard on free.
const GUARD_DEAD: u64 = 0xDEAD_F8EE_DEAD_F8EE;

/// A deferred mutation, produced on any thread and applied on the render thread.
enum PendingOp {
    Cmd(Command),
    Camera(CamOp),
}

enum CamOp {
    SetView(StandardView),
    Orbit(f32, f32),
    Pan(f32, f32),
    Dolly(f32),
}

/// Deck delta kinds handed to the Swift callback.
const CB_CHAT: u32 = 0;
const CB_COMMAND: u32 = 1;
const CB_DONE: u32 = 2;
const CB_ERROR: u32 = 3;

/// `extern fn(ctx, kind, utf8_cstr)` — Swift trampoline; hops to `@MainActor`.
pub type DeckCallback = extern "C" fn(*mut c_void, u32, *const c_char);

/// Wraps the opaque Swift context pointer so it can cross into a tokio task.
/// Safe because Swift owns the object for the app's lifetime and only reads it
/// on the main actor after the callback marshals back.
struct SendCtx(*mut c_void);
unsafe impl Send for SendCtx {}
unsafe impl Sync for SendCtx {}

pub struct AppHandle {
    /// Liveness guard; must equal [`GUARD_LIVE`]. Poisoned on free.
    guard: u64,

    // GPU (render-thread only)
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    renderer: SceneRenderer,
    depth_view: wgpu::TextureView,

    // scene (render-thread only)
    session: Session,
    camera: OrbitCamera,
    last_gen: Option<u64>,
    theme: Theme,
    mode: DisplayMode,

    // async / deck (shared across threads via Arc)
    runtime: tokio::runtime::Runtime,
    deck: Option<Arc<dyn LlmDeck>>,
    deck_config: Option<DeckConfig>,
    pending: Arc<Mutex<Vec<PendingOp>>>,
    history: Arc<Mutex<Vec<ChatMessage>>>,
}

/// Run `body` catching any panic (a panic across `extern "C"` is UB), returning
/// `default` if it unwinds. Every entry point routes through this.
fn guard_ffi<T>(default: T, body: impl FnOnce() -> T) -> T {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(v) => v,
        Err(_) => {
            eprintln!("[ijc] panic caught at FFI boundary; returning default");
            default
        }
    }
}

/// Borrow an [`AppHandle`] from a caller pointer, rejecting null, unaligned, and
/// poisoned/garbage (failed-guard) pointers. Returns `None` on any failure.
///
/// # Safety
/// `h` must either be null or a pointer returned by [`ijc_init`] that is still
/// live; on any other garbage value behavior is technically UB, but the guard
/// check catches the common stale/freed/uninitialized cases best-effort.
unsafe fn handle_ref<'a>(h: *mut AppHandle) -> Option<&'a AppHandle> {
    if h.is_null() || !(h as usize).is_multiple_of(std::mem::align_of::<AppHandle>()) {
        return None;
    }
    let app = unsafe { &*h };
    if app.guard != GUARD_LIVE {
        eprintln!("[ijc] rejected use of freed/invalid AppHandle");
        return None;
    }
    Some(app)
}

/// Mutable counterpart of [`handle_ref`].
///
/// # Safety
/// Same contract as [`handle_ref`]; additionally the caller must not alias the
/// handle from another thread (the Swift host calls these on its main thread).
unsafe fn handle_mut<'a>(h: *mut AppHandle) -> Option<&'a mut AppHandle> {
    if h.is_null() || !(h as usize).is_multiple_of(std::mem::align_of::<AppHandle>()) {
        return None;
    }
    let app = unsafe { &mut *h };
    if app.guard != GUARD_LIVE {
        eprintln!("[ijc] rejected use of freed/invalid AppHandle");
        return None;
    }
    Some(app)
}

fn make_depth(device: &wgpu::Device, w: u32, h: u32) -> wgpu::TextureView {
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("ijc_depth"),
        size: wgpu::Extent3d { width: w.max(1), height: h.max(1), depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    tex.create_view(&Default::default())
}

/// Route a command line to a deferred op: camera verbs go to the camera, every
/// other verb is parsed as a geometry `Command`. Runs on any thread (pure).
fn route_line(line: &str) -> Option<PendingOp> {
    let mut it = line.split_whitespace();
    let verb = it.next()?;
    let rest: Vec<&str> = it.collect();

    // `view <name>` or a bare standard-view name.
    let view_name = if verb.eq_ignore_ascii_case("view") {
        rest.first().copied()
    } else {
        Some(verb)
    };
    if let Some(v) = view_name.and_then(standard_view) {
        return Some(PendingOp::Camera(CamOp::SetView(v)));
    }

    match verb.to_ascii_lowercase().as_str() {
        "orbit" => {
            let dx = rest.first().and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let dy = rest.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            return Some(PendingOp::Camera(CamOp::Orbit(dx, dy)));
        }
        "pan" => {
            let dx = rest.first().and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let dy = rest.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            return Some(PendingOp::Camera(CamOp::Pan(dx, dy)));
        }
        "zoom" | "dolly" => {
            let d = match rest.first().copied() {
                Some("in") => 1.0,
                Some("out") => -1.0,
                Some(s) => s.parse().unwrap_or(0.0),
                None => 0.0,
            };
            return Some(PendingOp::Camera(CamOp::Dolly(d)));
        }
        _ => {}
    }

    parse(line).ok().map(PendingOp::Cmd)
}

fn standard_view(name: &str) -> Option<StandardView> {
    Some(match name.to_ascii_lowercase().as_str() {
        "top" => StandardView::Top,
        "bottom" => StandardView::Bottom,
        "front" => StandardView::Front,
        "back" => StandardView::Back,
        "left" => StandardView::Left,
        "right" => StandardView::Right,
        "persp" | "perspective" | "iso" => StandardView::Perspective,
        _ => return None,
    })
}

fn emit(cb: DeckCallback, ctx: &SendCtx, kind: u32, text: &str) {
    if let Ok(c) = CString::new(text) {
        cb(ctx.0, kind, c.as_ptr());
    }
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// Create the engine bound to a `CAMetalLayer`-backed `UIView`.
///
/// `ui_view` is a `*UIView` whose `layerClass` is `CAMetalLayer`. `w`/`h` are
/// the drawable size in physical pixels (points × contentsScale).
///
/// # Safety
/// `ui_view` must be null or a valid `*UIView` pointer for the lifetime of the
/// returned handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_init(ui_view: *mut c_void, w: u32, h: u32) -> *mut AppHandle {
    guard_ffi(std::ptr::null_mut(), || {
        let Some(view) = NonNull::new(ui_view) else {
            return std::ptr::null_mut();
        };

        let instance = wgpu::Instance::default();

        let raw_window_handle = RawWindowHandle::UiKit(UiKitWindowHandle::new(view));
        let raw_display_handle = RawDisplayHandle::UiKit(UiKitDisplayHandle::new());
        let surface = match unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: Some(raw_display_handle),
                raw_window_handle,
            })
        } {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[ijc] create_surface failed: {e:?}");
                return std::ptr::null_mut();
            }
        };

        let adapter =
            match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })) {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("[ijc] request_adapter failed: {e:?}");
                    return std::ptr::null_mut();
                }
            };
        // Request the adapter's own limits, not the desktop defaults — the iOS
        // simulator GPU does not meet `Limits::default()` and would reject the
        // device.
        let desc = wgpu::DeviceDescriptor {
            required_limits: adapter.limits(),
            ..Default::default()
        };
        let (device, queue) = match pollster::block_on(adapter.request_device(&desc)) {
            Ok(dq) => dq,
            Err(e) => {
                eprintln!("[ijc] request_device failed: {e:?}");
                return std::ptr::null_mut();
            }
        };

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: w.max(1),
            height: h.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let renderer = SceneRenderer::new(&device, format);
        let depth_view = make_depth(&device, config.width, config.height);

        let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(_) => return std::ptr::null_mut(),
        };

        let handle = AppHandle {
            guard: GUARD_LIVE,
            device,
            queue,
            surface,
            config,
            renderer,
            depth_view,
            session: Session::default(),
            camera: OrbitCamera::default(),
            last_gen: None,
            theme: Theme::Dark,
            mode: DisplayMode::default(),
            runtime,
            deck: None,
            deck_config: None,
            pending: Arc::new(Mutex::new(Vec::new())),
            history: Arc::new(Mutex::new(Vec::new())),
        };
        Box::into_raw(Box::new(handle))
    })
}

/// Destroy a handle created by [`ijc_init`]. Null-safe and idempotent-safe: the
/// guard word is poisoned before the drop so a subsequent stray call on the same
/// pointer is rejected by [`handle_ref`]/[`handle_mut`] rather than freeing twice.
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`] that is not concurrently
/// in use; after this call the pointer is dangling and must not be reused.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_free(h: *mut AppHandle) {
    guard_ffi((), || {
        if h.is_null() || !(h as usize).is_multiple_of(std::mem::align_of::<AppHandle>()) {
            return;
        }
        // Reject double-free / free-of-garbage: only drop a live handle.
        if unsafe { (*h).guard } != GUARD_LIVE {
            eprintln!("[ijc] ijc_free on freed/invalid handle ignored");
            return;
        }
        unsafe {
            (*h).guard = GUARD_DEAD;
            drop(Box::from_raw(h));
        }
    })
}

/// # Safety
/// `h` must be null or a live handle from [`ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_resize(h: *mut AppHandle, w: u32, h_px: u32) {
    guard_ffi((), || {
        let Some(app) = (unsafe { handle_mut(h) }) else { return };
        app.config.width = w.max(1);
        app.config.height = h_px.max(1);
        app.surface.configure(&app.device, &app.config);
        app.depth_view = make_depth(&app.device, app.config.width, app.config.height);
    })
}

// ---------------------------------------------------------------------------
// Documents
// ---------------------------------------------------------------------------

/// Largest JSON buffer we will accept from the host (256 MiB). Rejects absurd /
/// corrupt lengths before `from_raw_parts`.
const MAX_JSON_LEN: usize = 256 * 1024 * 1024;

/// Replace the session from a `.itsjustcad.json` buffer. Returns true on success.
///
/// # Safety
/// `h` must be null or a live handle. If `len > 0`, `ptr` must be non-null,
/// aligned, and valid for reads of `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_open_json(h: *mut AppHandle, ptr: *const u8, len: usize) -> bool {
    guard_ffi(false, || {
        let Some(app) = (unsafe { handle_mut(h) }) else { return false };
        // Reject null / absurd length. A zero-length buffer is a valid empty doc
        // request but cannot parse as JSON, so bail early either way.
        if ptr.is_null() || len == 0 || len > MAX_JSON_LEN {
            return false;
        }
        // `u8` has alignment 1, so no alignment check is required for `ptr`.
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        let Ok(json) = std::str::from_utf8(bytes) else { return false };
        match io::from_json(json) {
            Ok(session) => {
                app.session = session;
                if let Ok(mut hist) = app.history.lock() {
                    hist.clear();
                }
                frame_camera(app);
                app.last_gen = None; // force re-snapshot next frame
                true
            }
            Err(_) => false,
        }
    })
}

/// Point the camera at the whole scene (used after opening a document).
fn frame_camera(app: &mut AppHandle) {
    if let Some(bb) = app.session.doc.scene_aabb() {
        let center = bb.center();
        app.camera.target = glam::Vec3::new(center.x as f32, center.y as f32, center.z as f32);
        app.camera.distance = (bb.size().length() as f32 * 1.2).max(5.0);
    }
}

/// Queue a single command line (typed in the chat box). Applied next frame.
///
/// # Safety
/// `h` must be null or a live handle; `line` must be null or a valid
/// NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_run_command(h: *mut AppHandle, line: *const c_char) {
    guard_ffi((), || {
        let Some(app) = (unsafe { handle_ref(h) }) else { return };
        if line.is_null() {
            return;
        }
        let Ok(s) = (unsafe { CStr::from_ptr(line) }).to_str() else { return };
        if let Some(op) = route_line(s)
            && let Ok(mut pending) = app.pending.lock()
        {
            pending.push(op);
        }
    })
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

/// # Safety
/// `h` must be null or a live handle from [`ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_render_frame(h: *mut AppHandle) {
    guard_ffi((), || {
        let Some(app) = (unsafe { handle_mut(h) }) else { return };

        // Drain deferred ops (from typed input and the streaming deck).
        let ops: Vec<PendingOp> = match app.pending.lock() {
            Ok(mut g) => std::mem::take(&mut *g),
            Err(_) => Vec::new(),
        };
        for op in ops {
            match op {
                PendingOp::Cmd(cmd) => {
                    let _ = app.session.run(cmd);
                }
                PendingOp::Camera(k) => match k {
                    CamOp::SetView(v) => app.camera.set_view(v),
                    CamOp::Orbit(dx, dy) => app.camera.orbit(dx, dy),
                    CamOp::Pan(dx, dy) => app.camera.pan(dx, dy),
                    CamOp::Dolly(d) => app.camera.dolly(d),
                },
            }
        }

        // Re-upload geometry only when the document changed.
        let doc_gen = app.session.doc.generation;
        if app.last_gen != Some(doc_gen) {
            let scene = snapshot(&app.session.doc, app.theme);
            app.renderer.set_scene(&app.device, &app.queue, &scene, doc_gen);
            app.last_gen = Some(doc_gen);
        }

        // Camera uniform.
        let aspect = app.config.width as f32 / app.config.height.max(1) as f32;
        let vp = app.camera.view_proj(aspect);
        let eye = app.camera.eye();
        let cam = camera_uniform_with_mode(vp, eye, app.mode);
        app.renderer.write_camera(&app.device, &app.queue, 0, &cam);

        let frame = match app.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            _ => {
                // Timeout / Occluded / Outdated / Lost / OutOfMemory: reconfigure
                // and skip this frame.
                app.surface.configure(&app.device, &app.config);
                return;
            }
        };
        let view = frame.texture.create_view(&Default::default());
        let [r, g, b, a] = app.theme.background();

        let mut encoder = app.device.create_command_encoder(&Default::default());
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ijc_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: r as f64,
                            g: g as f64,
                            b: b as f64,
                            a: a as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &app.depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let mut pass = pass.forget_lifetime();
            app.renderer.paint(&mut pass, 0, app.mode, true);
        }
        app.queue.submit([encoder.finish()]);
        frame.present();
    })
}

// ---------------------------------------------------------------------------
// Deck (LLM)
// ---------------------------------------------------------------------------

/// Configure the LLM deck. `kind`: 0 = OpenAI-compatible, 1 = Anthropic.
/// `api_key` may be null (e.g. local Ollama).
///
/// # Safety
/// `h` must be null or a live handle; `base_url`/`model`/`api_key` must each be
/// null or a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_deck_configure(
    h: *mut AppHandle,
    kind: u32,
    base_url: *const c_char,
    model: *const c_char,
    api_key: *const c_char,
) -> bool {
    guard_ffi(false, || {
        let Some(app) = (unsafe { handle_mut(h) }) else { return false };
        // SAFETY: each pointer is null-checked and UTF-8-validated here.
        let cstr = |p: *const c_char| -> Option<String> {
            if p.is_null() {
                None
            } else {
                unsafe { CStr::from_ptr(p) }.to_str().ok().map(str::to_owned)
            }
        };
        let config = DeckConfig {
            name: "ios".to_string(),
            kind: match kind {
                1 => DeckKind::Anthropic,
                _ => DeckKind::OpenaiCompat,
            },
            base_url: cstr(base_url).unwrap_or_default(),
            model: cstr(model).unwrap_or_default(),
            api_key: cstr(api_key),
            grammar: false,
        };
        app.deck = Some(Arc::from(make_deck(&config)));
        app.deck_config = Some(config);
        true
    })
}

/// Send a chat prompt. Streams deltas to `cb`; parsed commands are queued and
/// applied on the next `ijc_render_frame`.
///
/// # Safety
/// `h` must be null or a live handle; `prompt` must be null or a valid
/// NUL-terminated C string; `cb` must be a valid function pointer and `ctx` an
/// opaque pointer the host keeps alive for the duration of the stream.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_deck_send(
    h: *mut AppHandle,
    prompt: *const c_char,
    cb: DeckCallback,
    ctx: *mut c_void,
) {
    guard_ffi((), || {
        let Some(app) = (unsafe { handle_mut(h) }) else { return };
        let ctx = SendCtx(ctx);
        if prompt.is_null() {
            emit(cb, &ctx, CB_ERROR, "null prompt");
            return;
        }
        let Ok(prompt) = (unsafe { CStr::from_ptr(prompt) }).to_str() else {
            emit(cb, &ctx, CB_ERROR, "invalid prompt");
            return;
        };

        let (Some(deck), Some(config)) = (app.deck.clone(), app.deck_config.clone()) else {
            emit(cb, &ctx, CB_ERROR, "deck not configured");
            return;
        };

        let system = system_prompt(&digest(&app.session.doc), &app.session.plugins);
        let messages = {
            let Ok(mut history) = app.history.lock() else {
                emit(cb, &ctx, CB_ERROR, "history unavailable");
                return;
            };
            history.push(ChatMessage { role: Role::User, content: prompt.to_string() });
            history.clone()
        };

        let req = ChatRequest::text(system, messages, config.model.clone(), 4096, 0.2, None);
        let pending = app.pending.clone();
        let history = app.history.clone();

        app.runtime.spawn(async move {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeckDelta>();
            let deck2 = deck.clone();
            tokio::spawn(async move { deck2.stream_chat(req, tx).await });

            let mut ex = Extractor::default();
            let mut assistant = String::new();

            let handle_events = |events: Vec<ExtractEvent>, assistant: &mut String| {
                for ev in events {
                    match ev {
                        ExtractEvent::Chat(c) => {
                            assistant.push_str(&c);
                            emit(cb, &ctx, CB_CHAT, &c);
                        }
                        ExtractEvent::Command(line) => {
                            emit(cb, &ctx, CB_COMMAND, &line);
                            if let Some(op) = route_line(&line)
                                && let Ok(mut p) = pending.lock()
                            {
                                p.push(op);
                            }
                        }
                    }
                }
            };

            while let Some(delta) = rx.recv().await {
                match delta {
                    DeckDelta::Text(t) => handle_events(ex.push(&t), &mut assistant),
                    DeckDelta::Session(_) => {}
                    DeckDelta::Done => break,
                    DeckDelta::Error(e) => emit(cb, &ctx, CB_ERROR, &e),
                }
            }
            handle_events(ex.finish(), &mut assistant);

            if let Ok(mut hist) = history.lock() {
                hist.push(ChatMessage { role: Role::Assistant, content: assistant });
            }
            emit(cb, &ctx, CB_DONE, "");
        });
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_verbs_route_to_camera() {
        assert!(matches!(
            route_line("view top"),
            Some(PendingOp::Camera(CamOp::SetView(StandardView::Top)))
        ));
        assert!(matches!(
            route_line("front"),
            Some(PendingOp::Camera(CamOp::SetView(StandardView::Front)))
        ));
        assert!(matches!(
            route_line("persp"),
            Some(PendingOp::Camera(CamOp::SetView(StandardView::Perspective)))
        ));
        assert!(matches!(route_line("orbit 0.1 0.2"), Some(PendingOp::Camera(CamOp::Orbit(..)))));
        assert!(matches!(route_line("zoom in"), Some(PendingOp::Camera(CamOp::Dolly(..)))));
    }

    #[test]
    fn geometry_verbs_route_to_session() {
        assert!(matches!(route_line("box 0,0,0 1,1,1"), Some(PendingOp::Cmd(_))));
    }

    #[test]
    fn unknown_verb_is_dropped() {
        assert!(route_line("floccinaucinihilipilification").is_none());
    }

    #[test]
    fn open_json_roundtrips_sample() {
        // The op-log emitted by the `sample_doc` example must replay cleanly.
        let mut s = Session::default();
        for line in ["box -4,-4,0 8,8,3", "box 1,1,3 2,2,6"] {
            s.run(parse(line).unwrap()).unwrap();
        }
        let json = io::to_json(&s);
        let reopened = io::from_json(&json).expect("replay");
        assert_eq!(reopened.doc.len(), s.doc.len());
        assert!(reopened.doc.scene_aabb().is_some());
    }

    // ---- C-ABI boundary: null / invalid handle must return a safe default,
    // never dereference. These exercise the null path of every entry point that
    // takes a handle (the non-null path needs a live GPU device, unavailable in
    // CI, so it is covered by the on-device example instead).

    #[test]
    fn null_handle_returns_safe_defaults() {
        let nil = std::ptr::null_mut::<AppHandle>();
        unsafe {
            // Must not panic / deref; must return the documented safe default.
            ijc_free(nil); // null-safe no-op
            ijc_resize(nil, 100, 100);
            ijc_render_frame(nil);
            assert!(!ijc_open_json(nil, b"{}".as_ptr(), 2));
            ijc_run_command(nil, c"box 0,0,0 1,1,1".as_ptr());
            assert!(!ijc_deck_configure(nil, 0, std::ptr::null(), std::ptr::null(), std::ptr::null()));
        }
    }

    #[test]
    fn open_json_rejects_null_and_absurd_len_on_null_handle() {
        let nil = std::ptr::null_mut::<AppHandle>();
        unsafe {
            // Null handle short-circuits before touching ptr/len.
            assert!(!ijc_open_json(nil, std::ptr::null(), 0));
            assert!(!ijc_open_json(nil, std::ptr::null(), usize::MAX));
        }
    }

    #[test]
    fn guard_words_are_distinct() {
        assert_ne!(GUARD_LIVE, GUARD_DEAD);
        assert_ne!(GUARD_LIVE, 0);
    }
}
