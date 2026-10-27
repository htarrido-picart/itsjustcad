// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Several cameras and pane surfaces (1/2/4 pane layouts), and named views
//! against the FFI camera (#165).
//!
//! # Cameras
//! Camera 0 always exists; [`ijc_camera_new`] adds more. Exactly one camera
//! is ACTIVE (camera 0 until [`ijc_camera_activate`]): the existing camera
//! calls ([`super::ijc_camera_orbit`], `pan`, `zoom`, `zoom_extents`, typed
//! view verbs, `view save`/`view <name>`), picks, [`super::ijc_project_point`],
//! [`super::ijc_screen_to_world`] and [`super::ijc_camera_state_json`] all
//! use the active camera and its pane size. A host with panes activates the
//! pane under the pointer before forwarding its gestures.
//!
//! # Surfaces
//! Surface 0 is the [`super::ijc_init`] view. [`ijc_surface_new`] binds
//! another `CAMetalLayer`-backed view on the same GPU device.
//! [`ijc_render_camera`] draws any camera into any surface and records that
//! surface's size as the camera's pane size. [`super::ijc_render_frame`] is
//! `ijc_render_camera(h, 0, 0)`, so a single-surface host is unchanged.
//!
//! # Named views
//! `view save <name>` stores the active camera (logged, undoable; saved in
//! the document); `view <name>` applies a saved view to the active camera;
//! [`ijc_camera_set_view`] applies a standard or saved view to any camera;
//! [`ijc_views_json`] lists them.

use std::collections::BTreeMap;
use std::ffi::{c_char, c_void};
use std::ptr::NonNull;

use itsjustcad_commands::{ApplyOutcome, Command, ExecError};
use itsjustcad_doc::{NamedView, PanoView};
use itsjustcad_render::{OrbitCamera, PanoProjection};
use raw_window_handle::{RawDisplayHandle, RawWindowHandle, UiKitDisplayHandle, UiKitWindowHandle};

use super::{
    AppHandle, Gpu, apply_pending, c_str, camera_state_json, clamp_dim, guard_ffi, handle_mut,
    handle_ref, into_c_string, make_depth, standard_view,
};

/// One inactive camera and its pane size in pixels (`None` until it is drawn
/// or sized; camera 0 then falls back to the main drawable).
#[derive(Clone, Copy, Debug)]
struct Slot {
    cam: OrbitCamera,
    viewport: Option<(f32, f32)>,
}

/// The handle's cameras other than the active one (which lives in
/// `AppHandle::camera`, so every existing camera path keeps working).
#[derive(Debug, Default)]
pub(crate) struct Cameras {
    active: u32,
    active_viewport: Option<(f32, f32)>,
    /// Inactive cameras by id, camera 0 included while another is active.
    store: BTreeMap<u32, Slot>,
    /// Last id handed out by [`ijc_camera_new`].
    last_id: u32,
}

impl Cameras {
    /// The active camera's pane size, if it has one.
    pub(crate) fn active_viewport(&self) -> Option<(f32, f32)> {
        self.active_viewport
    }

    fn exists(&self, id: u32) -> bool {
        id == self.active || self.store.contains_key(&id)
    }

    /// A copy of camera `id`; `active` is the handle's active camera.
    pub(crate) fn camera(&self, active: &OrbitCamera, id: u32) -> Option<OrbitCamera> {
        if id == self.active {
            Some(*active)
        } else {
            self.store.get(&id).map(|s| s.cam)
        }
    }

    /// Record camera `id`'s pane size.
    pub(crate) fn set_viewport(&mut self, id: u32, size: (f32, f32)) {
        if id == self.active {
            self.active_viewport = Some(size);
        } else if let Some(s) = self.store.get_mut(&id) {
            s.viewport = Some(size);
        }
    }

    fn viewport(&self, id: u32) -> Option<(f32, f32)> {
        if id == self.active {
            self.active_viewport
        } else {
            self.store.get(&id).and_then(|s| s.viewport)
        }
    }
}

