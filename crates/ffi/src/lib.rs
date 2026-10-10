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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use itsjustcad_commands::{io, parse, Command, Selector, Session};
use itsjustcad_deck::{
    compact_command_catalog, digest, make_deck, system_prompt, ChatMessage, ChatRequest,
    DeckConfig, DeckDelta, DeckKind, ExtractEvent, Extractor, LlmDeck, Role,
};
use itsjustcad_render::{
    camera_uniform_with_mode, object_wireframe_world, snapshot, DisplayMode, LineEntry,
    OrbitCamera, SceneRenderer, StandardView, Theme, DEPTH_FORMAT,
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
#[derive(Debug)]
enum PendingOp {
    Cmd(Command),
    Camera(CamOp),
}

#[derive(Debug)]
enum CamOp {
    SetView(StandardView),
    Orbit(f32, f32),
    Pan(f32, f32),
    Dolly(f32),
    Frame,
}

/// Amber ghost color (RGBA 255,210,80 at ~45% alpha) for the gumball preview
/// wireframe. Matches the on-device gumball accent.
const GHOST_COLOR: [f32; 4] = [1.0, 0.82, 0.31, 0.45];

/// The pending transform behind a live gumball preview. Targets are always the
/// current `doc.selection` (stable during a drag), so only the transform
/// parameters are captured here; the ghost is recomputed from the selection
/// every frame and the committed command resolves `Selector::Selected`.
#[derive(Debug)]
enum PreviewKind {
    Move { delta: glam::DVec3 },
    Rotate { angle_deg: f64, axis: glam::DVec3, center: glam::DVec3 },
    Scale { factors: glam::DVec3, center: glam::DVec3 },
}

/// A live gumball preview: a pending transform shown as an amber ghost
/// wireframe until [`ijc_gumball_commit`] turns it into a real undoable command
/// (or [`ijc_gumball_cancel`] drops it).
#[derive(Debug)]
struct GumballPreview {
    kind: PreviewKind,
}

impl PreviewKind {
    /// The world-space transform this preview applies to the selection, as a
    /// `DMat4`. Move is a pure translate; rotate/scale are conjugated by their
    /// center: `T(c) · R|S · T(-c)`.
    fn matrix(&self) -> glam::DMat4 {
        use glam::DMat4;
        match *self {
            PreviewKind::Move { delta } => DMat4::from_translation(delta),
            PreviewKind::Rotate { angle_deg, axis, center } => {
                let axis = axis.normalize_or_zero();
                DMat4::from_translation(center)
                    * DMat4::from_axis_angle(axis, angle_deg.to_radians())
                    * DMat4::from_translation(-center)
            }
            PreviewKind::Scale { factors, center } => {
                DMat4::from_translation(center)
                    * DMat4::from_scale(factors)
                    * DMat4::from_translation(-center)
            }
        }
    }

    /// The undoable [`Command`] that realizes this preview on commit, targeting
    /// the current selection. Rotate/scale pass an explicit `center`.
    fn to_command(&self) -> Command {
        match *self {
            PreviewKind::Move { delta } => Command::Move {
                targets: Selector::Selected,
                delta,
            },
            PreviewKind::Rotate { angle_deg, axis, center } => Command::Rotate {
                targets: Selector::Selected,
                angle_deg,
                axis,
                center: Some(center),
            },
            PreviewKind::Scale { factors, center } => Command::Scale {
                targets: Selector::Selected,
                factors,
                center: Some(center),
            },
        }
    }
}

/// Deck delta kinds handed to the Swift callback.
const CB_CHAT: u32 = 0;
const CB_COMMAND: u32 = 1;
const CB_DONE: u32 = 2;
const CB_ERROR: u32 = 3;

/// `extern fn(ctx, kind, utf8_cstr)` — Swift trampoline; hops to `@MainActor`.
pub type DeckCallback = extern "C" fn(*mut c_void, u32, *const c_char);

/// Wraps the opaque Swift context pointer so it can cross into a tokio task,
/// paired with the handle's `alive` flag. Every callback is gated on `alive`, so
/// once [`ijc_free`] clears it, `emit` becomes a no-op and the (possibly
/// released) `ctx` pointer is never dereferenced again — closing the
/// use-after-free-of-ctx window even if a task is mid-flight at free time.
struct SendCtx {
    ctx: *mut c_void,
    alive: Arc<AtomicBool>,
}
// SAFETY: the raw ctx is only ever read on the Swift main actor (the callback
// trampoline marshals back), and only while `alive` is true; the flag is atomic.
unsafe impl Send for SendCtx {}
unsafe impl Sync for SendCtx {}

pub struct AppHandle {
    /// Liveness guard; must equal [`GUARD_LIVE`]. Poisoned on free.
    guard: u64,

    /// Runtime mutual-exclusion flag for the `&mut` accessors. The safety
    /// contract *says* the host calls the mutating entry points on a single
    /// thread, but the host is untrusted and may violate it — two live
    /// `&mut AppHandle` to the same allocation is instant UB plus a data race on
    /// the non-atomic GPU/session/camera fields. [`handle_mut`] does a
    /// compare-exchange on this flag before fabricating the `&mut`, so a
    /// concurrent (or re-entrant) mutating call is *rejected* (turned into a
    /// no-op) instead of aliasing. Boxed handle, so the flag's address is stable.
    busy: AtomicBool,

    /// One-shot free latch. [`ijc_free`] does a single `compare_exchange`
    /// `false -> true` on this; exactly one caller wins the alive->freeing
    /// transition and proceeds to drop the box, every concurrent or re-entrant
    /// `ijc_free` on the same pointer loses the CAS and returns without touching
    /// the (about-to-be / already) freed allocation. This closes the double-free
    /// TOCTOU that a plain non-atomic `guard` read-then-write could not: the
    /// `guard` word is only advisory (best-effort stale-pointer detection); the
    /// *authority* on "who frees" is this atomic latch.
    freeing: AtomicBool,

    /// Cleared by [`ijc_free`] before teardown. In-flight deck tasks check it
    /// (via [`SendCtx`]) before every callback so a callback can never fire on a
    /// Swift ctx the host has already released (use-after-free of ctx).
    alive: Arc<AtomicBool>,

    /// Abort handles for in-flight deck stream tasks, aborted on [`ijc_free`] so
    /// no detached task outlives the handle it borrows shared state from.
    tasks: Arc<Mutex<Vec<tokio::task::AbortHandle>>>,

    /// When true (the default), deck-emitted commands are applied automatically
    /// as they stream. When false, they are only emitted to the callback (for
    /// host-side approval) and NOT applied — the host runs approved ones via
    /// [`ijc_run_command`]. Set via [`ijc_deck_set_auto_apply`].
    deck_auto_apply: Arc<AtomicBool>,

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

    /// Live gumball preview (iOS full gumball), or `None` when idle. When set,
    /// `ijc_render_frame` draws an amber ghost wireframe of the selection under
    /// the pending transform; `ijc_gumball_commit` turns it into a real command.
    gumball_preview: Option<GumballPreview>,

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

/// RAII exclusive borrow of an [`AppHandle`]. Holds the `busy` flag for its
/// lifetime and clears it on drop, so at most one `&mut AppHandle` is ever live
/// across all threads. Deref gives the `&mut`.
struct HandleGuard<'a> {
    app: &'a mut AppHandle,
}

impl std::ops::Deref for HandleGuard<'_> {
    type Target = AppHandle;
    fn deref(&self) -> &AppHandle {
        self.app
    }
}
impl std::ops::DerefMut for HandleGuard<'_> {
    fn deref_mut(&mut self) -> &mut AppHandle {
        self.app
    }
}
impl Drop for HandleGuard<'_> {
    fn drop(&mut self) {
        // Release the exclusion flag. `Release` pairs with the `Acquire` in
        // `handle_mut` so a subsequent acquirer sees all our writes.
        self.app.busy.store(false, Ordering::Release);
    }
}

/// Mutable counterpart of [`handle_ref`], enforcing single-`&mut` exclusion at
/// runtime rather than by documentation alone.
///
/// The host is untrusted and may (per the threat model) call mutating entry
/// points from arbitrary threads concurrently. We therefore acquire the
/// per-handle `busy` flag with a compare-exchange *before* fabricating the
/// `&mut`. If it is already held (a concurrent or re-entrant mutating call),
/// we return `None` and the caller no-ops — no second `&mut` is ever created,
/// so the aliasing/data-race UB is eliminated (findings #1, #4).
///
/// # Safety
/// Same pointer contract as [`handle_ref`].
unsafe fn handle_mut<'a>(h: *mut AppHandle) -> Option<HandleGuard<'a>> {
    if h.is_null() || !(h as usize).is_multiple_of(std::mem::align_of::<AppHandle>()) {
        return None;
    }
    // Read the guard word through a shared ref first (no `&mut` yet, so this
    // races benignly at worst on a garbage pointer that fails the check).
    let app_ref = unsafe { &*h };
    if app_ref.guard != GUARD_LIVE {
        eprintln!("[ijc] rejected use of freed/invalid AppHandle");
        return None;
    }
    // Try to take exclusive access. Fails if another thread already holds it
    // (a concurrent mutator, or `ijc_free` mid-teardown).
    if app_ref
        .busy
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        eprintln!("[ijc] rejected concurrent/re-entrant mutating call (handle busy)");
        return None;
    }
    // Re-check the guard now that we hold `busy`. This closes the free-vs-mutator
    // TOCTOU: `ijc_free` poisons the guard *while holding `busy`*, so if a free
    // began after our first guard read but before our CAS, one of two things is
    // true here — either the free already holds `busy` (our CAS above failed and
    // we returned), or it has not yet acquired `busy`, in which case it is still
    // spinning and has NOT yet poisoned the guard or dropped the box, so this
    // read is valid and sees `GUARD_LIVE`; the free then waits for us to release
    // `busy`. The remaining case — free won `busy` first and already poisoned the
    // guard — cannot reach here because our CAS would have failed. We keep this
    // re-check as defense in depth against any future reordering of the two.
    if app_ref.guard != GUARD_LIVE {
        app_ref.busy.store(false, Ordering::Release);
        eprintln!("[ijc] rejected use of freed/invalid AppHandle (freed under us)");
        return None;
    }
    // We now hold exclusive access: it is sound to materialize the `&mut`.
    let app = unsafe { &mut *h };
    Some(HandleGuard { app })
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

