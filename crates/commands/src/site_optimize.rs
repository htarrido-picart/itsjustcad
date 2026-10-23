// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! `siteoptimize` — search the `siteplan` parameters for the best yield (#145).
//!
//! Each candidate is a set of site-plan params folded onto a base
//! `SubdivisionSettings` and evaluated OFF-document with [`lot::plan_site`]
//! (nothing is baked) into a [`YieldReport`] plus hard-constraint checks.
//!
//! Search: a seeded Latin hypercube over the unlocked params (half the
//! budget), then an elitist (μ+λ) GA — uniform crossover, Gaussian mutation on
//! the continuous params with a shrinking step, enums and the layout seed
//! resampled. Every candidate is drawn from one seeded RNG before its batch is
//! evaluated in parallel (rayon, ordered collect), so the result is the same
//! for a given seed whatever the thread count.

use std::collections::BTreeMap;

use itsjustcad_doc::{
    Document, EdgeRoleOverride, FieldKind, Geometry, LotHeightOverride, ObjectId, StreetEdit,
};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use subdivision::{FrontageAt, Polygon2d, SubdivisionSettings, YieldInputs, YieldReport};

use crate::lot::{self, SitePlanBake};

/// What to maximize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Objective {
    #[default]
    Lots,
    FarNet,
    /// Σ lot area — the land actually sold as lots.
    NetArea,
    Gfa,
    /// Σ weight × metric over `weights` (`lots`, `far`, `net_area`, `gfa`, `open`).
    Weighted,
}

impl Objective {
    fn parse(s: &str) -> Option<Objective> {
        match s.to_ascii_lowercase().as_str() {
            "lots" => Some(Objective::Lots),
            "far" | "far_net" => Some(Objective::FarNet),
            "net_area" | "area" => Some(Objective::NetArea),
            "gfa" => Some(Objective::Gfa),
            "weighted" => Some(Objective::Weighted),
            _ => None,
        }
    }

    fn token(self) -> &'static str {
        match self {
            Objective::Lots => "lots",
            Objective::FarNet => "far_net",
            Objective::NetArea => "net_area",
            Objective::Gfa => "gfa",
            Objective::Weighted => "weighted",
        }
    }
}

/// A `siteoptimize` request: objective, budget, locks and hard constraints.
/// Shared by the command parser and the FFI (`ijc_site_optimize_json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OptimizeRequest {
    pub objective: Objective,
    /// Metric weights for [`Objective::Weighted`].
    pub weights: BTreeMap<String, f64>,
    /// Number of site plans evaluated.
    pub budget: usize,
    /// Search seed (not the layout `seed` param, which is searched).
    pub seed: u64,
    /// Candidates listed.
    pub top: usize,
    /// Params kept at their base value.
    pub lock: Vec<String>,
    /// Allowed street patterns (default: all).
    pub patterns: Vec<String>,
    pub min_lot_area: Option<f64>,
    pub min_lot_width: Option<f64>,
    pub min_frontage: Option<f64>,
    /// Open-space ratio in [0, 1].
    pub min_open: Option<f64>,
    pub max_far: Option<f64>,
    /// Lot coverage in [0, 1].
    pub max_coverage: Option<f64>,
}

impl Default for OptimizeRequest {
    fn default() -> Self {
        OptimizeRequest {
            objective: Objective::Lots,
            weights: BTreeMap::new(),
            budget: 200,
            seed: 1,
            top: 5,
            lock: Vec::new(),
            patterns: Vec::new(),
            min_lot_area: None,
            min_lot_width: None,
            min_frontage: None,
            min_open: None,
            max_far: None,
            max_coverage: None,
        }
    }
}

/// The params searched by default, in decode order (`lot_area_max` is a ratio
/// of `lot_area_min`, so it follows it).
pub const SEARCHED: &[&str] = &[
    "seed",
    "street_pattern",
    "road_width",
    "block_depth",
    "method",
    "lot_area_min",
    "lot_area_max",
    "lot_width_min",
    "typology",
    "open_space_reserve_frac",
];

