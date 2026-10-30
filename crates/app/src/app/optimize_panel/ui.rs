// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! The panel's egui drawing.

use super::*;

/// The `climate` line that sets `path` as the document's climate file
/// (`None` → `climate off`), exactly as the user would type it. The parser
/// takes the rest of the line as the path (args joined by single spaces), so
/// a path whose whitespace would not survive that is refused (`None`) rather
/// than silently mangled.
pub(crate) fn climate_line(path: Option<&std::path::Path>) -> Option<String> {
    let Some(path) = path else {
        return Some("climate off".into());
    };
    let s = path.to_str()?;
    let line = format!("climate {s}");
    match itsjustcad_commands::parse(&line) {
        Ok(itsjustcad_commands::Command::Climate { path: Some(p), .. }) if p == s => Some(line),
        _ => None,
    }
}

/// Ask for an EPW file (`None` when cancelled).
fn pick_epw() -> Option<std::path::PathBuf> {
    rfd::FileDialog::new()
        .add_filter("EnergyPlus weather (EPW)", &["epw", "EPW"])
        .pick_file()
}

/// A climate as the panel names it: `city (file)`.
fn climate_name(c: &itsjustcad_doc::Climate) -> String {
    format!("{} ({})", c.city, c.source)
}

/// Where daylight's sun hours came from, and the overheating month.
pub(in crate::app) fn daylight_source_text(climate: Option<&str>, overheat_month: Option<u32>) -> String {
    let mut s = match climate {
        Some(c) => format!("{} {c}", t("optimize.daylight.source_climate")),
        None => t("optimize.daylight.source_clear").to_string(),
    };
    if let Some(m) = overheat_month {
        s.push_str(&format!(" · {} {m}", t("optimize.daylight.overheat_month")));
    }
    s
}

/// The document's location as the optimizer takes it.
fn location(l: itsjustcad_doc::GeoLocation) -> site_optimize::Location {
    site_optimize::Location { lat_deg: l.lat_deg, lon_deg: l.lon_deg, tz_hours: l.tz_hours }
}

/// Grid spacing of the form and results: comfortable rows.
const GRID_SPACING: egui::Vec2 = egui::vec2(crate::theme::Spacing::S, 6.0);

impl App {
    /// Show `source`'s Optimize: select its boundary, open the Parameters
    /// tab on the Optimize segment.
    pub(in crate::app) fn open_optimize(&mut self, source: ObjectId) {
        if self.session.doc.get(source).is_none() {
            return;
        }
        self.session.doc.selection = [source].into();
        self.parametric_selected = None;
        self.focus_site_override = None;
        self.subdiv_segment = SEG_OPTIMIZE;
        self.panel_tabs.show(crate::tabstrip::PanelTab::Parameters);
    }

