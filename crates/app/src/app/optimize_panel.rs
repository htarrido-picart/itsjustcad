// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Site plan optimizer panel (#145, desktop host). An **Optimize** button in
//! the subdivision inspector of a `SitePlan` link opens an inline form
//! (objective, constraints, budget, seed, locks, street patterns). Run hands
//! the boundary, the link's settings and its hand edits to a worker thread
//! that calls [`site_optimize::optimize_with`], so frames keep painting; Cancel
//! drops the receiver (the search finishes and its result is discarded). Each
//! result row's **Apply** runs `siteoptimize apply` with the candidate's
//! params: one logged op, one undo step.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use itsjustcad_commands::site_optimize::{
    self, Objective, OptimizeRequest, OptimizeResult, SEARCHED,
};
use itsjustcad_commands::{subdiv_params, Command};
use itsjustcad_doc::ObjectId;

use super::App;
use crate::i18n::t;

/// The objectives offered, with their label keys.
const OBJECTIVES: &[(Objective, &str)] = &[
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

/// One result-table row.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ResultRow {
    pub rank: usize,
    pub score: f64,
    pub lots: usize,
    /// Σ lot area, m².
    pub net_area: f64,
    pub gfa: f64,
    pub far: Option<f64>,
    /// Open-space ratio, percent.
    pub open_pct: f64,
    pub pareto: bool,
    pub params: BTreeMap<String, String>,
}

pub(super) fn result_rows(r: &OptimizeResult) -> Vec<ResultRow> {
    r.candidates
        .iter()
        .map(|c| {
            let y = &c.eval.report;
            ResultRow {
                rank: c.rank,
                score: c.eval.score,
                lots: y.lot_count,
                net_area: y.total_lot_area,
                gfa: y.total_gfa,
                far: y.far_net,
                open_pct: y.open_space_ratio * 100.0,
                pareto: r.pareto.contains(&c.rank),
                params: c.params.clone(),
            }
        })
        .collect()
}

/// A finished search: rows, evaluated count, feasible count.
type Finished = (Vec<ResultRow>, usize, usize);

/// A search running on a worker thread.
pub(super) struct Job {
    started: Instant,
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
}