impl OptimizeRequest {
    /// Set one `key=value` option (the command form).
    pub fn set(&mut self, key: &str, v: &str) -> Result<(), String> {
        let num = || {
            v.parse::<f64>()
                .map_err(|_| format!("{key}= needs a number, got '{v}'"))
        };
        let list = || {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };
        match key {
            "objective" => {
                self.objective = Objective::parse(v).ok_or_else(|| {
                    format!("objective= must be lots|far_net|net_area|gfa|weighted, got '{v}'")
                })?;
            }
            "weights" => {
                for kv in v.split(',') {
                    let (k, w) = kv
                        .split_once(':')
                        .ok_or("weights= takes metric:weight pairs, e.g. lots:1,far:2")?;
                    let w = w
                        .parse::<f64>()
                        .map_err(|_| format!("weights: bad weight '{w}'"))?;
                    self.weights.insert(k.trim().to_string(), w);
                }
                if self.objective == Objective::Lots {
                    self.objective = Objective::Weighted;
                }
            }
            "budget" => self.budget = num()? as usize,
            "seed" => self.seed = num()? as u64,
            "top" => self.top = num()? as usize,
            "lock" => self.lock = list(),
            "patterns" => self.patterns = list(),
            "min_lot_area" => self.min_lot_area = Some(num()?),
            "min_lot_width" => self.min_lot_width = Some(num()?),
            "min_frontage" => self.min_frontage = Some(num()?),
            "min_open" => self.min_open = Some(num()?),
            "max_far" => self.max_far = Some(num()?),
            "max_coverage" => self.max_coverage = Some(num()?),
            _ => return Err(format!("siteoptimize: unknown option '{key}'")),
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), String> {
        if self.budget == 0 || self.budget > 5000 {
            return Err("siteoptimize: budget must be 1..=5000".into());
        }
        if let Some(bad) = self.lock.iter().find(|n| !SEARCHED.contains(&n.as_str())) {
            return Err(format!(
                "siteoptimize: cannot lock '{bad}' (searched: {})",
                SEARCHED.join(", ")
            ));
        }
        if self.objective == Objective::Weighted {
            if self.weights.is_empty() {
                return Err("siteoptimize: objective=weighted needs weights=lots:1,far:…".into());
            }
            if let Some(bad) = self.weights.keys().find(|k| !METRICS.contains(&k.as_str())) {
                return Err(format!(
                    "siteoptimize: unknown weight '{bad}' (use {})",
                    METRICS.join(", ")
                ));
            }
        }
        Ok(())
    }

    fn score(&self, r: &YieldReport) -> f64 {
        match self.objective {
            Objective::Weighted => self.weights.iter().map(|(k, w)| w * metric(r, k)).sum(),
            o => metric(r, o.token()),
        }
    }
}

/// `15` and `0.15` both mean 15 %.
fn ratio(v: f64) -> f64 {
    if v > 1.0 { v / 100.0 } else { v }
}

const METRICS: &[&str] = &["lots", "far", "net_area", "gfa", "open"];

fn metric(r: &YieldReport, key: &str) -> f64 {
    match key {
        "lots" => r.lot_count as f64,
        "far" | "far_net" => r.far_net.unwrap_or(0.0),
        "net_area" => r.total_lot_area,
        "gfa" => r.total_gfa,
        "open" => r.open_space_ratio,
        _ => 0.0,
    }
}

/// One evaluated site plan.
#[derive(Debug, Clone)]
pub struct Evaluation {
    pub report: YieldReport,
    /// Hard constraints this plan breaks (empty = feasible).
    pub violations: Vec<&'static str>,
    pub score: f64,
}

/// Lot fronts as the constraints read them, from the street-tagged lots (#143):
/// each lot's primary-front frontage at the setback line, its primary-front
/// width at the curb, and how many lots touch a real street edge (a lot that
/// touches only the site boundary has no access). The one place `min_frontage`,
/// `min_lot_width` and street access read fronts.
struct Fronts {
    setback: Vec<f64>,
    curb: Vec<f64>,
    with_street: usize,
}

fn lot_fronts(plan: &SitePlanBake, settings: &SubdivisionSettings) -> Fronts {
    let len = |at| {
        lot::measure_frontage(&plan.lots.lots, at, settings)
            .into_iter()
            .map(|m| m.length)
            .collect()
    };
    Fronts {
        setback: len(FrontageAt::Setback),
        curb: len(FrontageAt::Curb),
        with_street: plan.lots.lots.iter().filter(|l| l.has_street()).count(),
    }
}

/// A site plan's hand edits that a re-derive re-applies: `streetset` street
/// edits and `edgerole` overrides. A boundary's existing plan lends its edits
/// to the search, so a scored plan is the plan `siteoptimize apply` bakes.
#[derive(Debug, Clone, Default)]
pub struct PlanEdits {
    pub streets: BTreeMap<u32, StreetEdit>,
    pub edge_roles: Vec<EdgeRoleOverride>,
    pub lot_heights: Vec<LotHeightOverride>,
}

/// Evaluate one site plan off-document: run the chain, compute the yield,
/// check the hard constraints.
pub fn evaluate(
    site: &Polygon2d,
    settings: &SubdivisionSettings,
    edits: &PlanEdits,
    req: &OptimizeRequest,
) -> Result<Evaluation, String> {
    let mut roles = edits.edge_roles.clone();
    let mut heights = edits.lot_heights.clone();
    let plan = lot::plan_site(site, 0.0, settings, &edits.streets, &mut roles, &mut heights)?;
    if plan.lots.polygons.is_empty() {
        return Err("the plan has no lots".into());
    }
    let fronts = lot_fronts(&plan, settings);
    let (gfas, footprints) = plan
        .buildings
        .as_ref()
        .map_or((Vec::new(), Vec::new()), |b| {
            b.buildings
                .iter()
                .map(|b| (b.gfa, b.footprint.area()))
                .unzip()
        });
    let report = YieldReport::compute(&YieldInputs {
        lot_areas: plan.lots.polygons.iter().map(|p| p.area()).collect(),
        lot_frontages: fronts.setback.clone(),
        gross_site_area: site.area(),
        open_space_feature_areas: Vec::new(),
        reserved_block_areas: plan.reserved.iter().map(|p| p.area()).collect(),
        building_gfas: gfas,
        building_footprints: footprints,
    });
    let min = |v: &[f64]| v.iter().copied().fold(f64::INFINITY, f64::min);
    let checks = [
        (
            "min_lot_area",
            req.min_lot_area.is_some_and(|m| report.min_lot_area < m),
        ),
        (
            "min_lot_width",
            req.min_lot_width.is_some_and(|m| min(&fronts.curb) < m),
        ),
        (
            "min_frontage",
            req.min_frontage.is_some_and(|m| min(&fronts.setback) < m),
        ),
        (
            "min_open",
            req.min_open.is_some_and(|m| report.open_space_ratio < m),
        ),
        (
            "max_far",
            req.max_far
                .is_some_and(|m| report.far_net.unwrap_or(0.0) > m),
        ),
        (
            "max_coverage",
            req.max_coverage
                .is_some_and(|m| report.lot_coverage.unwrap_or(0.0) > m),
        ),
        ("street_access", fronts.with_street < report.lot_count),
    ];
    let violations = checks.iter().filter(|c| c.1).map(|c| c.0).collect();
    let score = req.score(&report);
    Ok(Evaluation {
        report,
        violations,
        score,
    })
}

/// One ranked, feasible candidate.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub rank: usize,
    /// The searched params as `lotsetparams` tokens (what `apply` folds).
    pub params: BTreeMap<String, String>,
    pub eval: Evaluation,
}

