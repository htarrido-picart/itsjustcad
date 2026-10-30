// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Log replay and the fast-open history rebuild.

use itsjustcad_doc::Document;

use super::{AppliedOp, Session};
use crate::error::ExecError;
use crate::Command;

impl Session {
    /// Fast-open from a checkpoint: seed the document directly from a snapshot
    /// and adopt the forward log for saving, skipping the (potentially costly)
    /// geometry replay. `doc` must equal what `replay(log.clone())?.doc` would
    /// produce — the checkpoint sidecar is a cache, and callers only invoke this
    /// after confirming the checkpoint's op count matches `log`.
    ///
    /// Inverses (needed for undo) are *not* materialized here — that would
    /// require the very replay we are skipping. New edits stack on top with
    /// their own inverses (#224); the prefix's are rebuilt only to undo past
    /// it, synchronously or by a background [`HistoryJob`].
    pub fn from_snapshot(doc: Document, log: Vec<Command>) -> Self {
        static NEXT_TOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let mut s = Session {
            doc,
            log: Vec::new(),
            cursor: 0,
            plugins: crate::plugin::PluginRegistry::default(),
            pending_log: Some(log),
            pending_token: NEXT_TOKEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            ..Session::default()
        };
        s.record_open_doc();
        s
    }

    #[cfg(debug_assertions)]
    fn record_open_doc(&mut self) {
        self.open_doc = Some(Box::new(self.doc.clone()));
    }

    #[cfg(not(debug_assertions))]
    fn record_open_doc(&mut self) {}

    /// Whether the fast-open prefix still lacks its inverses: undoing past it
    /// needs a rebuild ([`Session::history_job`]).
    pub fn history_pending(&self) -> bool {
        self.pending_log.is_some()
    }

    /// Refuse undo past the fast-open boundary until a background rebuild is
    /// installed (`true`), or rebuild synchronously there (`false`, default).
    pub fn set_background_history(&mut self, on: bool) {
        self.background_history = on;
    }

    /// Whether an undo was refused with [`ExecError::HistoryPending`] since the
    /// last call (which clears it): a host starts its rebuild on this.
    pub fn history_requested(&mut self) -> bool {
        std::mem::take(&mut self.history_requested)
    }

    /// A copy of the non-materialized prefix to rebuild off this session
    /// (#224), or `None` when the history is live.
    pub fn history_job(&self) -> Option<HistoryJob> {
        self.pending_log.as_ref().map(|prefix| HistoryJob {
            token: self.pending_token,
            prefix: prefix.clone(),
        })
    }

    /// Install a rebuilt prefix history below the new edits. Returns false
    /// (and changes nothing) when it is stale: the history is already live or
    /// the session was replaced since [`Session::history_job`].
    pub fn install_history(&mut self, rebuilt: RebuiltHistory) -> bool {
        if self.pending_log.is_none() || rebuilt.token != self.pending_token {
            return false;
        }
        // The snapshot doc is authoritative; the replay only supplies the
        // inverses. Checked against the document as opened.
        #[cfg(debug_assertions)]
        if let Some(open) = self.open_doc.take() {
            assert_eq!(*open, rebuilt.doc, "checkpoint snapshot diverged from op-log replay");
        }
        let mut log = rebuilt.log;
        self.cursor += log.len();
        log.append(&mut self.log);
        self.log = log;
        self.pending_log = None;
        true
    }

    /// Rebuild the fast-open prefix history synchronously. A no-op once the
    /// history is live. Returns any replay error (the prefix is kept).
    pub(super) fn ensure_history(&mut self) -> Result<(), ExecError> {
        let Some(job) = self.history_job() else {
            return Ok(());
        };
        let rebuilt = job.run(ReplayControl::default())?;
        self.install_history(rebuilt);
        Ok(())
    }

    /// Fast-open from a snapshot of the first `op_count` ops of `log`, then
    /// replay only the newer ops `log[op_count..]` on top of it (#217). `doc`
    /// must equal what `replay(log[..op_count])?.doc` would produce; callers
    /// check that the snapshot belongs to this log first. The history stays
    /// deferred as with [`Session::from_snapshot`]. `ctl` reports and can
    /// cancel the tail replay (`done` of the tail's `total` ops).
    pub fn from_snapshot_tail(
        doc: Document,
        mut log: Vec<Command>,
        op_count: usize,
        ctl: ReplayControl<'_>,
    ) -> Result<Self, ExecError> {
        let tail = log.split_off(op_count.min(log.len()));
        let mut session = Session::from_snapshot(doc, log);
        session.defer_history = true;
        let out = session.run_all(tail, ctl);
        session.defer_history = false;
        session.record_open_doc();
        out.map(|()| session)
    }