    /// The finish notice (#201), bottom right above the status bar, for
    /// [`NOTICE_FOR`]. Clicking it opens the site's Optimize; it never
    /// applies a plan.
    pub(in crate::app) fn optimize_notice(&mut self, ctx: &egui::Context) {
        let Some(n) = &self.optimize.notice else { return };
        let left = NOTICE_FOR.saturating_sub(n.at.elapsed());
        if left.is_zero() {
            self.optimize.notice = None;
            return;
        }
        ctx.request_repaint_after(left);
        let (source, text) = (n.source, n.text.clone());
        let pad = crate::theme::Spacing::M;
        let (mut open, mut close) = (false, false);
        egui::Area::new(egui::Id::new("optimize_notice"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-pad, -(pad + 28.0)))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(egui::Margin::symmetric(pad as i8, crate::theme::Spacing::S as i8))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = crate::theme::Spacing::M;
                            open = ui
                                .add(egui::Label::new(egui::RichText::new(&text).strong()).sense(egui::Sense::click()))
                                .on_hover_text(t("optimize.status_tip"))
                                .clicked();
                            close = ui.add(egui::Button::new("×").frame(false)).clicked();
                        });
                    });
            });
        if open {
            self.open_optimize(source);
        }
        if open || close {
            self.optimize.notice = None;
        }
    }

    /// Dev hook `ITSJUSTCAD_OPTIMIZE` (screenshots): `run` starts a long
    /// search of the first site plan and leaves its panel (the status-bar
    /// indicator shows); `done` runs a short one to completion (the notice).
    pub(in crate::app) fn optimize_dev_hook(&mut self, ctx: &egui::Context) {
        let Ok(mode) = std::env::var("ITSJUSTCAD_OPTIMIZE") else { return };
        let Some(source) = self
            .session
            .doc
            .subdivision_links
            .iter()
            .find(|(_, l)| l.kind == SubdivKind::SitePlan)
            .map(|(id, _)| *id)
        else {
            return;
        };
        let settings = self.session.doc.subdivision_links[&source].settings.clone();
        self.optimize.open_for(source, &settings, None);
        if mode == "done" {
            self.optimize.form.budget = 40;
            self.optimize.run_now(&mut self.session, source);
        } else {
            self.optimize.form.budget = 5000;
            self.optimize.start(&self.session.doc, source, ctx);
            self.session.doc.selection.clear();
        }
    }

    /// The `Optimize` segment of a `SitePlan` link's inspector.
    pub(in crate::app) fn optimize_panel(
        &mut self,
        ui: &mut egui::Ui,
        source: ObjectId,
        link: &itsjustcad_doc::SubdivLink,
    ) {
        let saved = self.session.optimize_run(source);
        self.optimize.open_for(source, &link.settings, saved);
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
                self.optimize_daylight(ui, source);
                ui.add_space(crate::theme::Spacing::SM);
                self.optimize_run_row(ui, source);
                ui.add_space(crate::theme::Spacing::S);
                self.optimize_results(ui, source, base, &link.settings);
                ui.add_space(crate::theme::Spacing::S);
                self.optimize_advanced(ui, source);
            });
    }

    /// Objective (+ weights) and the constraint list.
    fn optimize_form(&mut self, ui: &mut egui::Ui, source: ObjectId) {
        let busy = self.optimize.running_source() == Some(source);
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
                    // The unit-size rule turning floor area into dwellings.
                    ui.label(t("optimize.dwelling_area"));
                    ui.add(
                        egui::DragValue::new(&mut form.dwelling_area)
                            .speed(1.0)
                            .range(20.0..=1000.0)
                            .suffix(" m²"),
                    );
                    ui.end_row();
                    ui.label(t("optimize.dwelling_efficiency"));
                    let mut pct = form.dwelling_efficiency * 100.0;
                    if ui
                        .add(
                            egui::DragValue::new(&mut pct)
                                .speed(0.5)
                                .range(30.0..=100.0)
                                .suffix(" %"),
                        )
                        .changed()
                    {
                        form.dwelling_efficiency = pct / 100.0;
                    }
                    ui.end_row();
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
        match self.optimize.job.as_ref().filter(|j| j.source == source) {
            Some(job) => {
                let p = job.progress();
                let text = progress_text(&p, job.started.elapsed().as_secs_f64());
                let mut cancel = false;
                ui.horizontal(|ui| {
                    // Cancel on the right; the bar takes the rest of the row.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let h = crate::theme::Spacing::HIT_TARGET;
                        cancel = crate::widgets::fit_button(ui, h, t("optimize.cancel")).clicked();
                        ui.add(
                            egui::ProgressBar::new(p.fraction() as f32)
                                .desired_height(crate::theme::Spacing::HIT_TARGET)
                                .show_percentage(),
                        );
                    });
                });
                ui.add_space(crate::theme::Spacing::XS);
                ui.weak(text);
                if cancel {
                    self.optimize.cancel();
                }
                ui.ctx().request_repaint_after(Duration::from_millis(100));
            }
            None => {
                let fill = ui.visuals().selection.bg_fill;
                let text = egui::RichText::new(t("optimize.run"))
                    .strong()
                    .color(ui.visuals().strong_text_color());
                let run = egui::Button::new(text).fill(fill);
                let other = self.optimize.running_source().is_some();
                let mut start = false;
                if crate::widgets::full_width_button(ui, crate::theme::Spacing::HIT_TARGET, run).clicked() {
                    // One search per document: another site's search runs,
                    // so ask before stopping it.
                    if other {
                        self.optimize.confirm_stop = Some(source);
                    } else {
                        start = true;
                    }
                }
                if other && self.optimize.confirm_stop == Some(source) {
                    ui.add_space(crate::theme::Spacing::S);
                    ui.label(t("optimize.stop_confirm"));
                    ui.add_space(crate::theme::Spacing::XS);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = crate::theme::Spacing::S;
                        let h = crate::theme::Spacing::HIT_TARGET;
                        if crate::widgets::fit_button(ui, h, t("optimize.stop_and_run")).clicked() {
                            start = true;
                        }
                        if crate::widgets::fit_button(ui, h, t("optimize.keep_running")).clicked() {
                            self.optimize.confirm_stop = None;
                        }
                    });
                } else if other {
                    ui.add_space(crate::theme::Spacing::XS);
                    ui.weak(t("optimize.other_running"));
                }
                if start {
                    let ctx = ui.ctx().clone();
                    self.optimize.start(&self.session.doc, source, &ctx);
                } else if self.optimize.cancelled == Some(source) {
                    ui.add_space(crate::theme::Spacing::XS);
                    ui.weak(t("optimize.cancelled"));
                }
            }
        }
    }

    /// Results table (deltas vs `base`, the current plan), then the scatter.
    /// Apply comes first so a narrow panel never clips it.
    fn optimize_results(
        &mut self,
        ui: &mut egui::Ui,
        source: ObjectId,
        base: Option<Metrics>,
        settings: &subdivision::SubdivisionSettings,
    ) {
        if let Some((s, e)) = &self.optimize.error
            && *s == source
        {
            ui.colored_label(ui.visuals().error_fg_color, e);
            ui.add_space(crate::theme::Spacing::S);
        }
        let saved = self.session.optimize_run(source);
        let Some((elapsed, stopped, stale)) = saved.map(|r| {
            (
                r.elapsed_s,
                r.result.stopped_early,
                optimize_runs::is_stale(&self.session.doc, source, r),
            )
        }) else {
            return;
        };
        let Some(f) = self.optimize.shown(source, saved, settings) else {
            return;
        };
        ui.weak(format!(
            "{} {} · {} {} · {:.1} s",
            f.evaluated,
            t("optimize.evaluated"),
            f.feasible,
            t("optimize.feasible"),
            elapsed
        ));
        // A stopped run and a changed plan say so; Apply still works.
        if stopped {
            ui.add_space(crate::theme::Spacing::XS);
            ui.weak(t("optimize.stopped_early"));
        }
        if stale {
            ui.add_space(crate::theme::Spacing::XS);
            ui.colored_label(ui.visuals().warn_fg_color, t("optimize.stale"));
        }
        ui.add_space(crate::theme::Spacing::XS);
        // Deltas are against the current plan as the search scored it; the
        // document's yield stands in when it could not be planned.
        let base = f.current.as_ref().map(|c| c.metrics).or(base);
        let hover = |row: &ResultRow| {
            format!(
                "{} {}\n\n{}\n\n{}\n{} {}\n\n{}",
                t(f.score_key),
                fmt_abs(Col::Score, row.score),
                ledger_text(&row.ledger),
                row.metrics.summary(),
                t("optimize.yield.floors"),
                floors_text(&row.floors),
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
                            let head = ui.strong(t(key));
                            // Short headers keep the table in the panel; the
                            // hover names the metric in full.
                            let full = match c {
                                Col::Park => Some("optimize.yield.park"),
                                Col::Daylight => Some("optimize.yield.daylight"),
                                Col::Overheat => Some("optimize.yield.overheat"),
                                Col::Dwellings => Some("optimize.yield.dwellings"),
                                Col::Density => Some("optimize.yield.density"),
                                _ => None,
                            };
                            if let Some(k) = full {
                                head.on_hover_text(t(k));
                            }
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
                            // A labelled button, not a bare check mark: the
                            // grid sizes the column to the (translated) text.
                            // `small_button` keeps the theme's tight side
                            // padding; give the label room to breathe.
                            let pad_x = crate::theme::Spacing::S + crate::theme::Spacing::XS;
                            let applied = ui
                                .scope(|ui| {
                                    ui.spacing_mut().button_padding.x = pad_x;
                                    ui.small_button(t("optimize.apply"))
                                })
                                .inner
                                .clicked();
                            if applied {
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
        if let Some(d) = &f.daylight {
            ui.add_space(crate::theme::Spacing::XS);
            ui.weak(daylight_source_text(d.climate.as_deref(), d.overheat_month));
        }

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
    /// id follows the objective, so switching to `value` opens it). Develop
    /// (the default) prices built floor area; otherwise serviced lots.
    fn optimize_economics(&mut self, ui: &mut egui::Ui, source: ObjectId) {
        let busy = self.optimize.running_source() == Some(source);
        let no_location = self.session.doc.location.is_none();
        let form = &mut self.optimize.form;
        let daylight_on = form.constraints[11].0 || form.constraints[12].0;
        if daylight_on && no_location {
            ui.colored_label(ui.visuals().warn_fg_color, t("optimize.daylight_needs_location"));
        }
        let value = form.objective == Objective::Value;
        egui::CollapsingHeader::new(t("optimize.economics"))
            .id_salt(("optimize_economics", source, value))
            .default_open(value)
            .show(ui, |ui| {
                ui.add_enabled_ui(!busy, |ui| {
                    let e = &mut form.economics;
                    ui.checkbox(&mut e.develop, t("optimize.econ.develop"));
                    egui::Grid::new(("optimize_economics_grid", source))
                        .num_columns(2)
                        .spacing(GRID_SPACING)
                        .show(ui, |ui| {
                            let price_key = if e.develop {
                                "optimize.econ.site_price"
                            } else {
                                "optimize.econ.price"
                            };
                            let mut rows: Vec<(&str, &mut f64, f64, &str)> =
                                vec![(price_key, &mut e.price, 1.0, "")];
                            if e.develop {
                                rows.push(("optimize.econ.sale_price", &mut e.sale_price, 10.0, ""));
                                rows.push(("optimize.econ.build_cost", &mut e.build_cost, 10.0, ""));
                            }
                            rows.extend([
                                ("optimize.econ.road_cost", &mut e.road_cost, 1.0, ""),
                                ("optimize.econ.utility_cost", &mut e.utility_cost, 10.0, ""),
                                ("optimize.econ.lot_cost", &mut e.lot_cost, 100.0, ""),
                                ("optimize.econ.open_cost", &mut e.open_cost, 1.0, ""),
                                ("optimize.econ.park_distance", &mut e.park_distance, 10.0, " m"),
                            ]);
                            for (key, v, speed, suffix) in rows {
                                ui.label(t(key));
                                ui.add(
                                    egui::DragValue::new(v)
                                        .speed(speed)
                                        .range(0.0..=f64::MAX)
                                        .suffix(suffix),
                                );
                                ui.end_row();
                            }
                            // Percentages, edited as percent.
                            let pct = |ui: &mut egui::Ui, key: &str, v: &mut f64| {
                                let mut p = *v * 100.0;
                                ui.label(t(key));
                                if ui
                                    .add(egui::DragValue::new(&mut p).speed(0.5).range(0.0..=100.0).suffix(" %"))
                                    .changed()
                                {
                                    *v = p / 100.0;
                                }
                                ui.end_row();
                            };
                            pct(ui, "optimize.econ.park_premium", &mut e.park_premium);
                            if e.develop {
                                pct(ui, "optimize.econ.height_cost", &mut e.height_cost);
                            } else {
                                ui.label(t("optimize.econ.size_exponent"))
                                    .on_hover_text(t("optimize.econ.size_exponent.hint"));
                                ui.add(
                                    egui::DragValue::new(&mut e.size_exponent)
                                        .speed(0.01)
                                        .range(0.5..=1.0)
                                        .fixed_decimals(2),
                                );
                                ui.end_row();
                            }
                        });
                });
            });
    }

    /// Facade daylight settings: the period (annual by default, the site's
    /// worst month, or a date — never a hemisphere assumption), the daily sun
    /// hours a facade needs, and the overheating threshold. Open when a
    /// daylight constraint is on.
    fn optimize_daylight(&mut self, ui: &mut egui::Ui, source: ObjectId) {
        let busy = self.optimize.running_source() == Some(source);
        let climate = self.session.doc.climate.as_ref().map(climate_name);
        let no_dialog = self.headless_no_dialog();
        // Set (`Some(Some(path))`) or remove (`Some(None)`) the climate file,
        // once the form's borrow ends.
        let mut climate_change: Option<Option<std::path::PathBuf>> = None;
        let form = &mut self.optimize.form;
        let on = form.constraints[11].0 || form.constraints[12].0;
        egui::CollapsingHeader::new(t("optimize.daylight"))
            .id_salt(("optimize_daylight", source, on))
            .default_open(on)
            .show(ui, |ui| {
                ui.add_enabled_ui(!busy, |ui| {
                    // The sun hours' source: the site's climate file, or
                    // clear-sky geometry.
                    match &climate {
                        Some(name) => {
                            ui.checkbox(
                                &mut form.use_climate,
                                format!("{} {name}", t("optimize.daylight.use_climate")),
                            )
                            .on_hover_text(t("optimize.daylight.climate_hint"));
                        }
                        None => {
                            ui.weak(t("optimize.daylight.no_climate"));
                        }
                    }
                    ui.add_space(crate::theme::Spacing::XS);
                    ui.horizontal(|ui| {
                        // Buttons keep side padding around their labels.
                        ui.spacing_mut().button_padding.x =
                            crate::theme::Spacing::S + crate::theme::Spacing::XS;
                        let pick = if climate.is_some() {
                            t("optimize.daylight.change_epw")
                        } else {
                            t("optimize.daylight.choose_epw")
                        };
                        if ui
                            .button(pick)
                            .on_hover_text(t("optimize.daylight.epw_hint"))
                            .clicked()
                            && !no_dialog
                            && let Some(p) = pick_epw()
                        {
                            climate_change = Some(Some(p));
                        }
                        if climate.is_some()
                            && ui.button(t("optimize.daylight.remove_epw")).clicked()
                        {
                            climate_change = Some(None);
                        }
                    });
                    ui.add_space(crate::theme::Spacing::XS);
                    egui::Grid::new(("optimize_daylight_grid", source))
                        .num_columns(2)
                        .spacing(GRID_SPACING)
                        .show(ui, |ui| {
                            ui.label(t("optimize.daylight.period"));
                            let custom = DaylightPeriod::Custom { month: 3, day: 21 };
                            let label = |p: &DaylightPeriod| match p {
                                DaylightPeriod::Annual => t("optimize.daylight.annual"),
                                DaylightPeriod::WorstMonth => t("optimize.daylight.worst"),
                                DaylightPeriod::Custom { .. } => t("optimize.daylight.custom"),
                            };
                            egui::ComboBox::from_id_salt(("optimize_daylight_period", source))
                                .selected_text(label(&form.daylight_period))
                                .show_ui(ui, |ui| {
                                    for p in [DaylightPeriod::Annual, DaylightPeriod::WorstMonth, custom] {
                                        let sel = std::mem::discriminant(&form.daylight_period)
                                            == std::mem::discriminant(&p);
                                        if ui.selectable_label(sel, label(&p)).clicked() && !sel {
                                            form.daylight_period = p;
                                        }
                                    }
                                });
                            ui.end_row();
                            if let DaylightPeriod::Custom { month, day } = &mut form.daylight_period {
                                ui.label(t("optimize.daylight.date"));
                                ui.horizontal(|ui| {
                                    ui.add(egui::DragValue::new(month).range(1..=12).prefix("M "));
                                    ui.add(egui::DragValue::new(day).range(1..=28).prefix("D "));
                                });
                                ui.end_row();
                            }
                            ui.label(t("optimize.daylight.hours"));
                            ui.add(egui::DragValue::new(&mut form.daylight_hours).speed(0.1).range(0.5..=12.0).suffix(" h"));
                            ui.end_row();
                            ui.label(t("optimize.daylight.overheat_hours"));
                            ui.add(egui::DragValue::new(&mut form.overheat_hours).speed(0.1).range(1.0..=14.0).suffix(" h"));
                            ui.end_row();
                        });
                });
            });
        if let Some(path) = climate_change {
            self.set_climate_file(path.as_deref());
        }
    }

    /// The Yield segment's `Economics` block for a site plan: the current
    /// plan's ledger at the Optimize form's unit prices, its storeys, park
    /// access and (with a location) winter daylight. Measured once per
    /// document generation.
    pub(in crate::app) fn yield_economics(&mut self, ui: &mut egui::Ui, source: ObjectId) {
        let generation = self.session.doc.generation;
        // The form's daylight settings; overheating always measured here.
        let form = &self.optimize.form;
        let opts = site_optimize::DaylightOpts {
            overheat_hours: Some(form.overheat_hours),
            climate: self
                .session
                .doc
                .climate
                .clone()
                .filter(|_| form.use_climate)
                .map(Arc::new),
            ..form.to_request().daylight_opts()
        };
        let fresh = matches!(&self.optimize.current_q, Some((id, g, o, _)) if *id == source && *g == generation && *o == opts);
        if !fresh {
            let doc = &self.session.doc;
            let q = site_optimize::site_boundary(doc, &[source]).and_then(|(_, site)| {
                let (base, edits) = site_optimize::base_plan(doc, source);
                let mut q = site_optimize::evaluate(&site, &base, &edits, &OptimizeRequest::default())
                    .ok()?
                    .quantities;
                let d = doc
                    .location
                    .map(location)
                    .and_then(|loc| site_optimize::plan_daylight(&site, &base, &edits, &loc, &opts));
                site_optimize::set_daylight(&mut q, d);
                Some(q)
            });
            self.optimize.current_q = Some((source, generation, opts, q));
        }
        let Some((_, _, _, Some(q))) = &self.optimize.current_q else {
            return;
        };
        let lot_area_min = self
            .session
            .doc
            .subdivision_links
            .get(&source)
            .map_or(0.0, |l| l.settings.lot_area_min);
        let ledger = self.optimize.form.economics.resolved(lot_area_min).ledger(q);
        let mut lines: Vec<(&str, String, &str)> =
            ledger_lines(&ledger).into_iter().map(|(k, v)| (k, v, "")).collect();
        if let Some(last) = lines.last_mut() {
            last.2 = "/m²";
        }
        lines.push(("optimize.yield.park", fmt_abs(Col::Park, q.park_access() * 100.0), "%"));
        if let Some(d) = q.daylight {
            lines.push(("optimize.yield.daylight", fmt_abs(Col::Daylight, d * 100.0), "%"));
        }
        if let Some(d) = q.daylight_floors {
            lines.push(("optimize.yield.daylight_floors", fmt_abs(Col::Daylight, d * 100.0), "%"));
        }
        if let Some(d) = q.overheat {
            lines.push(("optimize.yield.overheat", fmt_abs(Col::Overheat, d * 100.0), "%"));
        }
        let floors = q.floors_sorted();
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
                        for (key, v, unit) in lines {
                            ui.label(t(key));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.monospace(v);
                            });
                            ui.monospace(unit);
                            ui.end_row();
                        }
                    });
                ui.add_space(crate::theme::Spacing::XS);
                ui.label(format!("{} {}", t("optimize.yield.floors"), floors_text(&floors)));
                if q.daylight_source.is_some() {
                    let climate = (q.daylight_source
                        == Some(site_optimize::DaylightSource::Climate))
                    .then(|| self.session.doc.climate.as_ref().map(climate_name))
                    .flatten();
                    ui.weak(daylight_source_text(climate.as_deref(), None));
                }
            });
    }

    /// Collapsed-by-default budget, seed, locked parameters, street patterns.
    fn optimize_advanced(&mut self, ui: &mut egui::Ui, source: ObjectId) {
        let busy = self.optimize.running_source() == Some(source);
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
    // Room above and to the right for the rank labels.
    let inner = egui::Rect::from_min_max(
        rect.min + egui::vec2(10.0, 18.0),
        rect.max - egui::vec2(18.0, 10.0),
    );
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
