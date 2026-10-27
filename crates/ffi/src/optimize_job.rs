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
//! - [`ijc_site_optimize_cancel`] stops it at the next chunk of plans.
//! - [`ijc_site_optimize_result`] takes the finished result (the same JSON as
//!   `ijc_site_optimize_json`, or `{"error"}`; a cancelled job's error is
//!   `"siteoptimize: cancelled"`) and forgets the job.
//!
//! Freeing the handle cancels every running job. A completed job's result is
//! the blocking call's result: cancel and progress never change the search.

use std::collections::BTreeMap;
use std::ffi::c_char;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use itsjustcad_commands::site_optimize::{self, Control, OptimizeRequest, Progress};
use serde_json::{Value, json};

use super::{AppHandle, c_arg, guard_ffi, handle_ref, into_c_string, object_by_short_id};

/// One search.
struct Job {
    cancel: AtomicBool,
    started: Instant,
    state: Mutex<JobState>,
}

#[derive(Default)]
struct JobState {
    progress: Progress,
    /// `Some` once the search ended: the result JSON or the error.
    result: Option<Result<Value, String>>,
    /// Elapsed seconds when it ended.
    elapsed: Option<f64>,
}

impl Job {
    fn finish(&self, result: Result<Value, String>) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.result = Some(result);
        st.elapsed = Some(self.started.elapsed().as_secs_f64());
    }

    fn progress_json(&self, id: u64) -> Value {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let state = match &st.result {
            None if self.cancel.load(Ordering::Relaxed) => "cancelling",
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
            started: Instant::now(),
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
                        progress: Some(&report),
                    };
                    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| p.run(ctl)))
                        .unwrap_or_else(|_| Err("siteoptimize: the search failed".into()));
                    job.finish(out.map(|r| r.to_json()));
                });
            }
        }
        id
    })
}

/// A job's progress: `{"job","state":"running"|"cancelling"|"done"|"failed"|
/// "cancelled","phase":"search"|"daylight","evaluated","budget","feasible",
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

/// Take a finished job's result — the [`super::ijc_site_optimize_json`]
/// result, or `{"error":"…"}` — and forget the job. `null` while it is still
/// running, for an unknown (or already taken) job, or a null/invalid handle.
/// Free with [`super::ijc_string_free`].
///
/// # Safety
/// `h` must be null or a live handle from [`super::ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_site_optimize_result(h: *mut AppHandle, job_id: u64) -> *mut c_char {
    guard_ffi(std::ptr::null_mut(), || {
        let null = || into_c_string("null".into());
        let Some(app) = (unsafe { handle_ref(h) }) else {
            return null();
        };
        let mut jobs = jobs(app);
        let Some(j) = jobs.jobs.get(&job_id).cloned() else {
            return null();
        };
        let Some(result) = j
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .result
            .take()
        else {
            return null();
        };
        jobs.jobs.remove(&job_id);
        into_c_string(result.unwrap_or_else(|e| json!({ "error": e })).to_string())
    })
}