    /// Rebuild a session by replaying a saved log through the same `apply`
    /// path used live. Ids stored in the log are reused, so the result is
    /// identical to the session that saved it.
    pub fn replay(log: Vec<Command>) -> Result<Self, ExecError> {
        Self::replay_with(log, ReplayControl::default())
    }

    /// [`Session::replay`] with progress and cancel hooks (#217): `ctl`
    /// reports `done` of `total` ops and stops with [`ExecError::Cancelled`]
    /// between ops once its cancel flag is set. The default control is
    /// exactly `replay`.
    pub fn replay_with(log: Vec<Command>, ctl: ReplayControl<'_>) -> Result<Self, ExecError> {
        let mut session = Session::default();
        session.run_all(log, ctl)?;
        Ok(session)
    }

    /// Run the logged ops of a saved log in order (see [`Session::replay`]).
    fn run_all(&mut self, log: Vec<Command>, ctl: ReplayControl<'_>) -> Result<(), ExecError> {
        let total = log.len();
        for (done, cmd) in log.into_iter().enumerate() {
            ctl.step(done, total)?;
            // Security: the app only ever appends *logged* ops to the saved log
            // (`is_logged` == true). A non-logged op in a loaded file therefore
            // means the file was hand-crafted/tampered — replaying it would run
            // filesystem/subprocess side effects (`export` → arbitrary write,
            // `import` → arbitrary read + `dwg2dxf` spawn, etc.) the instant a
            // victim opens a shared document, bypassing the deck confirm-gate.
            // Skip these on replay; this exactly mirrors the write-side invariant.
            if !cmd.is_logged() {
                continue;
            }
            self.run(cmd)?;
        }
        ctl.report(total, total);
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    /// History rebuilds run on this thread (tests assert none happen).
    pub(crate) static HISTORY_REBUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A fast-open prefix to rebuild off the session ([`Session::history_job`],
/// #224), e.g. on a worker thread.
pub struct HistoryJob {
    token: u64,
    prefix: Vec<Command>,
}

/// The inverses rebuilt by [`HistoryJob::run`], for
/// [`Session::install_history`].
pub struct RebuiltHistory {
    token: u64,
    log: Vec<AppliedOp>,
    #[cfg(debug_assertions)]
    doc: Document,
}

impl HistoryJob {
    /// Ops to replay.
    pub fn len(&self) -> usize {
        self.prefix.len()
    }

    /// Whether there is nothing to replay.
    pub fn is_empty(&self) -> bool {
        self.prefix.is_empty()
    }

    /// Replay the prefix into a scratch session; `ctl` reports and can cancel.
    pub fn run(self, ctl: ReplayControl<'_>) -> Result<RebuiltHistory, ExecError> {
        #[cfg(test)]
        HISTORY_REBUILDS.with(|n| n.set(n.get() + 1));
        let s = Session::replay_with(self.prefix, ctl)?;
        Ok(RebuiltHistory {
            token: self.token,
            log: s.log,
            #[cfg(debug_assertions)]
            doc: s.doc,
        })
    }
}

/// Progress and cancel hooks for a long replay ([`Session::replay_with`],
/// [`Session::from_snapshot_tail`]; #217), checked between ops. The default
/// does nothing.
#[derive(Clone, Copy, Default)]
pub struct ReplayControl<'a> {
    pub cancel: Option<&'a std::sync::atomic::AtomicBool>,
    /// Called with `(done, total)` before each op and once at the end, from
    /// the replaying thread.
    pub progress: Option<&'a (dyn Fn(usize, usize) + Sync)>,
}

impl ReplayControl<'_> {
    fn report(&self, done: usize, total: usize) {
        if let Some(p) = self.progress {
            p(done, total);
        }
    }

    /// Report progress, or stop with [`ExecError::Cancelled`] when cancelled.
    fn step(&self, done: usize, total: usize) -> Result<(), ExecError> {
        if self
            .cancel
            .is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
        {
            return Err(ExecError::Cancelled);
        }
        self.report(done, total);
        Ok(())
    }
}
