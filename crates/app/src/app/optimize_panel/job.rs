// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! The panel state and its background search: start, poll, cancel, progress.

use super::*;

/// A search running on a worker thread: the document's one run (#201).
pub(in crate::app) struct Job {
    /// The site plan searched.
    pub(super) source: ObjectId,
    pub(super) started: Instant,
    /// The form the search ran with (picks the shown columns).
    form: OptimizeForm,
    /// What the search plans, saved with its result.
    prepared: site_optimize::Prepared,
    rx: mpsc::Receiver<Result<OptimizeResult, String>>,
    /// Set by Cancel; the worker stops at its next plan and its result is
    /// dropped (the panel already let go of the job).
    pub(super) cancel: Arc<AtomicBool>,
    /// The search's latest progress report.
    pub(super) progress: Arc<Mutex<site_optimize::Progress>>,
    /// No idle sleep while it runs (the display may sleep).
    _awake: crate::keep_awake::KeepAwake,
}

impl Job {
    pub(super) fn progress(&self) -> site_optimize::Progress {
        *self.progress.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The running row's text: phase, `done / total`, feasible so far, elapsed.
pub(in crate::app) fn progress_text(p: &site_optimize::Progress, elapsed: f64) -> String {
    let phase = match p.phase {
        site_optimize::Phase::Search => t("optimize.running"),
        site_optimize::Phase::Daylight => t("optimize.running_daylight"),
    };
    let mut s = format!("{phase} {} / {}", p.evaluated, p.budget);
    if p.phase == site_optimize::Phase::Search {
        s.push_str(&format!(" · {} {}", p.feasible, t("optimize.feasible")));
    }
    format!("{s} · {elapsed:.1} s")
}

/// A finished run's notice, shown wherever the user is (#201). It never
/// applies anything; clicking it opens the site's Optimize.
pub(in crate::app) struct Notice {
    pub(in crate::app) source: ObjectId,
    pub(in crate::app) text: String,
    pub(in crate::app) at: Instant,
}

/// How long a notice stays up.
pub(in crate::app) const NOTICE_FOR: Duration = Duration::from_secs(8);

/// The panel state, kept on [`App`]. One search runs per document (#201):
/// opening another site plan's Optimize, selecting or editing never stops
/// it; only Cancel, deleting its site plan, or closing/replacing the
/// document does. Results are the site plan's saved run
/// ([`itsjustcad_commands::Session::optimize_run`]).
#[derive(Default)]
pub(in crate::app) struct OptimizePanel {
    /// The site plan boundary the form is open for.
    source: Option<ObjectId>,
    pub(in crate::app) form: OptimizeForm,
    pub(super) job: Option<Job>,
    /// Run was pressed for this site while another site's search runs: ask
    /// "Stop the running search?" first.
    pub(super) confirm_stop: Option<ObjectId>,
    /// The last search of this site failed.
    pub(super) error: Option<(ObjectId, String)>,
    /// The last search of this site was cancelled.
    pub(super) cancelled: Option<ObjectId>,
    /// The shown saved run, ready to draw: (source, finished_at, view).
    shown: Option<(ObjectId, u64, Finished)>,
    /// The finish notice.
    pub(in crate::app) notice: Option<Notice>,
    /// The Yield economics block's plan quantities, per (source, generation).
    pub(super) current_q: Option<(ObjectId, u64, site_optimize::DaylightOpts, Option<Quantities>)>,
}

impl OptimizePanel {
    /// Show the form for `source`: the running search's form, else the saved
    /// run's, else the current form seeded from the plan. Never stops a run.
    pub(super) fn open_for(
        &mut self,
        source: ObjectId,
        settings: &subdivision::SubdivisionSettings,
        saved: Option<&SavedRun>,
    ) {
        if self.source != Some(source) {
            self.confirm_stop = None;
            match (&self.job, saved) {
                (Some(job), _) if job.source == source => self.form = job.form.clone(),
                (_, Some(run)) => self.form = OptimizeForm::from_request(&run.request, settings),
                _ => self.form.seed_from_plan(settings),
            }
        }
        self.source = Some(source);
    }

    /// The site plan being searched, if a search runs.
    pub(in crate::app) fn running_source(&self) -> Option<ObjectId> {
        self.job.as_ref().map(|j| j.source)
    }

    /// The running search's progress and elapsed seconds.
    pub(in crate::app) fn running_progress(&self) -> Option<(site_optimize::Progress, f64)> {
        self.job
            .as_ref()
            .map(|j| (j.progress(), j.started.elapsed().as_secs_f64()))
    }

    /// Start a search of `source` off the UI thread (stopping any running
    /// one); repaints `ctx` when done.
    pub(super) fn start(&mut self, doc: &itsjustcad_doc::Document, source: ObjectId, ctx: &egui::Context) {
        self.cancel();
        self.confirm_stop = None;
        self.cancelled = None;
        self.error = None;
        let prepared = match site_optimize::prepare_doc(doc, &[source], &self.form.to_request()) {
            Ok(p) => p,
            Err(e) => {
                self.error = Some((source, e));
                return;
            }
        };
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(site_optimize::Progress {
            budget: prepared.req.budget,
            ..Default::default()
        }));
        let ctx = ctx.clone();
        let (flag, latest) = (cancel.clone(), progress.clone());
        let worker = prepared.clone();
        std::thread::spawn(move || {
            let report = |p: &site_optimize::Progress| {
                *latest.lock().unwrap_or_else(|e| e.into_inner()) = *p;
                ctx.request_repaint();
            };
            let ctl = site_optimize::Control {
                cancel: Some(&flag),
                progress: Some(&report),
                ..Default::default()
            };
            // A dropped job (cancelled) dropped `rx`; the send then fails
            // harmlessly.
            let _ = tx.send(worker.run(ctl));
            ctx.request_repaint();
        });
        self.job = Some(Job {
            source,
            started: Instant::now(),
            form: self.form.clone(),
            prepared,
            rx,
            cancel,
            progress,
            _awake: crate::keep_awake::KeepAwake::new("ItsJustCAD site plan optimizer"),
        });
    }

    /// Stop the running search. The panel lets go of the job at once (back to
    /// Run, nothing applied); the worker sees the flag before its next plan
    /// and exits, its result unread. Waiting for the worker instead kept the
    /// panel on "Stopping…" until in-flight plans finished — over a second
    /// each on a large site in a debug build.
    pub(in crate::app) fn cancel(&mut self) {
        if let Some(job) = self.job.take() {
            job.cancel.store(true, Ordering::Relaxed);
            self.cancelled = Some(job.source);
        }
    }

    /// The document closed or was replaced: stop the search, forget it all.
    pub(in crate::app) fn close_document(&mut self) {
        self.cancel();
        *self = OptimizePanel {
            form: std::mem::take(&mut self.form),
            ..OptimizePanel::default()
        };
    }

    /// Collect a finished search, if any, into `session` as its site plan's
    /// saved run, and raise the finish notice. A search whose site plan was
    /// deleted is stopped. Called every frame, wherever the user is.
    pub(in crate::app) fn poll(&mut self, session: &mut itsjustcad_commands::Session) {
        let Some(job) = &self.job else { return };
        if !optimize_runs::has_site_plan(&session.doc, job.source) {
            self.cancel();
            self.cancelled = None;
            return;
        }
        let got = match job.rx.try_recv() {
            Ok(r) => r,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("siteoptimize: the search stopped".into()),
        };
        let Some(job) = self.job.take() else { return };
        let elapsed = job.started.elapsed().as_secs_f64();
        let text = match got {
            Ok(r) => {
                let n = r.candidates.len();
                session.set_optimize_run(job.source, SavedRun::new(&job.prepared, r, elapsed));
                format!("{} {n} {}", t("optimize.done"), t("optimize.plans"))
            }
            // A cancelled search shows nothing (the Run row says so).
            Err(e) if e == site_optimize::CANCELLED => {
                self.cancelled = Some(job.source);
                return;
            }
            Err(e) => {
                self.error = Some((job.source, e));
                t("optimize.failed").to_string()
            }
        };
        self.notice = Some(Notice {
            source: job.source,
            text,
            at: Instant::now(),
        });
    }

    /// The saved run of `source`, built for drawing (cached per run).
    pub(super) fn shown(&mut self, source: ObjectId, run: Option<&SavedRun>, settings: &subdivision::SubdivisionSettings) -> Option<&Finished> {
        let run = run?;
        let fresh = matches!(&self.shown, Some((s, at, _)) if *s == source && *at == run.finished_at);
        if !fresh {
            let form = OptimizeForm::from_request(&run.request, settings);
            self.shown = Some((source, run.finished_at, Finished::new(&form, &run.result)));
        }
        self.shown.as_ref().map(|(_, _, f)| f)
    }

    /// Run a search for `source` to completion on this thread, save it and
    /// post the notice (previews, the dev hook).
    pub(in crate::app) fn run_now(&mut self, session: &mut itsjustcad_commands::Session, source: ObjectId) {
        let link = session.doc.subdivision_links.get(&source).expect("a site plan").clone();
        self.open_for(source, &link.settings, None);
        let started = Instant::now();
        match site_optimize::prepare_doc(&session.doc, &[source], &self.form.to_request())
            .and_then(|p| Ok((p.run(Default::default())?, p)))
        {
            Ok((r, p)) => {
                let n = r.candidates.len();
                session.set_optimize_run(source, SavedRun::new(&p, r, started.elapsed().as_secs_f64()));
                self.notice = Some(Notice {
                    source,
                    text: format!("{} {n} {}", t("optimize.done"), t("optimize.plans")),
                    at: Instant::now(),
                });
            }
            Err(e) => self.error = Some((source, e)),
        }
    }
}

#[cfg(test)]
impl OptimizePanel {
    /// Show a search for `source` as running at `p` (previews).
    pub(in crate::app) fn fake_running(
        &mut self,
        doc: &itsjustcad_doc::Document,
        source: ObjectId,
        p: site_optimize::Progress,
    ) {
        let (tx, rx) = mpsc::channel();
        std::mem::forget(tx); // never finishes
        self.source = Some(source);
        self.job = Some(Job {
            source,
            started: Instant::now() - Duration::from_millis(3200),
            form: self.form.clone(),
            prepared: site_optimize::prepare_doc(doc, &[source], &self.form.to_request()).unwrap(),
            rx,
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(Mutex::new(p)),
            _awake: crate::keep_awake::KeepAwake::new("preview"),
        });
    }
}
