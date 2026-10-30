// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Background document opens (#217): parse and replay a `.itsjustcad.json`
//! buffer on a worker thread, poll its progress, cancel it, and install the
//! result — so a host can open a large document without holding the handle
//! (and blocking every frame) for the whole replay. [`super::ijc_open_json`]
//! stays for small, synchronous opens.
//!
//! A job is a number scoped to its handle:
//!
//! - [`ijc_open_start`] copies the document and an optional checkpoint
//!   ([`super::ijc_checkpoint_json`]) and starts the replay; returns the job id.
//! - [`ijc_open_progress`] reads `{"job","state","done","total"}`.
//! - [`ijc_open_cancel`] stops the replay at the next op and discards it.
//! - [`ijc_open_take`] installs the opened document as the handle's (on the
//!   thread that renders, like any mutating call) and forgets the job.
//!
//! The worker never touches the handle: rendering and every other call stay
//! safe while it runs, and the current document is untouched until
//! [`ijc_open_take`]. Freeing the handle cancels every running job.

use std::ffi::c_char;
use std::sync::{Arc, MutexGuard};

use itsjustcad_commands::{ExecError, io};
use serde_json::{Value, json};

use super::job::{self, Cancellable, ReplayJob};
use super::{AppHandle, MAX_JSON_LEN, guard_ffi, handle_mut, handle_ref, into_c_string};

/// The error of a cancelled job.
const CANCELLED: &str = "open: cancelled";

/// One open (`done` of `total` counts the tail only, when a checkpoint is
/// used).
type Job = ReplayJob<io::Opened>;

/// The open jobs of one handle.
pub(crate) type Jobs = job::Jobs<Job>;

fn jobs(app: &AppHandle) -> MutexGuard<'_, Jobs> {
    job::lock(&app.open_jobs)
}

fn job(app: &AppHandle, id: u64) -> Option<Arc<Job>> {
    jobs(app).get(id)
}

/// A host buffer as an owned UTF-8 string.
unsafe fn copy_utf8(ptr: *const u8, len: usize, name: &str) -> Result<String, String> {
    if ptr.is_null() || len == 0 {
        return Err(format!("open: {name} is empty"));
    }
    if len > MAX_JSON_LEN {
        return Err(format!("open: {name} is too large"));
    }
    // `u8` has alignment 1, so no alignment check is required for `ptr`.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    String::from_utf8(bytes.to_vec()).map_err(|_| format!("open: {name} is not valid UTF-8"))
}

/// Start opening a `.itsjustcad.json` buffer (the [`super::ijc_open_json`]
/// input) on a worker thread. `cp_ptr`/`cp_len` is an optional checkpoint from
/// [`super::ijc_checkpoint_json`] (null/0 = none): when it matches the
/// document, only the ops newer than it are replayed; otherwise it is ignored
/// and the whole log is replayed. Both buffers are copied before this returns.
/// Returns the job id (> 0) for [`ijc_open_progress`] / [`ijc_open_cancel`] /
/// [`ijc_open_take`]; an empty, oversized or non-UTF-8 document still returns
/// a job, already failed with the message. Returns 0 only for a null/invalid
/// handle.
///
/// # Safety
/// `h` must be null or a live handle from [`super::ijc_init`]. If `len > 0`,
/// `ptr` must be non-null and valid for reads of `len` bytes; likewise
/// `cp_ptr` for `cp_len`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_open_start(
    h: *mut AppHandle,
    ptr: *const u8,
    len: usize,
    cp_ptr: *const u8,
    cp_len: usize,
) -> u64 {
    guard_ffi(0, || {
        let Some(app) = (unsafe { handle_ref(h) }) else {
            return 0;
        };
        let json = unsafe { copy_utf8(ptr, len, "document") };
        // A bad checkpoint is ignored like a stale one.
        let checkpoint = unsafe { copy_utf8(cp_ptr, cp_len, "checkpoint") }.ok();
        let job = Arc::new(Job::default());
        let id = jobs(app).insert(job.clone());
        match json {
            Err(e) => job.state().result = Some(Err(e)),
            Ok(json) => job.spawn("open: the replay failed", move |ctl| {
                match io::open_json(&json, checkpoint.as_deref(), ctl) {
                    Ok(opened) => Ok(opened),
                    Err(io::IoError::Replay(ExecError::Cancelled)) => Err(CANCELLED.into()),
                    Err(e) => Err(format!("open: {e}")),
                }
            }),
        }
        id
    })
}

/// A job's progress: `{"job","state":"running"|"cancelling"|"done"|"failed"|
/// "cancelled","done","total"}` (`done` of `total` ops replayed; with a
/// matching checkpoint, `total` counts only the ops newer than it). `null`
/// for an unknown job or a null/invalid handle. Free with
/// [`super::ijc_string_free`].
///
/// # Safety
/// `h` must be null or a live handle from [`super::ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_open_progress(h: *mut AppHandle, job_id: u64) -> *mut c_char {
    guard_ffi(std::ptr::null_mut(), || {
        let v = (unsafe { handle_ref(h) })
            .and_then(|app| job(app, job_id))
            .map_or(Value::Null, |j| j.progress_json(job_id, true, CANCELLED));
        into_c_string(v.to_string())
    })
}

/// Ask a running job to stop; it ends before its next op and
/// [`ijc_open_take`] then returns `{"error":"open: cancelled"}`. The current
/// document is never touched, even when the job already finished (#237).
/// Returns whether the job is known.
///
/// # Safety
/// `h` must be null or a live handle from [`super::ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_open_cancel(h: *mut AppHandle, job_id: u64) -> bool {
    guard_ffi(false, || {
        (unsafe { handle_ref(h) })
            .and_then(|app| job(app, job_id))
            .map(|j| j.cancel())
            .is_some()
    })
}

/// Take a finished job and forget it. A successful open replaces the
/// document with the same effects as [`super::ijc_open_json`] (guided run and
/// deck history cleared, camera framed, re-snapshot next frame) and returns
/// `{"ok":true,"from_checkpoint":bool}`; a failed or cancelled one returns
/// `{"error":"…"}` and leaves the document as it was. `null` (the job kept)
/// while it is still running, for an unknown (or already taken) job, or a
/// null/invalid/busy handle. Call it from the thread that renders. Free with
/// [`super::ijc_string_free`].
///
/// # Safety
/// `h` must be null or a live handle from [`super::ijc_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_open_take(h: *mut AppHandle, job_id: u64) -> *mut c_char {
    guard_ffi(std::ptr::null_mut(), || {
        let null = || into_c_string("null".into());
        let Some(mut app) = (unsafe { handle_mut(h) }) else {
            return null();
        };
        let Some((j, result)) = jobs(&app).take(job_id, |j| j.state().result.take()) else {
            return null();
        };
        let out = match (result, j.is_cancelled()) {
            (Ok(_), true) => json!({ "error": CANCELLED }),
            (Ok(opened), false) => {
                super::install_session(&mut app, opened.session);
                json!({ "ok": true, "from_checkpoint": opened.from_checkpoint })
            }
            (Err(e), _) => json!({ "error": e }),
        };
        into_c_string(out.to_string())
    })
}