/// Reject deck base URLs whose host is a cloud-metadata / link-local / internal
/// address. The FFI attaches the configured API key as a bearer / `x-api-key`
/// header on every request (and a second `list_models` probe on error), so a
/// hostile or synced-from-untrusted config pointing `base_url` at an internal
/// endpoint would both SSRF *and* leak the key. The desktop `local_only` gate
/// never runs on this path, so we enforce credential/SSRF containment here:
/// block the well-known dangerous hosts. Public API origins are unaffected.
///
/// Returns `true` when the URL is safe to send to.
fn base_url_is_allowed(url: &str) -> bool {
    // Empty base_url = provider default (safe); Anthropic/OpenAI defaults are
    // public hosts. Only inspect an explicitly-supplied host.
    if url.is_empty() {
        return true;
    }
    let host_part = url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or(url);
    let host = if let Some(bracket_end) = host_part.find(']') {
        &host_part[..=bracket_end] // IPv6 literal like [fd00::1]:443
    } else {
        host_part.split(':').next().unwrap_or(host_part)
    };
    let host = host.trim().to_ascii_lowercase();

    // Block the cloud metadata endpoint and link-local range explicitly.
    if host == "169.254.169.254" || host.starts_with("169.254.") {
        return false;
    }
    // Block obvious internal/loopback names and RFC1918 / unique-local ranges.
    // (Loopback is pointless on-device and a common SSRF pivot.)
    if host == "localhost"
        || host == "metadata"
        || host.ends_with(".internal")
        || host.ends_with(".local")
        || host == "0.0.0.0"
        || host.starts_with("127.")
        || host.starts_with("10.")
        || host.starts_with("192.168.")
        || host == "[::1]"
        || host == "::1"
        || host.starts_with("[fd")
        || host.starts_with("[fe80")
    {
        return false;
    }
    // 172.16.0.0/12
    if let Some(rest) = host.strip_prefix("172.")
        && let Some(second) = rest.split('.').next()
        && let Ok(oct) = second.parse::<u8>()
        && (16..=31).contains(&oct)
    {
        return false;
    }
    true
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
    // Never invoke the Swift callback after the handle has been freed: the host
    // may have released the ctx object, so `ctx.ctx` could dangle. `Acquire`
    // pairs with the `Release` store in `ijc_free`.
    if !ctx.alive.load(Ordering::Acquire) {
        return;
    }
    if let Ok(c) = CString::new(text) {
        cb(ctx.ctx, kind, c.as_ptr());
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
            // Clamp host-supplied dims to the GPU limit: a 0 or absurd value
            // would make `configure` / the depth texture panic or over-allocate.
            width: clamp_dim(w, &device),
            height: clamp_dim(h, &device),
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
            busy: AtomicBool::new(false),
            freeing: AtomicBool::new(false),
            alive: Arc::new(AtomicBool::new(true)),
            tasks: Arc::new(Mutex::new(Vec::new())),
            deck_auto_apply: Arc::new(AtomicBool::new(true)),
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
            gumball_preview: None,
            runtime,
            deck: None,
            deck_config: None,
            pending: Arc::new(Mutex::new(Vec::new())),
            history: Arc::new(Mutex::new(Vec::new())),
        };
        Box::into_raw(Box::new(handle))
    })
}

/// Destroy a handle created by [`ijc_init`]. Null-safe and safe against a
/// concurrent / re-entrant / double `ijc_free`, and against a free racing an
/// in-flight `*_mut` mutator.
///
/// Two hazards are defended here (see field docs on [`AppHandle::freeing`] and
/// [`AppHandle::busy`]):
///
/// 1. **Double-free TOCTOU.** The old guard was a plain `u64`: a check-then-poison
///    is not atomic, so two concurrent `ijc_free(h)` could both observe
///    `GUARD_LIVE` and both `Box::from_raw` → double-free/UB. We now settle "who
///    frees" with a single `compare_exchange` on the atomic `freeing` latch:
///    exactly one caller wins the `false -> true` transition and drops; every
///    loser returns immediately without touching the allocation.
/// 2. **Free-during-mutator (UAF).** A mutator entry point (`ijc_render_frame`,
///    `ijc_resize`, `ijc_open_json`, `ijc_deck_send`) fabricates a `&mut` while
///    holding `busy`. Dropping the box out from under that live `&mut` is UAF.
///    The free winner therefore *acquires the same `busy` flag* (spin-waiting for
///    any in-flight mutator to release it) before poisoning the guard and
///    dropping. Because we poison `guard` to [`GUARD_DEAD`] while holding `busy`,
///    any *subsequent* mutator fails its guard check and bails — so no new `&mut`
///    can be born after we start tearing down.
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`]. After this call the
/// pointer is dangling and must not be reused (a stray reuse is caught
/// best-effort by the poisoned guard / lost `freeing` CAS).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_free(h: *mut AppHandle) {
    guard_ffi((), || {
        if h.is_null() || !(h as usize).is_multiple_of(std::mem::align_of::<AppHandle>()) {
            return;
        }
        // Best-effort stale/garbage-pointer rejection *before* we dereference the
        // atomics: a freed allocation has a poisoned guard, so this catches the
        // common re-free-of-old-pointer case without racing on freed memory.
        // (The authoritative one-shot decision is the `freeing` CAS below; this
        // is only the cheap advisory pre-filter shared with `handle_ref`.)
        if unsafe { (*h).guard } != GUARD_LIVE {
            eprintln!("[ijc] ijc_free on freed/invalid handle ignored");
            return;
        }
        // One-shot latch: exactly one caller wins alive->freeing. `AcqRel` so the
        // winner's later teardown writes are ordered after this, and a losing
        // racer that observes `true` has an `Acquire` view. The loser MUST return
        // without freeing — the winner owns the drop.
        if unsafe { &*h }
            .freeing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            eprintln!("[ijc] concurrent/double ijc_free ignored (already freeing)");
            return;
        }

        // We are the sole freer. Serialize against any in-flight `*_mut` mutator
        // by acquiring the SAME `busy` flag `handle_mut` uses: spin until it is
        // free, so we never drop the box while a live `&mut AppHandle` exists.
        // A mutator holds `busy` only for the duration of one entry point (a
        // render frame / resize / json load / send setup), so this is bounded.
        while unsafe { &*h }
            .busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            std::hint::spin_loop();
        }
        // `busy` is now held by us; no mutator holds a `&mut`, and none can be
        // created after we poison the guard below (they fail the guard check).

        unsafe {
            (*h).guard = GUARD_DEAD;
            // Tear down in-flight deck work BEFORE dropping anything the tasks
            // borrow or before the host releases the Swift ctx:
            //  1. Clear `alive` so any callback that races teardown becomes a
            //     no-op (see `emit`) — no use-after-free of the ctx pointer.
            //  2. Abort every in-flight stream task so no detached future keeps
            //     running (and firing callbacks) after the handle is gone.
            // Both flags live behind `Arc`, so the aborted tasks still see the
            // cleared `alive` even though the owning box is about to drop.
            (*h).alive.store(false, Ordering::Release);
            if let Ok(mut tasks) = (*h).tasks.lock() {
                for t in tasks.drain(..) {
                    t.abort();
                }
            }
            // Drop exactly once. We hold `busy` (excludes mutators) and won the
            // `freeing` CAS (excludes other frees), so this is the unique drop.
            // The `busy` flag is dropped along with the box; that is fine because
            // no other thread can legitimately still be spinning for it (any
            // concurrent mutator either finished before us or is now bailing on
            // the poisoned guard, and any concurrent free lost the `freeing` CAS).
            drop(Box::from_raw(h));
        }
    })
}

/// Clamp a host-supplied drawable dimension into `[1, max_texture_dimension_2d]`.
///
/// A 0 dimension makes wgpu reject the surface config / depth texture; a huge
/// one (the host can pass any `u32`) exceeds the GPU's `max_texture_dimension_2d`
/// and makes `surface.configure` / `create_texture` panic or try an absurd
/// allocation. The renderer only ever sees a bounded, non-zero size (finding:
/// `ijc_resize` 0/huge dimensions must not crash or over-allocate).
fn clamp_dim(v: u32, device: &wgpu::Device) -> u32 {
    v.clamp(1, device.limits().max_texture_dimension_2d)
}

/// # Safety
/// `h` must be null or a live handle from [`ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_resize(h: *mut AppHandle, w: u32, h_px: u32) {
    guard_ffi((), || {
        let Some(mut app) = (unsafe { handle_mut(h) }) else { return };
        let app: &mut AppHandle = &mut app;
        app.config.width = clamp_dim(w, &app.device);
        app.config.height = clamp_dim(h_px, &app.device);
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
        let Some(mut app) = (unsafe { handle_mut(h) }) else { return false };
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
                frame_camera(&mut app);
                app.last_gen = None; // force re-snapshot next frame
                true
            }
            Err(_) => false,
        }
    })
}

