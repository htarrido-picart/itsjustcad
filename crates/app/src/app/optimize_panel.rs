// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Site plan optimizer panel (#145, desktop host): the **Optimize** segment of
//! a `SitePlan` link's subdivision inspector (`Controls | Yield | Optimize`).
//! Objective, constraints and the `value` objective's **Economics** (unit
//! prices) on top, a full-width **Run**, then the results as deltas vs the
//! current plan (scored like a candidate), the Pareto scatter, and an **Advanced**
//! section (budget, seed, locks, street patterns). Run hands the boundary, the
//! link's settings and its hand edits to a worker thread that calls
//! [`site_optimize::optimize_with`], so frames keep painting; Cancel drops the
//! receiver (the search finishes and its result is discarded). Each result
//! row's **Apply** runs `siteoptimize apply` with the candidate's params (one
//! logged op, one undo step), then the inspector switches to `Yield`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use itsjustcad_commands::site_optimize::{
    self, Economics, Ledger, Objective, OptimizeRequest, OptimizeResult, Quantities, SEARCHED,
};
use itsjustcad_commands::{subdiv_params, Command};
use itsjustcad_doc::{ObjectId, SubdivKind};

use super::App;
use crate::i18n::t;

/// The objectives offered, with their label keys.
const OBJECTIVES: &[(Objective, &str)] = &[
    (Objective::Value, "optimize.objective.value"),
    (Objective::Lots, "optimize.objective.lots"),
    (Objective::FarNet, "optimize.objective.far_net"),
    (Objective::NetArea, "optimize.objective.net_area"),
    (Objective::Gfa, "optimize.objective.gfa"),
    (Objective::Weighted, "optimize.objective.weighted"),
];

/// Weighted-objective metrics (core `METRICS` tokens) and their label keys.
const METRICS: &[(&str, &str)] = &[
    ("lots", "optimize.objective.lots"),
    ("far", "optimize.objective.far_net"),
    ("net_area", "optimize.objective.net_area"),
    ("gfa", "optimize.objective.gfa"),
    ("open", "optimize.weight.open"),
];

/// Hard constraints, in form order. Ratios are entered as percentages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Constraint {
    MinLotArea,
    MinLotWidth,
    MinFrontage,
    MinOpen,
    MaxFar,
    MaxCoverage,
}

impl Constraint {
    pub(super) const ALL: [Constraint; 6] = [
        Constraint::MinLotArea,
        Constraint::MinLotWidth,
        Constraint::MinFrontage,
        Constraint::MinOpen,
        Constraint::MaxFar,
        Constraint::MaxCoverage,
    ];

    fn label_key(self) -> &'static str {
        match self {
            Constraint::MinLotArea => "optimize.min_lot_area",
            Constraint::MinLotWidth => "optimize.min_lot_width",
            Constraint::MinFrontage => "optimize.min_frontage",
            Constraint::MinOpen => "optimize.min_open",
            Constraint::MaxFar => "optimize.max_far",
            Constraint::MaxCoverage => "optimize.max_coverage",
        }
    }

    /// (default value, step, unit) as shown in the form.
    fn widget(self) -> (f64, f64, &'static str) {
        match self {
            Constraint::MinLotArea => (300.0, 10.0, "m²"),
            Constraint::MinLotWidth => (10.0, 0.5, "m"),
            Constraint::MinFrontage => (8.0, 0.5, "m"),
            Constraint::MinOpen => (15.0, 1.0, "%"),
            Constraint::MaxFar => (1.5, 0.05, ""),
            Constraint::MaxCoverage => (60.0, 1.0, "%"),
        }
    }

    fn is_percent(self) -> bool {
        matches!(self, Constraint::MinOpen | Constraint::MaxCoverage)
    }
}

/// The optimize form, as the user edits it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct OptimizeForm {
    pub objective: Objective,
    /// Weights per [`METRICS`] entry (used by `Weighted`; zero = omitted).
    pub weights: [f64; 5],
    /// Per [`Constraint::ALL`]: (enabled, value in form units).
    pub constraints: [(bool, f64); 6],
    pub budget: usize,
    pub seed: u64,
    pub locks: BTreeSet<String>,
    /// Allowed street patterns; all of them (or none) means unrestricted.
    pub patterns: BTreeSet<String>,
    /// Unit prices of the `value` objective (and the Yield economics block).
    pub economics: Economics,
}

impl Default for OptimizeForm {
    fn default() -> Self {
        let d = OptimizeRequest::default();
        OptimizeForm {
            objective: d.objective,
            weights: [1.0, 0.0, 0.0, 0.0, 0.0],
            constraints: Constraint::ALL.map(|c| (false, c.widget().0)),
            budget: d.budget,
            seed: d.seed,
            locks: BTreeSet::new(),
            patterns: street_patterns().into_iter().collect(),
            economics: d.economics,
        }
    }
}

/// The street pattern tokens of the subdivision schema.
pub(super) fn street_patterns() -> Vec<String> {
    subdiv_params::subdivision_fields()
        .into_iter()
        .find(|f| f.name == "street_pattern")
        .map_or_else(Vec::new, |f| f.choices.iter().map(|c| c.to_string()).collect())
}

