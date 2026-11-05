// SPDX-License-Identifier: AGPL-3.0-only

//! What a search keeps of the plans it evaluated ([`Tally`]), and the saved
//! form of a stopped search that continues it (#226, [`SearchState`]).
//!
//! Every genome has a `seq`: its place in the order the search draws them
//! from the one seeded RNG. Wherever two plans compare equal, `seq` breaks
//! the tie (it used to be the position in the list of trials), so a search
//! stopped and continued orders its plans as an unbroken one would, whatever
//! the chunk a stop cut short. A batch (the Latin hypercube, a generation) is
//! saved as the RNG state and parents that drew it, not as its genomes: a
//! continued search draws it again and skips the genomes already evaluated.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{
    Dim, Evaluation, PARETO_AXES, Rng, cmp_keys, intern, latin_hypercube,
};

/// The [`SearchState`] format this build writes and continues. Bump it
/// whenever the search draws, breeds or ranks differently.
pub const SEARCH_STATE_VERSION: u32 = 1;

/// The elite size μ: the fittest plans the next generation is bred from.
pub(super) const MU: usize = 8;

/// The first generation's mutation step; each later one takes 0.8 of it.
pub(super) const SIGMA0: f64 = 0.2;

/// Resuming a run saved by a build whose search differs.
pub const RESUME_OLD: &str =
    "siteoptimize: this run was saved by another version of ItsJustCAD and cannot be continued; \
     start a new run";

/// Resuming a run whose plan changed since.
pub const RESUME_STALE: &str =
    "siteoptimize: the plan changed since this run, so it cannot be continued; start a new run";

/// Resuming a run that finished, or has no saved search.
pub const RESUME_NONE: &str =
    "siteoptimize: this run cannot be continued (it finished, or was saved without its search); \
     start a new run";

/// One plan the search drew.
#[derive(Clone)]
pub(super) struct Trial {
    pub seq: usize,
    pub genome: Vec<f64>,
    pub params: BTreeMap<String, String>,
    pub out: Outcome,
}

#[derive(Clone)]
pub(super) enum Outcome {
    Eval(Box<Evaluation>),
    Failed(String),
    /// Restored from a [`SearchState`]: what ranking reads. `replay`: the
    /// genome was kept, so the plan is evaluated again if it is listed.
    Saved { sum: Summary, replay: bool },
}

/// What selection and ranking read of an evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    infeasibility: f64,
    score: f64,
    net_area: f64,
    gfa: f64,
    open: f64,
    lots: usize,
}

impl Summary {
    pub(super) fn of(e: &Evaluation) -> Summary {
        let r = &e.report;
        Summary {
            infeasibility: e.infeasibility,
            score: e.score,
            net_area: r.total_lot_area,
            gfa: r.total_gfa,
            open: r.open_space_ratio,
            lots: r.lot_count,
        }
    }

    pub(super) fn score(&self) -> f64 {
        self.score
    }

    /// [`super::sort_key`].
    pub(super) fn key(&self) -> [f64; 4] {
        [-self.score, -self.net_area, -self.gfa, -self.open]
    }

    /// Two layouts with the same yield are duplicates; ranking keeps the
    /// better.
    fn yield_key(&self) -> (usize, i64, i64, i64) {
        (
            self.lots,
            self.net_area.round() as i64,
            self.gfa.round() as i64,
            (self.open * 1e4).round() as i64,
        )
    }

    /// A [`PARETO_AXES`] metric ([`super::metric`]).
    pub(super) fn metric(&self, axis: &str) -> f64 {
        match axis {
            "lots" => self.lots as f64,
            "net_area" => self.net_area,
            "gfa" => self.gfa,
            "open" => self.open,
            _ => 0.0,
        }
    }
}

impl Trial {
    /// A ranked plan, with its evaluation.
    pub(super) fn listed(params: BTreeMap<String, String>, e: Evaluation) -> Trial {
        Trial {
            seq: 0,
            genome: Vec::new(),
            params,
            out: Outcome::Eval(Box::new(e)),
        }
    }

    pub(super) fn summary(&self) -> Option<Summary> {
        match &self.out {
            Outcome::Eval(e) => Some(Summary::of(e)),
            Outcome::Saved { sum, .. } => Some(*sum),
            Outcome::Failed(_) => None,
        }
    }