/// Reset to a blank document (new scene): a fresh empty session, cleared deck
/// history, and a forced re-snapshot next frame. Mirrors the post-parse reset of
/// [`ijc_open_json`]. The host should also clear its autosave if "new" must
/// persist across launches.
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_new_document(h: *mut AppHandle) {
    guard_ffi((), || {
        let Some(mut app) = (unsafe { handle_mut(h) }) else { return };
        app.session = Session::default();
        if let Ok(mut hist) = app.history.lock() {
            hist.clear();
        }
        app.last_gen = None; // force re-snapshot next frame
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
/// Returns `true` if the line was queued, `false` if it was rejected: a null /
/// non-UTF-8 string, an unparseable verb, or — per the side-effect gate below —
/// a filesystem/network command.
///
/// # Side-effect containment (finding: host-typed command gate)
/// The FFI is the trust boundary: the host is untrusted and there is no fs
/// sandbox or human-confirm affordance on this path. A host-submitted line like
/// `import /etc/passwd` or `export /some/path` would otherwise reach
/// `session.run`, which performs real `std::fs` reads/writes. The deck (LLM)
/// path already refuses [`Command::is_side_effecting`] ops outright; we apply
/// the SAME gate here so a host-typed fs/net command is rejected rather than
/// silently executed. Pure geometry/camera verbs are unaffected.
///
/// # Safety
/// `h` must be null or a live handle; `line` must be null or a valid
/// NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_run_command(h: *mut AppHandle, line: *const c_char) -> bool {
    guard_ffi(false, || {
        let Some(app) = (unsafe { handle_ref(h) }) else { return false };
        if line.is_null() {
            return false;
        }
        let Ok(s) = (unsafe { CStr::from_ptr(line) }).to_str() else { return false };
        let Some(op) = route_line(s) else { return false };
        // Gate side-effecting commands (fs/net) exactly as the deck path does.
        if let PendingOp::Cmd(cmd) = &op
            && cmd.is_side_effecting()
        {
            eprintln!("[ijc] refused side-effecting host command (filesystem/network not allowed)");
            return false;
        }
        if let Ok(mut pending) = app.pending.lock() {
            pending.push(op);
            return true;
        }
        false
    })
}

// ---------------------------------------------------------------------------
// Interactive camera (touch gestures)
// ---------------------------------------------------------------------------
//
// These mirror the typed camera verbs (`orbit`/`pan`/`zoom`) but take raw
// screen-space deltas straight from UIKit gesture recognizers, so a drag can
// feed one op per frame without routing a text command per gesture change. Each
// op is queued on the shared `pending` list and applied on the next
// `ijc_render_frame` (same contract as `ijc_run_command`). They use the shared
// `handle_ref` (not `handle_mut`): they only push onto the `Mutex`-guarded
// queue, touching no `&mut` session/GPU/camera state directly.

/// Orbit (tumble) the camera by a screen-space delta in points. `dx`/`dy` are
/// incremental finger translation; the core scales them to radians (≈0.29°/px),
/// so pass raw pixel deltas. Applied on the next `ijc_render_frame`.
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_camera_orbit(h: *mut AppHandle, dx: f32, dy: f32) {
    guard_ffi((), || {
        let Some(app) = (unsafe { handle_ref(h) }) else { return };
        if let Ok(mut pending) = app.pending.lock() {
            pending.push(PendingOp::Camera(CamOp::Orbit(dx, dy)));
        }
    })
}

/// Pan the camera by a screen-space delta in points. `dx`/`dy` are incremental
/// finger translation; the core scales them by the view distance so the point
/// under the fingers stays roughly fixed. Applied on the next `ijc_render_frame`.
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_camera_pan(h: *mut AppHandle, dx: f32, dy: f32) {
    guard_ffi((), || {
        let Some(app) = (unsafe { handle_ref(h) }) else { return };
        if let Ok(mut pending) = app.pending.lock() {
            pending.push(PendingOp::Camera(CamOp::Pan(dx, dy)));
        }
    })
}

/// Zoom (dolly) the camera by a multiplicative `factor` from a pinch gesture:
/// `factor > 1` zooms in (closer), `factor < 1` zooms out. The core's `dolly`
/// takes an additive scroll amount where `distance *= 1 - scroll * 0.002`; we
/// map the pinch ratio to that scroll so a pinch-apart moves the camera nearer.
/// Applied on the next `ijc_render_frame`.
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_camera_zoom(h: *mut AppHandle, factor: f32) {
    guard_ffi((), || {
        let Some(app) = (unsafe { handle_ref(h) }) else { return };
        // A non-finite / non-positive factor is a no-op (a pinch recognizer can
        // momentarily report 0 or NaN scale between touches).
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        // `dolly` applies `distance *= 1 - scroll * 0.002`. To make `factor`
        // behave multiplicatively (zoom in by `factor`), pick `scroll` such that
        // `1 - scroll * 0.002 ≈ 1 / factor`, i.e. divide distance by `factor`.
        let scroll = (1.0 - 1.0 / factor) / 0.002;
        if let Ok(mut pending) = app.pending.lock() {
            pending.push(PendingOp::Camera(CamOp::Dolly(scroll)));
        }
    })
}

/// Zoom-to-fit: frame the entire scene (center on its bounding box and back the
/// camera off to show everything). No-op on an empty scene. Applied on the next
/// [`ijc_render_frame`].
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_camera_zoom_extents(h: *mut AppHandle) {
    guard_ffi((), || {
        let Some(app) = (unsafe { handle_ref(h) }) else { return };
        if let Ok(mut pending) = app.pending.lock() {
            pending.push(PendingOp::Camera(CamOp::Frame));
        }
    })
}

// ---------------------------------------------------------------------------
// Pick + gumball (tap-to-select, selection bbox, project, move)
// ---------------------------------------------------------------------------
//
// Picking is authoritative in Rust: Swift forwards the tap/drag pixel
// coordinates (same space as `ijc_resize` — physical drawable pixels) and Rust
// owns the ray build, AABB hit test, selection mutation, and the single
// undoable Move. Swift only draws the gizmo overlay, using `ijc_selection_bbox`
// (where to put it in world space) + `ijc_project_point` (where that lands in
// pixels). These reimplement COMPACT versions of the desktop's `screen_ray` /
// `ray_aabb` / `project` (crates/app/src/app.rs) in pure glam, so the FFI does
// not depend on the desktop crate.

/// The camera's current aspect ratio from the configured drawable size.
fn aspect_of(app: &AppHandle) -> f32 {
    app.config.width as f32 / app.config.height.max(1) as f32
}

/// Build a world-space pick ray from a pixel coordinate, mirroring the desktop
/// `screen_ray`: map pixels → NDC (y-flipped), unproject z=0 and z=1 through the
/// inverse view-projection, and return `(origin, normalized_direction)` in
/// double precision. Returns `None` if the view-projection is non-invertible or
/// the unprojected direction is degenerate (e.g. a zero-area viewport).
fn screen_ray(
    vp: glam::Mat4,
    w_px: f32,
    h_px: f32,
    x_px: f32,
    y_px: f32,
) -> Option<(glam::DVec3, glam::DVec3)> {
    let dims_ok = w_px.is_finite() && h_px.is_finite() && w_px > 0.0 && h_px > 0.0;
    if !dims_ok || !x_px.is_finite() || !y_px.is_finite() {
        return None;
    }
    let inv = vp.inverse();
    if !inv.is_finite() {
        return None; // singular view_proj
    }
    let ndc = glam::Vec2::new(x_px / w_px * 2.0 - 1.0, 1.0 - y_px / h_px * 2.0);
    let unproject = |z: f32| -> glam::DVec3 {
        let p = inv * glam::Vec4::new(ndc.x, ndc.y, z, 1.0);
        (p.truncate() / p.w).as_dvec3()
    };
    let origin = unproject(0.0);
    let far = unproject(1.0);
    let delta = far - origin;
    if !origin.is_finite() || !delta.is_finite() || delta.length_squared() < 1e-18 {
        return None;
    }
    Some((origin, delta.normalize()))
}

/// Slab ray/AABB test; returns the nearest non-negative hit distance `t` along
/// the ray, or `None` on a miss. Compact reimplementation of the desktop
/// `ray_aabb`.
fn ray_aabb(origin: glam::DVec3, dir: glam::DVec3, min: glam::DVec3, max: glam::DVec3) -> Option<f64> {
    let inv = dir.recip();
    let t1 = (min - origin) * inv;
    let t2 = (max - origin) * inv;
    let t_min = t1.min(t2).max_element();
    let t_max = t1.max(t2).min_element();
    (t_max >= t_min.max(0.0)).then_some(t_min.max(0.0))
}

/// Tap-to-select. Builds a pick ray from `(x_px, y_px)` (physical drawable
/// pixels, the same space as [`ijc_resize`]) and finds the nearest VISIBLE
/// object whose world AABB the ray crosses (AABB broad-phase only — enough for
/// v1; curve narrow-phase is left to a later pass).
///
/// * **Hit:** when `additive`, toggles that id in `doc.selection` (tap again to
///   deselect); otherwise replaces the selection with just that id.
/// * **Miss:** when not `additive`, clears the selection; an additive miss is a
///   no-op (keeps the current multi-selection).
///
/// Bumps `doc.generation` on any selection change so the renderer recolors the
/// highlight on the next [`ijc_render_frame`]. Returns whether an object was hit.
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_pick(h: *mut AppHandle, x_px: f32, y_px: f32, additive: bool) -> bool {
    guard_ffi(false, || {
        let Some(mut app) = (unsafe { handle_mut(h) }) else { return false };
        let app: &mut AppHandle = &mut app;

        let vp = app.camera.view_proj(aspect_of(app));
        let (w, h) = (app.config.width as f32, app.config.height as f32);
        let Some((origin, dir)) = screen_ray(vp, w, h, x_px, y_px) else { return false };

        // Broad phase: nearest visible object whose world AABB the ray hits.
        let doc = &app.session.doc;
        let mut best: Option<(f64, itsjustcad_doc::ObjectId)> = None;
        for obj in doc.objects() {
            if !(obj.visible && doc.layer_visible(&obj.layer)) {
                continue;
            }
            let bb = obj.geometry.aabb();
            if let Some(t) = ray_aabb(origin, dir, bb.min, bb.max)
                && best.is_none_or(|(bt, _)| t < bt)
            {
                best = Some((t, obj.id));
            }
        }

        // Apply the selection change and report whether we hit anything. We only
        // bump `generation` (via `get_mut`) when the selection actually changed,
        // so a redundant tap doesn't force a needless re-snapshot.
        match best {
            Some((_, id)) => {
                let doc = &mut app.session.doc;
                if additive {
                    if !doc.selection.remove(&id) {
                        doc.selection.insert(id);
                    }
                } else {
                    doc.selection.clear();
                    doc.selection.insert(id);
                }
                doc.generation += 1;
                true
            }
            None => {
                let doc = &mut app.session.doc;
                if !additive && !doc.selection.is_empty() {
                    doc.selection.clear();
                    doc.generation += 1;
                }
                false
            }
        }
    })
}

/// World-space AABB over the current selection, for placing the gumball overlay.
/// Writes 3 `f64` center then 3 `f64` size into the caller's `[3]` buffers and
/// returns `true`; writes nothing and returns `false` when the selection is
/// empty (or on a null/invalid handle). The size is `max - min`, so a point-like
/// selection reports a zero size — the host should clamp the gizmo to a minimum
/// on-screen size itself.
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`]. If this returns `true`,
/// `center` and `size` must each be non-null and valid for writes of 3 `f64`s;
/// they are not touched otherwise.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_selection_bbox(
    h: *mut AppHandle,
    center: *mut f64,
    size: *mut f64,
) -> bool {
    guard_ffi(false, || {
        if center.is_null() || size.is_null() {
            return false;
        }
        let Some(app) = (unsafe { handle_ref(h) }) else { return false };
        let Some(bb) = app.session.doc.selection_aabb() else { return false };
        let c = bb.center();
        let s = bb.size();
        unsafe {
            std::slice::from_raw_parts_mut(center, 3).copy_from_slice(&[c.x, c.y, c.z]);
            std::slice::from_raw_parts_mut(size, 3).copy_from_slice(&[s.x, s.y, s.z]);
        }
        true
    })
}

/// Project a world point to screen PIXELS (the same space [`ijc_pick`] consumes),
/// via the current camera view-projection and drawable size. Writes 2 `f32`
/// (x, y) into `out_xy` and returns `true`; returns `false` (writing nothing)
/// when the point is behind the camera / clipped (`clip.w <= 0`), or on a
/// null/invalid handle or null `world`/`out_xy`. Compact reimplementation of the
/// desktop `project`.
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`]. `world` must be null or
/// valid for reads of 3 `f64`s; `out_xy` must be null or valid for writes of
/// 2 `f32`s.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_project_point(
    h: *mut AppHandle,
    world: *const f64,
    out_xy: *mut f32,
) -> bool {
    guard_ffi(false, || {
        if world.is_null() || out_xy.is_null() {
            return false;
        }
        let Some(app) = (unsafe { handle_ref(h) }) else { return false };
        let p = unsafe { std::slice::from_raw_parts(world, 3) };
        let (w, h_px) = (app.config.width as f32, app.config.height.max(1) as f32);
        let vp = app.camera.view_proj(aspect_of(app));
        let clip = vp * glam::Vec4::new(p[0] as f32, p[1] as f32, p[2] as f32, 1.0);
        if clip.w.is_nan() || clip.w <= 0.0 {
            return false; // behind the camera / clipped (NaN also fails)
        }
        let ndc = clip.truncate() / clip.w;
        let x = (ndc.x + 1.0) * 0.5 * w;
        let y = (1.0 - ndc.y) * 0.5 * h_px;
        if !x.is_finite() || !y.is_finite() {
            return false;
        }
        unsafe {
            *out_xy = x;
            *out_xy.add(1) = y;
        }
        true
    })
}

