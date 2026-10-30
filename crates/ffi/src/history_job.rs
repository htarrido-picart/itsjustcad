// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Background undo-history rebuilds (#224). After a fast open from a
//! checkpoint the opened ops have no inverses; new edits, and undo/redo of
//! them, work without them. Undoing past the open boundary is refused with
//! `"Preparing undo history…"` until a rebuild is installed:
//!
//! - [`ijc_history_pending`]: whether the open prefix still needs a rebuild.
//! - [`ijc_history_start`] copies the prefix ops and replays them on a worker
//!   into a scratch session; returns the job id.
//! - [`ijc_history_progress`] reads `{"job","state","done","total"}`.
//! - [`ijc_history_cancel`] stops the replay at the next op.
//! - [`ijc_history_take`] installs the rebuilt inverses below the new edits
//!   (on the thread that renders) and forgets the job.
//!
//! The worker never touches the handle; edits made while it runs are kept.
//! Freeing the handle cancels every running job.

use std::collections::BTreeMap;
use std::ffi::c_char;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use itsjustcad_commands::{ExecError, RebuiltHistory, ReplayControl};
use serde_json::{Value, json};

use super::{AppHandle, guard_ffi, handle_mut, handle_ref, into_c_string};

/// The error of a cancelled job.
const CANCELLED: &str = "history: cancelled";

/// One rebuild.
#[derive(Default)]
struct Job {
    cancel: AtomicBool,
    state: Mutex<JobState>,
}

#[derive(Default)]
struct JobState {
    done: usize,
    total: usize,
    /// `Some` once the rebuild ended.
    result: Option<Result<RebuiltHistory, String>>,
}

impl Job {
    fn state(&self) -> std::sync::MutexGuard<'_, JobState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn progress_json(&self, id: u64) -> Value {
        let st = self.state();
        let state = match &st.result {
            None if self.cancel.load(Ordering::Relaxed) => "cancelling",
            None => "running",
            Some(Ok(_)) => "done",
            Some(Err(e)) if e == CANCELLED => "cancelled",
            Some(Err(_)) => "failed",
        };
        json!({ "job": id, "state": state, "done": st.done, "total": st.total })
    }
}

/// The history rebuilds of one handle.
#[derive(Default)]
pub(crate) struct Jobs {
    next: u64,
    jobs: BTreeMap<u64, Arc<Job>>,
}

impl Jobs {
    /// Stop every running rebuild (a freed handle, or a new document).
    pub(crate) fn cancel_all(&self) {
        for j in self.jobs.values() {
            j.cancel.store(true, Ordering::Relaxed);
        }
    }
}

impl Drop for Jobs {
    fn drop(&mut self) {
        self.cancel_all();
    }
}

pub(crate) fn jobs(app: &AppHandle) -> std::sync::MutexGuard<'_, Jobs> {
    app.history_jobs.lock().unwrap_or_else(|e| e.into_inner())
}

fn job(app: &AppHandle, id: u64) -> Option<Arc<Job>> {
    jobs(app).jobs.get(&id).cloned()
}

/// Whether the document was fast-opened and its opened ops still lack undo
/// history: undoing past them needs [`ijc_history_start`]. False for a
/// null/invalid handle.
///
/// # Safety
/// `h` must be null or a live handle from [`super::ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_history_pending(h: *mut AppHandle) -> bool {
    guard_ffi(false, || {
        (unsafe { handle_ref(h) }).is_some_and(|app| app.session.history_pending())
    })
}