/// Make camera `id` the active one. Applies queued ops first, so gestures
/// queued for the old camera land on it. False for an unknown id.
fn activate(app: &mut AppHandle, id: u32) -> bool {
    if !app.cams.exists(id) {
        return false;
    }
    apply_pending(app);
    if id == app.cams.active {
        return true;
    }
    let Some(next) = app.cams.store.remove(&id) else {
        return false;
    };
    let prev = Slot {
        cam: app.camera,
        viewport: app.cams.active_viewport,
    };
    app.cams.store.insert(app.cams.active, prev);
    app.camera = next.cam;
    app.cams.active_viewport = next.viewport;
    app.cams.active = id;
    true
}

/// Mutable camera `id`.
fn camera_mut(app: &mut AppHandle, id: u32) -> Option<&mut OrbitCamera> {
    if id == app.cams.active {
        Some(&mut app.camera)
    } else {
        app.cams.store.get_mut(&id).map(|s| &mut s.cam)
    }
}

// ── named views ─────────────────────────────────────────────────────────────

/// The camera as document-storable named-view parameters (the desktop's
/// `named_view_of`).
pub(crate) fn named_view_of(cam: &OrbitCamera) -> NamedView {
    NamedView {
        target: cam.target.to_array(),
        distance: cam.distance,
        yaw: cam.yaw,
        pitch: cam.pitch,
        fov_y: cam.fov_y,
        ortho: cam.ortho,
        two_point: cam.two_point,
        pano: cam.pano.map(|p| match p {
            PanoProjection::Equirect => PanoView::Equirect,
            PanoProjection::Fisheye { fov } => PanoView::Fisheye { fov },
        }),
    }
}

/// Drive `cam` to a saved view (the desktop's `apply_named_view`).
pub(crate) fn apply_named_view(cam: &mut OrbitCamera, view: &NamedView) {
    cam.target = glam::Vec3::from_array(view.target);
    cam.distance = view.distance;
    cam.yaw = view.yaw;
    cam.pitch = view.pitch;
    cam.fov_y = view.fov_y;
    cam.ortho = view.ortho;
    cam.two_point = view.two_point;
    cam.pano = view.pano.map(|p| match p {
        PanoView::Equirect => PanoProjection::Equirect,
        PanoView::Fisheye { fov } => PanoProjection::Fisheye { fov },
    });
}

/// Run a document command with the FFI camera standing in for the desktop
/// viewport: `view save <name>` captures the active camera, and a `view
/// <name>` restore parked in the document mailbox is applied to it.
pub(crate) fn run_with_camera(
    app: &mut AppHandle,
    cmd: Command,
) -> Result<ApplyOutcome, ExecError> {
    let cmd = match cmd {
        Command::ViewSave { name, camera: None } => Command::ViewSave {
            name,
            camera: Some(named_view_of(&app.camera)),
        },
        other => other,
    };
    let out = app.session.run(cmd);
    if let Some(view) = app.session.doc.pending_view.take() {
        apply_named_view(&mut app.camera, &view);
    }
    out
}

/// The saved named views, sorted by name: `[{"name", "target":[x,y,z],
/// "distance", "yaw", "pitch", "fov_y", "ortho", "two_point", "pano"?}]`
/// (radians; `pano` only for a panorama/fisheye view, as
/// `{"kind":"equirect"}` / `{"kind":"fisheye","fov"}`). Save with the
/// `view save <name>` command (it captures the active camera), apply with
/// `view <name>` or [`ijc_camera_set_view`]. Returns `"[]"` for a
/// null/invalid handle. Free with [`super::ijc_string_free`].
///
/// # Safety
/// `h` must be null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_views_json(h: *mut AppHandle) -> *mut c_char {
    guard_ffi(std::ptr::null_mut(), || {
        let Some(app) = (unsafe { handle_ref(h) }) else {
            return into_c_string(String::from("[]"));
        };
        let views: Vec<serde_json::Value> = app
            .session
            .doc
            .named_views
            .iter()
            .filter_map(|(name, v)| {
                let mut o = serde_json::to_value(v).ok()?;
                o["name"] = name.clone().into();
                Some(o)
            })
            .collect();
        into_c_string(serde_json::Value::Array(views).to_string())
    })
}