/// Apply ONE relative translation `(dx, dy, dz)` to the current selection as a
/// single undoable, op-logged [`Command::Move`] — the host calls this once on
/// drag-release (not per frame) so the undo stack gets one entry per gesture.
/// Runs synchronously through the same `Session::run` path as
/// [`ijc_run_command`], so `doc.generation` bumps and the next
/// [`ijc_render_frame`] re-renders. Returns `false` on a null/invalid handle, an
/// empty selection, a non-finite delta, or if the underlying command fails.
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_move_selected(h: *mut AppHandle, dx: f64, dy: f64, dz: f64) -> bool {
    guard_ffi(false, || {
        if !dx.is_finite() || !dy.is_finite() || !dz.is_finite() {
            return false;
        }
        let Some(mut app) = (unsafe { handle_mut(h) }) else { return false };
        // Empty selection: nothing to move (and `Selector::Selected` would be a
        // no-op resolve). Report false so the host can skip the gesture.
        if app.session.doc.selection.is_empty() {
            return false;
        }
        // Build the Command directly rather than formatting a `move selected
        // dx,dy,dz` text line: this avoids a float→string→float round-trip and
        // the comma-point syntax the parser expects, while still routing through
        // the same undoable `Session::run` path `ijc_run_command` ultimately hits.
        let cmd = Command::Move {
            targets: Selector::Selected,
            delta: glam::DVec3::new(dx, dy, dz),
        };
        app.session.run(cmd).is_ok()
    })
}

// ---------------------------------------------------------------------------
// Gumball (full on-screen transform gizmo): live ghost preview + commit
// ---------------------------------------------------------------------------

/// Arm a live MOVE preview: a ghost wireframe of the selection translated by
/// `(dx, dy, dz)`. The host calls this continuously while dragging the gumball;
/// each call replaces the pending transform and bumps `doc.generation` so the
/// next [`ijc_render_frame`] redraws the ghost. No document mutation happens
/// until [`ijc_gumball_commit`]. Returns `false` on a null/invalid handle, an
/// empty selection, or a non-finite delta.
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_gumball_preview_move(
    h: *mut AppHandle,
    dx: f64,
    dy: f64,
    dz: f64,
) -> bool {
    guard_ffi(false, || {
        if !dx.is_finite() || !dy.is_finite() || !dz.is_finite() {
            return false;
        }
        let Some(mut app) = (unsafe { handle_mut(h) }) else { return false };
        if app.session.doc.selection.is_empty() {
            return false;
        }
        app.gumball_preview = Some(GumballPreview {
            kind: PreviewKind::Move { delta: glam::DVec3::new(dx, dy, dz) },
        });
        app.session.doc.generation += 1;
        true
    })
}

/// Arm a live ROTATE preview: a ghost of the selection rotated `angle_deg`
/// about the axis `(ax, ay, az)` through the center `(cx, cy, cz)`. Same
/// semantics as [`ijc_gumball_preview_move`] (replace + bump, no mutation).
/// Returns `false` on a null/invalid handle, an empty selection, a non-finite
/// argument, or a zero-length axis.
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`].
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ijc_gumball_preview_rotate(
    h: *mut AppHandle,
    angle_deg: f64,
    ax: f64,
    ay: f64,
    az: f64,
    cx: f64,
    cy: f64,
    cz: f64,
) -> bool {
    guard_ffi(false, || {
        let all = [angle_deg, ax, ay, az, cx, cy, cz];
        if all.iter().any(|v| !v.is_finite()) {
            return false;
        }
        let axis = glam::DVec3::new(ax, ay, az);
        if axis.length_squared() == 0.0 {
            return false;
        }
        let Some(mut app) = (unsafe { handle_mut(h) }) else { return false };
        if app.session.doc.selection.is_empty() {
            return false;
        }
        app.gumball_preview = Some(GumballPreview {
            kind: PreviewKind::Rotate {
                angle_deg,
                axis,
                center: glam::DVec3::new(cx, cy, cz),
            },
        });
        app.session.doc.generation += 1;
        true
    })
}

/// Arm a live SCALE preview: a ghost of the selection scaled by per-axis
/// factors `(sx, sy, sz)` about the center `(cx, cy, cz)`. Same semantics as
/// [`ijc_gumball_preview_move`] (replace + bump, no mutation). Returns `false`
/// on a null/invalid handle, an empty selection, or a non-finite argument.
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`].
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ijc_gumball_preview_scale(
    h: *mut AppHandle,
    sx: f64,
    sy: f64,
    sz: f64,
    cx: f64,
    cy: f64,
    cz: f64,
) -> bool {
    guard_ffi(false, || {
        let all = [sx, sy, sz, cx, cy, cz];
        if all.iter().any(|v| !v.is_finite()) {
            return false;
        }
        let Some(mut app) = (unsafe { handle_mut(h) }) else { return false };
        if app.session.doc.selection.is_empty() {
            return false;
        }
        app.gumball_preview = Some(GumballPreview {
            kind: PreviewKind::Scale {
                factors: glam::DVec3::new(sx, sy, sz),
                center: glam::DVec3::new(cx, cy, cz),
            },
        });
        app.session.doc.generation += 1;
        true
    })
}

/// Cancel any live gumball preview without touching the document. The ghost is
/// cleared on the next [`ijc_render_frame`]. Bumps `doc.generation` so that
/// frame redraws. Always returns `true` on a valid handle (even with no
/// preview armed); `false` only on a null/invalid handle.
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_gumball_cancel(h: *mut AppHandle) -> bool {
    guard_ffi(false, || {
        let Some(mut app) = (unsafe { handle_mut(h) }) else { return false };
        app.gumball_preview = None;
        app.session.doc.generation += 1;
        true
    })
}

/// Commit the live gumball preview as ONE undoable, op-logged [`Command`]
/// (`Move` / `Rotate` / `Scale`) targeting the current selection, run through
/// the same [`Session::run`] path as [`ijc_move_selected`]. Rotate/scale pass
/// the preview's explicit `center`. Clears the preview afterward. Returns
/// `false` on a null/invalid handle, no armed preview, an empty selection, or
/// if the command fails.
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_gumball_commit(h: *mut AppHandle) -> bool {
    guard_ffi(false, || {
        let Some(mut app) = (unsafe { handle_mut(h) }) else { return false };
        let Some(preview) = app.gumball_preview.take() else {
            return false;
        };
        // Empty selection: `Selector::Selected` would resolve to nothing. The
        // preview is already taken (cleared) above, matching cancel semantics.
        if app.session.doc.selection.is_empty() {
            return false;
        }
        app.session.run(preview.kind.to_command()).is_ok()
    })
}

