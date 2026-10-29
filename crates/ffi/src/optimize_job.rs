// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Background site optimizer jobs (#170): start a `siteoptimize` search on a
//! worker thread, poll its progress, cancel it, and take its result — so a
//! host can show a progress bar and a working Cancel instead of blocking on
//! [`super::ijc_site_optimize_json`] (which stays, unchanged).
//!
//! A job is a number scoped to its handle:
//!
//! - [`ijc_site_optimize_start`] snapshots the boundary, its plan and the
//!   request from the document and starts the search; returns the job id.
//! - [`ijc_site_optimize_progress`] reads `{"job","state","phase","evaluated",
//!   "budget","feasible","best","fraction","elapsed_s"}`.
//! - [`ijc_site_optimize_cancel`] stops it at the next chunk of plans and
//!   discards it (a user Cancel).
//! - [`ijc_site_optimize_stop`] stops it at the next chunk of plans and keeps
//!   the best plans found so far, marked `"stopped_early":true` (#201: a
//!   system stop, e.g. the iOS background time running out).
//! - [`ijc_site_optimize_result`] takes the finished result (the same JSON as
//!   `ijc_site_optimize_json`, plus `"saved"`, or `{"error"}`; a cancelled
//!   job's error is `"siteoptimize: cancelled"`) and forgets the job. A
//!   result (full or stopped early) is saved in the document as its site
//!   plan's latest run (#201), read back by [`ijc_site_optimize_saved_json`].
//!
//! Freeing the handle cancels every running job. A completed job's result is
//! the blocking call's result: cancel and progress never change the search.

use std::collections::BTreeMap;
use std::ffi::c_char;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use itsjustcad_commands::optimize_runs::{self, SavedRun};
use itsjustcad_commands::site_optimize::{
    self, Control, OptimizeRequest, OptimizeResult, Prepared, Progress,
};
use serde_json::{Value, json};

use super::{
    AppHandle, c_arg, guard_ffi, handle_mut, handle_ref, into_c_string, object_by_short_id,
};

/// One search.
struct Job {
    cancel: AtomicBool,
    /// Stop and keep the plans found so far (#201).
    stop: AtomicBool,
    started: Instant,
    /// What the search plans (saved with its result); `None` when it failed
    /// to start.
    prepared: Option<Prepared>,
    state: Mutex<JobState>,
}

#[derive(Default)]
struct JobState {
    progress: Progress,
    /// `Some` once the search ended: the result or the error.
    result: Option<Result<OptimizeResult, String>>,
    /// Elapsed seconds when it ended.
    elapsed: Option<f64>,
}

impl Job {
    fn finish(&self, result: Result<OptimizeResult, String>) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.result = Some(result);
        st.elapsed = Some(self.started.elapsed().as_secs_f64());
    }

    fn progress_json(&self, id: u64) -> Value {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let state = match &st.result {
            None if self.cancel.load(Ordering::Relaxed) => "cancelling",
            None if self.stop.load(Ordering::Relaxed) => "stopping",
            None => "running",
            Some(Ok(_)) => "done",
            Some(Err(e)) if e == site_optimize::CANCELLED => "cancelled",
            Some(Err(_)) => "failed",
        };
        let p = st.progress;
        json!({
            "job": id,
            "state": state,
            "phase": p.phase,
            "evaluated": p.evaluated,
            "budget": p.budget,
            "feasible": p.feasible,
            "best": p.best,
            "fraction": p.fraction(),
            "elapsed_s": st.elapsed.unwrap_or_else(|| self.started.elapsed().as_secs_f64()),
        })
    }
}

/// The jobs of one handle.
#[derive(Default)]
pub(crate) struct Jobs {
    next: u64,
    jobs: BTreeMap<u64, Arc<Job>>,
}

impl Drop for Jobs {
    /// A freed handle stops its searches (their threads end at the next chunk).
    fn drop(&mut self) {
        for j in self.jobs.values() {
            j.cancel.store(true, Ordering::Relaxed);
        }
    }
}

fn jobs(app: &AppHandle) -> std::sync::MutexGuard<'_, Jobs> {
    app.optimize_jobs.lock().unwrap_or_else(|e| e.into_inner())
}