impl OptimizeForm {
    /// The core request this form describes.
    pub(super) fn to_request(&self) -> OptimizeRequest {
        let mut req = OptimizeRequest {
            objective: self.objective,
            budget: self.budget,
            seed: self.seed,
            economics: self.economics,
            lock: SEARCHED
                .iter()
                .filter(|k| self.locks.contains(**k))
                .map(|k| k.to_string())
                .collect(),
            ..OptimizeRequest::default()
        };
        if self.objective == Objective::Weighted {
            req.weights = METRICS
                .iter()
                .zip(self.weights)
                .filter(|(_, w)| *w != 0.0)
                .map(|((k, _), w)| (k.to_string(), w))
                .collect();
        }
        let all = street_patterns();
        if !all.iter().all(|p| self.patterns.contains(p)) {
            req.patterns = all.into_iter().filter(|p| self.patterns.contains(p)).collect();
        }
        for (c, (on, v)) in Constraint::ALL.into_iter().zip(self.constraints) {
            if !on {
                continue;
            }
            let v = Some(if c.is_percent() { v / 100.0 } else { v });
            match c {
                Constraint::MinLotArea => req.min_lot_area = v,
                Constraint::MinLotWidth => req.min_lot_width = v,
                Constraint::MinFrontage => req.min_frontage = v,
                Constraint::MinOpen => req.min_open = v,
                Constraint::MaxFar => req.max_far = v,
                Constraint::MaxCoverage => req.max_coverage = v,
            }
        }
        req
    }
}

/// Subdivision inspector segment indices (see [`segment_keys`]).
pub(super) const SEG_YIELD: usize = 1;
pub(super) const SEG_OPTIMIZE: usize = 2;

/// The subdivision inspector's segment label keys for a link kind: a Site link
/// bakes no lots (no segments), a Lots link has `Controls | Yield`, a site plan
/// adds `Optimize`.
pub(super) fn segment_keys(kind: SubdivKind) -> &'static [&'static str] {
    const ALL: &[&str] = &[
        "param.subdivision.controls",
        "param.subdivision.yield",
        "param.subdivision.optimize",
    ];
    match kind {
        SubdivKind::Site => &[],
        SubdivKind::Lots => &ALL[..2],
        SubdivKind::SitePlan => ALL,
    }
}

/// `seg` clamped to `n` segments (the index is shared across links).
pub(super) fn clamp_segment(seg: usize, n: usize) -> usize {
    seg.min(n.saturating_sub(1))
}

/// The yield metrics the results table compares.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Metrics {
    pub lots: usize,
    /// Σ lot area, m².
    pub net_area: f64,
    pub gfa: f64,
    pub far: Option<f64>,
    /// Open-space ratio, percent.
    pub open_pct: f64,
}

impl Metrics {
    pub(super) fn from_report(y: &subdivision::YieldReport) -> Self {
        Metrics {
            lots: y.lot_count,
            net_area: y.total_lot_area,
            gfa: y.total_gfa,
            far: y.far_net,
            open_pct: y.open_space_ratio * 100.0,
        }
    }

    /// The value shown in `col` (`Score` is not a yield metric).
    fn get(&self, col: Col) -> Option<f64> {
        match col {
            Col::Score => None,
            Col::Lots => Some(self.lots as f64),
            Col::Net => Some(self.net_area),
            Col::Gfa => Some(self.gfa),
            Col::Far => self.far,
            Col::Open => Some(self.open_pct),
        }
    }

    /// The `Current` row's cell for `col`.
    pub(super) fn cell(&self, col: Col) -> String {
        self.get(col).map_or(String::new(), |v| fmt_abs(col, v))
    }

    /// `self − base` for `col`; absolute when the base has no value.
    pub(super) fn delta_cell(&self, base: &Metrics, col: Col) -> String {
        match (self.get(col), base.get(col)) {
            (Some(a), Some(b)) => fmt_delta(col, a - b),
            (Some(a), None) => fmt_abs(col, a),
            (None, _) => "—".into(),
        }
    }

    /// Every metric, for a row's hover.
    fn summary(&self) -> String {
        Col::ALL[1..]
            .iter()
            .map(|c| format!("{} {}", t(c.label_key()), self.cell(*c)))
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// A results-table column (units live in the header label).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Col {
    Score,
    Lots,
    Net,
    Gfa,
    Far,
    Open,
}

impl Col {
    const ALL: [Col; 6] = [Col::Score, Col::Lots, Col::Net, Col::Gfa, Col::Far, Col::Open];

    fn label_key(self) -> &'static str {
        match self {
            Col::Score => "optimize.col.score",
            Col::Lots => "param.subdivision.yield.lots",
            Col::Net => "optimize.col.net",
            Col::Gfa => "optimize.col.gfa",
            Col::Far => "optimize.col.far",
            Col::Open => "optimize.col.open",
        }
    }

    /// `a` (≥ 0) as shown: areas compact (`950`, `12.3k`, `412k`, `2.95M`, `12.0M`),
    /// scores as integers when they are.
    fn magnitude(self, a: f64) -> String {
        match self {
            Col::Score if (a - a.round()).abs() < 1e-9 => format!("{a:.0}"),
            Col::Score | Col::Far => format!("{a:.2}"),
            Col::Lots => format!("{a:.0}"),
            Col::Open => format!("{a:.1}"),
            Col::Net | Col::Gfa => match a {
                a if a < 10_000.0 => format!("{a:.0}"),
                a if a < 100_000.0 => format!("{:.1}k", a / 1e3),
                a if a < 999_500.0 => format!("{:.0}k", a / 1e3),
                a if a < 9_995_000.0 => format!("{:.2}M", a / 1e6),
                a => format!("{:.1}M", a / 1e6),
            },
        }
    }
}

fn rounds_to_zero(s: &str) -> bool {
    s.chars().all(|c| c == '0' || c == '.')
}

