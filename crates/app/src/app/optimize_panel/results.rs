// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Result rows, metrics, columns and their formatting.

use super::*;

/// The yield metrics the results table compares.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::app) struct Metrics {
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
    pub(in crate::app) fn from_report(y: &subdivision::YieldReport) -> Self {
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
    pub(in crate::app) fn from_eval(e: &site_optimize::Evaluation) -> Self {
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
    pub(in crate::app) fn cell(&self, col: Col) -> String {
        self.get(col).map_or(String::new(), |v| fmt_abs(col, v))
    }

    /// `self − base` for `col`; absolute when the base has no value.
    pub(in crate::app) fn delta_cell(&self, base: &Metrics, col: Col) -> String {
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
    pub(super) fn summary(&self) -> String {
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
pub(in crate::app) enum Col {
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
    pub(super) const ALL: [Col; 12] = [
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

    pub(super) fn label_key(self) -> &'static str {
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
pub(in crate::app) fn fmt_abs(col: Col, v: f64) -> String {
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
pub(in crate::app) fn fmt_delta(col: Col, d: f64) -> String {
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
pub(in crate::app) fn columns(form: &OptimizeForm, rows: &[ResultRow]) -> Vec<Col> {
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
pub(super) fn axis_of(axis: &str) -> (fn(&ResultRow) -> f64, &'static str) {
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
pub(in crate::app) struct ResultRow {
    pub rank: usize,
    pub score: f64,
    pub metrics: Metrics,
    pub pareto: bool,
    pub params: BTreeMap<String, String>,
    pub ledger: Ledger,
    /// Building storeys, sorted.
    pub floors: Vec<usize>,
}

pub(in crate::app) fn result_rows(r: &OptimizeResult) -> Vec<ResultRow> {
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
pub(in crate::app) fn current_row(r: &OptimizeResult) -> Option<ResultRow> {
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
pub(in crate::app) fn fmt_money(v: f64) -> String {
    fmt_abs(Col::Net, v)
}

/// A ledger as (label key, value) lines: revenue, each cost (negative),
/// profit, profit per m².
pub(in crate::app) fn ledger_lines(l: &Ledger) -> Vec<(&'static str, String)> {
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
pub(in crate::app) fn floors_text(floors: &[usize]) -> String {
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

pub(super) fn ledger_text(l: &Ledger) -> String {
    ledger_lines(l)
        .iter()
        .map(|(k, v)| format!("{} {v}", t(k)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `s` without the minus of a value that rounds to zero (`-0`, `-0.0`).
pub(in crate::app) fn unsigned_zero(s: String) -> String {
    match s.strip_prefix('-') {
        Some(rest) if rounds_to_zero(rest) => rest.to_string(),
        _ => s,
    }
}

/// A finished search, ready to show.
pub(in crate::app) struct Finished {
    pub(super) rows: Vec<ResultRow>,
    pub(super) evaluated: usize,
    pub(super) feasible: usize,
    /// `OptimizeResult::pareto_axis`.
    pub(super) axis: &'static str,
    pub(super) cols: Vec<Col>,
    /// Score header (`Profit/m²` for `value`).
    pub(super) score_key: &'static str,
    /// The current plan, scored like a candidate.
    pub(super) current: Option<ResultRow>,
    pub(super) beats_current: bool,
    /// How daylight was measured (when it was).
    pub(super) daylight: Option<site_optimize::DaylightInfo>,
}

impl Finished {
    pub(super) fn new(form: &OptimizeForm, r: &OptimizeResult) -> Finished {
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
