// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! The **Raytrace render window**: a modeless `egui::Window` that drives the
//! CPU path tracer's progressive API (`itsjustcad_raytrace::render_progressive`)
//! off the UI thread and shows the image refining live, sibling to the
//! diffusion "AI Render" window.
//!
//! This module owns the *testable* pieces — the UI-state → `raytrace::Settings`
//! mapping, resolution presets/parsing, and the background-job state machine
//! (`idle → running → done/cancelled`) — so the GUI painting in `app.rs` is a
//! thin shell over pure logic. The renderer runs on a plain background thread
//! (CPU-bound rayon work, no tokio needed); the UI thread only polls a shared
//! `Arc<Mutex<Shared>>` each frame and uploads the newest frame as a texture.
//! **Cancel** flips an `AtomicBool` the tracer checks between passes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use itsjustcad_raytrace::{Image, Settings, ToneMap};

/// A resolution the window can render at. Presets keep common aspect ratios one
/// click away; `Custom` carries an explicit W×H the user typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// A named preset (label + pixels).
    Preset(&'static str, u32, u32),
    /// A user-entered custom size.
    Custom(u32, u32),
}

impl Resolution {
    /// The built-in presets, in menu order. The first is the default.
    pub const PRESETS: &'static [Resolution] = &[
        Resolution::Preset("640 × 400", 640, 400),
        Resolution::Preset("800 × 500", 800, 500),
        Resolution::Preset("1280 × 800", 1280, 800),
        Resolution::Preset("1920 × 1200", 1920, 1200),
    ];

    pub fn width(self) -> u32 {
        match self {
            Resolution::Preset(_, w, _) | Resolution::Custom(w, _) => w,
        }
    }

    pub fn height(self) -> u32 {
        match self {
            Resolution::Preset(_, _, h) | Resolution::Custom(_, h) => h,
        }
    }

    /// A short human label (`"800 × 500"` / `"custom 640 × 360"`).
    pub fn label(self) -> String {
        match self {
            Resolution::Preset(name, _, _) => name.to_string(),
            Resolution::Custom(w, h) => format!("custom {w} × {h}"),
        }
    }
}

/// Parse a free-form `W×H` string into a [`Resolution::Custom`]. Accepts `x`,
/// `×`, `X`, `*`, or `,` as the separator and tolerates surrounding spaces.
/// Clamps each dimension to `[16, 8192]` so a fat-fingered value can't ask for a
/// terabyte of pixels or a zero-size buffer. Pure — unit-testable.
pub fn parse_custom_res(s: &str) -> Option<Resolution> {
    let norm = s.replace(['×', 'X', '*', ','], "x");
    let (w, h) = norm.split_once('x')?;
    let w: u32 = w.trim().parse().ok()?;
    let h: u32 = h.trim().parse().ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    Some(Resolution::Custom(w.clamp(16, 8192), h.clamp(16, 8192)))
}

/// The window's editable render parameters. Mirrors the diffusion window's small
/// state struct; converts to a `raytrace::Settings` via [`Self::to_settings`].
#[derive(Clone, Copy, Debug)]
pub struct RtControls {
    pub resolution: Resolution,
    pub samples_per_pixel: u32,
    pub max_bounces: u32,
    pub sun_on: bool,
    pub sky_on: bool,
    pub tonemap: ToneMap,
}

impl Default for RtControls {
    fn default() -> Self {
        Self {
            resolution: Resolution::PRESETS[1], // 800 × 500
            samples_per_pixel: 64,
            max_bounces: 6,
            sun_on: true,
            sky_on: true,
            tonemap: ToneMap::Aces,
        }
    }
}

impl RtControls {
    /// Map the UI controls onto a `raytrace::Settings`. Samples/bounces are
    /// clamped to sane, non-zero ranges so the slider extremes can't stall or
    /// panic the tracer. Pure — unit-testable.
    pub fn to_settings(self) -> Settings {
        Settings {
            width: self.resolution.width().clamp(16, 8192),
            height: self.resolution.height().clamp(16, 8192),
            samples_per_pixel: self.samples_per_pixel.clamp(1, 4096),
            max_bounces: self.max_bounces.clamp(0, 32),
            seed: Settings::default().seed,
            tonemap: self.tonemap,
        }
    }
}