/// An absolute value: `0` when it rounds to zero (never `−0.0`).
pub(super) fn fmt_abs(col: Col, v: f64) -> String {
    let mag = col.magnitude(v.abs());
    if rounds_to_zero(&mag) {
        "0".into()
    } else if v < 0.0 {
        format!("−{mag}")
    } else {
        mag
    }
}

/// A signed delta: `+12`, `−3.0` (true minus sign), `±0` when it rounds to
/// zero.
pub(super) fn fmt_delta(col: Col, d: f64) -> String {
    let mag = col.magnitude(d.abs());
    let sign = if rounds_to_zero(&mag) {
        return "±0".into();
    } else if d > 0.0 {
        "+"
    } else {
        "−"
    };
    format!("{sign}{mag}")
}

/// The columns worth a place in a narrow panel: Score only when it is not
/// one of the shown metrics (weighted, or profit per m² for `value`, which
/// then drops GFA), FAR when the search is about FAR, open space when it is
/// constrained or varies.
pub(super) fn columns(form: &OptimizeForm, rows: &[ResultRow]) -> Vec<Col> {
    let on = |c: Constraint| {
        Constraint::ALL
            .iter()
            .position(|x| *x == c)
            .is_some_and(|i| form.constraints[i].0)
    };
    let weighted = form.objective == Objective::Weighted;
    let weight = |k: &str| {
        weighted && METRICS.iter().zip(form.weights).any(|((m, _), w)| *m == k && w != 0.0)
    };
    let value = form.objective == Objective::Value;
    let mut cols = Vec::new();
    if weighted || value {
        cols.push(Col::Score);
    }
    cols.extend([Col::Lots, Col::Net]);
    if !value {
        cols.push(Col::Gfa);
    }
    if form.objective == Objective::FarNet || on(Constraint::MaxFar) || weight("far") {
        cols.push(Col::Far);
    }
    let open_varies = rows.iter().any(|r| r.metrics.open_pct.abs() >= 0.05);
    if open_varies || on(Constraint::MinOpen) || weight("open") {
        cols.push(Col::Open);
    }
    cols
}

/// The Pareto axis token (`OptimizeResult::pareto_axis`) as a metric getter
/// and its label key.
fn axis_of(axis: &str) -> (fn(&ResultRow) -> f64, &'static str) {
    match axis {
        "net_area" => (|r| r.metrics.net_area, "optimize.objective.net_area"),
        "gfa" => (|r| r.metrics.gfa, "optimize.objective.gfa"),
        "lots" => (|r| r.metrics.lots as f64, "optimize.objective.lots"),
        _ => (|r| r.metrics.open_pct, "optimize.weight.open"),
    }
}

/// One result-table row.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ResultRow {
    pub rank: usize,
    pub score: f64,
    pub metrics: Metrics,
    pub pareto: bool,
    pub params: BTreeMap<String, String>,
    pub ledger: Ledger,
}

pub(super) fn result_rows(r: &OptimizeResult) -> Vec<ResultRow> {
    r.candidates
        .iter()
        .map(|c| ResultRow {
            rank: c.rank,
            score: c.eval.score,
            metrics: Metrics::from_report(&c.eval.report),
            pareto: r.pareto.contains(&c.rank),
            params: c.params.clone(),
            ledger: c.eval.ledger,
        })
        .collect()
}

/// The current plan as a (rank 0) row, scored like the candidates.
pub(super) fn current_row(r: &OptimizeResult) -> Option<ResultRow> {
    r.current.as_ref().map(|e| ResultRow {
        rank: 0,
        score: e.score,
        metrics: Metrics::from_report(&e.report),
        pareto: false,
        params: BTreeMap::new(),
        ledger: e.ledger,
    })
}

/// Money, compact like areas (`950`, `12.3k`, `94.1M`), never `−0`.
pub(super) fn fmt_money(v: f64) -> String {
    fmt_abs(Col::Net, v)
}