/// The outcome of a search.
#[derive(Debug, Clone)]
pub struct OptimizeResult {
    pub objective: Objective,
    /// The top-N ranked candidates plus any Pareto member ranked lower.
    pub candidates: Vec<Candidate>,
    /// Ranks on the (score, open-space ratio) Pareto front.
    pub pareto: Vec<usize>,
    pub evaluated: usize,
    pub feasible: usize,
}

/// How one searched param maps the unit interval.
enum Range {
    Float {
        lo: f64,
        hi: f64,
        step: f64,
    },
    Int {
        lo: i64,
        hi: i64,
    },
    Choice(Vec<String>),
    /// `lot_area_max` as a multiple of `lot_area_min`.
    RatioOfMin {
        lo: f64,
        hi: f64,
    },
}

struct Dim {
    name: &'static str,
    range: Range,
}

impl Dim {
    /// Mutated by resampling, not by a Gaussian step.
    fn is_discrete(&self) -> bool {
        matches!(self.range, Range::Choice(_) | Range::Int { .. })
    }
}

/// The unlocked search dimensions, with bounds derived from the site size, the
/// base settings and the constraints (a lot-size floor never searches below
/// its constraint).
fn dimensions(site: &Polygon2d, base: &SubdivisionSettings, req: &OptimizeRequest) -> Vec<Dim> {
    let (lo, hi) = site.aabb();
    let short = (hi.x - lo.x).min(hi.y - lo.y).max(1.0);
    let choices = |name: &str| -> Vec<String> {
        crate::subdiv_params::subdivision_fields()
            .into_iter()
            .find(|f| f.name == name)
            .map(|f| f.choices.iter().map(|c| c.to_string()).collect())
            .unwrap_or_default()
    };
    let patterns = if req.patterns.is_empty() {
        choices("street_pattern")
    } else {
        req.patterns.clone()
    };
    let (area_lo, width_lo) = floors(base, req);
    let open = req.min_open.unwrap_or(0.0);
    SEARCHED
        .iter()
        .filter(|n| !req.lock.iter().any(|l| l == *n))
        .filter_map(|&name| {
            let range = match name {
                "seed" => Range::Int { lo: 0, hi: 9999 },
                "street_pattern" => Range::Choice(patterns.clone()),
                "road_width" => Range::Float {
                    lo: 8.0,
                    hi: 20.0,
                    step: 0.5,
                },
                "block_depth" => Range::Float {
                    lo: (short / 8.0).max(20.0),
                    hi: (short / 2.0).max(40.0),
                    step: 1.0,
                },
                "method" => Range::Choice(choices("method")),
                "lot_area_min" => Range::Float {
                    lo: area_lo,
                    hi: (base.lot_area_min * 1.2).max(area_lo * 1.5),
                    step: 10.0,
                },
                "lot_area_max" => Range::RatioOfMin { lo: 1.2, hi: 2.5 },
                "lot_width_min" => Range::Float {
                    lo: width_lo,
                    hi: (base.lot_width_min * 1.2).max(width_lo * 1.5),
                    step: 0.5,
                },
                "typology" => Range::Choice(choices("typology")),
                // Open space only costs yield: search it just above the floor.
                "open_space_reserve_frac" if open > 0.0 => Range::Float {
                    lo: open,
                    hi: (open + 0.15).min(0.9),
                    step: 0.01,
                },
                _ => return None,
            };
            Some(Dim { name, range })
        })
        .collect()
}

/// The lowest lot area / width the search tries. Unless the request sets its
/// own, they double as the `min_lot_area` / `min_lot_width` constraints, so a
/// subdivider that slivers below its targets never wins on lot count.
fn floors(base: &SubdivisionSettings, req: &OptimizeRequest) -> (f64, f64) {
    (
        req.min_lot_area
            .unwrap_or(0.0)
            .max(base.lot_area_min * 0.2)
            .max(50.0),
        req.min_lot_width
            .unwrap_or(0.0)
            .max(base.lot_width_min * 0.2)
            .max(4.0),
    )
}

