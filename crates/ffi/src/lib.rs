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
                                    if let Ok(mut p) = pending.lock() {
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
            assert!(!ijc_run_command(nil, c"box 0,0,0 1,1,1".as_ptr()));
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