/// Set camera `cam` to a view by `name`: a standard view (`top`, `front`,
/// `right`, `left`, `back`, `bottom`, `persp`/`iso`) or a saved named view.
/// Applies now, without activating the camera (queued ops are applied
/// first). False for an unknown camera or view name, or a null/invalid/busy
/// handle.
///
/// # Safety
/// `h` must be null or a live handle; `name` must be null or a valid
/// NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_camera_set_view(
    h: *mut AppHandle,
    cam: u32,
    name: *const c_char,
) -> bool {
    guard_ffi(false, || {
        let Some(mut guard) = (unsafe { handle_mut(h) }) else {
            return false;
        };
        let app: &mut AppHandle = &mut guard;
        let Some(name) = (unsafe { c_str(name) }) else {
            return false;
        };
        apply_pending(app);
        let named = app.session.doc.named_views.get(name).copied();
        let Some(camera) = camera_mut(app, cam) else {
            return false;
        };
        if let Some(v) = standard_view(name) {
            camera.set_view(v);
        } else if let Some(view) = named {
            apply_named_view(camera, &view);
        } else {
            return false;
        }
        true
    })
}

// ── cameras ─────────────────────────────────────────────────────────────────

/// Add a camera, a copy of the active one, and return its id (≥ 1). It is not
/// activated. Returns 0 on a null/invalid/busy handle.
///
/// # Safety
/// `h` must be null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_camera_new(h: *mut AppHandle) -> u32 {
    guard_ffi(0, || {
        let Some(mut guard) = (unsafe { handle_mut(h) }) else {
            return 0;
        };
        let app: &mut AppHandle = &mut guard;
        apply_pending(app);
        let Some(id) = app.cams.last_id.checked_add(1) else {
            return 0;
        };
        app.cams.last_id = id;
        let slot = Slot {
            cam: app.camera,
            viewport: app.cams.active_viewport,
        };
        app.cams.store.insert(id, slot);
        id
    })
}

/// Remove camera `cam`. Camera 0 cannot be removed. Removing the active
/// camera activates camera 0 first. False for camera 0, an unknown id, or a
/// null/invalid/busy handle.
///
/// # Safety
/// `h` must be null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_camera_free(h: *mut AppHandle, cam: u32) -> bool {
    guard_ffi(false, || {
        let Some(mut guard) = (unsafe { handle_mut(h) }) else {
            return false;
        };
        let app: &mut AppHandle = &mut guard;
        if cam == 0 || !app.cams.exists(cam) {
            return false;
        }
        if app.cams.active == cam && !activate(app, 0) {
            return false;
        }
        app.cams.store.remove(&cam).is_some()
    })
}

/// Make camera `cam` the active camera (see the module docs: every existing
/// camera call, pick and screen query then uses it and its pane size). Queued
/// ops are applied to the previous camera first. False for an unknown id or a
/// null/invalid/busy handle.
///
/// # Safety
/// `h` must be null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_camera_activate(h: *mut AppHandle, cam: u32) -> bool {
    guard_ffi(false, || {
        let Some(mut guard) = (unsafe { handle_mut(h) }) else {
            return false;
        };
        activate(&mut guard, cam)
    })
}

/// The active camera's id (0 until [`ijc_camera_activate`]); 0 on a
/// null/invalid handle.
///
/// # Safety
/// `h` must be null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_camera_active(h: *mut AppHandle) -> u32 {
    guard_ffi(0, || {
        unsafe { handle_ref(h) }.map_or(0, |app| app.cams.active)
    })
}

/// Set camera `cam`'s pane size in pixels (the space picks and
/// [`super::ijc_screen_to_world`] take while it is active).
/// [`ijc_render_camera`] sets it from the surface it draws into, so a GPU host
/// rarely needs this; a headless host does. Camera 0 uses the main drawable
/// ([`super::ijc_resize`]) and is refused here. False for camera 0, an unknown
/// id, a zero or non-finite size, or a null/invalid/busy handle.
///
/// # Safety
/// `h` must be null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_camera_resize(h: *mut AppHandle, cam: u32, w: u32, h_px: u32) -> bool {
    guard_ffi(false, || {
        let Some(mut guard) = (unsafe { handle_mut(h) }) else {
            return false;
        };
        if cam == 0 || w == 0 || h_px == 0 || !guard.cams.exists(cam) {
            return false;
        }
        guard.cams.set_viewport(cam, (w as f32, h_px as f32));
        true
    })
}