/// Progress shared between the render thread (writer) and the UI thread
/// (reader), behind a mutex. The render thread swaps in the newest tone-mapped
/// frame after each pass; the UI thread drains it into a texture.
#[derive(Default)]
pub struct Shared {
    /// Newest tone-mapped frame not yet uploaded to a texture. `take()`-drained
    /// by the UI each frame.
    pub latest: Option<Image>,
    /// 1-based passes accumulated so far.
    pub passes_done: u32,
    /// Total passes planned.
    pub passes_total: u32,
    /// Set once the render thread returns (completed *or* cancelled).
    pub finished: bool,
    /// A human error (empty scene / no meshes), surfaced instead of a blank
    /// window. Mutually exclusive with a useful image.
    pub error: Option<String>,
}

/// The lifecycle of a render job, as a plain value the UI can match on. Kept
/// separate from the (thread-holding) [`RaytraceJob`] so the state machine is
/// pure-testable without spawning threads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    /// No render has run (or the last was cleared).
    Idle,
    /// A pass loop is in flight.
    Running,
    /// The render ran to completion (all passes).
    Done,
    /// The render was cancelled mid-flight.
    Cancelled,
}

/// Derive the [`JobState`] from the observable flags a poll sees: whether a job
/// thread exists, whether it reported `finished`, and whether cancel was
/// requested. Pure — the single source of truth for the state machine, so the
/// transitions can be unit-tested without any threads or a renderer.
pub fn job_state(has_job: bool, finished: bool, cancel_requested: bool) -> JobState {
    match (has_job, finished, cancel_requested) {
        (false, _, _) => JobState::Idle,
        (true, false, _) => JobState::Running,
        (true, true, true) => JobState::Cancelled,
        (true, true, false) => JobState::Done,
    }
}

/// A one-line progress readout, e.g. `"pass 12 / 128 · 9% · 3.4s"`. Pure —
/// unit-testable. `done == total` reads `"done · …"`.
pub fn progress_line(done: u32, total: u32, elapsed: Duration) -> String {
    let pct = if total == 0 {
        0
    } else {
        ((done as f64 / total as f64) * 100.0).round() as u32
    };
    let secs = elapsed.as_secs_f64();
    if done >= total && total > 0 {
        format!("done · {total} passes · {secs:.1}s")
    } else {
        format!("pass {done} / {total} · {pct}% · {secs:.1}s")
    }
}

/// An in-flight (or finished) progressive raytrace running on a background
/// thread. Dropping the job or calling [`Self::cancel`] flips the shared cancel
/// flag; the render loop checks it between passes and returns promptly.
pub struct RaytraceJob {
    shared: Arc<Mutex<Shared>>,
    cancel: Arc<AtomicBool>,
    cancel_requested: bool,
    handle: Option<std::thread::JoinHandle<()>>,
    pub started: Instant,
    pub tri_count: usize,
}

impl RaytraceJob {
    /// Spawn the progressive render on a background thread. The closure receives
    /// the cancel flag + shared state so the (already-built) scene and camera —
    /// which the caller owns — can be moved into the worker. `tri_count` is
    /// captioned in the window; a zero count means the scene had no meshes and
    /// the render will be an empty sky (surfaced as guidance, not a blank).
    pub fn spawn<F>(settings: Settings, tri_count: usize, render_fn: F) -> Self
    where
        F: FnOnce(&Arc<Mutex<Shared>>, &AtomicBool) + Send + 'static,
    {
        let shared = Arc::new(Mutex::new(Shared {
            passes_total: settings.samples_per_pixel.max(1),
            ..Default::default()
        }));
        let cancel = Arc::new(AtomicBool::new(false));
        let handle = {
            let shared = Arc::clone(&shared);
            let cancel = Arc::clone(&cancel);
            std::thread::Builder::new()
                .name("raytrace-render".into())
                .spawn(move || {
                    render_fn(&shared, &cancel);
                    if let Ok(mut s) = shared.lock() {
                        s.finished = true;
                    }
                })
                .expect("spawn raytrace thread")
        };
        Self {
            shared,
            cancel,
            cancel_requested: false,
            handle: Some(handle),
            started: Instant::now(),
            tri_count,
        }
    }