fn job(app: &AppHandle, id: u64) -> Option<Arc<Job>> {
    jobs(app).jobs.get(&id).cloned()
}

/// Start a [`super::ijc_site_optimize_json`] search of the site boundary
/// `source` (short id) on a worker thread. `request_json` is the same request
/// (null = defaults). Returns the job id (> 0) for
/// [`ijc_site_optimize_progress`] / [`ijc_site_optimize_cancel`] /
/// [`ijc_site_optimize_result`]; a bad source or request still returns a job,
/// already failed with the message. Returns 0 only for a null/invalid handle.
///
/// # Safety
/// `h` must be null or a live handle from [`super::ijc_init`]; `source` and
/// `request_json` must be null or valid NUL-terminated C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_site_optimize_start(
    h: *mut AppHandle,
    source: *const c_char,
    request_json: *const c_char,
) -> u64 {
    guard_ffi(0, || {
        let Some(app) = (unsafe { handle_ref(h) }) else {
            return 0;
        };
        let prepared = (|| {
            let source = unsafe { c_arg(source, "source") }?;
            let request: OptimizeRequest = match request_json.is_null() {
                true => Default::default(),
                false => serde_json::from_str(unsafe { c_arg(request_json, "request_json") }?)
                    .map_err(|e| format!("bad request: {e}"))?,
            };
            let doc = &app.session.doc;
            let id =
                object_by_short_id(doc, source).ok_or_else(|| format!("no object '{source}'"))?;
            site_optimize::prepare_doc(doc, &[id], &request)
        })();
        let job = Arc::new(Job {
            cancel: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            started: Instant::now(),
            prepared: prepared.as_ref().ok().cloned(),
            state: Default::default(),
        });
        let id = {
            let mut jobs = jobs(app);
            jobs.next += 1;
            let id = jobs.next;
            jobs.jobs.insert(id, job.clone());
            id
        };
        match prepared {
            Err(e) => job.finish(Err(e)),
            Ok(p) => {
                std::thread::spawn(move || {
                    let report = |pr: &Progress| {
                        job.state.lock().unwrap_or_else(|e| e.into_inner()).progress = *pr;
                    };
                    let ctl = Control {
                        cancel: Some(&job.cancel),
                        stop: Some(&job.stop),
                        progress: Some(&report),
                    };
                    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| p.run(ctl)))
                        .unwrap_or_else(|_| Err("siteoptimize: the search failed".into()));
                    job.finish(out);
                });
            }
        }
        id
    })
}

/// A job's progress: `{"job","state":"running"|"cancelling"|"stopping"|"done"|
/// "failed"|"cancelled","phase":"search"|"daylight","evaluated","budget","feasible",
/// "best":score|null,"fraction":0..1,"elapsed_s"}` (`evaluated` of `budget`
/// in the current phase). `null` for an unknown job or a null/invalid handle.
/// Free with [`super::ijc_string_free`].
///
/// # Safety
/// `h` must be null or a live handle from [`super::ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_site_optimize_progress(h: *mut AppHandle, job_id: u64) -> *mut c_char {
    guard_ffi(std::ptr::null_mut(), || {
        let v = (unsafe { handle_ref(h) })
            .and_then(|app| job(app, job_id))
            .map_or(Value::Null, |j| j.progress_json(job_id));
        into_c_string(v.to_string())
    })
}

/// Ask a running job to stop; it ends at its next chunk of plans and its
/// result becomes `{"error":"siteoptimize: cancelled"}`. Returns whether the
/// job is known (true for one already finished: a no-op).
///
/// # Safety
/// `h` must be null or a live handle from [`super::ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_site_optimize_cancel(h: *mut AppHandle, job_id: u64) -> bool {
    guard_ffi(false, || {
        (unsafe { handle_ref(h) })
            .and_then(|app| job(app, job_id))
            .map(|j| j.cancel.store(true, Ordering::Relaxed))
            .is_some()
    })
}

