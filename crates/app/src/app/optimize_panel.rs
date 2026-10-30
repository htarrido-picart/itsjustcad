// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Site plan optimizer panel (#145, desktop host): the **Optimize** segment of
//! a `SitePlan` link's subdivision inspector (`Controls | Yield | Optimize`).
//! Objective, constraints and the `value` objective's **Economics** (unit
//! prices) on top, a full-width **Run**, then the results as deltas vs the
//! current plan (scored like a candidate), the Pareto scatter, and an **Advanced**
//! section (budget, seed, locks, street patterns). Run hands the boundary, the
//! link's settings and its hand edits to a worker thread that runs
//! [`site_optimize::Prepared::run`], so frames keep painting; a progress bar
//! follows the search and Cancel returns the panel to Run at once (the worker
//! stops before its next plan, its result dropped). Each result
//! row's **Apply** runs `siteoptimize apply` with the candidate's params (one
//! logged op, one undo step), then the inspector switches to `Yield`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use itsjustcad_commands::site_optimize::{
    self, DaylightPeriod, Economics, Ledger, Objective, OptimizeRequest, OptimizeResult, Quantities,
    SEARCHED,
};
use itsjustcad_commands::optimize_runs::{self, SavedRun};
use itsjustcad_commands::{subdiv_params, Command};
use itsjustcad_doc::{ObjectId, SubdivKind};

use super::App;
use crate::i18n::t;

mod form;
mod job;
mod results;
mod ui;