/// Camera `cam` as [`super::ijc_camera_state_json`] reports the active one,
/// with `viewport` its pane size (camera 0: the main drawable, or the
/// headless [`super::ijc_resize`] size). `null` for an unknown id or a
/// null/invalid handle. Free with [`super::ijc_string_free`].
///
/// # Safety
/// `h` must be null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_camera_json(h: *mut AppHandle, cam: u32) -> *mut c_char {
    guard_ffi(std::ptr::null_mut(), || {
        let Some(app) = (unsafe { handle_ref(h) }) else {
            return into_c_string(String::from("null"));
        };
        let Some(camera) = app.cams.camera(&app.camera, cam) else {
            return into_c_string(String::from("null"));
        };
        let main = || {
            let c = app
                .gpu
                .as_ref()
                .map(|g| (g.config.width as f32, g.config.height.max(1) as f32));
            c.or(app.viewport)
        };
        let viewport = app
            .cams
            .viewport(cam)
            .or_else(|| (cam == 0).then(main).flatten());
        into_c_string(camera_state_json(&camera, viewport))
    })
}

// ── surfaces ────────────────────────────────────────────────────────────────

/// An extra pane surface on the main GPU device.
pub(crate) struct PaneSurface {
    pub(crate) surface: wgpu::Surface<'static>,
    pub(crate) config: wgpu::SurfaceConfiguration,
    pub(crate) depth_view: wgpu::TextureView,
}

/// Surface `id`'s size in pixels (0 = the main surface).
pub(crate) fn surface_size(gpu: &Gpu, id: u32) -> Option<(u32, u32)> {
    let c = if id == 0 {
        &gpu.config
    } else {
        &gpu.panes.get(&id)?.config
    };
    Some((c.width, c.height))
}

/// Bind another `CAMetalLayer`-backed `UIView` as a pane surface, `w`×`h`
/// physical pixels, sharing the main device, renderer and scene. Returns its
/// id (≥ 1), or 0 on a headless or null/invalid/busy handle, a null view, or
/// a view whose layer cannot take the main surface's pixel format. Draw into
/// it with [`ijc_render_camera`]; free it with [`ijc_surface_free`] before
/// the view goes away.
///
/// # Safety
/// `h` must be null or a live handle from [`super::ijc_init`]; `ui_view`
/// must be null or a valid `*UIView` that outlives the surface.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_surface_new(
    h: *mut AppHandle,
    ui_view: *mut c_void,
    w: u32,
    h_px: u32,
) -> u32 {
    guard_ffi(0, || {
        let Some(mut guard) = (unsafe { handle_mut(h) }) else {
            return 0;
        };
        let Some(gpu) = guard.gpu.as_mut() else {
            return 0;
        };
        let Some(view) = NonNull::new(ui_view) else {
            return 0;
        };
        let surface = match unsafe {
            gpu.instance
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: Some(RawDisplayHandle::UiKit(UiKitDisplayHandle::new())),
                    raw_window_handle: RawWindowHandle::UiKit(UiKitWindowHandle::new(view)),
                })
        } {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[ijc] pane create_surface failed: {e:?}");
                return 0;
            }
        };
        let caps = surface.get_capabilities(&gpu.adapter);
        // The shared renderer's pipelines target the main format.
        if !caps.formats.contains(&gpu.config.format) {
            return 0;
        }
        let config = wgpu::SurfaceConfiguration {
            width: clamp_dim(w, &gpu.device),
            height: clamp_dim(h_px, &gpu.device),
            alpha_mode: caps.alpha_modes[0],
            ..gpu.config.clone()
        };
        surface.configure(&gpu.device, &config);
        let depth_view = make_depth(&gpu.device, config.width, config.height);
        let Some(id) = gpu.next_pane.checked_add(1) else {
            return 0;
        };
        gpu.next_pane = id;
        gpu.panes.insert(
            id,
            PaneSurface {
                surface,
                config,
                depth_view,
            },
        );
        id
    })
}

