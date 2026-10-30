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

/// The objectives offered, with their label keys.
const OBJECTIVES: &[(Objective, &str)] = &[
    (Objective::Value, "optimize.objective.value"),
    (Objective::Lots, "optimize.objective.lots"),
    (Objective::FarNet, "optimize.objective.far_net"),
    (Objective::NetArea, "optimize.objective.net_area"),
    (Objective::Gfa, "optimize.objective.gfa"),
    (Objective::Dwellings, "optimize.objective.dwellings"),
    (Objective::Weighted, "optimize.objective.weighted"),
];

/// Weighted-objective metrics (core `METRICS` tokens) and their label keys.
const METRICS: &[(&str, &str)] = &[
    ("lots", "optimize.objective.lots"),
    ("far", "optimize.objective.far_net"),
    ("net_area", "optimize.objective.net_area"),
    ("gfa", "optimize.objective.gfa"),
    ("open", "optimize.weight.open"),
    ("dwellings", "optimize.objective.dwellings"),
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
    MaxLotArea,
    MaxDepthRatio,
    MaxBlockPerimeter,
    MaxStories,
    MinParkAccess,
    MinDaylight,
    MaxOverheat,
    MinDwellings,
    /// Dwellings per hectare of net developable area.
    MinDensity,
}

impl Constraint {
    pub(super) const ALL: [Constraint; 15] = [
        Constraint::MinLotArea,
        Constraint::MinLotWidth,
        Constraint::MinFrontage,
        Constraint::MinOpen,
        Constraint::MaxFar,
        Constraint::MaxCoverage,
        Constraint::MaxLotArea,
        Constraint::MaxDepthRatio,
        Constraint::MaxBlockPerimeter,
        Constraint::MaxStories,
        Constraint::MinParkAccess,
        Constraint::MinDaylight,
        Constraint::MaxOverheat,
        Constraint::MinDwellings,
        Constraint::MinDensity,
    ];

    /// On out of the box (the core's defaults): no strip lots, no
    /// super-blocks, 10 % green space with a park near most lots, ≤ 6 storeys.
    fn default_on(self) -> bool {
        matches!(
            self,
            Constraint::MaxDepthRatio
                | Constraint::MaxBlockPerimeter
                | Constraint::MinOpen
                | Constraint::MaxStories
                | Constraint::MinParkAccess
        )
    }

    fn label_key(self) -> &'static str {
        match self {
            Constraint::MinLotArea => "optimize.min_lot_area",
            Constraint::MinLotWidth => "optimize.min_lot_width",
            Constraint::MinFrontage => "optimize.min_frontage",
            Constraint::MinOpen => "optimize.min_open",
            Constraint::MaxFar => "optimize.max_far",
            Constraint::MaxCoverage => "optimize.max_coverage",
            Constraint::MaxLotArea => "optimize.max_lot_area",
            Constraint::MaxDepthRatio => "optimize.max_depth_ratio",
            Constraint::MaxBlockPerimeter => "optimize.max_block_perimeter",
            Constraint::MaxStories => "optimize.max_stories",
            Constraint::MinParkAccess => "optimize.min_park_access",
            Constraint::MinDaylight => "optimize.min_daylight",
            Constraint::MaxOverheat => "optimize.max_overheat",
            Constraint::MinDwellings => "optimize.min_dwellings",
            Constraint::MinDensity => "optimize.min_density",
        }
    }

    /// (default value, step, unit) as shown in the form.
    fn widget(self) -> (f64, f64, &'static str) {
        match self {
            Constraint::MinLotArea => (300.0, 10.0, "m²"),
            Constraint::MinLotWidth => (10.0, 0.5, "m"),
            Constraint::MinFrontage => (8.0, 0.5, "m"),
            Constraint::MinOpen => (10.0, 1.0, "%"),
            Constraint::MaxFar => (1.5, 0.05, ""),
            Constraint::MaxCoverage => (60.0, 1.0, "%"),
            Constraint::MaxLotArea => (900.0, 10.0, "m²"),
            Constraint::MaxDepthRatio => (3.0, 0.05, ": 1"),
            Constraint::MaxBlockPerimeter => (800.0, 10.0, "m"),
            Constraint::MaxStories => (6.0, 1.0, ""),
            Constraint::MinParkAccess => (90.0, 1.0, "%"),
            Constraint::MinDaylight => (50.0, 1.0, "%"),
            Constraint::MaxOverheat => (30.0, 1.0, "%"),
            Constraint::MinDwellings => (100.0, 1.0, ""),
            Constraint::MinDensity => (60.0, 1.0, "/ha"),
        }
    }

    fn is_percent(self) -> bool {
        matches!(
            self,
            Constraint::MinOpen
                | Constraint::MaxCoverage
                | Constraint::MinParkAccess
                | Constraint::MinDaylight
                | Constraint::MaxOverheat
        )
    }
}