/// Decode a unit-cube genome into `lotsetparams` tokens.
fn decode(dims: &[Dim], genome: &[f64], base: &SubdivisionSettings) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut area_min = base.lot_area_min;
    for (d, &u) in dims.iter().zip(genome) {
        let v = match &d.range {
            Range::Float { lo, hi, step } => {
                let v = ((lo + u * (hi - lo)) / step).round() * step;
                let v = v.clamp(*lo, *hi);
                if d.name == "lot_area_min" {
                    area_min = v;
                }
                format!("{v}")
            }
            Range::Int { lo, hi } => {
                format!("{}", (lo + (u * (hi - lo + 1) as f64) as i64).min(*hi))
            }
            Range::Choice(c) => c[((u * c.len() as f64) as usize).min(c.len() - 1)].clone(),
            Range::RatioOfMin { lo, hi } => format!(
                "{}",
                (area_min * (lo + u * (hi - lo)) / 10.0).round() * 10.0
            ),
        };
        out.insert(d.name.to_string(), v);
    }
    out
}

/// Fold `lotsetparams`-style tokens onto `base` exactly as `lotsetparams`
/// does, so an applied candidate bakes the plan that was scored.
pub fn settings_with(
    base: &SubdivisionSettings,
    params: &BTreeMap<String, String>,
) -> Result<SubdivisionSettings, String> {
    let fields = crate::subdiv_params::subdivision_fields();
    let mut map = crate::subdiv_params::settings_to_params(base);
    for (k, v) in params {
        let field = fields
            .iter()
            .find(|f| f.name == k.as_str())
            .ok_or_else(|| format!("no subdivision parameter '{k}'"))?;
        let pv = crate::exec::parse_param_value(field, v).unwrap_or_else(|| field.default.clone());
        map.insert(k.clone(), pv);
    }
    Ok(crate::subdiv_params::params_to_settings(base, &map))
}

/// Small deterministic RNG (SplitMix64) — no dependency, stable across platforms.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn below(&mut self, n: usize) -> usize {
        ((self.unit() * n as f64) as usize).min(n.saturating_sub(1))
    }

    /// Standard normal (Box–Muller).
    fn normal(&mut self) -> f64 {
        let u1 = self.unit().max(1e-12);
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * self.unit()).cos()
    }
}

/// `n` Latin-hypercube points in `[0,1)^d`: each axis is cut into `n` strata,
/// one point per stratum, strata shuffled independently per axis.
fn latin_hypercube(n: usize, d: usize, rng: &mut Rng) -> Vec<Vec<f64>> {
    let mut pts = vec![vec![0.0; d]; n];
    for j in 0..d {
        let mut strata: Vec<usize> = (0..n).collect();
        for i in (1..n).rev() {
            strata.swap(i, rng.below(i + 1));
        }
        for (p, s) in pts.iter_mut().zip(strata) {
            p[j] = (s as f64 + rng.unit()) / n as f64;
        }
    }
    pts
}

struct Trial {
    genome: Vec<f64>,
    params: BTreeMap<String, String>,
    eval: Result<Evaluation, String>,
}

impl Trial {
    /// GA selection key, smaller = better: feasible first, then fewer
    /// violations, then a higher score.
    fn fitness(&self) -> (usize, f64) {
        match &self.eval {
            Ok(e) => (e.violations.len(), -e.score),
            Err(_) => (usize::MAX, 0.0),
        }
    }
}

/// Search the site-plan params of `site` for the best feasible layouts.
pub fn optimize(
    site: &Polygon2d,
    base: &SubdivisionSettings,
    req: &OptimizeRequest,
) -> Result<OptimizeResult, String> {
    optimize_with(site, base, &PlanEdits::default(), req)
}

/// [`optimize`] with a plan's hand edits applied to every candidate.
pub fn optimize_with(
    site: &Polygon2d,
    base: &SubdivisionSettings,
    edits: &PlanEdits,
    req: &OptimizeRequest,
) -> Result<OptimizeResult, String> {
    req.validate()?;
    let (area_lo, width_lo) = floors(base, req);
    let req = &OptimizeRequest {
        min_lot_area: Some(req.min_lot_area.unwrap_or(area_lo)),
        min_lot_width: Some(req.min_lot_width.unwrap_or(width_lo)),
        min_open: req.min_open.map(ratio),
        max_coverage: req.max_coverage.map(ratio),
        ..req.clone()
    };
    let dims = dimensions(site, base, req);
    let d = dims.len();
    // Everything locked: one plan is all there is to evaluate.
    let budget = if d == 0 { 1 } else { req.budget };
    let mut rng = Rng(req.seed);
    let run = |genomes: Vec<Vec<f64>>| -> Vec<Trial> {
        genomes
            .into_par_iter()
            .map(|genome| {
                let params = decode(&dims, &genome, base);
                let eval =
                    settings_with(base, &params).and_then(|s| evaluate(site, &s, edits, req));
                Trial {
                    genome,
                    params,
                    eval,
                }
            })
            .collect()
    };

    let n_init = budget.div_ceil(2);
    let mut trials = run(latin_hypercube(n_init, d, &mut rng));
    let (mu, lambda) = (8, ((budget - n_init) / 4).max(4));
    let mut sigma = 0.2;
    while trials.len() < budget {
        let mut order: Vec<usize> = (0..trials.len()).collect();
        order.sort_by(|&a, &b| {
            let (fa, fb) = (trials[a].fitness(), trials[b].fitness());
            fa.0.cmp(&fb.0).then(fa.1.total_cmp(&fb.1)).then(a.cmp(&b))
        });
        let elite = &order[..mu.min(order.len())];
        let children: Vec<Vec<f64>> = (0..lambda.min(budget - trials.len()))
            .map(|_| {
                let (a, b) = (
                    &trials[elite[rng.below(elite.len())]].genome,
                    &trials[elite[rng.below(elite.len())]].genome,
                );
                dims.iter()
                    .enumerate()
                    .map(|(i, dim)| {
                        let g = if rng.unit() < 0.5 { a[i] } else { b[i] };
                        if dim.is_discrete() {
                            if rng.unit() < 0.2 { rng.unit() } else { g }
                        } else {
                            (g + sigma * rng.normal()).clamp(0.0, 0.999_999)
                        }
                    })
                    .collect()
            })
            .collect();
        trials.extend(run(children));
        sigma *= 0.8;
    }

    rank(trials, req)
}