/// Ask a running job to stop and keep what it found (#201): it ends at its
/// next chunk of plans and its result is the best plans evaluated so far,
/// ranked as usual, with `"stopped_early":true` (or
/// `{"error":"siteoptimize: stopped before any plan was evaluated"}`). For a
/// system stop (background time running out), not a user Cancel. A daylight
/// search stopped before any layout's daylight was measured still measures
/// one batch (10 layouts) first. Returns whether the job is known (true for
/// one already finished: a no-op).
///
/// # Safety
/// `h` must be null or a live handle from [`super::ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_site_optimize_stop(h: *mut AppHandle, job_id: u64) -> bool {
    guard_ffi(false, || {
        (unsafe { handle_ref(h) })
            .and_then(|app| job(app, job_id))
            .map(|j| j.stop.store(true, Ordering::Relaxed))
            .is_some()
    })
}

/// Take a finished job's result — the [`super::ijc_site_optimize_json`]
/// result plus `"saved":bool`, or `{"error":"…"}` — and forget the job.
/// `null` while it is still running, for an unknown (or already taken) job,
/// or a null/invalid handle. Free with [`super::ijc_string_free`].
///
/// A result (complete, or `"stopped_early":true` after
/// [`ijc_site_optimize_stop`]) is saved in the document as the latest run of
/// its site plan, replacing the previous one (`"saved":true`; read it back
/// with [`ijc_site_optimize_saved_json`]). Saving is document metadata, not
/// an undo step: the host should mark the document edited so it gets saved.
/// Nothing is saved (`"saved":false`) when the boundary no longer has a
/// site plan. Cancelled and failed jobs save nothing.
///
/// # Safety
/// `h` must be null or a live handle from [`super::ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_site_optimize_result(h: *mut AppHandle, job_id: u64) -> *mut c_char {
    guard_ffi(std::ptr::null_mut(), || {
        let null = || into_c_string("null".into());
        let Some(mut app) = (unsafe { handle_mut(h) }) else {
            return null();
        };
        let (j, result, elapsed) = {
            let mut jobs = jobs(&app);
            let Some(j) = jobs.jobs.get(&job_id).cloned() else {
                return null();
            };
            let mut st = j.state.lock().unwrap_or_else(|e| e.into_inner());
            let Some(result) = st.result.take() else {
                return null();
            };
            let elapsed = st.elapsed.unwrap_or(0.0);
            drop(st);
            jobs.jobs.remove(&job_id);
            (j, result, elapsed)
        };
        let out = match (result, &j.prepared) {
            (Ok(r), Some(p)) => {
                let mut v = r.to_json();
                let saved = app
                    .session
                    .set_optimize_run(p.source, SavedRun::new(p, r, elapsed));
                v["saved"] = saved.into();
                v
            }
            (Ok(r), None) => r.to_json(),
            (Err(e), _) => json!({ "error": e }),
        };
        into_c_string(out.to_string())
    })
}

/// The saved latest optimizer run of the site plan on `source` (short id,
/// #201), or `null` when it has none (never run, its site plan deleted, an
/// unknown source, a file from an older build, or a null/invalid handle):
///
/// `{"version":1,"request":{the request, as ijc_site_optimize_json takes it},
/// "result":{the ijc_site_optimize_json result, with "stopped_early"},
/// "stopped_early":bool,"finished_at":unix seconds,"elapsed_s":seconds,
/// "evaluated":n,"feasible":n,"budget":n,"fingerprint":"16 hex digits",
/// "stale":bool}`
///
/// `stale` is true when the plan (boundary, settings or hand edits) changed
/// since the run: show "Plan changed since this run"; Apply
/// ([`super::ijc_site_optimize_apply`] with a row's `params`) still works.
/// Free with [`super::ijc_string_free`].
///
/// # Safety
/// `h` must be null or a live handle from [`super::ijc_init`]; `source` must
/// be null or a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_site_optimize_saved_json(
    h: *mut AppHandle,
    source: *const c_char,
) -> *mut c_char {
    guard_ffi(std::ptr::null_mut(), || {
        let v = (|| {
            let app = unsafe { handle_ref(h) }?;
            let source = unsafe { c_arg(source, "source") }.ok()?;
            let doc = &app.session.doc;
            let id = object_by_short_id(doc, source)?;
            let run = app.session.optimize_run(id)?;
            Some(run.to_json(optimize_runs::is_stale(doc, id, run)))
        })();
        into_c_string(v.unwrap_or(Value::Null).to_string())
    })
}