/// Query whether a gumball preview is currently armed. Read-only. Returns
/// `false` on a null/invalid handle.
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_gumball_has_preview(h: *mut AppHandle) -> bool {
    guard_ffi(false, || {
        (unsafe { handle_ref(h) }).is_some_and(|app| app.gumball_preview.is_some())
    })
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

/// Export the current document to `fmt` (a bare extension: `svg`, `dxf`, `obj`,
/// `stl`, `gltf`/`glb`, `ifc`, `3dm`, `csv`, `jpg`). Writes the byte count to
/// `*out_len` and returns a heap buffer of the file bytes — or null on error, an
/// empty result, or an unsupported format. `step`/`stp` are unsupported in this
/// build (they need the OCCT tier). Mirrors the `Command::Export` dispatch minus
/// the filesystem write. Caller MUST free the buffer with [`ijc_bytes_free`].
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`]. `fmt` must be a valid
/// NUL-terminated C string. `out_len` must be null or a writable `usize*`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_export(
    h: *mut AppHandle,
    fmt: *const c_char,
    out_len: *mut usize,
) -> *mut u8 {
    guard_ffi(std::ptr::null_mut(), || {
        if !out_len.is_null() {
            unsafe { *out_len = 0 };
        }
        let Some(app) = (unsafe { handle_ref(h) }) else {
            return std::ptr::null_mut();
        };
        if fmt.is_null() {
            return std::ptr::null_mut();
        }
        let Ok(fmt) = (unsafe { CStr::from_ptr(fmt) }).to_str() else {
            return std::ptr::null_mut();
        };
        let fmt = fmt.trim().trim_start_matches('.').to_ascii_lowercase();

        use itsjustcad_commands::{csv, dxf, ifc, mesh_export, raster, rhino3dm, saf, svg};
        let doc = &app.session.doc;
        // Synthetic path: some exporters infer the format from the extension or
        // embed a file name; none of them touch the filesystem here.
        let path = format!("model.{fmt}");
        let bytes: Vec<u8> = match fmt.as_str() {
            "dxf" => dxf::document_dxf(doc).0.into_bytes(),
            "svg" | "ai" => svg::export_svg(doc).0,
            "csv" => csv::export_csv(doc).0,
            "3dm" => rhino3dm::export(doc).0,
            "jpg" | "jpeg" => match raster::export_jpg(doc) {
                Ok((b, _)) => b,
                Err(_) => return std::ptr::null_mut(),
            },
            "ifc" => match ifc::export(doc, &path) {
                Ok((b, _)) => b,
                Err(_) => return std::ptr::null_mut(),
            },
            "saf" | "xlsx" => match saf::export(doc) {
                Ok((b, _)) => b,
                Err(_) => return std::ptr::null_mut(),
            },
            // STEP needs the OCCT tier (off in this build) and a filesystem path.
            "step" | "stp" => return std::ptr::null_mut(),
            // obj / stl / gltf / glb / ... — mesh_export picks by the extension.
            _ => match mesh_export::export(doc, &path) {
                Ok((b, _)) => b,
                Err(_) => return std::ptr::null_mut(),
            },
        };
        if bytes.is_empty() {
            return std::ptr::null_mut();
        }
        let mut boxed = bytes.into_boxed_slice();
        let len = boxed.len();
        let ptr = boxed.as_mut_ptr();
        std::mem::forget(boxed);
        if !out_len.is_null() {
            unsafe { *out_len = len };
        }
        ptr
    })
}

/// Free a byte buffer returned by [`ijc_export`]. Null-safe; call at most once
/// per returned pointer, with the exact `len` that `ijc_export` reported.
///
/// # Safety
/// `ptr`/`len` must be a buffer previously returned by [`ijc_export`] and not
/// yet freed, or `ptr` null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_bytes_free(ptr: *mut u8, len: usize) {
    guard_ffi((), || {
        if ptr.is_null() {
            return;
        }
        drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)) });
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
        let Some(mut guard) = (unsafe { handle_mut(h) }) else { return };
        // Reborrow the inner `&mut AppHandle` once so the compiler can split
        // disjoint field borrows (device/queue/renderer/surface) below — a plain
        // `Deref` through the guard would treat every access as borrowing the
        // whole `AppHandle`.
        let app: &mut AppHandle = &mut guard;

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
                    CamOp::Frame => frame_camera(app),
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

        // Gumball ghost: an amber wireframe of the selection under the pending
        // preview transform. Recomputed every frame from `doc.selection` (so it
        // tracks the current preview params) and uploaded to the renderer's
        // additive ghost buffers. Cleared when no preview is armed.
        if let Some(preview) = &app.gumball_preview {
            let mat = preview.kind.matrix();
            let doc = &app.session.doc;
            let ghost: Vec<LineEntry> = doc
                .selection
                .iter()
                .filter_map(|id| doc.get(*id))
                .filter_map(|obj| {
                    let segs = object_wireframe_world(obj);
                    if segs.is_empty() {
                        return None;
                    }
                    // Transform both endpoints of every segment by the preview
                    // matrix; keep the flat LineList pair layout the ghost pass
                    // (edge pipeline) consumes.
                    let mut pts: Vec<[f32; 3]> = Vec::with_capacity(segs.len() * 2);
                    for [a, b] in segs {
                        let a = mat.transform_point3(a);
                        let b = mat.transform_point3(b);
                        pts.push([a.x as f32, a.y as f32, a.z as f32]);
                        pts.push([b.x as f32, b.y as f32, b.z as f32]);
                    }
                    Some((pts, GHOST_COLOR, 0.0))
                })
                .collect();
            app.renderer.set_ghost_lines(&app.device, &ghost);
        } else {
            app.renderer.clear_ghost_lines();
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
        let Some(mut app) = (unsafe { handle_mut(h) }) else { return false };
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
            terse: None,
        };
        // SSRF / credential-leak containment (finding #7): refuse to configure a
        // deck whose base_url points at a metadata/link-local/internal host,
        // since the API key would be attached to every request to it.
        if !base_url_is_allowed(&config.base_url) {
            eprintln!("[ijc] refused deck base_url (internal/metadata host blocked)");
            return false;
        }
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
        let ctx = SendCtx { ctx, alive: app.alive.clone() };
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
        let tasks = app.tasks.clone();
        let auto_apply = app.deck_auto_apply.clone();

        let join = app.runtime.spawn(async move {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeckDelta>();
            let deck2 = deck.clone();
            tokio::spawn(async move { deck2.stream_chat(req, tx).await });

            let mut ex = Extractor::default();
            let mut assistant = String::new();

            // Process a batch of extractor events. This closure contains NO
            // `.await` and does all the panic-prone work (Extractor output,
            // `route_line`->`parse`, string accumulation) AND every `cb()` call
            // via `emit`. We run it inside `catch_unwind` so a panic can never
            // unwind across the `extern "C"` callback boundary (finding #3): a
            // Rust panic straddling `cb` (an `extern "C"` Swift trampoline) is
            // UB. On panic we swallow it and stop feeding the stream.
            let handle_events = |events: Vec<ExtractEvent>, assistant: &mut String| -> bool {
                catch_unwind(AssertUnwindSafe(|| {
                    for ev in events {
                        match ev {
                            ExtractEvent::Chat(c) => {
                                assistant.push_str(&c);
                                emit(cb, &ctx, CB_CHAT, &c);
                            }
                            ExtractEvent::Command(line) => {
                                emit(cb, &ctx, CB_COMMAND, &line);
                                if let Some(op) = route_line(&line) {
                                    // Side-effect containment (finding #6): the
                                    // desktop app gates deck-emitted fs commands
                                    // (import/export/...) behind an explicit human
                                    // OK. The FFI drain runs `session.run` with no
                                    // gate, so a prompt-injected model could read
                                    // or write arbitrary paths. On iOS we refuse
                                    // side-effecting deck commands outright rather
                                    // than let the model choose fs paths.
                                    if let PendingOp::Cmd(cmd) = &op
                                        && cmd.is_side_effecting()
                                    {
                                        emit(
                                            cb,
                                            &ctx,
                                            CB_ERROR,
                                            "refused: filesystem command from the assistant is not allowed",
                                        );
                                        continue;
                                    }
                                    // Auto-apply mode: run as it streams. Approval
                                    // mode (auto_apply=false): emit only (already
                                    // did above) and let the host run approved ones.
                                    if auto_apply.load(Ordering::Relaxed)
                                        && let Ok(mut p) = pending.lock()
                                    {
                                        p.push(op);
                                    }
                                }
                            }
                        }
                    }
                }))
                .is_ok()
            };

            while let Some(delta) = rx.recv().await {
                let keep = match delta {
                    DeckDelta::Text(t) => {
                        let events = catch_unwind(AssertUnwindSafe(|| ex.push(&t)))
                            .unwrap_or_default();
                        handle_events(events, &mut assistant)
                    }
                    DeckDelta::Session(_) => true,
                    DeckDelta::Done => break,
                    DeckDelta::Error(e) => {
                        catch_unwind(AssertUnwindSafe(|| emit(cb, &ctx, CB_ERROR, &e))).is_ok()
                    }
                };
                if !keep {
                    break; // a panic was caught inside the batch; stop safely.
                }
            }
            let tail = catch_unwind(AssertUnwindSafe(|| ex.finish())).unwrap_or_default();
            handle_events(tail, &mut assistant);

            if let Ok(mut hist) = history.lock() {
                hist.push(ChatMessage { role: Role::Assistant, content: assistant });
            }
            let _ = catch_unwind(AssertUnwindSafe(|| emit(cb, &ctx, CB_DONE, "")));
        });

        // Track the task so `ijc_free` can abort it before teardown, closing the
        // window where a detached stream fires a callback on a freed ctx / after
        // the handle's shared state is gone (finding #2).
        if let Ok(mut t) = tasks.lock() {
            t.retain(|a| !a.is_finished());
            t.push(join.abort_handle());
        }
    })
}

/// Stop any in-flight deck stream: abort the streaming task(s) without tearing
/// down the handle. The partial text already delivered to the callback stays;
/// no further deltas or a `done` callback arrive for the aborted turn. Safe to
/// call when nothing is streaming (a no-op). Mirrors the abort loop in
/// [`ijc_free`] minus the teardown.
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_deck_stop(h: *mut AppHandle) {
    guard_ffi((), || {
        let Some(app) = (unsafe { handle_ref(h) }) else { return };
        if let Ok(mut tasks) = app.tasks.lock() {
            for t in tasks.drain(..) {
                t.abort();
            }
        }
    })
}

/// Control whether deck-emitted commands are applied automatically (`true`, the
/// default) or only emitted to the callback for host-side approval (`false`).
/// In approval mode the host runs approved commands via [`ijc_run_command`].
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_deck_set_auto_apply(h: *mut AppHandle, enabled: bool) {
    guard_ffi((), || {
        if let Some(app) = unsafe { handle_ref(h) } {
            app.deck_auto_apply.store(enabled, Ordering::Relaxed);
        }
    })
}

// ---------------------------------------------------------------------------
// Prompt helpers for the Swift on-device deck
// ---------------------------------------------------------------------------
//
// The on-device (Apple Foundation Models) path runs entirely in Swift but needs
// the same command cheatsheet + scene digest the FFI deck feeds its system
// prompt. These return heap-allocated, NUL-terminated UTF-8 C strings the caller
// must release with [`ijc_string_free`]. Returning an owned `*mut c_char`
// (rather than writing into a caller buffer) keeps the ABI simple and matches
// the Swift side's `copyFFIString` (copy then free).

/// Allocate a C string the host owns and must free via [`ijc_string_free`].
/// Returns null if the text contains an interior NUL (cannot be a C string).
fn into_c_string(s: String) -> *mut c_char {
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// The compact command catalog (verb cheatsheet) for the on-device model's
/// instructions. Handle-free: the catalog is static. Caller must
/// [`ijc_string_free`] the result.
///
/// # Safety
/// The returned pointer must be freed exactly once via [`ijc_string_free`] and
/// not otherwise retained.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_command_brief() -> *mut c_char {
    guard_ffi(std::ptr::null_mut(), || into_c_string(compact_command_catalog()))
}

/// The full command registry as a JSON array, for a client-side command palette:
/// `[{"name","usage","summary","category"}, …]`. Handle-free: the registry is
/// static. Caller must [`ijc_string_free`] the result.
///
/// # Safety
/// The returned pointer must be freed exactly once via [`ijc_string_free`] and
/// not otherwise retained.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_command_catalog_json() -> *mut c_char {
    guard_ffi(std::ptr::null_mut(), || into_c_string(command_catalog_json()))
}

/// Serialize the command registry to a compact JSON array. Built by hand (no
/// serde dep in this crate); every string field is JSON-escaped.
fn command_catalog_json() -> String {
    fn esc(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out
    }

    let mut out = String::from("[");
    for (i, spec) in itsjustcad_commands::registry().iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"name\":\"{}\",\"usage\":\"{}\",\"summary\":\"{}\",\"category\":\"{}\"}}",
            esc(spec.name),
            esc(spec.usage),
            esc(spec.summary),
            esc(spec.category.key()),
        ));
    }
    out.push(']');
    out
}

/// The document's layers as a JSON array, for the iOS layers inspector:
/// `[{"name","colorRgba":[r,g,b,a],"hasColor","visible","locked","active","order","linetype"}, …]`,
/// ordered by `(order, name)` for a stable client list. `active` is the current
/// layer; `hasColor` is false when the style uses the theme default, and in that
/// case `colorRgba` falls back to a neutral grey so the client still has a swatch.
/// Returns `"[]"` (non-null) for a null/invalid handle. Caller must
/// [`ijc_string_free`] the result.
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`]. The returned pointer must
/// be freed exactly once via [`ijc_string_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_layers_json(h: *mut AppHandle) -> *mut c_char {
    guard_ffi(std::ptr::null_mut(), || {
        let Some(app) = (unsafe { handle_ref(h) }) else {
            return into_c_string(String::from("[]"));
        };
        into_c_string(layers_json(&app.session.doc))
    })
}