/// Rank the feasible trials, drop layouts whose yield duplicates a better one,
/// and mark the (score, open-space) Pareto front. With nothing feasible, name
/// the constraint broken most often.
fn rank(trials: Vec<Trial>, req: &OptimizeRequest) -> Result<OptimizeResult, String> {
    let evaluated = trials.len();
    let mut feasible: Vec<(usize, Trial)> = Vec::new();
    let mut broken: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut first_err = None;
    for (i, t) in trials.into_iter().enumerate() {
        match &t.eval {
            Ok(e) if e.violations.is_empty() => feasible.push((i, t)),
            Ok(e) => e
                .violations
                .iter()
                .for_each(|v| *broken.entry(v).or_default() += 1),
            Err(err) => {
                first_err.get_or_insert_with(|| err.clone());
            }
        }
    }
    let n_feasible = feasible.len();
    if feasible.is_empty() {
        let Some((name, n)) = broken
            .iter()
            .max_by_key(|(_, n)| **n)
            .map(|(k, n)| (*k, *n))
        else {
            return Err(format!(
                "siteoptimize: none of the {evaluated} candidates produced a site plan ({})",
                first_err.unwrap_or_default()
            ));
        };
        let limit = match name {
            "min_lot_area" => req.min_lot_area,
            "min_lot_width" => req.min_lot_width,
            "min_frontage" => req.min_frontage,
            "min_open" => req.min_open,
            "max_far" => req.max_far,
            "max_coverage" => req.max_coverage,
            _ => None,
        };
        let what = limit.map_or("every lot fronts a street".to_string(), |v| {
            format!("{name}={v}")
        });
        return Err(format!(
            "siteoptimize: no feasible layout in {evaluated} evaluations — binding constraint: {name} \
             ({what}) violated by {n}/{evaluated}; relax it or unlock more params"
        ));
    }

    let eval = |t: &Trial| {
        t.eval
            .as_ref()
            .ok()
            .cloned()
            .expect("feasible trials evaluated")
    };
    feasible.sort_by(|(ia, a), (ib, b)| {
        let (a, b) = (eval(a), eval(b));
        b.score
            .total_cmp(&a.score)
            .then(
                b.report
                    .open_space_ratio
                    .total_cmp(&a.report.open_space_ratio),
            )
            .then(ia.cmp(ib))
    });
    let mut seen = std::collections::BTreeSet::new();
    let unique: Vec<Candidate> = feasible
        .into_iter()
        .filter_map(|(_, t)| {
            let e = eval(&t);
            let r = &e.report;
            let key = (
                r.lot_count,
                r.total_lot_area.round() as i64,
                r.total_gfa.round() as i64,
                (r.open_space_ratio * 1e4).round() as i64,
            );
            seen.insert(key).then_some((t.params, e))
        })
        .enumerate()
        .map(|(i, (params, eval))| Candidate {
            rank: i + 1,
            params,
            eval,
        })
        .collect();
    let dominated = |c: &Candidate| {
        unique.iter().any(|o| {
            let (s, os) = (c.eval.score, o.eval.score);
            let (p, op) = (
                c.eval.report.open_space_ratio,
                o.eval.report.open_space_ratio,
            );
            // A better-ranked tie also dominates, so the front lists one per point.
            os >= s && op >= p && (os > s || op > p || o.rank < c.rank)
        })
    };
    let pareto: Vec<usize> = unique
        .iter()
        .filter(|c| !dominated(c))
        .map(|c| c.rank)
        .collect();
    let candidates = unique
        .into_iter()
        .filter(|c| c.rank <= req.top.max(1) || pareto.contains(&c.rank))
        .collect();
    Ok(OptimizeResult {
        objective: req.objective,
        candidates,
        pareto,
        evaluated,
        feasible: n_feasible,
    })
}

impl OptimizeResult {
    /// The candidate with `rank`, if listed.
    pub fn get(&self, rank: usize) -> Option<&Candidate> {
        self.candidates.iter().find(|c| c.rank == rank)
    }