/// Start rebuilding the undo history of the fast-opened ops on a worker
/// thread. Call it when an undo is refused with `"Preparing undo history…"`
/// (or earlier, when idle). Returns the job id (> 0) for
/// [`ijc_history_progress`] / [`ijc_history_cancel`] / [`ijc_history_take`];
/// a rebuild still running is reused (its id is returned). Returns 0 when
/// nothing needs rebuilding ([`ijc_history_pending`] is false), or for a
/// null/invalid/busy handle.
///
/// # Safety
/// `h` must be null or a live handle from [`super::ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_history_start(h: *mut AppHandle) -> u64 {
    guard_ffi(0, || {
        let Some(app) = (unsafe { handle_mut(h) }) else {
            return 0;
        };
        let Some(work) = app.session.history_job() else {
            return 0;
        };
        let mut jobs = jobs(&app);
        let running = jobs
            .jobs
            .iter()
            .find(|(_, j)| !j.cancel.load(Ordering::Relaxed) && j.state().result.is_none());
        if let Some((&id, _)) = running {
            return id;
        }
        let job = Arc::new(Job::default());
        jobs.next += 1;
        let id = jobs.next;
        jobs.jobs.insert(id, job.clone());
        job.state().total = work.len();
        std::thread::spawn(move || {
            let report = |done: usize, total: usize| {
                let mut st = job.state();
                st.done = done;
                st.total = total;
            };
            let ctl = ReplayControl {
                cancel: Some(&job.cancel),
                progress: Some(&report),
            };
            let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work.run(ctl)));
            let out = match out {
                Ok(Ok(rebuilt)) => Ok(rebuilt),
                Ok(Err(ExecError::Cancelled)) => Err(CANCELLED.into()),
                Ok(Err(e)) => Err(format!("history: {e}")),
                Err(_) => Err("history: the replay failed".into()),
            };
            job.state().result = Some(out);
        });
        id
    })
}

/// A job's progress: `{"job","state":"running"|"cancelling"|"done"|"failed"|
/// "cancelled","done","total"}` (`done` of `total` ops replayed). `null` for
/// an unknown job or a null/invalid handle. Free with
/// [`super::ijc_string_free`].
///
/// # Safety
/// `h` must be null or a live handle from [`super::ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_history_progress(h: *mut AppHandle, job_id: u64) -> *mut c_char {
    guard_ffi(std::ptr::null_mut(), || {
        let v = (unsafe { handle_ref(h) })
            .and_then(|app| job(app, job_id))
            .map_or(Value::Null, |j| j.progress_json(job_id));
        into_c_string(v.to_string())
    })
}

/// Ask a running job to stop; [`ijc_history_take`] then returns
/// `{"error":"history: cancelled"}`. Returns whether the job is known.
///
/// # Safety
/// `h` must be null or a live handle from [`super::ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_history_cancel(h: *mut AppHandle, job_id: u64) -> bool {
    guard_ffi(false, || {
        (unsafe { handle_ref(h) })
            .and_then(|app| job(app, job_id))
            .map(|j| j.cancel.store(true, Ordering::Relaxed))
            .is_some()
    })
}

/// Take a finished job and forget it. A successful rebuild is installed below
/// the edits made since the open (the document is unchanged; undo past the
/// open boundary works from now on) and returns `{"ok":true,"installed":bool}`
/// — `installed` is false when the rebuild no longer matches the document
/// (another document was opened, or the history was rebuilt already). A
/// failed or cancelled one returns `{"error":"…"}`. `null` (the job kept)
/// while it is still running, for an unknown job, or a null/invalid/busy
/// handle. Call it from the thread that renders. Free with
/// [`super::ijc_string_free`].
///
/// # Safety
/// `h` must be null or a live handle from [`super::ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_history_take(h: *mut AppHandle, job_id: u64) -> *mut c_char {
    guard_ffi(std::ptr::null_mut(), || {
        let null = || into_c_string("null".into());
        let Some(mut app) = (unsafe { handle_mut(h) }) else {
            return null();
        };
        let result = {
            let mut jobs = jobs(&app);
            let Some(j) = jobs.jobs.get(&job_id).cloned() else {
                return null();
            };
            let Some(result) = j.state().result.take() else {
                return null();
            };
            jobs.jobs.remove(&job_id);
            result
        };
        let out = match result {
            Ok(rebuilt) => json!({ "ok": true, "installed": app.session.install_history(rebuilt) }),
            Err(e) => json!({ "error": e }),
        };
        into_c_string(out.to_string())
    })
}