    /// GA selection key, smaller = better: less infeasible first (0 =
    /// feasible), then a higher score, then the [`super::TIE_BREAK`] metrics.
    fn fitness(&self) -> (f64, [f64; 4]) {
        self.summary()
            .map_or((f64::INFINITY, [0.0; 4]), |s| (s.infeasibility, s.key()))
    }

    /// Fitter than `o` (then drawn first).
    fn fitter(&self, o: &Trial) -> bool {
        let (a, b) = (self.fitness(), o.fitness());
        a.0.total_cmp(&b.0)
            .then(cmp_keys(&a.1, &b.1))
            .then(self.seq.cmp(&o.seq))
            .is_lt()
    }

    /// Ranked before `o`: by [`super::sort_key`], then drawn first.
    pub(super) fn ranks_before(&self, o: &Trial) -> std::cmp::Ordering {
        let key = |t: &Trial| t.summary().map_or([0.0; 4], |s| s.key());
        cmp_keys(&key(self), &key(o)).then(self.seq.cmp(&o.seq))
    }

    /// The selection-only copy kept in the elite.
    fn parent(&self) -> Trial {
        Trial {
            seq: self.seq,
            genome: self.genome.clone(),
            params: BTreeMap::new(),
            out: match self.summary() {
                Some(sum) => Outcome::Saved { sum, replay: true },
                None => Outcome::Failed(String::new()),
            },
        }
    }
}

/// What a search keeps of the plans it evaluated: their counts, the elite
/// the next generation is bred from, and the best feasible plan of each
/// yield, which ranking lists. The same whatever order plans arrive in.
#[derive(Clone, Default)]
pub(super) struct Tally {
    pub evaluated: usize,
    pub feasible: usize,
    pub best: Option<f64>,
    /// How many plans broke each constraint.
    pub broken: BTreeMap<&'static str, usize>,
    /// The first failure, by seq.
    pub first_err: Option<(usize, String)>,
    /// The [`MU`] fittest plans, fittest first.
    pub elite: Vec<Trial>,
    unique: BTreeMap<(usize, i64, i64, i64), Trial>,
}

impl Tally {
    pub(super) fn add(&mut self, t: Trial) {
        self.evaluated += 1;
        let feasible = match &t.out {
            Outcome::Eval(e) if e.violations.is_empty() => {
                self.feasible += 1;
                self.best = Some(self.best.map_or(e.score, |b| b.max(e.score)));
                true
            }
            Outcome::Eval(e) => {
                for v in &e.violations {
                    *self.broken.entry(*v).or_default() += 1;
                }
                false
            }
            Outcome::Failed(err) => {
                if self.first_err.as_ref().is_none_or(|(s, _)| t.seq < *s) {
                    self.first_err = Some((t.seq, err.clone()));
                }
                false
            }
            Outcome::Saved { .. } => false,
        };
        let at = self.elite.partition_point(|o| o.fitter(&t));
        if at < MU {
            self.elite.insert(at, t.parent());
            self.elite.truncate(MU);
        }
        if feasible {
            self.list(t);
        }
    }

    /// Keep `t` if it is its yield's best feasible plan.
    fn list(&mut self, t: Trial) {
        let Some(sum) = t.summary() else { return };
        match self.unique.entry(sum.yield_key()) {
            std::collections::btree_map::Entry::Vacant(v) => {
                v.insert(t);
            }
            std::collections::btree_map::Entry::Occupied(mut o) => {
                if t.ranks_before(o.get()).is_lt() {
                    o.insert(t);
                }
            }
        }
    }

    /// The listed plans, best first.
    pub(super) fn ranked(self) -> Vec<Trial> {
        let mut v: Vec<Trial> = self.unique.into_values().collect();
        v.sort_by(|a, b| a.ranks_before(b));
        v
    }

    /// [`Tally::ranked`], borrowed.
    fn listed(&self) -> Vec<&Trial> {
        let mut v: Vec<&Trial> = self.unique.values().collect();
        v.sort_by(|a, b| a.ranks_before(b));
        v
    }
}

/// The genomes the search draws next: the Latin hypercube (no parents) or a
/// generation bred from the elite.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct Batch {
    /// The seq of its first genome.
    pub start: usize,
    pub len: usize,
    /// The RNG state it is drawn from.
    pub rng: u64,
    pub sigma: f64,
    pub parents: Vec<Vec<f64>>,
}