    /// A Markdown table of the candidates (the chat renders tables).
    pub fn to_markdown(&self) -> String {
        let mut s = format!(
            "**Site optimize — {}** ({} evaluated, {} feasible)\n\n\
             | # | score | lots | lot area m² | FAR net | open % | params |\n|---|---|---|---|---|---|---|\n",
            self.objective.token(),
            self.evaluated,
            self.feasible
        );
        for c in &self.candidates {
            let r = &c.eval.report;
            let params: Vec<String> = SEARCHED
                .iter()
                .filter_map(|k| c.params.get(*k).map(|v| format!("{k}={v}")))
                .collect();
            s.push_str(&format!(
                "| {} | {:.2} | {} | {:.0} | {} | {:.1} | {} |\n",
                c.rank,
                c.eval.score,
                r.lot_count,
                r.total_lot_area,
                r.far_net.map_or("—".into(), |f| format!("{:.2}", f + 0.0)),
                r.open_space_ratio * 100.0 + 0.0,
                params.join(" ")
            ));
        }
        let front: Vec<String> = self.pareto.iter().map(|r| format!("#{r}")).collect();
        s.push_str(&format!(
            "\nPareto (score vs open space): {}. Bake one with `siteoptimize apply <rank>`.",
            front.join(", ")
        ));
        s
    }

    /// `{"candidates":[{"rank","score","params":{…},"yield":{…}}],"pareto":[…],"evaluated","feasible"}`.
    /// Params are typed (numbers / enum tokens) per the subdivision schema.
    pub fn to_json(&self) -> serde_json::Value {
        let fields = crate::subdiv_params::subdivision_fields();
        let typed = |k: &str, v: &str| -> serde_json::Value {
            match fields.iter().find(|f| f.name == k).map(|f| f.kind) {
                Some(FieldKind::Float | FieldKind::Int) => v.parse::<f64>().map_or(v.into(), |n| {
                    if n.fract() == 0.0 && n.abs() < 1e15 {
                        (n as i64).into()
                    } else {
                        n.into()
                    }
                }),
                _ => v.into(),
            }
        };
        let candidates: Vec<serde_json::Value> = self
            .candidates
            .iter()
            .map(|c| {
                let params: serde_json::Map<String, serde_json::Value> =
                    c.params.iter().map(|(k, v)| (k.clone(), typed(k, v))).collect();
                serde_json::json!({ "rank": c.rank, "score": c.eval.score, "params": params, "yield": c.eval.report })
            })
            .collect();
        serde_json::json!({
            "candidates": candidates,
            "pareto": self.pareto,
            "evaluated": self.evaluated,
            "feasible": self.feasible,
        })
    }
}

/// JSON params (numbers or tokens) → `lotsetparams` tokens.
pub fn params_from_json(v: &serde_json::Value) -> Result<BTreeMap<String, String>, String> {
    let obj = v.as_object().ok_or("params must be a JSON object")?;
    obj.iter()
        .map(|(k, v)| match v {
            serde_json::Value::String(s) => Ok((k.clone(), s.clone())),
            serde_json::Value::Number(n) => Ok((k.clone(), n.to_string())),
            serde_json::Value::Bool(b) => Ok((k.clone(), b.to_string())),
            _ => Err(format!("param '{k}' must be a number, string or bool")),
        })
        .collect()
}

/// The site boundary among `ids`: the largest closed curve (as `siteplan`).
pub fn site_boundary(doc: &Document, ids: &[ObjectId]) -> Option<(ObjectId, Polygon2d)> {
    ids.iter()
        .filter_map(|id| match &doc.get(*id)?.geometry {
            Geometry::Curve(c) if c.is_closed() => Some((*id, lot::curve_to_polygon(c)?)),
            _ => None,
        })
        .fold(
            None,
            |best: Option<(ObjectId, Polygon2d)>, (id, p)| match best {
                Some(b) if b.1.area() >= p.area() => Some(b),
                _ => Some((id, p)),
            },
        )
}

/// The settings and hand edits a search starts from: the boundary's existing
/// site plan when it has one (apply then replaces it through `lotsetparams`,
/// which keeps its street edits and the edge role overrides that still match),
/// else the sticky `lotsettings` and no edits.
pub fn base_plan(doc: &Document, source: ObjectId) -> (SubdivisionSettings, PlanEdits) {
    match doc.subdivision_links.get(&source) {
        Some(l) if l.kind == itsjustcad_doc::SubdivKind::SitePlan => (
            l.settings.clone(),
            PlanEdits {
                streets: l.streets.clone(),
                edge_roles: l.edge_roles.clone(),
                lot_heights: l.lot_heights.clone(),
            },
        ),
        _ => (doc.subdivision_settings.clone(), PlanEdits::default()),
    }
}

