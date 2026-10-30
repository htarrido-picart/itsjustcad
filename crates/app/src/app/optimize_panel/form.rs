// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! The optimize form: objective, constraints, economics and advanced settings.

use super::*;

/// The objectives offered, with their label keys.
pub(super) const OBJECTIVES: &[(Objective, &str)] = &[
    (Objective::Value, "optimize.objective.value"),
    (Objective::Lots, "optimize.objective.lots"),
    (Objective::FarNet, "optimize.objective.far_net"),
    (Objective::NetArea, "optimize.objective.net_area"),
    (Objective::Gfa, "optimize.objective.gfa"),
    (Objective::Dwellings, "optimize.objective.dwellings"),
    (Objective::Weighted, "optimize.objective.weighted"),
];

/// Weighted-objective metrics (core `METRICS` tokens) and their label keys.
pub(super) const METRICS: &[(&str, &str)] = &[
    ("lots", "optimize.objective.lots"),
    ("far", "optimize.objective.far_net"),
    ("net_area", "optimize.objective.net_area"),
    ("gfa", "optimize.objective.gfa"),
    ("open", "optimize.weight.open"),
    ("dwellings", "optimize.objective.dwellings"),
];

/// Hard constraints, in form order. Ratios are entered as percentages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum Constraint {
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
    pub(in crate::app) const ALL: [Constraint; 15] = [
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

    pub(super) fn label_key(self) -> &'static str {
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
    pub(super) fn widget(self) -> (f64, f64, &'static str) {
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
pub(in crate::app) struct OptimizeForm {
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
pub(in crate::app) fn street_patterns() -> Vec<String> {
    subdiv_params::subdivision_fields()
        .into_iter()
        .find(|f| f.name == "street_pattern")
        .map_or_else(Vec::new, |f| f.choices.iter().map(|c| c.to_string()).collect())
}

impl OptimizeForm {
    /// The core request this form describes.
    pub(in crate::app) fn to_request(&self) -> OptimizeRequest {
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

impl OptimizeForm {
    /// Lot-size bounds from the current plan, on: its `lot_width_min`, at least half its
    /// `lot_area_min` (a full `lot_area_min` floor leaves no feasible layout
    /// once blocks are walkable — edge remnants fall below it) and at most
    /// 3 × it (no super-lots).
    pub(in crate::app) fn seed_from_plan(&mut self, settings: &subdivision::SubdivisionSettings) {
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
    pub(in crate::app) fn from_request(
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