/// Resize pane surface `surface` to `w`×`h` physical pixels (surface 0 is
/// [`super::ijc_resize`]). False for an unknown surface, a headless handle or
/// a null/invalid/busy handle.
///
/// # Safety
/// `h` must be null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_surface_resize(
    h: *mut AppHandle,
    surface: u32,
    w: u32,
    h_px: u32,
) -> bool {
    guard_ffi(false, || {
        let Some(mut guard) = (unsafe { handle_mut(h) }) else {
            return false;
        };
        let Some(gpu) = guard.gpu.as_mut() else {
            return false;
        };
        let device = &gpu.device;
        let Some(p) = gpu.panes.get_mut(&surface) else {
            return false;
        };
        p.config.width = clamp_dim(w, device);
        p.config.height = clamp_dim(h_px, device);
        p.surface.configure(device, &p.config);
        p.depth_view = make_depth(device, p.config.width, p.config.height);
        true
    })
}

/// Release pane surface `surface` (not surface 0, which lives as long as the
/// handle). False for an unknown surface or a null/invalid/busy handle.
///
/// # Safety
/// `h` must be null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_surface_free(h: *mut AppHandle, surface: u32) -> bool {
    guard_ffi(false, || {
        let Some(mut guard) = (unsafe { handle_mut(h) }) else {
            return false;
        };
        guard
            .gpu
            .as_mut()
            .is_some_and(|g| g.panes.remove(&surface).is_some())
    })
}