/// Optimize the site boundary among `ids` in `doc`.
pub fn optimize_doc(
    doc: &Document,
    ids: &[ObjectId],
    req: &OptimizeRequest,
) -> Result<(ObjectId, OptimizeResult), String> {
    let (source, site) = site_boundary(doc, ids)
        .ok_or("siteoptimize needs a closed site boundary curve (draw or select one first)")?;
    let (base, edits) = base_plan(doc, source);
    Ok((source, optimize_with(&site, &base, &edits, req)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Session, parse};

    fn rect(w: f64, h: f64) -> Polygon2d {
        Polygon2d::new(vec![
            glam::DVec2::new(0.0, 0.0),
            glam::DVec2::new(w, 0.0),
            glam::DVec2::new(w, h),
            glam::DVec2::new(0.0, h),
        ])
        .unwrap()
    }

    fn req(budget: usize) -> OptimizeRequest {
        OptimizeRequest {
            budget,
            ..OptimizeRequest::default()
        }
    }

    fn ranking(r: &OptimizeResult) -> Vec<(usize, BTreeMap<String, String>, f64)> {
        r.candidates
            .iter()
            .map(|c| (c.rank, c.params.clone(), c.eval.score))
            .collect()
    }

    fn run(s: &mut Session, line: &str) -> crate::ApplyOutcome {
        s.run(parse(line).unwrap())
            .unwrap_or_else(|e| panic!("{line}: {e}"))
    }

    #[test]
    fn same_seed_same_ranking_whatever_the_thread_count() {
        let site = rect(200.0, 150.0);
        let base = SubdivisionSettings::default();
        let pool = |n| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(n)
                .build()
                .unwrap()
        };
        let one = pool(1).install(|| optimize(&site, &base, &req(24)).unwrap());
        let four = pool(4).install(|| optimize(&site, &base, &req(24)).unwrap());
        assert_eq!(ranking(&one), ranking(&four));
        assert_eq!(
            ranking(&one),
            ranking(&optimize(&site, &base, &req(24)).unwrap())
        );
        let other = optimize(&site, &base, &OptimizeRequest { seed: 2, ..req(24) }).unwrap();
        assert_ne!(ranking(&one), ranking(&other), "the seed drives the search");
    }

    /// Acceptance (#145): on a 200×150 rectangle, objective=lots budget=200
    /// beats the default `siteplan` lot count while meeting every constraint.
    #[test]
    fn beats_default_siteplan_lot_count() {
        let site = rect(200.0, 150.0);
        let base = SubdivisionSettings::default();
        let default_lots = lot::plan_site(&site, 0.0, &base, &BTreeMap::new(), &mut Vec::new(), &mut Vec::new())
            .unwrap()
            .lots
            .polygons
            .len();
        let t = std::time::Instant::now();
        let r = optimize(&site, &base, &req(200)).unwrap();
        let best = &r.candidates[0];
        eprintln!(
            "budget 200: {:?}, best {} lots, default {default_lots} lots",
            t.elapsed(),
            best.eval.report.lot_count
        );
        assert_eq!(best.rank, 1);
        assert!(
            best.eval.report.lot_count > default_lots,
            "{} vs {default_lots}",
            best.eval.report.lot_count
        );
        assert!(r.candidates.iter().all(|c| c.eval.violations.is_empty()));
        assert!(r.pareto.contains(&1), "the best score is on the front");
    }

    #[test]
    fn constraints_hold_for_every_candidate() {
        let r = optimize(
            &rect(200.0, 150.0),
            &SubdivisionSettings::default(),
            &OptimizeRequest {
                min_open: Some(0.15),
                min_frontage: Some(15.0),
                ..req(40)
            },
        )
        .unwrap();
        for c in &r.candidates {
            let y = &c.eval.report;
            assert!(y.open_space_ratio >= 0.15, "open {}", y.open_space_ratio);
            assert!(y.frontage.as_ref().unwrap().min >= 15.0);
            assert!(
                c.params.contains_key("open_space_reserve_frac"),
                "reserve searched"
            );
        }
    }

    #[test]
    fn infeasible_names_the_binding_constraint() {
        let err = optimize(
            &rect(200.0, 150.0),
            &SubdivisionSettings::default(),
            &OptimizeRequest {
                max_coverage: Some(0.5),
                min_lot_area: Some(20_000.0),
                ..req(12)
            },
        )
        .unwrap_err();
        assert!(err.contains("binding constraint: min_lot_area"), "{err}");
    }

    #[test]
    fn bad_requests_are_rejected() {
        let site = rect(200.0, 150.0);
        let base = SubdivisionSettings::default();
        let bad = OptimizeRequest {
            lock: vec!["nope".into()],
            ..req(4)
        };
        assert!(
            optimize(&site, &base, &bad)
                .unwrap_err()
                .contains("cannot lock 'nope'")
        );
        let bad = OptimizeRequest {
            objective: Objective::Weighted,
            ..req(4)
        };
        assert!(
            optimize(&site, &base, &bad)
                .unwrap_err()
                .contains("needs weights")
        );
        assert!(parse("siteoptimize objective=height").is_err());
    }

    #[test]
    fn apply_bakes_one_siteplan_link_in_one_undo_step() {
        use itsjustcad_doc::SubdivKind;
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 200 150");
        let source = s.doc.objects().next().unwrap().id;
        let objects = s.doc.objects().count();

        let out = run(&mut s, "siteoptimize last budget=12 lock=street_pattern");
        assert!(out.message.contains("| 1 |"), "{}", out.message);
        let undo_len = s.undo_len();
        assert_eq!(s.doc.objects().count(), objects, "the search bakes nothing");

        let chosen = s
            .last_site_optimize
            .as_ref()
            .unwrap()
            .1
            .get(1)
            .unwrap()
            .clone();
        let out = run(&mut s, "siteoptimize apply 1");
        assert_eq!(s.undo_len(), undo_len + 1, "one op");
        let link = s.doc.subdivision_links.get(&source).unwrap().clone();
        assert_eq!(link.kind, SubdivKind::SitePlan);
        let want = settings_with(&s.doc.subdivision_settings, &chosen.params).unwrap();
        assert_eq!(link.settings, want, "the link carries the chosen params");
        let lots = |s: &Session| {
            s.doc
                .objects()
                .filter(|o| o.layer == lot::LOTS_LAYER)
                .count()
        };
        assert_eq!(
            lots(&s),
            chosen.eval.report.lot_count,
            "baked plan is the scored plan"
        );
        assert_eq!(link.produced, out.created);

        // Applying again replaces the plan; one undo brings the first one back.
        let second = s
            .last_site_optimize
            .as_ref()
            .unwrap()
            .1
            .candidates
            .last()
            .unwrap()
            .rank;
        run(&mut s, &format!("siteoptimize apply {second}"));
        run(&mut s, "undo");
        assert_eq!(s.doc.subdivision_links.get(&source).unwrap().settings, want);
        assert_eq!(lots(&s), chosen.eval.report.lot_count);
        run(&mut s, "undo");
        assert!(s.doc.subdivision_links.is_empty());
        assert_eq!(
            s.doc.objects().count(),
            objects,
            "undo restores the bare site"
        );
    }

    /// #143: on a boundary whose plan has a `streetset` edit and an
    /// `edgerole` override, the search scores with both, and apply keeps both
    /// (the lots did not move, so the override still matches).
    #[test]
    fn apply_keeps_street_edits_and_matching_edge_roles() {
        use crate::{Command, Selector};
        use subdivision::EdgeRole::{Front, Rear};
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 600 450");
        let source = s.doc.objects().next().unwrap().id;
        run(&mut s, "siteplan last");
        let (road, info) = s
            .doc
            .objects()
            .find_map(|o| Some((o.id, o.street.clone()?)))
            .unwrap();
        s.run(Command::StreetSet {
            targets: Selector::Ids { ids: vec![road] },
            name: Some("Main".into()),
            class: None,
            width: Some(info.width + 4.0),
            ids: None,
        })
        .unwrap();
        let (lot, rear) = s
            .doc
            .objects()
            .filter(|o| o.layer == lot::LOTS_LAYER)
            .find_map(|o| {
                let t = o.valid_edge_tags()?;
                let one = |r| t.iter().filter(|e| e.role == Some(r)).count() == 1;
                (one(Front) && one(Rear))
                    .then(|| (o.id, t.iter().position(|e| e.role == Some(Rear)).unwrap()))
            })
            .unwrap();
        s.run(Command::EdgeRole {
            targets: Selector::Ids { ids: vec![lot] },
            edge: rear,
            role: Some(Front),
            ids: None,
        })
        .unwrap();
        let before = s.doc.subdivision_links[&source].clone();
        assert_eq!(before.edge_roles.len(), 1);

        let Command::SiteOptimize { request, .. } = parse(
            "siteoptimize last budget=4 lock=seed,street_pattern,road_width,block_depth,\
             method,lot_area_min,lot_area_max,lot_width_min",
        )
        .unwrap() else {
            unreachable!()
        };
        s.run(Command::SiteOptimize {
            targets: Selector::Ids { ids: vec![source] },
            request,
        })
        .unwrap();
        let chosen = s
            .last_site_optimize
            .as_ref()
            .unwrap()
            .1
            .get(1)
            .unwrap()
            .clone();
        let out = run(&mut s, "siteoptimize apply 1");
        assert!(!out.message.contains("dropped"), "{}", out.message);
        let link = &s.doc.subdivision_links[&source];
        assert_eq!(link.streets, before.streets, "street edits kept");
        assert_eq!(link.edge_roles, before.edge_roles, "override kept");
        assert!(
            s.doc
                .objects()
                .filter_map(|o| o.valid_edge_tags())
                .any(|t| t.iter().any(|e| e.overridden)),
            "the override is baked"
        );
        let lots = s
            .doc
            .objects()
            .filter(|o| o.layer == lot::LOTS_LAYER)
            .count();
        assert_eq!(
            lots, chosen.eval.report.lot_count,
            "baked plan is the scored plan"
        );
    }

    #[test]
    fn applied_open_space_reserve_bakes_and_undoes() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 200 150");
        run(&mut s, "siteoptimize last budget=8 min_open=0.15");
        let want = s
            .last_site_optimize
            .as_ref()
            .unwrap()
            .1
            .get(1)
            .unwrap()
            .eval
            .report
            .clone();
        run(&mut s, "siteoptimize apply 1");
        let open: f64 = s
            .doc
            .objects()
            .filter(|o| o.layer == lot::OPENSPACE_LAYER)
            .filter_map(|o| match &o.geometry {
                Geometry::Curve(c) => lot::curve_to_polygon(c).map(|p| p.area()),
                _ => None,
            })
            .sum();
        assert!(
            (open / want.gross_site_area - want.open_space_ratio).abs() < 1e-9,
            "reserve baked as scored"
        );
        run(&mut s, "undo");
        assert!(!s.doc.layers.contains_key(lot::OPENSPACE_LAYER));
    }

    #[test]
    fn apply_without_a_search_errors() {
        let mut s = Session::default();
        let err = s
            .run(parse("siteoptimize apply 1").unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("run `siteoptimize` first"), "{err}");
    }
}