    /// Request cancellation: the render loop stops after its current pass.
    pub fn cancel(&mut self) {
        self.cancel_requested = true;
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Take the newest frame (if any) plus the current progress counters, for
    /// the UI to upload/caption. Never blocks longer than the brief per-pass
    /// swap the worker holds the mutex for.
    pub fn drain(&self) -> (Option<Image>, u32, u32, Option<String>, bool) {
        match self.shared.lock() {
            Ok(mut s) => (
                s.latest.take(),
                s.passes_done,
                s.passes_total,
                s.error.clone(),
                s.finished,
            ),
            Err(_) => (None, 0, 0, Some("render state poisoned".into()), true),
        }
    }

    /// Whether the worker has returned (join is non-blocking-ready).
    pub fn is_finished(&self) -> bool {
        self.shared.lock().map(|s| s.finished).unwrap_or(true)
    }

    /// The current [`JobState`] derived from the shared flags.
    pub fn state(&self) -> JobState {
        job_state(true, self.is_finished(), self.cancel_requested)
    }

    /// Join the worker thread (called on drop / clear). Cancel first so it
    /// returns promptly rather than finishing a full render.
    pub fn join(mut self) {
        self.cancel();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for RaytraceJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use itsjustcad_raytrace::{scene_from_doc, Camera, Sky};

    #[test]
    fn parse_custom_res_accepts_separators_and_clamps() {
        assert_eq!(parse_custom_res("640x480"), Some(Resolution::Custom(640, 480)));
        assert_eq!(parse_custom_res(" 800 × 500 "), Some(Resolution::Custom(800, 500)));
        assert_eq!(parse_custom_res("1024X768"), Some(Resolution::Custom(1024, 768)));
        assert_eq!(parse_custom_res("100*200"), Some(Resolution::Custom(100, 200)));
        assert_eq!(parse_custom_res("320,240"), Some(Resolution::Custom(320, 240)));
        // Clamped to the [16, 8192] range.
        assert_eq!(parse_custom_res("2x999999"), Some(Resolution::Custom(16, 8192)));
        // Garbage / zero / missing dim → None (not a panic).
        assert_eq!(parse_custom_res("wide"), None);
        assert_eq!(parse_custom_res("0x100"), None);
        assert_eq!(parse_custom_res("100x"), None);
    }

    #[test]
    fn resolution_dims_and_labels() {
        let p = Resolution::PRESETS[1];
        assert_eq!((p.width(), p.height()), (800, 500));
        assert_eq!(p.label(), "800 × 500");
        assert_eq!(Resolution::Custom(7, 9).label(), "custom 7 × 9");
    }

    #[test]
    fn controls_map_to_settings_and_clamp() {
        let c = RtControls {
            resolution: Resolution::Custom(1280, 800),
            samples_per_pixel: 128,
            max_bounces: 8,
            sun_on: true,
            sky_on: true,
            tonemap: ToneMap::Reinhard,
        };
        let s = c.to_settings();
        assert_eq!((s.width, s.height), (1280, 800));
        assert_eq!(s.samples_per_pixel, 128);
        assert_eq!(s.max_bounces, 8);
        assert_eq!(s.tonemap, ToneMap::Reinhard);

        // Extremes clamp rather than stall/panic.
        let z = RtControls {
            resolution: Resolution::Custom(1, 1),
            samples_per_pixel: 0,
            max_bounces: 999,
            ..c
        };
        let s = z.to_settings();
        assert_eq!((s.width, s.height), (16, 16));
        assert_eq!(s.samples_per_pixel, 1, "spp floored to 1");
        assert_eq!(s.max_bounces, 32, "bounces capped");
    }

    #[test]
    fn job_state_machine_transitions() {
        assert_eq!(job_state(false, false, false), JobState::Idle);
        assert_eq!(job_state(true, false, false), JobState::Running);
        assert_eq!(job_state(true, false, true), JobState::Running); // cancel pending, not yet done
        assert_eq!(job_state(true, true, false), JobState::Done);
        assert_eq!(job_state(true, true, true), JobState::Cancelled);
    }

    #[test]
    fn progress_line_formats_percent_and_done() {
        let l = progress_line(12, 128, Duration::from_millis(3400));
        assert!(l.starts_with("pass 12 / 128 · 9%"), "{l}");
        assert!(l.contains("3.4s"), "{l}");
        let done = progress_line(64, 64, Duration::from_secs(10));
        assert!(done.starts_with("done · 64 passes"), "{done}");
        // No divide-by-zero when total is 0.
        assert!(progress_line(0, 0, Duration::ZERO).contains("0%"));
    }

    /// A tiny end-to-end: spawn a real progressive render on a box scene and
    /// wait for it to finish, asserting the shared state advances idle→running→
    /// done and yields a non-empty final frame.
    #[test]
    fn spawn_runs_progressive_to_done_with_frames() {
        use glam::DVec3;
        use itsjustcad_doc::{Document, Geometry, ObjectId, SceneObject};

        let mut doc = Document::default();
        doc.insert(SceneObject {
            visible: true,
            id: ObjectId::new(),
            name: None,
            layer: "default".into(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Mesh(kernel_mesh::make_box(
                DVec3::ZERO,
                DVec3::new(1.0, 1.0, 1.0),
            )),
        });
        let scene = scene_from_doc(&doc, Sky::default());
        let tris = scene.triangle_count();
        assert_eq!(tris, 12);
        let cam = Camera::look_at(
            DVec3::new(4.0, -4.0, 3.0),
            DVec3::new(0.5, 0.5, 0.5),
            DVec3::Z,
            45f64.to_radians(),
            32.0 / 20.0,
        );
        let settings = RtControls {
            resolution: Resolution::Custom(32, 20),
            samples_per_pixel: 6,
            max_bounces: 2,
            ..Default::default()
        }
        .to_settings();

        let job = RaytraceJob::spawn(settings, tris, move |shared, cancel| {
            itsjustcad_raytrace::render_progressive(
                &scene,
                &cam,
                &settings,
                |pr| {
                    if let Ok(mut s) = shared.lock() {
                        s.passes_done = pr.passes_done;
                        s.passes_total = pr.passes_total;
                        s.latest = Some(pr.image.clone());
                    }
                },
                cancel,
            );
        });

        // Poll until finished (bounded).
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut saw_frame = false;
        loop {
            let (frame, done, total, err, finished) = job.drain();
            assert!(err.is_none(), "no error expected: {err:?}");
            if frame.is_some() {
                saw_frame = true;
            }
            if finished {
                assert_eq!((done, total), (6, 6), "ran all passes");
                break;
            }
            assert!(Instant::now() < deadline, "render never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(saw_frame, "at least one intermediate frame was published");
        assert_eq!(job.state(), JobState::Done);
        job.join();
    }

    /// Cancelling a long render stops it promptly with a partial (or no) frame,
    /// and the state reads `Cancelled`.
    #[test]
    fn cancel_stops_a_long_render() {
        use glam::DVec3;
        use itsjustcad_doc::{Document, Geometry, ObjectId, SceneObject};

        let mut doc = Document::default();
        doc.insert(SceneObject {
            visible: true,
            id: ObjectId::new(),
            name: None,
            layer: "default".into(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Mesh(kernel_mesh::make_box(
                DVec3::ZERO,
                DVec3::new(1.0, 1.0, 1.0),
            )),
        });
        let scene = scene_from_doc(&doc, Sky::default());
        let cam = Camera::look_at(
            DVec3::new(4.0, -4.0, 3.0),
            DVec3::splat(0.5),
            DVec3::Z,
            45f64.to_radians(),
            1.6,
        );
        let settings = RtControls {
            resolution: Resolution::Custom(64, 40),
            samples_per_pixel: 100_000, // would take ages
            max_bounces: 4,
            ..Default::default()
        }
        .to_settings();

        let mut job = RaytraceJob::spawn(settings, 12, move |shared, cancel| {
            itsjustcad_raytrace::render_progressive(
                &scene,
                &cam,
                &settings,
                |pr| {
                    if let Ok(mut s) = shared.lock() {
                        s.passes_done = pr.passes_done;
                        s.latest = Some(pr.image.clone());
                    }
                },
                cancel,
            );
        });
        // Let a pass or two land, then cancel.
        std::thread::sleep(Duration::from_millis(50));
        job.cancel();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !job.is_finished() {
            assert!(Instant::now() < deadline, "cancel did not stop the render");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(job.state(), JobState::Cancelled);
        job.join();
    }
}