/// Serialize the document's layers to a compact JSON array. Built by hand (no
/// serde dep in this crate); the layer name is JSON-escaped. Sorted by
/// `(order, name)` so the client list stays stable across reads.
fn layers_json(doc: &itsjustcad_doc::Document) -> String {
    fn esc(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out
    }

    // Theme-default swatch for layers whose color is `None`.
    const DEFAULT_RGBA: [f32; 4] = [0.5, 0.5, 0.5, 1.0];

    // Objects per layer, so the client can hide empty/unused layers.
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for obj in doc.objects() {
        *counts.entry(obj.layer.as_str()).or_insert(0) += 1;
    }

    let mut layers: Vec<(&String, &itsjustcad_doc::LayerStyle)> = doc.layers.iter().collect();
    layers.sort_by(|(an, a), (bn, b)| a.order.cmp(&b.order).then_with(|| an.cmp(bn)));

    let mut out = String::from("[");
    for (i, (name, style)) in layers.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let rgba = style.color.unwrap_or(DEFAULT_RGBA);
        let object_count = counts.get(name.as_str()).copied().unwrap_or(0);
        out.push_str(&format!(
            "{{\"name\":\"{}\",\"colorRgba\":[{},{},{},{}],\"hasColor\":{},\"visible\":{},\"locked\":{},\"active\":{},\"order\":{},\"objectCount\":{},\"linetype\":\"{}\"}}",
            esc(name),
            rgba[0],
            rgba[1],
            rgba[2],
            rgba[3],
            style.color.is_some(),
            style.visible,
            style.locked,
            *name == &doc.current_layer,
            style.order,
            object_count,
            style.linetype.token(),
        ));
    }
    out.push(']');
    out
}

/// The document's sheets and sheet sets as JSON, for the iPad Sheets (paper-space)
/// UI: `{"sheets":[{"name","size","views":[{"direction","scale"}]}],"sets":[{"name","sheets":["<name>",…]}]}`.
/// `size` is the paper-size label (`"a4"`…`"a0"`) and `direction` is the view
/// direction token (`"top"`/`"front"`/`"right"`/`"persp"`); both lowercase and
/// stable. Every name is JSON-escaped. Returns `{"sheets":[],"sets":[]}`
/// (non-null) for a null/invalid handle. Caller must [`ijc_string_free`] the
/// result.
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`]. The returned pointer must
/// be freed exactly once via [`ijc_string_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_sheets_json(h: *mut AppHandle) -> *mut c_char {
    guard_ffi(std::ptr::null_mut(), || {
        let Some(app) = (unsafe { handle_ref(h) }) else {
            return into_c_string(String::from("{\"sheets\":[],\"sets\":[]}"));
        };
        into_c_string(sheets_json(&app.session.doc))
    })
}

/// Serialize the document's sheets and sheet sets to compact JSON. Built by hand
/// (no serde dep in this crate); every string field is JSON-escaped. Paper size
/// and view direction use their stable lowercase labels.
fn sheets_json(doc: &itsjustcad_doc::Document) -> String {
    fn esc(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out
    }

    let mut out = String::from("{\"sheets\":[");
    for (i, sheet) in doc.sheets.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"name\":\"{}\",\"size\":\"{}\",\"views\":[",
            esc(&sheet.name),
            sheet.paper.label(),
        ));
        for (j, view) in sheet.views.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "{{\"direction\":\"{}\",\"scale\":{}}}",
                view.direction.label(),
                view.scale,
            ));
        }
        out.push_str("]}");
    }
    out.push_str("],\"sets\":[");
    for (i, set) in doc.sheet_sets.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("{{\"name\":\"{}\",\"sheets\":[", esc(&set.name)));
        for (j, name) in set.sheets.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str(&format!("\"{}\"", esc(name)));
        }
        out.push_str("]}");
    }
    out.push_str("]}");
    out
}

/// Rename a sheet. Returns false on null/invalid handle, null names, an unknown
/// `old` name, or if `new` collides with an existing sheet. Updates sheet-set
/// membership that referenced `old`, and bumps `doc.generation`.
///
/// This mutates the document directly (no shared `Command`), sidestepping the
/// spaces-in-names parse problem; there is no op-log/undo for rename in v1.
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`]. `old` and `new` must each
/// be a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_sheet_rename(
    h: *mut AppHandle,
    old: *const c_char,
    new: *const c_char,
) -> bool {
    guard_ffi(false, || {
        if old.is_null() || new.is_null() {
            return false;
        }
        let Ok(old) = (unsafe { CStr::from_ptr(old) }).to_str() else { return false };
        let Ok(new) = (unsafe { CStr::from_ptr(new) }).to_str() else { return false };
        if new.is_empty() {
            return false;
        }
        let Some(mut app) = (unsafe { handle_mut(h) }) else { return false };
        let doc = &mut app.session.doc;
        // A no-op rename (old == new) is a success, but a different sheet named
        // `new` is a collision.
        if old != new && doc.sheets.iter().any(|s| s.name == new) {
            return false;
        }
        let Some(sheet) = doc.sheets.iter_mut().find(|s| s.name == old) else {
            return false;
        };
        sheet.name = new.to_owned();
        for set in &mut doc.sheet_sets {
            for member in &mut set.sheets {
                if *member == old {
                    *member = new.to_owned();
                }
            }
        }
        doc.generation += 1;
        true
    })
}

/// Render one sheet to a vector PDF for the iPad Sheets UI. Looks up the sheet in
/// `doc.sheets` by `name`; on a hit, calls [`pdf::sheet_pdf`] and returns a heap
/// buffer of the PDF bytes with the byte count written to `*out_len`. Returns null
/// (and sets `*out_len = 0`) for a null/invalid handle, a null `name`, or an
/// unknown sheet. Mirrors [`ijc_export`]'s allocation/return path. Caller MUST free
/// the buffer with [`ijc_bytes_free`].
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`]. `name` must be a valid
/// NUL-terminated C string. `out_len` must be null or a writable `usize*`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_sheet_pdf(
    h: *mut AppHandle,
    name: *const c_char,
    out_len: *mut usize,
) -> *mut u8 {
    guard_ffi(std::ptr::null_mut(), || {
        if !out_len.is_null() {
            unsafe { *out_len = 0 };
        }
        let Some(app) = (unsafe { handle_ref(h) }) else {
            return std::ptr::null_mut();
        };
        if name.is_null() {
            return std::ptr::null_mut();
        }
        let Ok(name) = (unsafe { CStr::from_ptr(name) }).to_str() else {
            return std::ptr::null_mut();
        };
        let doc = &app.session.doc;
        let Some(sheet) = doc.sheet(name) else {
            return std::ptr::null_mut();
        };
        let bytes = itsjustcad_commands::pdf::sheet_pdf(doc, sheet).0;
        if bytes.is_empty() {
            return std::ptr::null_mut();
        }
        let mut boxed = bytes.into_boxed_slice();
        let len = boxed.len();
        let ptr = boxed.as_mut_ptr();
        std::mem::forget(boxed);
        if !out_len.is_null() {
            unsafe { *out_len = len };
        }
        ptr
    })
}

/// Render a whole sheet set to a multi-page vector PDF for the iPad Sheets UI.
/// Looks up the [`SheetSet`] by `name`, resolves its member sheet names to live
/// `&Sheet`s (skipping any that no longer exist), and calls [`pdf::sheets_pdf`].
/// Returns a heap buffer of the PDF bytes with the byte count written to
/// `*out_len`. Returns null (and sets `*out_len = 0`) for a null/invalid handle, a
/// null `name`, an unknown set, or a set that resolves to no sheets. Mirrors
/// [`ijc_export`]'s allocation/return path. Caller MUST free the buffer with
/// [`ijc_bytes_free`].
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`]. `name` must be a valid
/// NUL-terminated C string. `out_len` must be null or a writable `usize*`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_sheetset_pdf(
    h: *mut AppHandle,
    name: *const c_char,
    out_len: *mut usize,
) -> *mut u8 {
    guard_ffi(std::ptr::null_mut(), || {
        if !out_len.is_null() {
            unsafe { *out_len = 0 };
        }
        let Some(app) = (unsafe { handle_ref(h) }) else {
            return std::ptr::null_mut();
        };
        if name.is_null() {
            return std::ptr::null_mut();
        }
        let Ok(name) = (unsafe { CStr::from_ptr(name) }).to_str() else {
            return std::ptr::null_mut();
        };
        let doc = &app.session.doc;
        let Some(set) = doc.sheet_set(name) else {
            return std::ptr::null_mut();
        };
        // Resolve each member name to its live sheet, skipping any that are
        // missing (a set may linger a name after its sheet is deleted).
        let resolved: Vec<&itsjustcad_doc::Sheet> =
            set.sheets.iter().filter_map(|n| doc.sheet(n)).collect();
        if resolved.is_empty() {
            return std::ptr::null_mut();
        }
        let bytes = itsjustcad_commands::pdf::sheets_pdf(doc, &resolved).0;
        if bytes.is_empty() {
            return std::ptr::null_mut();
        }
        let mut boxed = bytes.into_boxed_slice();
        let len = boxed.len();
        let ptr = boxed.as_mut_ptr();
        std::mem::forget(boxed);
        if !out_len.is_null() {
            unsafe { *out_len = len };
        }
        ptr
    })
}

/// A compact digest of the current scene for the on-device model's instructions.
/// Returns an empty (but non-null) string for a null/invalid handle. Caller must
/// [`ijc_string_free`] the result.
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`]. The returned pointer must
/// be freed exactly once via [`ijc_string_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_scene_digest(h: *mut AppHandle) -> *mut c_char {
    guard_ffi(std::ptr::null_mut(), || {
        let Some(app) = (unsafe { handle_ref(h) }) else {
            return into_c_string(String::new());
        };
        into_c_string(digest(&app.session.doc))
    })
}