/// The optimize form, as the user edits it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct OptimizeForm {
    pub objective: Objective,
    /// Weights per [`METRICS`] entry (used by `Weighted`; zero = omitted).
    pub weights: [f64; 6],
    /// Per [`Constraint::ALL`]: (enabled, value in form units).
    pub constraints: [(bool, f64); 15],
    pub budget: usize,
    pub seed: u64,
    pub locks: BTreeSet<String>,
    /// Allowed street patterns; all of them (or none) means unrestricted.
    pub patterns: BTreeSet<String>,
    /// Unit prices of the `value` objective (and the Yield economics block).
    pub economics: Economics,
    /// Facade daylight: period, sun hours a day, overheating hours.
    pub daylight_period: DaylightPeriod,
    pub daylight_hours: f64,
    pub overheat_hours: f64,
    /// Use the document's climate file for daylight (when it has one).
    pub use_climate: bool,
    /// Unit-size rule: average dwelling m² and net-to-gross efficiency
    /// (a fraction), seeded from the plan; sent as overrides when changed.
    pub dwelling_area: f64,
    pub dwelling_efficiency: f64,
    /// The plan's own rule (what an unchanged form leaves alone).
    pub plan_rule: (f64, f64),
}

impl Default for OptimizeForm {
    fn default() -> Self {
        let d = OptimizeRequest::default();
        let s = subdivision::SubdivisionSettings::default();
        let rule = (s.dwelling_area, s.dwelling_efficiency);
        OptimizeForm {
            objective: d.objective,
            weights: [1.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            constraints: Constraint::ALL.map(|c| (c.default_on(), c.widget().0)),
            budget: d.budget,
            seed: d.seed,
            locks: BTreeSet::new(),
            patterns: d.patterns.iter().cloned().collect(),
            economics: d.economics,
            daylight_period: d.daylight_period,
            daylight_hours: d.daylight_hours,
            overheat_hours: d.overheat_hours,
            use_climate: d.use_climate,
            dwelling_area: rule.0,
            dwelling_efficiency: rule.1,
            plan_rule: rule,
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
            daylight_period: self.daylight_period,
            daylight_hours: self.daylight_hours,
            overheat_hours: self.overheat_hours,
            use_climate: self.use_climate,
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
        // The checked patterns, in schema order (none checked = all).
        req.patterns = street_patterns()
            .into_iter()
            .filter(|p| self.patterns.contains(p))
            .collect();
        // An unchecked box lifts the core's default-on limits too.
        req.max_depth_ratio = None;
        req.max_block_perimeter = None;
        req.min_open = None;
        req.max_stories = None;
        req.min_park_access = None;
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
                Constraint::MaxLotArea => req.max_lot_area = v,
                Constraint::MaxDepthRatio => req.max_depth_ratio = v,
                Constraint::MaxBlockPerimeter => req.max_block_perimeter = v,
                Constraint::MaxStories => req.max_stories = v.map(|n| n.round().max(1.0) as usize),
                Constraint::MinParkAccess => req.min_park_access = v,
                Constraint::MinDaylight => req.min_daylight = v,
                Constraint::MaxOverheat => req.max_overheat = v,
                Constraint::MinDwellings => req.min_dwellings = v,
                Constraint::MinDensity => req.min_density = v,
            }
        }
        if (self.dwelling_area, self.dwelling_efficiency) != self.plan_rule {
            req.dwelling_area = Some(self.dwelling_area);
            req.dwelling_efficiency = Some(self.dwelling_efficiency);
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
    /// Median building storeys.
    pub floors: Option<f64>,
    /// Lots within walking distance of a park, percent.
    pub park_pct: Option<f64>,
    /// Facade area with the daylight sun hours, percent (when measured).
    pub daylight_pct: Option<f64>,
    /// Facade area overheating on the hottest month, percent (when asked).
    pub overheat_pct: Option<f64>,
    /// Dwellings, and dwellings per net hectare (with a unit-size rule).
    pub dwellings: Option<f64>,
    pub density: Option<f64>,
}

impl Metrics {
    pub(super) fn from_report(y: &subdivision::YieldReport) -> Self {
        Metrics {
            lots: y.lot_count,
            net_area: y.total_lot_area,
            gfa: y.total_gfa,
            far: y.far_net,
            open_pct: y.open_space_ratio * 100.0,
            floors: None,
            park_pct: None,
            daylight_pct: None,
            overheat_pct: None,
            dwellings: y.dwellings.as_ref().map(|d| d.total as f64),
            density: y.dwellings.as_ref().and_then(|d| d.per_ha_net),
        }
    }

    /// The report plus what only an optimizer evaluation measures.
    pub(super) fn from_eval(e: &site_optimize::Evaluation) -> Self {
        let f = e.quantities.floors_sorted();
        Metrics {
            floors: f.get(f.len() / 2).map(|n| *n as f64),
            park_pct: Some(e.quantities.park_access() * 100.0),
            daylight_pct: e.quantities.daylight.map(|d| d * 100.0),
            overheat_pct: e.quantities.overheat.map(|d| d * 100.0),
            ..Metrics::from_report(&e.report)
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
            Col::Floors => self.floors,
            Col::Park => self.park_pct,
            Col::Daylight => self.daylight_pct,
            Col::Overheat => self.overheat_pct,
            Col::Dwellings => self.dwellings,
            Col::Density => self.density,
        }
    }

    /// The `Current` row's cell for `col`.
    pub(super) fn cell(&self, col: Col) -> String {
        self.get(col).map_or(String::new(), |v| fmt_abs(col, v))
    }

    /// `self − base` for `col`; absolute when the base has no value.
    pub(super) fn delta_cell(&self, base: &Metrics, col: Col) -> String {
        if col == Col::Floors {
            return self.cell(col);
        }
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
            .filter(|c| self.get(**c).is_some())
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
    /// Median storeys (absolute, not a delta).
    Floors,
    Park,
    Daylight,
    Overheat,
    Dwellings,
    /// Dwellings per net hectare.
    Density,
}

impl Col {
    const ALL: [Col; 12] = [
        Col::Score,
        Col::Lots,
        Col::Net,
        Col::Gfa,
        Col::Far,
        Col::Open,
        Col::Floors,
        Col::Park,
        Col::Daylight,
        Col::Overheat,
        Col::Dwellings,
        Col::Density,
    ];

    fn label_key(self) -> &'static str {
        match self {
            Col::Score => "optimize.col.score",
            Col::Lots => "param.subdivision.yield.lots",
            Col::Net => "optimize.col.net",
            Col::Gfa => "optimize.col.gfa",
            Col::Far => "optimize.col.far",
            Col::Open => "optimize.col.open",
            Col::Floors => "optimize.col.floors",
            Col::Park => "optimize.col.park",
            Col::Daylight => "optimize.col.daylight",
            Col::Overheat => "optimize.col.overheat",
            Col::Dwellings => "optimize.col.dwellings",
            Col::Density => "optimize.col.density",
        }
    }

    /// `a` (≥ 0) as shown: areas compact (`950`, `12.3k`, `412k`, `2.95M`, `12.0M`),
    /// scores as integers when they are.
    fn magnitude(self, a: f64) -> String {
        match self {
            Col::Score if (a - a.round()).abs() < 1e-9 => format!("{a:.0}"),
            Col::Score | Col::Far => format!("{a:.2}"),
            Col::Lots
            | Col::Floors
            | Col::Park
            | Col::Daylight
            | Col::Overheat
            | Col::Dwellings
            | Col::Density => {
                format!("{a:.0}")
            }
            Col::Open => format!("{a:.1}"),
            Col::Net | Col::Gfa => match a {
                a if a < 10_000.0 => format!("{a:.0}"),
                a if a < 100_000.0 => format!("{:.1}k", a / 1e3),
                a if a < 999_500.0 => format!("{:.0}k", a / 1e3),
                a if a < 9_995_000.0 => format!("{:.2}M", a / 1e6),
                a if a < 999_500_000.0 => format!("{:.1}M", a / 1e6),
                a => format!("{:.2}B", a / 1e9),
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
    // Dwellings sit next to lots (storeys stay too); density when it is
    // constrained.
    let density = on(Constraint::MinDensity).then_some(Col::Density);
    if value {
        // Profit comes from built floor area: storeys and park access are
        // what tell the layouts apart (areas are in the hover).
        cols.extend([Col::Lots, Col::Dwellings]);
        cols.extend(density);
        cols.extend([Col::Floors, Col::Park]);
        if on(Constraint::MinDaylight) || weight("daylight") {
            cols.push(Col::Daylight);
        }
        if on(Constraint::MaxOverheat) {
            cols.push(Col::Overheat);
        }
        // A narrow panel holds five: lots, then park access, go to the hover.
        for drop in [Col::Lots, Col::Park] {
            if cols.len() > 5 {
                cols.retain(|c| *c != drop);
            }
        }
        return cols;
    }
    cols.extend([Col::Lots, Col::Dwellings]);
    cols.extend(density);
    cols.extend([Col::Net, Col::Gfa]);
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
        "dwellings" => (
            |r| r.metrics.dwellings.unwrap_or(0.0),
            "optimize.objective.dwellings",
        ),
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
    /// Building storeys, sorted.
    pub floors: Vec<usize>,
}

pub(super) fn result_rows(r: &OptimizeResult) -> Vec<ResultRow> {
    r.candidates
        .iter()
        .map(|c| ResultRow {
            rank: c.rank,
            score: c.eval.score,
            metrics: Metrics::from_eval(&c.eval),
            pareto: r.pareto.contains(&c.rank),
            params: c.params.clone(),
            ledger: c.eval.ledger,
            floors: c.eval.quantities.floors_sorted(),
        })
        .collect()
}

/// The current plan as a (rank 0) row, scored like the candidates.
pub(super) fn current_row(r: &OptimizeResult) -> Option<ResultRow> {
    r.current.as_ref().map(|e| ResultRow {
        rank: 0,
        score: e.score,
        metrics: Metrics::from_eval(e),
        pareto: false,
        params: BTreeMap::new(),
        ledger: e.ledger,
        floors: e.quantities.floors_sorted(),
    })
}

/// Money, compact like areas (`950`, `12.3k`, `94.1M`), never `−0`.
pub(super) fn fmt_money(v: f64) -> String {
    fmt_abs(Col::Net, v)
}

/// A ledger as (label key, value) lines: revenue, each cost (negative),
/// profit, profit per m².
pub(super) fn ledger_lines(l: &Ledger) -> Vec<(&'static str, String)> {
    let mut lines = vec![("optimize.econ.revenue", fmt_money(l.revenue))];
    // Site purchase and construction only exist when developing.
    if l.land != 0.0 || l.construction != 0.0 {
        lines.push(("optimize.econ.land", fmt_money(-l.land)));
        lines.push(("optimize.econ.construction", fmt_money(-l.construction)));
    }
    lines.extend([
        ("optimize.econ.roads", fmt_money(-l.roads)),
        ("optimize.econ.utilities", fmt_money(-l.utilities)),
        ("optimize.econ.lots", fmt_money(-l.lots)),
        ("optimize.econ.open_space", fmt_money(-l.open_space)),
        ("optimize.econ.profit", fmt_money(l.profit)),
        ("optimize.objective.value", fmt_abs(Col::Far, l.value)),
    ]);
    lines
}

/// Storeys as `min / median / max` plus a histogram, e.g.
/// `1 / 4 / 6 · 1:49 2:99 … 6:456`.
pub(super) fn floors_text(floors: &[usize]) -> String {
    let (Some(lo), Some(hi)) = (floors.first(), floors.last()) else {
        return "—".into();
    };
    let mut hist = BTreeMap::<usize, usize>::new();
    for f in floors {
        *hist.entry(*f).or_default() += 1;
    }
    let hist: Vec<String> = hist.iter().map(|(f, n)| format!("{f}:{n}")).collect();
    format!("{lo} / {} / {hi} · {}", floors[floors.len() / 2], hist.join(" "))
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
    /// How daylight was measured (when it was).
    daylight: Option<site_optimize::DaylightInfo>,
}

/// A search running on a worker thread: the document's one run (#201).
pub(super) struct Job {
    /// The site plan searched.
    source: ObjectId,
    started: Instant,
    /// The form the search ran with (picks the shown columns).
    form: OptimizeForm,
    /// What the search plans, saved with its result.
    prepared: site_optimize::Prepared,
    rx: mpsc::Receiver<Result<OptimizeResult, String>>,
    /// Set by Cancel; the worker stops at its next plan and its result is
    /// dropped (the panel already let go of the job).
    cancel: Arc<AtomicBool>,
    /// The search's latest progress report.
    progress: Arc<Mutex<site_optimize::Progress>>,
    /// No idle sleep while it runs (the display may sleep).
    _awake: crate::keep_awake::KeepAwake,
}

impl Job {
    fn progress(&self) -> site_optimize::Progress {
        *self.progress.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The running row's text: phase, `done / total`, feasible so far, elapsed.
pub(super) fn progress_text(p: &site_optimize::Progress, elapsed: f64) -> String {
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
pub(super) struct Notice {
    pub(super) source: ObjectId,
    pub(super) text: String,
    pub(super) at: Instant,
}

/// How long a notice stays up.
pub(super) const NOTICE_FOR: Duration = Duration::from_secs(8);

/// The panel state, kept on [`App`]. One search runs per document (#201):
/// opening another site plan's Optimize, selecting or editing never stops
/// it; only Cancel, deleting its site plan, or closing/replacing the
/// document does. Results are the site plan's saved run
/// ([`itsjustcad_commands::Session::optimize_run`]).
#[derive(Default)]
pub(super) struct OptimizePanel {
    /// The site plan boundary the form is open for.
    source: Option<ObjectId>,
    pub(super) form: OptimizeForm,
    job: Option<Job>,
    /// Run was pressed for this site while another site's search runs: ask
    /// "Stop the running search?" first.
    confirm_stop: Option<ObjectId>,
    /// The last search of this site failed.
    error: Option<(ObjectId, String)>,
    /// The last search of this site was cancelled.
    cancelled: Option<ObjectId>,
    /// The shown saved run, ready to draw: (source, finished_at, view).
    shown: Option<(ObjectId, u64, Finished)>,
    /// The finish notice.
    pub(super) notice: Option<Notice>,
    /// The Yield economics block's plan quantities, per (source, generation).
    current_q: Option<(ObjectId, u64, site_optimize::DaylightOpts, Option<Quantities>)>,
}

impl OptimizeForm {
    /// Lot-size bounds from the current plan, on: its `lot_width_min`, at least half its
    /// `lot_area_min` (a full `lot_area_min` floor leaves no feasible layout
    /// once blocks are walkable — edge remnants fall below it) and at most
    /// 3 × it (no super-lots).
    pub(super) fn seed_from_plan(&mut self, settings: &subdivision::SubdivisionSettings) {
        self.constraints[0] = (true, 0.5 * settings.lot_area_min);
        self.constraints[1] = (true, settings.lot_width_min);
        self.constraints[6] = (true, 3.0 * settings.lot_area_min);
        self.plan_rule = (settings.dwelling_area, settings.dwelling_efficiency);
        (self.dwelling_area, self.dwelling_efficiency) = self.plan_rule;
    }
}

impl OptimizeForm {
    /// The form a saved run's `req` came from (its unchecked constraints keep
    /// the plan's seeded values), so a reopened panel shows what ran.
    pub(super) fn from_request(
        req: &OptimizeRequest,
        settings: &subdivision::SubdivisionSettings,
    ) -> OptimizeForm {
        let mut f = OptimizeForm::default();
        f.seed_from_plan(settings);
        f.objective = req.objective;
        f.budget = req.budget;
        f.seed = req.seed;
        f.economics = req.economics;
        f.daylight_period = req.daylight_period;
        f.daylight_hours = req.daylight_hours;
        f.overheat_hours = req.overheat_hours;
        f.use_climate = req.use_climate;
        f.locks = req.lock.iter().cloned().collect();
        f.patterns = req.patterns.iter().cloned().collect();
        if req.objective == Objective::Weighted {
            for ((k, _), w) in METRICS.iter().zip(f.weights.iter_mut()) {
                *w = req.weights.get(*k).copied().unwrap_or(0.0);
            }
        }
        for (c, slot) in Constraint::ALL.into_iter().zip(f.constraints.iter_mut()) {
            let v = match c {
                Constraint::MinLotArea => req.min_lot_area,
                Constraint::MinLotWidth => req.min_lot_width,
                Constraint::MinFrontage => req.min_frontage,
                Constraint::MinOpen => req.min_open,
                Constraint::MaxFar => req.max_far,
                Constraint::MaxCoverage => req.max_coverage,
                Constraint::MaxLotArea => req.max_lot_area,
                Constraint::MaxDepthRatio => req.max_depth_ratio,
                Constraint::MaxBlockPerimeter => req.max_block_perimeter,
                Constraint::MaxStories => req.max_stories.map(|n| n as f64),
                Constraint::MinParkAccess => req.min_park_access,
                Constraint::MinDaylight => req.min_daylight,
                Constraint::MaxOverheat => req.max_overheat,
                Constraint::MinDwellings => req.min_dwellings,
                Constraint::MinDensity => req.min_density,
            };
            *slot = match v {
                Some(v) => (true, if c.is_percent() { v * 100.0 } else { v }),
                None => (false, slot.1),
            };
        }
        if let (Some(a), Some(e)) = (req.dwelling_area, req.dwelling_efficiency) {
            (f.dwelling_area, f.dwelling_efficiency) = (a, e);
        }
        f
    }
}

impl OptimizePanel {
    /// Show the form for `source`: the running search's form, else the saved
    /// run's, else the current form seeded from the plan. Never stops a run.
    fn open_for(
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
    pub(super) fn running_source(&self) -> Option<ObjectId> {
        self.job.as_ref().map(|j| j.source)
    }

    /// The running search's progress and elapsed seconds.
    pub(super) fn running_progress(&self) -> Option<(site_optimize::Progress, f64)> {
        self.job
            .as_ref()
            .map(|j| (j.progress(), j.started.elapsed().as_secs_f64()))
    }

    /// Start a search of `source` off the UI thread (stopping any running
    /// one); repaints `ctx` when done.
    fn start(&mut self, doc: &itsjustcad_doc::Document, source: ObjectId, ctx: &egui::Context) {
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
    pub(super) fn cancel(&mut self) {
        if let Some(job) = self.job.take() {
            job.cancel.store(true, Ordering::Relaxed);
            self.cancelled = Some(job.source);
        }
    }

    /// The document closed or was replaced: stop the search, forget it all.
    pub(super) fn close_document(&mut self) {
        self.cancel();
        *self = OptimizePanel {
            form: std::mem::take(&mut self.form),
            ..OptimizePanel::default()
        };
    }

    /// Collect a finished search, if any, into `session` as its site plan's
    /// saved run, and raise the finish notice. A search whose site plan was
    /// deleted is stopped. Called every frame, wherever the user is.
    pub(super) fn poll(&mut self, session: &mut itsjustcad_commands::Session) {
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
    fn shown(&mut self, source: ObjectId, run: Option<&SavedRun>, settings: &subdivision::SubdivisionSettings) -> Option<&Finished> {
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
    pub(super) fn run_now(&mut self, session: &mut itsjustcad_commands::Session, source: ObjectId) {
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
    pub(super) fn fake_running(
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

impl Finished {
    fn new(form: &OptimizeForm, r: &OptimizeResult) -> Finished {
        let rows = result_rows(r);
        Finished {
            cols: columns(form, &rows),
            rows,
            evaluated: r.evaluated,
            feasible: r.feasible,
            axis: r.pareto_axis,
            score_key: if form.objective == Objective::Value {
                "optimize.col.value"
            } else {
                "optimize.col.score"
            },
            current: current_row(r),
            beats_current: r.beats_current(),
            daylight: r.daylight.clone(),
        }
    }
}

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
pub(super) fn daylight_source_text(climate: Option<&str>, overheat_month: Option<u32>) -> String {
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
    pub(super) fn open_optimize(&mut self, source: ObjectId) {
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
    pub(super) fn optimize_notice(&mut self, ctx: &egui::Context) {
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
    pub(super) fn optimize_dev_hook(&mut self, ctx: &egui::Context) {
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
    pub(super) fn optimize_panel(
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
    pub(super) fn yield_economics(&mut self, ui: &mut egui::Ui, source: ObjectId) {
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