pub(super) use form::*;
pub(super) use job::*;
pub(super) use results::*;
pub(super) use ui::*;

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
            weights: [1.0, 0.0, 0.0, 0.5, 2.0, 0.0],
            budget: 40,
            seed: 7,
            ..OptimizeForm::default()
        };
        f.constraints[0] = (true, 350.0); // min lot area
        f.constraints[3] = (true, 15.0); // min open, percent
        f.constraints[5] = (false, 50.0); // max coverage, off
        f.locks.extend(["typology".to_string(), "seed".to_string()]);
        f.patterns = ["radial", "orthogonal"].map(String::from).into();
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
            weights: [1.0, 1.0, 1.0, 1.0, 1.0, 1.0],
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
            assert_eq!(row.metrics, Metrics::from_eval(&c.eval));
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
        assert_eq!(fmt_money(2_311_900_000.0), "2.31B");
        assert_eq!(fmt_abs(Col::Open, -0.0), "0", "signed zero");
        assert_eq!(fmt_abs(Col::Open, -0.01), "0");
        assert_eq!(fmt_abs(Col::Score, 16.0), "16", "integer score");
        assert_eq!(fmt_abs(Col::Score, 1.234), "1.23");
        let none = Metrics::from_report(&subdivision::YieldReport::compute(&Default::default()));
        let base = Metrics { lots: 40, net_area: 9000.0, gfa: 12000.0, far: Some(1.2), open_pct: -0.0, ..none };
        let cand = Metrics { lots: 52, net_area: 8500.0, gfa: 12000.0, far: Some(1.35), open_pct: 17.0, ..none };
        let deltas: Vec<String> = Col::ALL[1..6].iter().map(|c| cand.delta_cell(&base, *c)).collect();
        assert_eq!(deltas, ["+12", "−500", "±0", "+0.15", "+17.0"]);
        let cells: Vec<String> = Col::ALL[1..6].iter().map(|c| base.cell(*c)).collect();
        let tall = Metrics { floors: Some(4.0), park_pct: Some(98.0), ..cand };
        let low = Metrics { floors: Some(2.0), park_pct: Some(90.0), ..base };
        assert_eq!(tall.delta_cell(&low, Col::Floors), "4", "storeys are absolute");
        assert_eq!(tall.delta_cell(&low, Col::Park), "+8");
        assert_eq!(cells, ["40", "9000", "12.0k", "1.20", "0"]);
        let no_far = Metrics { far: None, ..base };
        assert_eq!(cand.delta_cell(&no_far, Col::Far), "1.35", "no baseline FAR → absolute");
        assert_eq!(no_far.delta_cell(&cand, Col::Far), "—");
    }

    fn row(open_pct: f64) -> ResultRow {
        ResultRow {
            rank: 1,
            score: 1.0,
            metrics: Metrics {
                lots: 1,
                net_area: 1.0,
                gfa: 1.0,
                far: None,
                open_pct,
                floors: None,
                park_pct: None,
                daylight_pct: None,
                overheat_pct: None,
                dwellings: Some(4.0),
                density: Some(40.0),
            },
            pareto: true,
            params: BTreeMap::new(),
            ledger: Ledger::default(),
            floors: Vec::new(),
        }
    }

    #[test]
    fn narrow_table_columns() {
        use Col::*;
        let value = OptimizeForm::default();
        assert_eq!(value.objective, Objective::Value, "value is the default");
        assert_eq!(
            columns(&value, &[row(0.0)]),
            [Score, Lots, Dwellings, Floors, Park],
            "profit/m², dwellings, storeys, parks"
        );
        let mut sunny = value.clone();
        sunny.constraints[11].0 = true;
        assert_eq!(
            columns(&sunny, &[row(0.0)]),
            [Score, Dwellings, Floors, Park, Daylight]
        );
        sunny.constraints[12].0 = true;
        assert_eq!(
            columns(&sunny, &[row(0.0)]),
            [Score, Dwellings, Floors, Daylight, Overheat],
            "five fit"
        );
        let mut f = OptimizeForm {
            objective: Objective::Lots,
            ..value
        };
        f.constraints[3].0 = false; // min open (on by default)
        assert_eq!(
            columns(&f, &[row(0.0)]),
            [Lots, Dwellings, Net, Gfa],
            "lots: score = lots column"
        );
        assert_eq!(
            columns(&f, &[row(0.0), row(4.0)]),
            [Lots, Dwellings, Net, Gfa, Open],
            "open varies"
        );
        let mut far = OptimizeForm {
            objective: Objective::FarNet,
            ..f.clone()
        };
        assert_eq!(columns(&far, &[row(0.0)]), [Lots, Dwellings, Net, Gfa, Far]);
        far.constraints[3].0 = true; // min open
        assert_eq!(
            columns(&far, &[row(0.0)]),
            [Lots, Dwellings, Net, Gfa, Far, Open]
        );
        let w = OptimizeForm {
            objective: Objective::Weighted,
            weights: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0],
            ..f.clone()
        };
        assert_eq!(
            columns(&w, &[row(0.0)]),
            [Score, Lots, Dwellings, Net, Gfa, Open]
        );
        let mut dense = OptimizeForm {
            objective: Objective::Dwellings,
            ..f
        };
        dense.constraints[14].0 = true; // min density
        assert_eq!(
            columns(&dense, &[row(0.0)]),
            [Lots, Dwellings, Density, Net, Gfa]
        );
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
            ..Ledger::default()
        };
        let v: Vec<String> = ledger_lines(&l).into_iter().map(|(_, v)| v).collect();
        assert_eq!(v, ["94.1M", "−1.23M", "−1.04M", "−93.0k", "0", "91.7M", "115.84"]);
        assert_eq!(unsigned_zero("-0".into()), "0");
        assert_eq!(unsigned_zero("-0.0".into()), "0.0");
        assert_eq!(unsigned_zero("-0.1".into()), "-0.1");
        assert_eq!(unsigned_zero("12".into()), "12");
    }

    #[test]
    fn floors_text_summarises_the_skyline() {
        assert_eq!(floors_text(&[1, 2, 2, 4, 6]), "1 / 2 / 6 · 1:1 2:2 4:1 6:1");
        assert_eq!(floors_text(&[]), "—");
        let built = Ledger { revenue: 10.0, land: 2.0, construction: 5.0, ..Ledger::default() };
        let keys: Vec<&str> = ledger_lines(&built).into_iter().map(|(k, _)| k).collect();
        assert_eq!(&keys[..3], ["optimize.econ.revenue", "optimize.econ.land", "optimize.econ.construction"]);
    }

    /// Green space, storeys and walkable patterns are on out of the box;
    /// unchecking lifts them; daylight is opt-in.
    #[test]
    fn walkable_form_defaults_and_toggles() {
        let mut f = OptimizeForm::default();
        let r = f.to_request();
        assert_eq!((r.min_open, r.max_stories, r.min_park_access), (Some(0.10), Some(6), Some(0.9)));
        assert_eq!(r.min_daylight, None);
        assert_eq!(r.patterns, ["orthogonal", "skewed", "hexagonal", "voronoi"]);
        for i in [3, 9, 10] {
            f.constraints[i].0 = false;
        }
        f.constraints[11] = (true, 40.0);
        f.constraints[12] = (true, 25.0);
        f.daylight_period = DaylightPeriod::WorstMonth;
        let r = f.to_request();
        assert_eq!((r.min_open, r.max_stories, r.min_park_access), (None, None, None));
        assert_eq!((r.min_daylight, r.max_overheat), (Some(0.4), Some(0.25)));
        assert_eq!(r.daylight_period, DaylightPeriod::WorstMonth);
        assert_eq!(r.daylight_opts().overheat_hours, Some(6.0));
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
        assert_eq!(f.constraints[0], (true, 210.0));
        assert_eq!(f.constraints[6], (true, 1260.0));
        assert_eq!(f.constraints[1], (true, s.lot_width_min));
        let r = f.to_request();
        assert_eq!((r.min_lot_area, r.max_lot_area), (Some(210.0), Some(1260.0)));
        assert_eq!((r.max_depth_ratio, r.max_block_perimeter), (Some(3.0), Some(800.0)));
        f.constraints[7].0 = false;
        f.constraints[8].0 = false;
        let r = f.to_request();
        assert_eq!((r.max_depth_ratio, r.max_block_perimeter), (None, None), "unchecked lifts them");
    }

    /// #170: dwelling constraints and the unit-size rule reach the request;
    /// an unchanged rule is the plan's and is not sent.
    #[test]
    fn dwelling_constraints_and_rule_reach_the_request() {
        let mut f = OptimizeForm::default();
        let s = subdivision::SubdivisionSettings {
            dwelling_area: 95.0,
            ..Default::default()
        };
        f.seed_from_plan(&s);
        assert_eq!(f.dwelling_area, 95.0);
        let r = f.to_request();
        assert_eq!(
            (
                r.dwelling_area,
                r.dwelling_efficiency,
                r.min_dwellings,
                r.min_density
            ),
            (None, None, None, None)
        );
        f.constraints[13] = (true, 120.0);
        f.constraints[14] = (true, 80.0);
        f.dwelling_efficiency = 0.7;
        f.objective = Objective::Dwellings;
        let r = f.to_request();
        assert_eq!((r.min_dwellings, r.min_density), (Some(120.0), Some(80.0)));
        assert_eq!(
            (r.dwelling_area, r.dwelling_efficiency),
            (Some(95.0), Some(0.7))
        );
        assert_eq!(r.objective, Objective::Dwellings);
        let y = subdivision::YieldReport::compute(&subdivision::YieldInputs {
            lot_areas: vec![500.0],
            gross_site_area: 10_000.0,
            building_gfas: vec![1000.0],
            dwelling_rule: Some(s.dwelling_rule()),
            ..Default::default()
        });
        let m = Metrics::from_report(&y);
        assert_eq!((m.dwellings, m.density), (Some(8.0), Some(8.0)));
        assert_eq!(m.cell(Col::Dwellings), "8");
    }

    /// #170: the running row reads phase, done / total, feasible and time;
    /// Cancel returns the panel to Run within 200 ms (time-to-cancel) with no
    /// result, only that it was cancelled, and the worker really stops (it
    /// ends long before its budget).
    #[test]
    fn progress_text_and_a_working_cancel() {
        use site_optimize::{Phase, Progress};
        let p = Progress {
            phase: Phase::Search,
            evaluated: 120,
            budget: 400,
            feasible: 35,
            best: Some(1.0),
        };
        assert_eq!(
            progress_text(&p, 3.21),
            "Searching… 120 / 400 · 35 feasible · 3.2 s"
        );
        let d = Progress {
            phase: Phase::Daylight,
            evaluated: 10,
            budget: 60,
            ..p
        };
        assert_eq!(
            progress_text(&d, 1.0),
            "Measuring daylight… 10 / 60 · 1.0 s"
        );

        let mut s = itsjustcad_commands::Session::default();
        for line in ["rect 0,0,0 200 150", "siteplan last"] {
            s.run(itsjustcad_commands::parse(line).unwrap()).unwrap();
        }
        let source = *s.doc.subdivision_links.keys().next().unwrap();
        let mut panel = OptimizePanel::default();
        panel.open_for(source, &s.doc.subdivision_links[&source].settings, None);
        panel.form.budget = 5000;
        panel.start(&s.doc, source, &egui::Context::default());
        let job = panel.job.as_ref().unwrap();
        let (flag, progress) = (job.cancel.clone(), job.progress.clone());
        // Mid-search: wait for the first chunk.
        let t = Instant::now();
        while progress.lock().unwrap().evaluated == 0 {
            assert!(t.elapsed() < Duration::from_secs(60), "search started");
            std::thread::sleep(Duration::from_millis(1));
        }
        // Time-to-cancel: the panel is back to Run (no job, no result) right
        // away — it used to wait for in-flight plans (1.1 s measured on a
        // 1000 x 800 m site, debug build).
        let t = Instant::now();
        panel.cancel();
        panel.poll(&mut s);
        let time_to_cancel = t.elapsed();
        assert!(panel.job.is_none() && panel.cancelled == Some(source));
        assert!(s.optimize_run(source).is_none(), "a cancel saves nothing");
        assert!(
            time_to_cancel < Duration::from_millis(200),
            "cancel took {time_to_cancel:?}"
        );
        // The worker really stops (drops its handles) long before its budget.
        let t = Instant::now();
        while Arc::strong_count(&flag) > 1 {
            assert!(t.elapsed() < Duration::from_secs(60), "worker exits");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            progress.lock().unwrap().evaluated < 5000,
            "cancel stops the search"
        );
    }

    /// #201: the form a saved request came from maps back to that request.
    #[test]
    fn form_from_request_round_trips() {
        let settings = subdivision::SubdivisionSettings::default();
        let mut f = OptimizeForm {
            objective: Objective::Weighted,
            weights: [1.0, 0.0, 0.0, 0.5, 2.0, 0.0],
            budget: 40,
            seed: 7,
            ..OptimizeForm::default()
        };
        f.seed_from_plan(&settings);
        f.constraints[3] = (true, 15.0); // percent
        f.constraints[9] = (true, 4.0); // max stories
        f.constraints[5] = (false, 50.0);
        f.locks.insert("typology".into());
        f.dwelling_area = 120.0;
        let req = f.to_request();
        assert_eq!(OptimizeForm::from_request(&req, &settings).to_request(), req);
    }

    /// #201: one run per document. Opening another site's Optimize keeps
    /// it; a finished run is saved with a notice; deleting its site plan or
    /// closing the document stops it.
    #[test]
    fn one_run_per_document() {
        let mut s = itsjustcad_commands::Session::default();
        for line in [
            "rect 0,0,0 200 150",
            "lotsettings depth=20",
            "siteplan last",
            "rect 400,0,0 200 150",
            "siteplan last",
        ] {
            s.run(itsjustcad_commands::parse(line).unwrap()).unwrap();
        }
        let ids: Vec<ObjectId> = s.doc.subdivision_links.keys().copied().collect();
        let (a, b) = (ids[0], ids[1]);
        let settings = s.doc.subdivision_links[&a].settings.clone();
        let ctx = egui::Context::default();
        let mut panel = OptimizePanel::default();
        panel.open_for(a, &settings, None);
        panel.form.budget = 16;
        panel.start(&s.doc, a, &ctx);
        panel.open_for(b, &settings, None);
        assert_eq!(panel.running_source(), Some(a), "opening another site keeps it");
        let t = Instant::now();
        while panel.running_source().is_some() {
            assert!(t.elapsed() < Duration::from_secs(60));
            panel.poll(&mut s);
            std::thread::sleep(Duration::from_millis(2));
        }
        let run = s.optimize_run(a).expect("saved");
        assert!(s.optimize_run(b).is_none());
        let n = panel.notice.as_ref().expect("finish notice");
        assert_eq!(n.source, a);
        assert!(n.text.contains(&run.result.candidates.len().to_string()), "{}", n.text);
        // Reopened, the form is the run's.
        panel.open_for(a, &settings, Some(run));
        assert_eq!(panel.form.budget, 16);

        // Deleting the site plan stops its run.
        panel.form.budget = 5000;
        panel.start(&s.doc, b, &ctx);
        s.run(Command::LotRemove {
            targets: itsjustcad_commands::Selector::Ids { ids: vec![b] },
        })
        .unwrap();
        panel.poll(&mut s);
        assert_eq!(panel.running_source(), None);
        // Closing the document stops it too.
        panel.start(&s.doc, a, &ctx);
        assert_eq!(panel.running_source(), Some(a));
        panel.close_document();
        assert_eq!(panel.running_source(), None);
        assert!(panel.notice.is_none());
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