impl OptimizePanel {
    fn open_for(&mut self, source: ObjectId) {
        if self.source != Some(source) {
            self.job = None;
            self.result = None;
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
        self.job = Some(Job { started: Instant::now(), rx });
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
        self.job = None;
        self.result = Some((elapsed, got.map(|r| (result_rows(&r), r.evaluated, r.feasible))));
    }
}

impl App {
    /// The Optimize button + inline panel of a `SitePlan` link's inspector.
    pub(super) fn optimize_section(&mut self, ui: &mut egui::Ui, source: ObjectId) {
        let open = self.optimize.source == Some(source);
        ui.add_space(4.0);
        if ui
            .selectable_label(open, t("param.subdivision.optimize"))
            .clicked()
        {
            if open {
                self.optimize.source = None;
                self.optimize.job = None;
            } else {
                self.optimize.open_for(source);
            }
        }
        if self.optimize.source != Some(source) {
            return;
        }
        self.optimize.poll();
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.strong(t("optimize.title"));
            self.optimize_form(ui, source);
            ui.separator();
            self.optimize_run_row(ui, source);
            self.optimize_results(ui, source);
        });
    }

    fn optimize_form(&mut self, ui: &mut egui::Ui, source: ObjectId) {
        let busy = self.optimize.job.is_some();
        let form = &mut self.optimize.form;
        ui.add_enabled_ui(!busy, |ui| {
            egui::Grid::new(("optimize_form", source))
                .num_columns(2)
                .spacing([crate::theme::Spacing::S, 4.0])
                .show(ui, |ui| {
                    ui.label(t("optimize.objective"));
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
                    ui.label(t("optimize.budget"));
                    ui.add(egui::DragValue::new(&mut form.budget).range(1..=5000));
                    ui.end_row();
                    ui.label(t("optimize.seed"));
                    ui.add(egui::DragValue::new(&mut form.seed));
                    ui.end_row();
                });

            egui::CollapsingHeader::new(t("optimize.constraints"))
                .id_salt(("optimize_constraints", source))
                .default_open(true)
                .show(ui, |ui| {
                    egui::Grid::new(("optimize_constraint_grid", source))
                        .num_columns(2)
                        .show(ui, |ui| {
                            for (c, (on, v)) in
                                Constraint::ALL.into_iter().zip(form.constraints.iter_mut())
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

            egui::CollapsingHeader::new(t("optimize.locks"))
                .id_salt(("optimize_locks", source))
                .show(ui, |ui| {
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
                });

            egui::CollapsingHeader::new(t("optimize.patterns"))
                .id_salt(("optimize_patterns", source))
                .show(ui, |ui| {
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

    fn optimize_run_row(&mut self, ui: &mut egui::Ui, source: ObjectId) {
        ui.horizontal(|ui| match &self.optimize.job {
            Some(job) => {
                ui.spinner();
                ui.label(format!(
                    "{} {:.1} s",
                    t("optimize.running"),
                    job.started.elapsed().as_secs_f64()
                ));
                if ui.button(t("optimize.cancel")).clicked() {
                    self.optimize.job = None;
                }
                ui.ctx().request_repaint_after(Duration::from_millis(100));
            }
            None => {
                if ui.button(t("optimize.run")).clicked() {
                    let ctx = ui.ctx().clone();
                    self.optimize.start(&self.session.doc, source, &ctx);
                }
            }
        });
    }

    fn optimize_results(&mut self, ui: &mut egui::Ui, source: ObjectId) {
        let Some((elapsed, result)) = &self.optimize.result else {
            return;
        };
        let (rows, evaluated, feasible) = match result {
            Ok(r) => r,
            Err(e) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
                return;
            }
        };
        ui.weak(format!(
            "{evaluated} {} · {feasible} {} · {:.1} s",
            t("optimize.evaluated"),
            t("optimize.feasible"),
            elapsed.as_secs_f64()
        ));
        let mut apply = None;
        egui::ScrollArea::horizontal()
            .id_salt(("optimize_results_scroll", source))
            .show(ui, |ui| {
                egui::Grid::new(("optimize_results", source))
                    .num_columns(8)
                    .striped(true)
                    .spacing([crate::theme::Spacing::S, 4.0])
                    .show(ui, |ui| {
                        for k in [
                            "optimize.col.rank",
                            "optimize.col.score",
                            "param.subdivision.yield.lots",
                            "optimize.objective.net_area",
                            "param.subdivision.yield.gfa",
                            "optimize.col.far",
                            "optimize.col.open",
                        ] {
                            ui.strong(t(k));
                        }
                        ui.label("");
                        ui.end_row();
                        for row in rows {
                            let rank = format!("{}{}", row.rank, if row.pareto { " ◆" } else { "" });
                            ui.monospace(rank).on_hover_text(params_text(&row.params));
                            ui.monospace(format!("{:.2}", row.score));
                            ui.monospace(row.lots.to_string());
                            ui.monospace(format!("{:.0}", row.net_area));
                            ui.monospace(format!("{:.0}", row.gfa));
                            ui.monospace(row.far.map_or("—".into(), |f| format!("{f:.2}")));
                            ui.monospace(format!("{:.1}", row.open_pct));
                            if ui.button(t("optimize.apply")).clicked() {
                                apply = Some(row.clone());
                            }
                            ui.end_row();
                        }
                    });
            });
        ui.add_space(4.0);
        ui.weak(t("optimize.pareto"));
        pareto_plot(ui, rows);

        if let Some(row) = apply {
            let out = self.session.run(Command::SiteOptimizeApply {
                rank: Some(row.rank),
                source: Some(source),
                params: Some(row.params),
                ids: None,
            });
            match out {
                Ok(o) => self.command_line.push_line(o.message),
                Err(e) => self.command_line.push_line(e.to_string()),
            }
        }
    }
}

fn params_text(params: &BTreeMap<String, String>) -> String {
    SEARCHED
        .iter()
        .filter_map(|k| params.get(*k).map(|v| format!("{k} = {v}")))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Tiny scatter of score (y) vs open-space % (x); the Pareto front is drawn in
/// the selection color and joined.
fn pareto_plot(ui: &mut egui::Ui, rows: &[ResultRow]) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width().min(260.0), 90.0),
        egui::Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    let weak = ui.visuals().weak_text_color();
    let accent = ui.visuals().selection.stroke.color;
    painter.rect_stroke(rect, 2.0, egui::Stroke::new(1.0, weak), egui::StrokeKind::Inside);
    if rows.is_empty() {
        return;
    }
    let span = |f: fn(&ResultRow) -> f64| {
        let (lo, hi) = rows
            .iter()
            .map(f)
            .fold((f64::MAX, f64::MIN), |(lo, hi), v| (lo.min(v), hi.max(v)));
        (lo, (hi - lo).max(1e-9))
    };
    let ((x0, dx), (y0, dy)) = (span(|r| r.open_pct), span(|r| r.score));
    let inner = rect.shrink(8.0);
    let at = |r: &ResultRow| {
        egui::pos2(
            inner.left() + ((r.open_pct - x0) / dx) as f32 * inner.width(),
            inner.bottom() - ((r.score - y0) / dy) as f32 * inner.height(),
        )
    };
    let mut front: Vec<&ResultRow> = rows.iter().filter(|r| r.pareto).collect();
    front.sort_by(|a, b| a.open_pct.total_cmp(&b.open_pct));
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
            assert_eq!((row.rank, row.lots), (c.rank, c.eval.report.lot_count));
            assert_eq!(row.open_pct, c.eval.report.open_space_ratio * 100.0);
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
}