/// A ledger as (label key, value) lines: revenue, each cost (negative),
/// profit, profit per m².
pub(super) fn ledger_lines(l: &Ledger) -> [(&'static str, String); 7] {
    [
        ("optimize.econ.revenue", fmt_money(l.revenue)),
        ("optimize.econ.roads", fmt_money(-l.roads)),
        ("optimize.econ.utilities", fmt_money(-l.utilities)),
        ("optimize.econ.lots", fmt_money(-l.lots)),
        ("optimize.econ.open_space", fmt_money(-l.open_space)),
        ("optimize.econ.profit", fmt_money(l.profit)),
        ("optimize.objective.value", fmt_abs(Col::Far, l.value)),
    ]
}

fn ledger_text(l: &Ledger) -> String {
    ledger_lines(l)
        .iter()
        .map(|(k, v)| format!("{} {v}", t(k)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `s` without the minus of a value that rounds to zero (`-0`, `-0.0`).
pub(super) fn unsigned_zero(s: String) -> String {
    match s.strip_prefix('-') {
        Some(rest) if rounds_to_zero(rest) => rest.to_string(),
        _ => s,
    }
}

/// A finished search, ready to show.
pub(super) struct Finished {
    rows: Vec<ResultRow>,
    evaluated: usize,
    feasible: usize,
    /// `OptimizeResult::pareto_axis`.
    axis: &'static str,
    cols: Vec<Col>,
    /// Score header (`Profit/m²` for `value`).
    score_key: &'static str,
    /// The current plan, scored like a candidate.
    current: Option<ResultRow>,
    beats_current: bool,
}

/// A search running on a worker thread.
pub(super) struct Job {
    started: Instant,
    /// The form the search ran with (picks the shown columns).
    form: OptimizeForm,
    rx: mpsc::Receiver<Result<OptimizeResult, String>>,
}

/// The panel state, kept on [`App`].
#[derive(Default)]
pub(super) struct OptimizePanel {
    /// The site plan boundary the form is open for.
    source: Option<ObjectId>,
    form: OptimizeForm,
    job: Option<Job>,
    /// Last finished search: (elapsed, rows + summary, or the error).
    result: Option<(Duration, Result<Finished, String>)>,
    /// The Yield economics block's plan quantities, per (source, generation).
    current_q: Option<(ObjectId, u64, Option<Quantities>)>,
}

impl OptimizeForm {
    /// Defaults that keep the current plan's lot size: `min_lot_area` on, at
    /// the plan's `lot_area_min` (maximising lots alone rewards slivers).
    pub(super) fn seed_from_plan(&mut self, settings: &subdivision::SubdivisionSettings) {
        self.constraints[0] = (true, settings.lot_area_min);
    }
}

impl OptimizePanel {
    fn open_for(&mut self, source: ObjectId, settings: &subdivision::SubdivisionSettings) {
        if self.source != Some(source) {
            self.job = None;
            self.result = None;
            self.form.seed_from_plan(settings);
        }
        self.source = Some(source);
    }

    /// Start a search off the UI thread; repaints `ctx` when done.
    fn start(&mut self, doc: &itsjustcad_doc::Document, source: ObjectId, ctx: &egui::Context) {
        let req = self.form.to_request();
        let Some((_, site)) = site_optimize::site_boundary(doc, &[source]) else {
            self.result = Some((
                Duration::ZERO,
                Err("siteoptimize needs a closed site boundary curve".into()),
            ));
            return;
        };
        let (base, edits) = site_optimize::base_plan(doc, source);
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            // A cancelled job dropped `rx`; the send then fails harmlessly.
            let _ = tx.send(site_optimize::optimize_with(&site, &base, &edits, &req));
            ctx.request_repaint();
        });
        self.result = None;
        self.job = Some(Job { started: Instant::now(), form: self.form.clone(), rx });
    }

    /// Collect a finished search, if any.
    fn poll(&mut self) {
        let Some(job) = &self.job else { return };
        let got = match job.rx.try_recv() {
            Ok(r) => r,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("siteoptimize: the search stopped".into()),
        };
        let elapsed = job.started.elapsed();
        let finished = got.map(|r| {
            let rows = result_rows(&r);
            Finished {
                cols: columns(&job.form, &rows),
                rows,
                evaluated: r.evaluated,
                feasible: r.feasible,
                axis: r.pareto_axis,
                score_key: if job.form.objective == Objective::Value {
                    "optimize.col.value"
                } else {
                    "optimize.col.score"
                },
                current: current_row(&r),
                beats_current: r.beats_current(),
            }
        });
        self.job = None;
        self.result = Some((elapsed, finished));
    }
}

/// Grid spacing of the form and results: comfortable rows.
const GRID_SPACING: egui::Vec2 = egui::vec2(crate::theme::Spacing::S, 6.0);

impl App {
    /// The `Optimize` segment of a `SitePlan` link's inspector.
    pub(super) fn optimize_panel(
        &mut self,
        ui: &mut egui::Ui,
        source: ObjectId,
        link: &itsjustcad_doc::SubdivLink,
    ) {
        self.optimize.open_for(source, &link.settings);
        self.optimize.poll();
        self.refresh_subdiv_yield(source, link);
        let base = self
            .subdiv_yield
            .as_ref()
            .map(|(_, _, r)| Metrics::from_report(r));
        let side = crate::theme::Spacing::M as i8;
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(side, 4))
            .show(ui, |ui| {
                self.optimize_form(ui, source);
                ui.add_space(crate::theme::Spacing::S);
                self.optimize_economics(ui, source);
                ui.add_space(crate::theme::Spacing::SM);
                self.optimize_run_row(ui, source);
                ui.add_space(crate::theme::Spacing::S);
                self.optimize_results(ui, source, base);
                ui.add_space(crate::theme::Spacing::S);
                self.optimize_advanced(ui, source);
            });
    }

    /// Objective (+ weights) and the constraint list.
    fn optimize_form(&mut self, ui: &mut egui::Ui, source: ObjectId) {
        let busy = self.optimize.job.is_some();
        let form = &mut self.optimize.form;
        ui.add_enabled_ui(!busy, |ui| {
            egui::Grid::new(("optimize_objective_grid", source))
                .num_columns(2)
                .spacing(GRID_SPACING)
                .show(ui, |ui| {
                    ui.strong(t("optimize.objective"));
                    let current = OBJECTIVES
                        .iter()
                        .find(|(o, _)| *o == form.objective)
                        .map_or("", |(_, k)| t(k));
                    egui::ComboBox::from_id_salt(("optimize_objective", source))
                        .selected_text(current)
                        .show_ui(ui, |ui| {
                            for (o, k) in OBJECTIVES {
                                ui.selectable_value(&mut form.objective, *o, t(k));
                            }
                        });
                    ui.end_row();
                    if form.objective == Objective::Weighted {
                        for ((_, k), w) in METRICS.iter().zip(form.weights.iter_mut()) {
                            ui.label(format!("  {}", t(k)));
                            ui.add(egui::DragValue::new(w).speed(0.05).range(-10.0..=10.0));
                            ui.end_row();
                        }
                    }
                });

            ui.add_space(crate::theme::Spacing::S);
            ui.strong(t("optimize.constraints"));
            egui::Grid::new(("optimize_constraint_grid", source))
                .num_columns(2)
                .spacing(GRID_SPACING)
                .show(ui, |ui| {
                    for (c, (on, v)) in Constraint::ALL.into_iter().zip(form.constraints.iter_mut())
                    {
                        ui.checkbox(on, t(c.label_key()));
                        let (_, step, unit) = c.widget();
                        ui.add_enabled(
                            *on,
                            egui::DragValue::new(v)
                                .speed(step)
                                .range(0.0..=f64::MAX)
                                .suffix(if unit.is_empty() {
                                    String::new()
                                } else {
                                    format!(" {unit}")
                                }),
                        );
                        ui.end_row();
                    }
                });
        });
    }

    /// Full-width Run, or spinner + elapsed + Cancel while a search runs.
    fn optimize_run_row(&mut self, ui: &mut egui::Ui, source: ObjectId) {
        match &self.optimize.job {
            Some(job) => {
                let elapsed = job.started.elapsed().as_secs_f64();
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!("{} {elapsed:.1} s", t("optimize.running")));
                    if ui.button(t("optimize.cancel")).clicked() {
                        self.optimize.job = None;
                    }
                });
                ui.ctx().request_repaint_after(Duration::from_millis(100));
            }
            None => {
                let fill = ui.visuals().selection.bg_fill;
                let text = egui::RichText::new(t("optimize.run"))
                    .strong()
                    .color(ui.visuals().strong_text_color());
                let run = egui::Button::new(text)
                    .fill(fill)
                    .min_size(egui::vec2(ui.available_width(), crate::theme::Spacing::HIT_TARGET));
                if ui.add(run).clicked() {
                    let ctx = ui.ctx().clone();
                    self.optimize.start(&self.session.doc, source, &ctx);
                }
            }
        }
    }

    /// Results table (deltas vs `base`, the current plan), then the scatter.
    /// Apply comes first so a narrow panel never clips it.
    fn optimize_results(&mut self, ui: &mut egui::Ui, source: ObjectId, base: Option<Metrics>) {
        let Some((elapsed, result)) = &self.optimize.result else {
            return;
        };
        let f = match result {
            Ok(f) => f,
            Err(e) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
                return;
            }
        };
        ui.weak(format!(
            "{} {} · {} {} · {:.1} s",
            f.evaluated,
            t("optimize.evaluated"),
            f.feasible,
            t("optimize.feasible"),
            elapsed.as_secs_f64()
        ));
        // Deltas are against the current plan as the search scored it; the
        // document's yield stands in when it could not be planned.
        let base = f.current.as_ref().map(|c| c.metrics).or(base);
        let hover = |row: &ResultRow| {
            format!(
                "{} {}\n\n{}\n\n{}\n\n{}",
                t(f.score_key),
                fmt_abs(Col::Score, row.score),
                ledger_text(&row.ledger),
                row.metrics.summary(),
                params_text(&row.params)
            )
            .trim_end()
            .to_string()
        };
        let mut apply = None;
        egui::ScrollArea::horizontal()
            .id_salt(("optimize_results_scroll", source))
            .show(ui, |ui| {
                egui::Grid::new(("optimize_results", source))
                    .num_columns(f.cols.len() + 2)
                    .striped(true)
                    .spacing(GRID_SPACING)
                    .show(ui, |ui| {
                        ui.label("");
                        ui.strong(t("optimize.col.rank"));
                        for c in &f.cols {
                            let key = if *c == Col::Score { f.score_key } else { c.label_key() };
                            ui.strong(t(key));
                        }
                        ui.end_row();
                        if let Some(b) = &base {
                            ui.label("");
                            let label = ui.strong(t("optimize.current"));
                            if let Some(cur) = &f.current {
                                label.on_hover_text(hover(cur));
                            }
                            for c in &f.cols {
                                ui.strong(match (c, &f.current) {
                                    (Col::Score, Some(cur)) => fmt_abs(Col::Score, cur.score),
                                    _ => b.cell(*c),
                                });
                            }
                            ui.end_row();
                        }
                        for row in &f.rows {
                            if ui
                                .small_button("✔")
                                .on_hover_text(t("optimize.apply"))
                                .clicked()
                            {
                                apply = Some(row.clone());
                            }
                            let rank = format!("{}{}", row.rank, if row.pareto { " ◆" } else { "" });
                            ui.monospace(rank).on_hover_text(hover(row));
                            for c in &f.cols {
                                ui.monospace(match (c, &base) {
                                    (Col::Score, _) => fmt_abs(Col::Score, row.score),
                                    (_, Some(b)) => row.metrics.delta_cell(b, *c),
                                    (_, None) => row.metrics.cell(*c),
                                });
                            }
                            ui.end_row();
                        }
                    });
            });
        if !f.beats_current {
            ui.add_space(crate::theme::Spacing::XS);
            ui.label(t("optimize.current_best"));
        }
        ui.add_space(crate::theme::Spacing::S);
        let (x_of, x_key) = axis_of(f.axis);
        ui.weak(format!("{} {}", t("optimize.pareto"), t(x_key)));
        pareto_plot(ui, &f.rows, x_of);

        if let Some(row) = apply {
            let out = self.session.run(Command::SiteOptimizeApply {
                rank: Some(row.rank),
                source: Some(source),
                params: Some(row.params),
                ids: None,
            });
            match out {
                Ok(o) => {
                    self.command_line.push_line(o.message);
                    self.subdiv_segment = SEG_YIELD;
                }
                Err(e) => self.command_line.push_line(e.to_string()),
            }
        }
    }

    /// Unit prices: open by default when the objective is `value` (the header
    /// id follows the objective, so switching to `value` opens it).
    fn optimize_economics(&mut self, ui: &mut egui::Ui, source: ObjectId) {
        let busy = self.optimize.job.is_some();
        let form = &mut self.optimize.form;
        let value = form.objective == Objective::Value;
        egui::CollapsingHeader::new(t("optimize.economics"))
            .id_salt(("optimize_economics", source, value))
            .default_open(value)
            .show(ui, |ui| {
                ui.add_enabled_ui(!busy, |ui| {
                    let e = &mut form.economics;
                    egui::Grid::new(("optimize_economics_grid", source))
                        .num_columns(2)
                        .spacing(GRID_SPACING)
                        .show(ui, |ui| {
                            for (key, v, speed) in [
                                ("optimize.econ.price", &mut e.price, 1.0),
                                ("optimize.econ.road_cost", &mut e.road_cost, 1.0),
                                ("optimize.econ.utility_cost", &mut e.utility_cost, 10.0),
                                ("optimize.econ.lot_cost", &mut e.lot_cost, 100.0),
                                ("optimize.econ.open_cost", &mut e.open_cost, 1.0),
                            ] {
                                ui.label(t(key));
                                ui.add(egui::DragValue::new(v).speed(speed).range(0.0..=f64::MAX));
                                ui.end_row();
                            }
                        });
                });
            });
    }

    /// The Yield segment's `Economics` block for a site plan: the current
    /// plan's ledger at the Optimize form's unit prices. The plan quantities
    /// are measured once per document generation.
    pub(super) fn yield_economics(&mut self, ui: &mut egui::Ui, source: ObjectId) {
        let generation = self.session.doc.generation;
        let fresh = matches!(&self.optimize.current_q, Some((id, g, _)) if *id == source && *g == generation);
        if !fresh {
            let doc = &self.session.doc;
            let q = site_optimize::site_boundary(doc, &[source]).and_then(|(_, site)| {
                let (base, edits) = site_optimize::base_plan(doc, source);
                site_optimize::evaluate(&site, &base, &edits, &OptimizeRequest::default())
                    .ok()
                    .map(|e| e.quantities)
            });
            self.optimize.current_q = Some((source, generation, q));
        }
        let Some((_, _, Some(q))) = &self.optimize.current_q else {
            return;
        };
        let ledger = self.optimize.form.economics.ledger(q);
        ui.add_space(crate::theme::Spacing::S);
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(crate::theme::Spacing::M as i8, 4))
            .show(ui, |ui| {
                ui.strong(t("optimize.economics"));
                // Same shape as the yield table: label, right-aligned value, unit.
                egui::Grid::new(("subdiv_yield_economics", source))
                    .num_columns(3)
                    .striped(true)
                    .spacing(GRID_SPACING)
                    .show(ui, |ui| {
                        for (key, v) in ledger_lines(&ledger) {
                            ui.label(t(key));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.monospace(v);
                            });
                            ui.monospace(if key == "optimize.objective.value" { "/m²" } else { "" });
                            ui.end_row();
                        }
                    });
            });
    }

    /// Collapsed-by-default budget, seed, locked parameters, street patterns.
    fn optimize_advanced(&mut self, ui: &mut egui::Ui, source: ObjectId) {
        let busy = self.optimize.job.is_some();
        let form = &mut self.optimize.form;
        egui::CollapsingHeader::new(t("optimize.advanced"))
            .id_salt(("optimize_advanced", source))
            .default_open(false)
            .show(ui, |ui| {
                ui.add_enabled_ui(!busy, |ui| {
                    egui::Grid::new(("optimize_advanced_grid", source))
                        .num_columns(2)
                        .spacing(GRID_SPACING)
                        .show(ui, |ui| {
                            ui.label(t("optimize.budget"));
                            ui.add(egui::DragValue::new(&mut form.budget).range(1..=5000));
                            ui.end_row();
                            ui.label(t("optimize.seed"));
                            ui.add(egui::DragValue::new(&mut form.seed));
                            ui.end_row();
                        });

                    ui.add_space(crate::theme::Spacing::S);
                    ui.strong(t("optimize.locks"));
                    let fields = subdiv_params::subdivision_fields();
                    for name in SEARCHED {
                        let label = fields
                            .iter()
                            .find(|f| f.name == *name)
                            .map_or(*name, |f| t(f.label_key));
                        let mut on = form.locks.contains(*name);
                        if ui.checkbox(&mut on, label).changed() {
                            if on {
                                form.locks.insert(name.to_string());
                            } else {
                                form.locks.remove(*name);
                            }
                        }
                    }

                    ui.add_space(crate::theme::Spacing::S);
                    ui.strong(t("optimize.patterns"));
                    ui.horizontal_wrapped(|ui| {
                        for p in street_patterns() {
                            let mut on = form.patterns.contains(&p);
                            if ui.checkbox(&mut on, &p).changed() {
                                if on {
                                    form.patterns.insert(p);
                                } else {
                                    form.patterns.remove(&p);
                                }
                            }
                        }
                    });
                });
            });
    }
}

