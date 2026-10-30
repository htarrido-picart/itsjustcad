// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! The plumbing shared by the background jobs of a handle (#239):
//! [`super::open_job`], [`super::history_job`] and [`super::optimize_job`].
//!
//! - [`Jobs`]: a handle's jobs by id, cancelled when the handle is freed.
//! - [`state_name`]: the `"state"` of a job's progress.
//! - [`run_guarded`]: a worker's run, with a panic turned into an error.
//! - [`ReplayJob`]: an op replay reporting `done` of `total` (opens and
//!   history rebuilds).
//!
//! What each job does with its result stays in its module.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use itsjustcad_commands::ReplayControl;
use serde_json::{Value, json};

/// Lock a job mutex, ignoring poisoning (a worker panic is reported as the
/// job's error instead).
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A job with a cancel flag.
pub(crate) trait Cancellable {
    fn cancel_flag(&self) -> &AtomicBool;

    fn cancel(&self) {
        self.cancel_flag().store(true, Ordering::Relaxed);
    }

    fn is_cancelled(&self) -> bool {
        self.cancel_flag().load(Ordering::Relaxed)
    }
}

/// The jobs of one kind of one handle; ids start at 1.
pub(crate) struct Jobs<J: Cancellable> {
    next: u64,
    jobs: BTreeMap<u64, Arc<J>>,
}

impl<J: Cancellable> Default for Jobs<J> {
    fn default() -> Self {
        Self {
            next: 0,
            jobs: BTreeMap::new(),
        }
    }
}

impl<J: Cancellable> Jobs<J> {
    /// Add a job; returns its id.
    pub(crate) fn insert(&mut self, job: Arc<J>) -> u64 {
        self.next += 1;
        self.jobs.insert(self.next, job);
        self.next
    }

    pub(crate) fn get(&self, id: u64) -> Option<Arc<J>> {
        self.jobs.get(&id).cloned()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (u64, &Arc<J>)> {
        self.jobs.iter().map(|(&id, j)| (id, j))
    }

    /// Forget job `id` when `finished` takes its result; `None` (the job
    /// kept) when it is unknown or still running.
    pub(crate) fn take<T>(
        &mut self,
        id: u64,
        finished: impl FnOnce(&J) -> Option<T>,
    ) -> Option<(Arc<J>, T)> {
        let j = self.get(id)?;
        let out = finished(&j)?;
        self.jobs.remove(&id);
        Some((j, out))
    }

    /// Stop every running job (their threads end at their next step).
    pub(crate) fn cancel_all(&self) {
        for j in self.jobs.values() {
            j.cancel();
        }
    }
}

impl<J: Cancellable> Drop for Jobs<J> {
    /// A freed handle stops its jobs.
    fn drop(&mut self) {
        self.cancel_all();
    }
}

/// The `"state"` of a job's progress: `"running"`, `"cancelling"`, `"done"`,
/// `"failed"` or `"cancelled"` (its error is `cancelled`, or — when
/// `discard_late` — it was cancelled after it finished, #237).
pub(crate) fn state_name<T>(
    result: &Option<Result<T, String>>,
    cancel: bool,
    discard_late: bool,
    cancelled: &str,
) -> &'static str {
    match result {
        None if cancel => "cancelling",
        None => "running",
        Some(Ok(_)) if cancel && discard_late => "cancelled",
        Some(Ok(_)) => "done",
        Some(Err(e)) if e == cancelled => "cancelled",
        Some(Err(_)) => "failed",
    }
}

/// Run a worker's job; a panic becomes the error `failed`.
pub(crate) fn run_guarded<R>(
    failed: &str,
    run: impl FnOnce() -> Result<R, String>,
) -> Result<R, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run))
        .unwrap_or_else(|_| Err(failed.into()))
}

/// An op replay on a worker thread.
pub(crate) struct ReplayJob<R> {
    cancel: AtomicBool,
    state: Mutex<ReplayState<R>>,
}

pub(crate) struct ReplayState<R> {
    /// Ops replayed of `total`.
    pub(crate) done: usize,
    pub(crate) total: usize,
    /// `Some` once the replay ended: its result or the error.
    pub(crate) result: Option<Result<R, String>>,
}

impl<R> Default for ReplayJob<R> {
    fn default() -> Self {
        Self {
            cancel: AtomicBool::new(false),
            state: Mutex::new(ReplayState {
                done: 0,
                total: 0,
                result: None,
            }),
        }
    }
}

impl<R> Cancellable for ReplayJob<R> {
    fn cancel_flag(&self) -> &AtomicBool {
        &self.cancel
    }
}

impl<R: Send + 'static> ReplayJob<R> {
    pub(crate) fn state(&self) -> MutexGuard<'_, ReplayState<R>> {
        lock(&self.state)
    }

    /// `{"job","state","done","total"}`; see [`state_name`].
    pub(crate) fn progress_json(&self, id: u64, discard_late: bool, cancelled: &str) -> Value {
        let st = self.state();
        let state = state_name(&st.result, self.is_cancelled(), discard_late, cancelled);
        json!({ "job": id, "state": state, "done": st.done, "total": st.total })
    }

    /// Run `replay` on a worker thread, reporting its progress and stopping
    /// at the next op once cancelled; a panic fails the job with `failed`.
    pub(crate) fn spawn(
        self: Arc<Self>,
        failed: &'static str,
        replay: impl FnOnce(ReplayControl) -> Result<R, String> + Send + 'static,
    ) {
        std::thread::spawn(move || {
            let report = |done: usize, total: usize| {
                let mut st = self.state();
                st.done = done;
                st.total = total;
            };
            let ctl = ReplayControl {
                cancel: Some(&self.cancel),
                progress: Some(&report),
            };
            let out = run_guarded(failed, || replay(ctl));
            self.state().result = Some(out);
        });
    }
}