impl Batch {
    /// Its genomes, and the RNG after drawing them.
    pub(super) fn genomes(&self, dims: &[Dim]) -> (Vec<Vec<f64>>, Rng) {
        let mut rng = Rng(self.rng);
        let g = if self.parents.is_empty() {
            latin_hypercube(self.len, dims.len(), &mut rng)
        } else {
            breed(&self.parents, self.len, dims, self.sigma, &mut rng)
        };
        (g, rng)
    }

    /// The generation after this batch, bred from `elite` with the RNG left
    /// after drawing this one.
    pub(super) fn next(&self, rng: Rng, len: usize, elite: &[Trial]) -> Batch {
        Batch {
            start: self.start + self.len,
            len,
            rng: rng.0,
            sigma: if self.parents.is_empty() {
                SIGMA0
            } else {
                self.sigma * 0.8
            },
            parents: elite.iter().map(|t| t.genome.clone()).collect(),
        }
    }
}

/// `n` children of `parents`: uniform crossover of two picked at random, then
/// a Gaussian step of `sigma` on the continuous genes and a 20 % resample of
/// the discrete ones.
fn breed(parents: &[Vec<f64>], n: usize, dims: &[Dim], sigma: f64, rng: &mut Rng) -> Vec<Vec<f64>> {
    (0..n)
        .map(|_| {
            let (a, b) = (
                &parents[rng.below(parents.len())],
                &parents[rng.below(parents.len())],
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
        .collect()
}

/// A saved plan: its seq, its genome when it may be listed or bred from, and
/// what ranking reads (`None`: it failed to plan).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct SavedTrial {
    seq: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    genome: Option<Vec<f64>>,
    summary: Option<Summary>,
}

/// A stopped search, saved with its run so it can be continued (#226): the
/// batch it was drawing and what it had not evaluated of it, the elite, the
/// counts, and every listed plan's ranking numbers (genomes only for the
/// plans that can still be listed: the best ones ranking lists or measures
/// daylight on, and the Pareto fronts). A continued search spends only the rest of the budget and
/// ends with the ranking the unbroken search would have.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchState {
    pub version: u32,
    /// The searched dimensions: a check that the plan and request match.
    dims: Vec<String>,
    budget: usize,
    batch: Batch,
    /// Offsets in `batch` still to evaluate: `skipped`, then `taken..len`.
    skipped: Vec<usize>,
    taken: usize,
    evaluated: usize,
    feasible: usize,
    best: Option<f64>,
    broken: BTreeMap<String, usize>,
    first_err: Option<(usize, String)>,
    elite: Vec<SavedTrial>,
    unique: Vec<SavedTrial>,
}

/// How far a duplicate's metric may drift from the plan it replaced (dedup
/// rounds net area and GFA to 1 m² and open space to 0.01 %), doubled.
fn drift(axis: &str) -> f64 {
    match axis {
        "lots" => 0.5,
        "open" => 2e-4,
        _ => 2.0,
    }
}

/// The plans of `sums` no later plan can push off the (score, `axis`) front
/// and that could still be listed: those no other plan beats on `axis` by
/// more than [`drift`] at an equal or higher score.
fn may_front(sums: &[Summary], axis: &str) -> Vec<bool> {
    let mut idx: Vec<usize> = (0..sums.len()).collect();
    idx.sort_by(|&a, &b| sums[b].score.total_cmp(&sums[a].score));
    let mut out = vec![false; sums.len()];
    let mut best = f64::NEG_INFINITY;
    let mut g = 0;
    while g < idx.len() {
        let s = sums[idx[g]].score;
        let h = g + idx[g..].iter().take_while(|&&i| sums[i].score == s).count();
        best = idx[g..h]
            .iter()
            .map(|&i| sums[i].metric(axis))
            .fold(best, f64::max);
        for &i in &idx[g..h] {
            out[i] = best < sums[i].metric(axis) + drift(axis);
        }
        g = h;
    }
    out
}

impl SearchState {
    /// The search at `batch`, with `todo` (offsets in it, the tail
    /// ascending) still to evaluate. The best `listed` plans keep their
    /// genomes, besides the Pareto fronts.
    pub(super) fn capture(
        tally: &Tally,
        dims: &[Dim],
        budget: usize,
        listed: usize,
        batch: &Batch,
        todo: &[usize],
    ) -> SearchState {
        let mut skipped = todo.to_vec();
        let mut taken = batch.len;
        while taken > 0 && skipped.last() == Some(&(taken - 1)) {
            skipped.pop();
            taken -= 1;
        }
        let save = |t: &Trial, genome: bool| SavedTrial {
            seq: t.seq,
            genome: genome.then(|| t.genome.clone()),
            summary: t.summary(),
        };
        let best = listed;
        let listed = tally.listed();
        let sums: Vec<Summary> = listed.iter().filter_map(|t| t.summary()).collect();
        let fronts: Vec<Vec<bool>> = PARETO_AXES.iter().map(|a| may_front(&sums, a)).collect();
        let keep = |i: usize, t: &Trial| {
            let replay = !matches!(t.out, Outcome::Saved { replay: false, .. });
            replay && (i < best || fronts.iter().any(|f| f[i]))
        };
        SearchState {
            version: SEARCH_STATE_VERSION,
            dims: dims.iter().map(|d| d.name.to_string()).collect(),
            budget,
            batch: batch.clone(),
            skipped,
            taken,
            evaluated: tally.evaluated,
            feasible: tally.feasible,
            best: tally.best,
            broken: tally
                .broken
                .iter()
                .map(|(k, n)| (k.to_string(), *n))
                .collect(),
            first_err: tally.first_err.clone(),
            elite: tally.elite.iter().map(|t| save(t, true)).collect(),
            unique: listed
                .iter()
                .enumerate()
                .map(|(i, t)| save(t, keep(i, t)))
                .collect(),
        }
    }

    /// Plans evaluated before the stop.
    pub fn evaluated(&self) -> usize {
        self.evaluated
    }

    /// Whether this build can continue it.
    pub fn is_current(&self) -> bool {
        self.version == SEARCH_STATE_VERSION
    }

    /// The tally, the batch and the offsets still to evaluate, for a search
    /// of `dims` with `budget`; [`RESUME_OLD`] when it does not match.
    pub(super) fn restore(
        &self,
        dims: &[Dim],
        budget: usize,
    ) -> Result<(Tally, Batch, Vec<usize>), String> {
        let names: Vec<&str> = dims.iter().map(|d| d.name).collect();
        if !self.is_current()
            || self.dims != names
            || self.budget != budget
            || self.batch.len > budget
            || self.taken > self.batch.len
            || self.skipped.iter().any(|&k| k >= self.batch.len)
            || self.batch.parents.iter().any(|p| p.len() != dims.len())
        {
            return Err(RESUME_OLD.into());
        }
        let trial = |s: &SavedTrial| Trial {
            seq: s.seq,
            genome: s.genome.clone().unwrap_or_default(),
            params: BTreeMap::new(),
            out: match s.summary {
                Some(sum) => Outcome::Saved {
                    sum,
                    replay: s.genome.as_ref().is_some_and(|g| g.len() == dims.len()),
                },
                None => Outcome::Failed(String::new()),
            },
        };
        let mut tally = Tally {
            evaluated: self.evaluated,
            feasible: self.feasible,
            best: self.best,
            broken: self.broken.iter().map(|(k, n)| (intern(k), *n)).collect(),
            first_err: self.first_err.clone(),
            elite: self.elite.iter().map(trial).collect(),
            unique: BTreeMap::new(),
        };
        if tally.elite.iter().any(|t| t.genome.len() != dims.len()) {
            return Err(RESUME_OLD.into());
        }
        for s in &self.unique {
            tally.list(trial(s));
        }
        let todo = self
            .skipped
            .iter()
            .copied()
            .chain(self.taken..self.batch.len)
            .collect();
        Ok((tally, self.batch.clone(), todo))
    }
}

/// A saved run's search state, or `None` when it is missing or this build
/// cannot read it: a run from a newer build still loads, just not resumable.
pub fn de_lenient<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<SearchState>, D::Error> {
    let v = Option::<serde_json::Value>::deserialize(d)?;
    Ok(v.and_then(|v| serde_json::from_value(v).ok()))
}