fn params_text(params: &BTreeMap<String, String>) -> String {
    SEARCHED
        .iter()
        .filter_map(|k| params.get(*k).map(|v| format!("{k} = {v}")))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Scatter of score (y) vs the Pareto axis `x_of` (x); the front is drawn in
/// the selection color, joined and labelled by rank.
fn pareto_plot(ui: &mut egui::Ui, rows: &[ResultRow], x_of: fn(&ResultRow) -> f64) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 96.0),
        egui::Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    let weak = ui.visuals().weak_text_color();
    let accent = ui.visuals().selection.stroke.color;
    painter.rect_stroke(rect, 2.0, egui::Stroke::new(1.0, weak), egui::StrokeKind::Inside);
    if rows.is_empty() {
        return;
    }
    let span = |f: &dyn Fn(&ResultRow) -> f64| {
        let (lo, hi) = rows
            .iter()
            .map(f)
            .fold((f64::MAX, f64::MIN), |(lo, hi), v| (lo.min(v), hi.max(v)));
        (lo, (hi - lo).max(1e-9))
    };
    let ((x0, dx), (y0, dy)) = (span(&x_of), span(&|r| r.score));
    let inner = rect.shrink(10.0);
    let at = |r: &ResultRow| {
        egui::pos2(
            inner.left() + ((x_of(r) - x0) / dx) as f32 * inner.width(),
            inner.bottom() - ((r.score - y0) / dy) as f32 * inner.height(),
        )
    };
    let mut front: Vec<&ResultRow> = rows.iter().filter(|r| r.pareto).collect();
    front.sort_by(|a, b| x_of(a).total_cmp(&x_of(b)));
    painter.add(egui::Shape::line(
        front.iter().map(|r| at(r)).collect(),
        egui::Stroke::new(1.0, accent),
    ));
    for r in rows {
        let color = if r.pareto { accent } else { weak };
        painter.circle_filled(at(r), 3.0, color);
        if r.pareto {
            painter.text(
                at(r) + egui::vec2(4.0, -4.0),
                egui::Align2::LEFT_BOTTOM,
                r.rank.to_string(),
                egui::FontId::proportional(10.0),
                color,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_form_is_the_default_request() {
        assert_eq!(OptimizeForm::default().to_request(), OptimizeRequest::default());
    }

    #[test]
    fn form_maps_to_request() {
        let mut f = OptimizeForm {
            objective: Objective::Weighted,
            weights: [1.0, 0.0, 0.0, 0.5, 2.0],
            budget: 40,
            seed: 7,
            ..OptimizeForm::default()
        };
        f.constraints[0] = (true, 350.0); // min lot area
        f.constraints[3] = (true, 15.0); // min open, percent
        f.constraints[5] = (false, 50.0); // max coverage, off
        f.locks.extend(["typology".to_string(), "seed".to_string()]);
        f.patterns.retain(|p| p == "orthogonal" || p == "radial");
        let r = f.to_request();
        assert_eq!(r.objective, Objective::Weighted);
        assert_eq!(
            r.weights,
            BTreeMap::from([
                ("lots".to_string(), 1.0),
                ("gfa".to_string(), 0.5),
                ("open".to_string(), 2.0)
            ])
        );
        assert_eq!((r.budget, r.seed), (40, 7));
        assert_eq!(r.min_lot_area, Some(350.0));
        assert_eq!(r.min_open, Some(0.15));
        assert_eq!(r.max_coverage, None);
        assert_eq!(r.lock, vec!["seed", "typology"], "SEARCHED order");
        assert_eq!(r.patterns, vec!["orthogonal", "radial"], "schema order");
    }

    #[test]
    fn weights_only_sent_for_weighted() {
        let f = OptimizeForm {
            objective: Objective::Gfa,
            weights: [1.0, 1.0, 1.0, 1.0, 1.0],
            ..OptimizeForm::default()
        };
        assert!(f.to_request().weights.is_empty());
    }

    #[test]
    fn rows_mirror_the_result() {
        let site = subdivision::Polygon2d::new(vec![
            glam::DVec2::new(0.0, 0.0),
            glam::DVec2::new(200.0, 0.0),
            glam::DVec2::new(200.0, 150.0),
            glam::DVec2::new(0.0, 150.0),
        ])
        .unwrap();
        let req = OptimizeRequest { budget: 12, ..OptimizeRequest::default() };
        let r = site_optimize::optimize(&site, &Default::default(), &req).unwrap();
        let rows = result_rows(&r);
        assert_eq!(rows.len(), r.candidates.len());
        for (row, c) in rows.iter().zip(&r.candidates) {
            assert_eq!(row.rank, c.rank);
            assert_eq!(row.metrics, Metrics::from_report(&c.eval.report));
            assert_eq!(row.pareto, r.pareto.contains(&c.rank));
            assert_eq!(row.params, c.params);
        }
        assert!(rows.iter().any(|r| r.pareto));
    }

    #[test]
    fn infeasible_form_names_the_binding_constraint() {
        let site = subdivision::Polygon2d::new(vec![
            glam::DVec2::new(0.0, 0.0),
            glam::DVec2::new(200.0, 0.0),
            glam::DVec2::new(200.0, 150.0),
            glam::DVec2::new(0.0, 150.0),
        ])
        .unwrap();
        let mut f = OptimizeForm { budget: 8, ..OptimizeForm::default() };
        f.constraints[0] = (true, 1e9);
        let err = site_optimize::optimize(&site, &Default::default(), &f.to_request()).unwrap_err();
        assert!(err.contains("binding constraint: min_lot_area"), "{err}");
    }

    #[test]
    fn cells_are_signed_compact_and_never_negative_zero() {
        assert_eq!(fmt_delta(Col::Lots, 12.0), "+12");
        assert_eq!(fmt_delta(Col::Open, -3.04), "−3.0");
        assert_eq!(fmt_delta(Col::Far, 0.004), "±0");
        assert_eq!(fmt_delta(Col::Net, -0.4), "±0");
        assert_eq!(fmt_delta(Col::Net, 412_345.0), "+412k");
        assert_eq!(fmt_delta(Col::Gfa, -12_345.0), "−12.3k");
        assert_eq!(fmt_abs(Col::Gfa, 2_947_725.0), "2.95M");
        assert_eq!(fmt_abs(Col::Gfa, 12_000_000.0), "12.0M");
        assert_eq!(fmt_abs(Col::Open, -0.0), "0", "signed zero");
        assert_eq!(fmt_abs(Col::Open, -0.01), "0");
        assert_eq!(fmt_abs(Col::Score, 16.0), "16", "integer score");
        assert_eq!(fmt_abs(Col::Score, 1.234), "1.23");
        let base = Metrics { lots: 40, net_area: 9000.0, gfa: 12000.0, far: Some(1.2), open_pct: -0.0 };
        let cand = Metrics { lots: 52, net_area: 8500.0, gfa: 12000.0, far: Some(1.35), open_pct: 17.0 };
        let deltas: Vec<String> = Col::ALL[1..].iter().map(|c| cand.delta_cell(&base, *c)).collect();
        assert_eq!(deltas, ["+12", "−500", "±0", "+0.15", "+17.0"]);
        let cells: Vec<String> = Col::ALL[1..].iter().map(|c| base.cell(*c)).collect();
        assert_eq!(cells, ["40", "9000", "12.0k", "1.20", "0"]);
        let no_far = Metrics { far: None, ..base };
        assert_eq!(cand.delta_cell(&no_far, Col::Far), "1.35", "no baseline FAR → absolute");
        assert_eq!(no_far.delta_cell(&cand, Col::Far), "—");
    }

    fn row(open_pct: f64) -> ResultRow {
        ResultRow {
            rank: 1,
            score: 1.0,
            metrics: Metrics { lots: 1, net_area: 1.0, gfa: 1.0, far: None, open_pct },
            pareto: true,
            params: BTreeMap::new(),
            ledger: Ledger::default(),
        }
    }

    #[test]
    fn narrow_table_columns() {
        use Col::*;
        let value = OptimizeForm::default();
        assert_eq!(value.objective, Objective::Value, "value is the default");
        assert_eq!(columns(&value, &[row(0.0)]), [Score, Lots, Net], "profit/m², no GFA");
        let f = OptimizeForm { objective: Objective::Lots, ..value };
        assert_eq!(columns(&f, &[row(0.0)]), [Lots, Net, Gfa], "lots: score = lots column");
        assert_eq!(columns(&f, &[row(0.0), row(4.0)]), [Lots, Net, Gfa, Open], "open varies");
        let mut far = OptimizeForm { objective: Objective::FarNet, ..f.clone() };
        assert_eq!(columns(&far, &[row(0.0)]), [Lots, Net, Gfa, Far]);
        far.constraints[3].0 = true; // min open
        assert_eq!(columns(&far, &[row(0.0)]), [Lots, Net, Gfa, Far, Open]);
        let w = OptimizeForm { objective: Objective::Weighted, weights: [1.0, 0.0, 0.0, 0.0, 1.0], ..f };
        assert_eq!(columns(&w, &[row(0.0)]), [Score, Lots, Net, Gfa, Open]);
    }

    #[test]
    fn ledger_lines_and_unsigned_zero() {
        let l = Ledger {
            revenue: 94_080_000.0,
            roads: 1_234_000.0,
            utilities: 1_036_800.0,
            lots: 93_000.0,
            open_space: 0.0,
            profit: 91_716_200.0,
            value: 115.844,
        };
        let v: Vec<String> = ledger_lines(&l).into_iter().map(|(_, v)| v).collect();
        assert_eq!(v, ["94.1M", "−1.23M", "−1.04M", "−93.0k", "0", "91.7M", "115.84"]);
        assert_eq!(unsigned_zero("-0".into()), "0");
        assert_eq!(unsigned_zero("-0.0".into()), "0.0");
        assert_eq!(unsigned_zero("-0.1".into()), "-0.1");
        assert_eq!(unsigned_zero("12".into()), "12");
    }

    #[test]
    fn form_economics_reach_the_request() {
        let mut f = OptimizeForm::default();
        f.economics.price = 10.0;
        f.economics.lot_cost = 0.0;
        let r = f.to_request();
        assert_eq!(r.objective, Objective::Value);
        assert_eq!(r.economics, Economics { price: 10.0, lot_cost: 0.0, ..Economics::default() });
    }

    #[test]
    fn plan_seeds_min_lot_area() {
        let mut f = OptimizeForm::default();
        let s = subdivision::SubdivisionSettings { lot_area_min: 420.0, ..Default::default() };
        f.seed_from_plan(&s);
        assert_eq!(f.constraints[0], (true, 420.0));
        assert_eq!(f.to_request().min_lot_area, Some(420.0));
    }

    #[test]
    fn segments_per_kind_and_clamp() {
        assert!(segment_keys(SubdivKind::Site).is_empty());
        assert_eq!(segment_keys(SubdivKind::Lots).len(), 2);
        assert_eq!(segment_keys(SubdivKind::SitePlan)[SEG_OPTIMIZE], "param.subdivision.optimize");
        assert_eq!(segment_keys(SubdivKind::Lots)[SEG_YIELD], "param.subdivision.yield");
        // Optimize selected on a site plan, then a Lots link is inspected.
        assert_eq!(clamp_segment(SEG_OPTIMIZE, 2), SEG_YIELD);
        assert_eq!(clamp_segment(SEG_OPTIMIZE, 3), SEG_OPTIMIZE);
        assert_eq!(clamp_segment(0, 2), 0);
        assert_eq!(clamp_segment(5, 0), 0);
    }
}