/// Apply the queued ops, then draw camera `cam` into surface `surface` (0 =
/// the [`super::ijc_init`] view), and record that surface's size as the
/// camera's pane size. Call once per pane per display-link tick.
/// `ijc_render_camera(h, 0, 0)` is [`super::ijc_render_frame`]. False on a
/// headless handle (nothing drawn, queue left for [`super::ijc_flush`]), an
/// unknown camera or surface, a skipped frame, or a null/invalid/busy handle.
///
/// # Safety
/// `h` must be null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_render_camera(h: *mut AppHandle, cam: u32, surface: u32) -> bool {
    guard_ffi(false, || {
        let Some(mut guard) = (unsafe { handle_mut(h) }) else {
            return false;
        };
        super::render_view(&mut guard, cam, surface)
    })
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use std::ffi::{CStr, CString};

    fn c(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    unsafe fn r(p: *mut c_char) -> serde_json::Value {
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_owned();
        unsafe { ijc_string_free(p) };
        serde_json::from_str(&s).unwrap()
    }

    fn ok(h: *mut AppHandle, line: &str) -> serde_json::Value {
        let v = unsafe { r(ijc_run_command_result(h, c(line).as_ptr())) };
        assert_eq!(v["ok"], true, "{line}: {v}");
        v
    }

    #[test]
    fn view_save_and_restore_use_the_ffi_camera() {
        let h = ijc_init_headless();
        unsafe {
            ok(h, "box 0,0,0 4,4,4");
            ok(h, "top");
            let saved = r(ijc_camera_state_json(h));
            let out = ok(h, "view save plan");
            assert!(out["message"].as_str().unwrap().contains("saved"), "{out}");
            ijc_camera_orbit(h, 120.0, 40.0);
            assert!(ijc_flush(h));
            assert_ne!(r(ijc_camera_state_json(h)), saved);
            ok(h, "view plan");
            assert_eq!(r(ijc_camera_state_json(h)), saved, "restored");

            // The queued path works too.
            ijc_camera_orbit(h, 50.0, 0.0);
            assert!(ijc_run_command(h, c("view plan").as_ptr()));
            assert!(ijc_flush(h));
            assert_eq!(r(ijc_camera_state_json(h)), saved);

            let views = r(ijc_views_json(h));
            assert_eq!(views.as_array().unwrap().len(), 1, "{views}");
            assert_eq!(views[0]["name"], "plan");
            assert_eq!(views[0]["ortho"], true);
            assert!(views[0]["target"].is_array());

            // Saved with the document; undo drops it.
            ok(h, "undo");
            assert_eq!(r(ijc_views_json(h)), serde_json::json!([]));
            let bad = r(ijc_run_command_result(h, c("view plan").as_ptr()));
            assert_eq!(bad["ok"], false);
            assert_eq!(
                r(ijc_views_json(std::ptr::null_mut())),
                serde_json::json!([])
            );
            ijc_free(h);
        }
    }

    #[test]
    fn cameras_are_independent_and_activation_routes_gestures() {
        let h = ijc_init_headless();
        unsafe {
            ok(h, "box 0,0,0 4,4,4");
            let main = r(ijc_camera_json(h, 0));
            let cam = ijc_camera_new(h);
            assert_eq!(cam, 1);
            assert_eq!(r(ijc_camera_json(h, cam))["eye"], main["eye"], "a copy");
            assert!(ijc_camera_set_view(h, cam, c("top").as_ptr()));
            assert_eq!(r(ijc_camera_json(h, cam))["projection"], "ortho");
            assert_eq!(r(ijc_camera_json(h, 0)), main, "camera 0 untouched");

            // Gestures go to the active camera only.
            assert!(ijc_camera_activate(h, cam));
            assert_eq!(ijc_camera_active(h), cam);
            assert_eq!(r(ijc_camera_state_json(h))["projection"], "ortho");
            ijc_camera_pan(h, 30.0, 0.0);
            assert!(ijc_flush(h));
            assert_eq!(r(ijc_camera_json(h, 0)), main);
            let moved = r(ijc_camera_json(h, cam));
            assert_ne!(moved["target"], main["target"]);

            // A queued gesture lands on the camera it was queued for.
            ijc_camera_orbit(h, 90.0, 0.0);
            assert!(ijc_camera_activate(h, 0));
            assert_ne!(r(ijc_camera_json(h, cam))["eye"], moved["eye"]);
            assert_eq!(r(ijc_camera_state_json(h)), main);

            // Pane size drives screen queries while active.
            let mut p = [0.0f64; 3];
            assert!(
                !ijc_screen_to_world(h, 10.0, 10.0, p.as_mut_ptr()),
                "no viewport yet"
            );
            assert!(ijc_camera_resize(h, cam, 200, 100));
            assert!(!ijc_camera_resize(h, 0, 200, 100));
            assert_eq!(
                r(ijc_camera_json(h, cam))["viewport"],
                serde_json::json!([200.0, 100.0])
            );
            assert!(ijc_camera_activate(h, cam));
            assert!(ijc_camera_set_view(h, cam, c("top").as_ptr()));
            assert!(ijc_screen_to_world(h, 100.0, 50.0, p.as_mut_ptr()));

            // Named views apply to any camera.
            ok(h, "view save pane");
            assert!(ijc_camera_set_view(h, 0, c("pane").as_ptr()));
            assert_eq!(r(ijc_camera_json(h, 0))["projection"], "ortho");
            assert!(!ijc_camera_set_view(h, 0, c("nope").as_ptr()));
            assert!(!ijc_camera_set_view(h, 9, c("top").as_ptr()));

            // Freeing the active camera falls back to camera 0.
            assert!(ijc_camera_free(h, cam));
            assert_eq!(ijc_camera_active(h), 0);
            assert!(!ijc_camera_free(h, cam));
            assert!(!ijc_camera_free(h, 0));
            assert!(!ijc_camera_activate(h, cam));
            assert_eq!(r(ijc_camera_json(h, cam)), serde_json::Value::Null);
            assert_eq!(ijc_camera_new(h), 2, "ids are not reused");

            // Headless: no surfaces, nothing drawn.
            assert_eq!(ijc_surface_new(h, std::ptr::null_mut(), 10, 10), 0);
            assert!(!ijc_surface_resize(h, 1, 10, 10));
            assert!(!ijc_surface_free(h, 1));
            assert!(!ijc_render_camera(h, 0, 0));
            ijc_render_frame(h);
            assert_eq!(ijc_camera_new(std::ptr::null_mut()), 0);
            assert!(!ijc_camera_activate(std::ptr::null_mut(), 0));
            ijc_free(h);
        }
    }
}