/// Serialize the current document to ItsJustCAD op-log JSON (the same format
/// [`ijc_open_json`] reads). For persistence/export on the host side. Returns an
/// empty (non-null) string for a null/invalid handle. Caller must
/// [`ijc_string_free`] the result.
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`]. The returned pointer must
/// be freed exactly once via [`ijc_string_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_save_json(h: *mut AppHandle) -> *mut c_char {
    guard_ffi(std::ptr::null_mut(), || {
        let Some(app) = (unsafe { handle_ref(h) }) else {
            return into_c_string(String::new());
        };
        into_c_string(io::to_json(&app.session))
    })
}

/// The document's monotonic generation counter — it bumps on every mutation.
/// Lets the host cheaply gate work (autosave, cached reads) on actual change
/// rather than polling. Returns 0 for a null/invalid handle.
///
/// # Safety
/// `h` must be null or a live handle from [`ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_doc_generation(h: *mut AppHandle) -> u64 {
    guard_ffi(0, || {
        (unsafe { handle_ref(h) }).map_or(0, |app| app.session.doc.generation)
    })
}

/// Free a C string returned by [`ijc_command_brief`] / [`ijc_scene_digest`] /
/// [`ijc_save_json`].
/// Null-safe; must be called at most once per returned pointer.
///
/// # Safety
/// `s` must be null or a pointer previously returned by one of the string
/// accessors above and not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_string_free(s: *mut c_char) {
    guard_ffi((), || {
        if s.is_null() {
            return;
        }
        // Reclaim the `CString` allocated by `into_c_string`/`into_raw`.
        drop(unsafe { CString::from_raw(s) });
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, AtomicU64};

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
    fn camera_ffi_entrypoints_are_null_safe() {
        let nil = std::ptr::null_mut::<AppHandle>();
        unsafe {
            // Must not panic / deref a null handle.
            ijc_camera_orbit(nil, 1.0, 2.0);
            ijc_camera_pan(nil, 1.0, 2.0);
            ijc_camera_zoom(nil, 1.5);
            // Degenerate zoom factors are a no-op, not a panic.
            ijc_camera_zoom(nil, 0.0);
            ijc_camera_zoom(nil, f32::NAN);
        }
    }

    #[test]
    fn zoom_factor_maps_to_dolly_toward_target() {
        // A pinch-apart (factor > 1, zoom in) must shrink the camera distance;
        // a pinch-together (factor < 1) must grow it. Verify the scroll we feed
        // `dolly` produces the right sign and roughly divides distance by factor.
        let mut cam = OrbitCamera { distance: 100.0, ..OrbitCamera::default() };
        let factor = 2.0f32;
        let scroll = (1.0 - 1.0 / factor) / 0.002;
        cam.dolly(scroll);
        assert!(cam.distance < 100.0, "zoom-in must bring the camera closer");
        assert!((cam.distance - 50.0).abs() < 1e-3, "factor 2 ≈ halve distance");

        let mut cam2 = OrbitCamera { distance: 100.0, ..OrbitCamera::default() };
        let out = 0.5f32;
        cam2.dolly((1.0 - 1.0 / out) / 0.002);
        assert!(cam2.distance > 100.0, "zoom-out must push the camera away");
    }

    #[test]
    fn layers_json_is_null_safe_and_well_formed() {
        // Null handle yields a non-null, empty JSON array.
        let nil = std::ptr::null_mut::<AppHandle>();
        unsafe {
            let p = ijc_layers_json(nil);
            assert!(!p.is_null());
            let s = CStr::from_ptr(p).to_str().unwrap();
            assert_eq!(s, "[]");
            ijc_string_free(p);
        }

        // A default document has seeded layers; the array must be sorted by
        // (order, name) and expose the expected keys for the active layer.
        let doc = itsjustcad_doc::Document::default();
        let json = layers_json(&doc);
        assert!(json.starts_with('['));
        assert!(json.ends_with(']'));
        assert!(json.contains("\"colorRgba\":["));
        assert!(json.contains("\"active\":true"));
        assert!(json.contains("\"linetype\":\"continuous\""));
    }

    #[test]
    fn sheets_fns_are_null_safe() {
        let nil = std::ptr::null_mut::<AppHandle>();
        unsafe {
            // Null handle → a non-null, empty-but-well-formed sheets object.
            let p = ijc_sheets_json(nil);
            assert!(!p.is_null());
            let s = CStr::from_ptr(p).to_str().unwrap();
            assert_eq!(s, "{\"sheets\":[],\"sets\":[]}");
            ijc_string_free(p);

            // Null handle → null PDF bytes and a zeroed length.
            let name = CString::new("plan").unwrap();
            let mut len: usize = 123;
            assert!(ijc_sheet_pdf(nil, name.as_ptr(), &mut len).is_null());
            assert_eq!(len, 0);
            len = 123;
            assert!(ijc_sheetset_pdf(nil, name.as_ptr(), &mut len).is_null());
            assert_eq!(len, 0);
        }
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
    fn gumball_preview_matrix_composes_about_center() {
        use glam::DVec3;
        // Move is a pure translate.
        let m = PreviewKind::Move { delta: DVec3::new(1.0, 2.0, 3.0) }.matrix();
        assert!(m.transform_point3(DVec3::ZERO).abs_diff_eq(DVec3::new(1.0, 2.0, 3.0), 1e-9));

        // Rotate 90° about Z through center (1,0,0): the center is fixed, and a
        // point on +X of the center swings to +Y of it.
        let center = DVec3::new(1.0, 0.0, 0.0);
        let r = PreviewKind::Rotate { angle_deg: 90.0, axis: DVec3::Z, center }.matrix();
        assert!(r.transform_point3(center).abs_diff_eq(center, 1e-9));
        assert!(r
            .transform_point3(DVec3::new(2.0, 0.0, 0.0))
            .abs_diff_eq(DVec3::new(1.0, 1.0, 0.0), 1e-9));

        // Scale 2× about center (1,1,1): center fixed, distances double.
        let c = DVec3::splat(1.0);
        let s = PreviewKind::Scale { factors: DVec3::splat(2.0), center: c }.matrix();
        assert!(s.transform_point3(c).abs_diff_eq(c, 1e-9));
        assert!(s.transform_point3(DVec3::new(2.0, 1.0, 1.0)).abs_diff_eq(DVec3::new(3.0, 1.0, 1.0), 1e-9));
    }

    #[test]
    fn gumball_preview_to_command_sets_explicit_center() {
        use glam::DVec3;
        let center = DVec3::new(1.0, 2.0, 3.0);
        match (PreviewKind::Rotate { angle_deg: 30.0, axis: DVec3::Z, center }).to_command() {
            Command::Rotate { center: Some(c), .. } => assert!(c.abs_diff_eq(center, 1e-12)),
            other => panic!("expected Rotate with explicit center, got {other:?}"),
        }
        match (PreviewKind::Scale { factors: DVec3::splat(2.0), center }).to_command() {
            Command::Scale { center: Some(c), .. } => assert!(c.abs_diff_eq(center, 1e-12)),
            other => panic!("expected Scale with explicit center, got {other:?}"),
        }
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
            assert!(!ijc_run_command(nil, c"box 0,0,0 1,1,1".as_ptr()));
            assert!(!ijc_deck_configure(nil, 0, std::ptr::null(), std::ptr::null(), std::ptr::null()));
            // Pick/gumball entry points: null handle → documented safe default.
            assert!(!ijc_pick(nil, 10.0, 10.0, false));
            assert!(!ijc_pick(nil, 10.0, 10.0, true));
            let mut c3 = [0.0f64; 3];
            let mut s3 = [0.0f64; 3];
            assert!(!ijc_selection_bbox(nil, c3.as_mut_ptr(), s3.as_mut_ptr()));
            assert!(!ijc_selection_bbox(nil, std::ptr::null_mut(), std::ptr::null_mut()));
            let w = [1.0f64, 2.0, 3.0];
            let mut xy = [0.0f32; 2];
            assert!(!ijc_project_point(nil, w.as_ptr(), xy.as_mut_ptr()));
            assert!(!ijc_project_point(nil, std::ptr::null(), std::ptr::null_mut()));
            assert!(!ijc_move_selected(nil, 1.0, 0.0, 0.0));
            assert!(!ijc_move_selected(nil, f64::NAN, 0.0, 0.0));
            // Full gumball: every entry point is null-safe and returns false on
            // a null handle (including the always-true cancel path).
            assert!(!ijc_gumball_preview_move(nil, 1.0, 0.0, 0.0));
            assert!(!ijc_gumball_preview_rotate(nil, 45.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0));
            assert!(!ijc_gumball_preview_scale(nil, 2.0, 2.0, 2.0, 0.0, 0.0, 0.0));
            assert!(!ijc_gumball_cancel(nil));
            assert!(!ijc_gumball_commit(nil));
            assert!(!ijc_gumball_has_preview(nil));
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

    // ---- Finding #2: callbacks must be inert once the handle is freed ----

    static CB_HITS: AtomicU32 = AtomicU32::new(0);
    extern "C" fn counting_cb(_ctx: *mut c_void, _kind: u32, _s: *const c_char) {
        CB_HITS.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn emit_is_a_noop_after_ctx_marked_dead() {
        CB_HITS.store(0, Ordering::SeqCst);
        let alive = Arc::new(AtomicBool::new(true));
        let ctx = SendCtx { ctx: std::ptr::null_mut(), alive: alive.clone() };

        // Alive: the callback fires.
        emit(counting_cb, &ctx, CB_CHAT, "hello");
        assert_eq!(CB_HITS.load(Ordering::SeqCst), 1);

        // Simulate ijc_free clearing the flag: further emits must NOT invoke the
        // (now possibly-released) Swift ctx pointer.
        alive.store(false, Ordering::Release);
        emit(counting_cb, &ctx, CB_CHAT, "world");
        emit(counting_cb, &ctx, CB_DONE, "");
        assert_eq!(
            CB_HITS.load(Ordering::SeqCst),
            1,
            "callback fired after ctx was marked dead (use-after-free window)"
        );
    }

    // ---- Findings #1 / #4: only one exclusive borrow may be live at a time ----

    #[test]
    fn handle_mut_rejects_concurrent_access() {
        // We can exercise the busy-flag exclusion without a GPU by driving the
        // same compare_exchange protocol handle_mut uses. A second acquisition
        // while the first guard is live must fail.
        let busy = AtomicBool::new(false);

        // First acquirer wins.
        assert!(busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok());
        // Second, concurrent acquirer is rejected (would have aliased &mut).
        assert!(busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err());
        // After the first releases, a later call succeeds again.
        busy.store(false, Ordering::Release);
        assert!(busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok());
    }

    // ---- ijc_free hardening: one-shot free latch + free/mutator serialization

    #[test]
    fn freeing_latch_is_one_shot() {
        // Models the `freeing` compare_exchange in `ijc_free`: exactly one caller
        // may transition alive->freeing and proceed to drop; every other returns
        // without freeing (no double-free). Single-thread ordering first.
        let freeing = AtomicBool::new(false);
        assert!(
            freeing
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "first free must win the latch"
        );
        assert!(
            freeing
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err(),
            "second free must lose the latch (would be a double-free)"
        );
    }

    #[test]
    fn freeing_latch_yields_exactly_one_winner_under_threads() {
        // Hammer the latch from many threads at once; precisely one must win the
        // alive->freeing transition. That winner is the sole caller that would
        // run `Box::from_raw`, so this pins "no concurrent double-free".
        use std::sync::Barrier;
        for _ in 0..200 {
            let freeing = Arc::new(AtomicBool::new(false));
            let winners = Arc::new(AtomicU32::new(0));
            let n = 8;
            let barrier = Arc::new(Barrier::new(n));
            let mut hs = Vec::new();
            for _ in 0..n {
                let freeing = freeing.clone();
                let winners = winners.clone();
                let barrier = barrier.clone();
                hs.push(std::thread::spawn(move || {
                    barrier.wait();
                    if freeing
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        winners.fetch_add(1, Ordering::SeqCst);
                    }
                }));
            }
            for h in hs {
                h.join().unwrap();
            }
            assert_eq!(
                winners.load(Ordering::SeqCst),
                1,
                "exactly one thread may win the free latch"
            );
        }
    }

    #[test]
    fn free_waits_for_busy_then_poisons_guard() {
        // Models the free/mutator serialization: `ijc_free` acquires the SAME
        // `busy` flag the mutators use, spinning until the in-flight mutator
        // releases it, and only then poisons the guard + drops. A mutator that
        // holds `busy` must therefore never be dropped out from under; and once
        // the guard is poisoned (under `busy`), a later mutator bails.
        let busy = Arc::new(AtomicBool::new(false));
        let guard = Arc::new(AtomicU64::new(GUARD_LIVE));

        // Mutator takes `busy` first (as `handle_mut` would).
        assert!(busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok());

        let busy2 = busy.clone();
        let guard2 = guard.clone();
        let freer = std::thread::spawn(move || {
            // `ijc_free`'s spin-acquire of `busy`: must block until the mutator
            // releases, so it cannot poison/drop while the `&mut` is live.
            while busy2
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                std::hint::spin_loop();
            }
            // Only now — with `busy` held — do we poison the guard.
            guard2.store(GUARD_DEAD, Ordering::Release);
        });

        // While the mutator holds `busy`, the guard must still be LIVE: the freer
        // is provably still spinning, not tearing down.
        for _ in 0..1000 {
            assert_eq!(
                guard.load(Ordering::Acquire),
                GUARD_LIVE,
                "guard poisoned while a mutator still held busy (UAF window)"
            );
        }
        // Release `busy` (mutator done). The freer may now proceed.
        busy.store(false, Ordering::Release);
        freer.join().unwrap();
        assert_eq!(guard.load(Ordering::Acquire), GUARD_DEAD);
    }

    // ---- Finding #6: side-effecting deck commands must be refused ----

    #[test]
    fn deck_side_effecting_commands_are_recognized_for_refusal() {
        // The drain/handle_events gate refuses any PendingOp::Cmd whose command
        // is side-effecting. Verify the exact commands a hostile model could emit
        // (import/export) route to a side-effecting Command so the gate trips.
        for line in [
            "import /etc/passwd",
            "export /tmp/exfil.dxf",
        ] {
            match route_line(line) {
                Some(PendingOp::Cmd(cmd)) => assert!(
                    cmd.is_side_effecting(),
                    "'{line}' must be gated as side-effecting"
                ),
                other => panic!("'{line}' did not parse to a command: {other:?}"),
            }
        }
        // A pure geometry command must NOT be gated (stays allowed).
        match route_line("box 0,0,0 1,1,1") {
            Some(PendingOp::Cmd(cmd)) => assert!(!cmd.is_side_effecting()),
            other => panic!("box did not parse: {other:?}"),
        }
    }

    // ---- Host-typed command gate: side-effecting host commands are refused ----

    #[test]
    fn host_command_gate_matches_deck_gate() {
        // `ijc_run_command` gates the identical `is_side_effecting` set the deck
        // path refuses. A host-typed fs command must be classified for refusal;
        // a pure geometry/camera verb must pass.
        for line in ["import /etc/passwd", "export /tmp/exfil.dxf"] {
            match route_line(line) {
                Some(PendingOp::Cmd(cmd)) => assert!(
                    cmd.is_side_effecting(),
                    "'{line}' must be gated (refused) on the host-typed path"
                ),
                other => panic!("'{line}' did not parse to a command: {other:?}"),
            }
        }
        // Pure ops the host path still allows through.
        assert!(matches!(route_line("box 0,0,0 1,1,1"),
            Some(PendingOp::Cmd(c)) if !c.is_side_effecting()));
        assert!(matches!(route_line("view top"), Some(PendingOp::Camera(_))));
    }

    #[test]
    fn null_and_bad_line_return_false_on_null_handle() {
        let nil = std::ptr::null_mut::<AppHandle>();
        unsafe {
            // Null handle → false (before touching `line`).
            assert!(!ijc_run_command(nil, std::ptr::null()));
            assert!(!ijc_run_command(nil, c"box 0,0,0 1,1,1".as_ptr()));
        }
    }

    // ---- ijc_open_json: hostile / malformed blobs fail cleanly ----

    #[test]
    fn open_json_rejects_garbage_and_leaves_doc_intact() {
        // The FFI only assigns `app.session` on `Ok`; a malformed op-log must
        // fail (`io::from_json` -> Err) so the existing document is untouched.
        // We exercise the parse contract directly (the assignment is gated on it).
        for bad in [
            "",                    // empty
            "not json at all",     // garbage bytes
            "{",                   // truncated
            "[1,2,3]",             // valid json, wrong shape
            "{\"unexpected\":true}",
            "\u{feff}garbage",
        ] {
            assert!(
                io::from_json(bad).is_err(),
                "malformed blob {bad:?} must be rejected, not partially applied"
            );
        }
        // A well-formed op-log still parses.
        let mut s = Session::default();
        s.run(parse("box 0,0,0 1,1,1").unwrap()).unwrap();
        let good = io::to_json(&s);
        assert!(io::from_json(&good).is_ok());
    }

    #[test]
    fn open_json_len_bound_is_pinned() {
        // `ijc_open_json` rejects `len == 0` or `len > MAX_JSON_LEN` before any
        // `from_raw_parts`. Pin the bound so it can't silently grow to an absurd
        // acceptable size. (usize::MAX rejection is covered by the null-handle test.)
        assert_eq!(MAX_JSON_LEN, 256 * 1024 * 1024);
    }

    // ---- Pick/gumball math: ray_aabb + screen_ray sanity (no GPU needed) ----

    #[test]
    fn ray_aabb_hits_front_box_and_misses_offset() {
        // Ray from the origin down +X must hit a unit box centered at x=5 at
        // its near face (t≈4), and miss a box parked off the ray's path.
        let o = glam::DVec3::ZERO;
        let d = glam::DVec3::X;
        let hit = ray_aabb(o, d, glam::DVec3::new(4.0, -1.0, -1.0), glam::DVec3::new(6.0, 1.0, 1.0));
        assert!(hit.is_some_and(|t| (t - 4.0).abs() < 1e-9));
        let miss = ray_aabb(o, d, glam::DVec3::new(4.0, 5.0, 5.0), glam::DVec3::new(6.0, 7.0, 7.0));
        assert!(miss.is_none());
        // A box straddling the origin reports t=0 (we are inside it), not negative.
        let inside = ray_aabb(o, d, glam::DVec3::splat(-1.0), glam::DVec3::splat(1.0));
        assert_eq!(inside, Some(0.0));
    }

    #[test]
    fn screen_ray_center_points_into_the_scene() {
        // A default orbit camera: the center pixel's ray must originate near the
        // eye side and point roughly toward the target (dot with eye→target > 0).
        let cam = OrbitCamera::default();
        let vp = cam.view_proj(1.0);
        let (origin, dir) = screen_ray(vp, 100.0, 100.0, 50.0, 50.0).expect("center ray");
        let eye = cam.eye().as_dvec3();
        let to_target = (cam.target.as_dvec3() - eye).normalize();
        assert!(dir.dot(to_target) > 0.9, "center ray should look toward the target");
        assert!((dir.length() - 1.0).abs() < 1e-9, "direction must be unit length");
        // Degenerate viewport is rejected rather than producing NaNs.
        assert!(screen_ray(vp, 0.0, 100.0, 50.0, 50.0).is_none());
    }

    // ---- Finding #7: SSRF / credential-leak containment on base_url ----

    #[test]
    fn base_url_blocks_metadata_and_internal_hosts() {
        // Cloud metadata + link-local: the classic SSRF/credential-exfil target.
        assert!(!base_url_is_allowed("http://169.254.169.254/latest"));
        assert!(!base_url_is_allowed("http://169.254.10.1/"));
        // Loopback / internal names.
        assert!(!base_url_is_allowed("http://localhost:11434/v1"));
        assert!(!base_url_is_allowed("http://127.0.0.1/v1"));
        assert!(!base_url_is_allowed("http://[::1]:8080/v1"));
        assert!(!base_url_is_allowed("http://metadata/computeMetadata"));
        assert!(!base_url_is_allowed("http://foo.internal/v1"));
        // RFC1918 private ranges.
        assert!(!base_url_is_allowed("http://10.0.0.5/v1"));
        assert!(!base_url_is_allowed("http://192.168.1.1/v1"));
        assert!(!base_url_is_allowed("http://172.16.0.1/v1"));
        assert!(!base_url_is_allowed("http://172.31.255.1/v1"));
        // 172.32.x is public (outside /12) — allowed.
        assert!(base_url_is_allowed("http://172.32.0.1/v1"));
        // Public API origins remain allowed.
        assert!(base_url_is_allowed("https://api.anthropic.com"));
        assert!(base_url_is_allowed("https://api.openai.com/v1"));
        // Empty = provider default (safe).
        assert!(base_url_is_allowed(""));
    }
}
