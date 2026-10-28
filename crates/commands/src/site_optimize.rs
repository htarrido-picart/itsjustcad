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
//!
//! The default objective is `value`: profit per m² of gross site area, from a
//! simple land-development ledger ([`Economics`] → [`Ledger`]): lot land sold
//! at a price per m², minus road, utility (per m of lot frontage), per-lot and
//! open-space costs. The current plan is scored too, so a search that finds
//! nothing better says so.

use std::collections::BTreeMap;

use itsjustcad_doc::{
    Document, EdgeRoleOverride, FieldKind, Geometry, LotOverride, ObjectId, StreetEdit,
};
use glam::DVec2;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use subdivision::{FrontageAt, Polygon2d, SubdivisionSettings, YieldInputs, YieldReport};

use crate::lot::{self, SitePlanBake};
pub use crate::site_metrics::{DaylightOpts, DaylightPeriod, DaylightSource, FacadeDaylight};
use crate::site_metrics::facade_daylight;

/// What to maximize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Objective {
    /// Profit per m² of gross site area ([`Ledger::value`]).
    #[default]
    Value,
    Lots,
    FarNet,
    /// Σ lot area — the land actually sold as lots.
    NetArea,
    Gfa,
    /// Dwellings: GFA through the plan's unit-size rule
    /// ([`SubdivisionSettings::dwelling_rule`]).
    Dwellings,
    /// Σ weight × metric over `weights` (`lots`, `far`, `net_area`, `gfa`,
    /// `open`, `dwellings`, `density`, …).
    Weighted,
}

impl Objective {
    fn parse(s: &str) -> Option<Objective> {
        match s.to_ascii_lowercase().as_str() {
            "value" | "profit" => Some(Objective::Value),
            "lots" => Some(Objective::Lots),
            "far" | "far_net" => Some(Objective::FarNet),
            "net_area" | "area" => Some(Objective::NetArea),
            "gfa" => Some(Objective::Gfa),
            "dwellings" | "units" => Some(Objective::Dwellings),
            "weighted" => Some(Objective::Weighted),
            _ => None,
        }
    }

    fn token(self) -> &'static str {
        match self {
            Objective::Value => "value",
            Objective::Lots => "lots",
            Objective::FarNet => "far_net",
            Objective::NetArea => "net_area",
            Objective::Gfa => "gfa",
            Objective::Dwellings => "dwellings",
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
    /// Number of site plans evaluated (default 400: on a ~80 ha walkable
    /// site, seeds then agree within ~1 % in ~7 s).
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
    /// Largest lot, m² (none by default; the desktop form seeds 3 × the
    /// plan's `lot_area_min`).
    pub max_lot_area: Option<f64>,
    /// Largest lot depth : frontage, with depth = area / curb frontage (so the
    /// ratio is area / frontage²). Default 3 — no strip lots.
    pub max_depth_ratio: Option<f64>,
    /// Share of lots in [0, 1] allowed to break the per-lot rules (min lot
    /// area / width / frontage, max depth : frontage). Default 5 %: an
    /// irregular site always leaves a few odd remnant lots at its edges, and
    /// a strict rule then rejects every walkable layout.
    #[serde(alias = "depth_ratio_share")]
    pub lot_rule_share: f64,
    /// Share of lots allowed without a street edge (interior remnants).
    /// Default 2 %.
    pub access_share: f64,
    /// Largest perimeter of a block that gets subdivided, m. Default 800 —
    /// no super-blocks.
    pub max_block_perimeter: Option<f64>,
    /// Storey cap: the search draws heights within it (`floors_cap`).
    /// Default 6.
    pub max_stories: Option<usize>,
    /// Fewest dwellings in the plan.
    pub min_dwellings: Option<f64>,
    /// Least density, dwellings per hectare of net developable area.
    pub min_density: Option<f64>,
    /// Unit-size rule overrides (the plan's `dwelling_area` /
    /// `dwelling_efficiency` when unset); an override is folded into every
    /// candidate's params, so applying one keeps the rule it was scored with.
    pub dwelling_area: Option<f64>,
    pub dwelling_efficiency: Option<f64>,
    /// Least share of lots within `economics.park_distance` of a park.
    pub min_park_access: Option<f64>,
    /// Least share of facade area whose direct sun averages ≥ `daylight_hours`
    /// a day over `daylight_period` (checked on the best candidates only;
    /// needs `location`).
    pub min_daylight: Option<f64>,
    /// Direct-sun hours a day a facade sample needs. Default 2.
    pub daylight_hours: f64,
    /// The days the daylight analysis averages. Default: the whole year.
    pub daylight_period: DaylightPeriod,
    /// Most share of facade area with more than `overheat_hours` of direct
    /// sun on the hottest month (hot / tropical sites). Off by default.
    pub max_overheat: Option<f64>,
    /// Sun hours on the hottest month above which a facade overheats.
    /// Default 6.
    pub overheat_hours: f64,
    /// Observer location for daylight (the document's, when unset).
    pub location: Option<Location>,
    /// Use the document's climate (`climate <file.epw>`) for daylight when it
    /// has one (default); false keeps clear-sky sun hours.
    pub use_climate: bool,
    /// The site climate daylight reads (filled from the document by
    /// [`prepare_doc`] when `use_climate`; not part of the request JSON).
    #[serde(skip)]
    pub climate: Option<std::sync::Arc<itsjustcad_doc::Climate>>,
    /// Unit prices of the `value` objective.
    pub economics: Economics,
}

/// Where the sun is computed for daylight.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Location {
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub tz_hours: f64,
}

/// Unit prices of the land-development ledger, in one (any) currency. The
/// defaults are illustrative, not market data: raw serviced land sold at 120
/// per m² of lot; roads 90 per m² (paving, curbs, drainage); utilities 600 per
/// m of lot frontage (water, sewer, power along the street); 3000 per lot
/// (survey, titling, connections); open space 15 per m² (landscaping).
/// Developing (the default) instead buys the site at `price` per m² gross,
/// sells floor area at 2000 per m² and builds it at 1200 per m² plus 10 % per
/// storey above the first; lots and buildings within 400 m of a park sell
/// 10 % higher.
///
/// Lot value has diminishing returns on size: `price × ref × (area/ref)^γ`
/// with `γ = size_exponent` (0.85), so a lot of the reference area (`ref_lot_area`,
/// 0 = the plan's `lot_area_min`) sells at `price` per m² and bigger lots earn
/// less per m². Lots deeper than 2.5 × their frontage lose up to 10 % of their
/// value at 3 : 1 (a soft shape penalty under the hard `max_depth_ratio`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Economics {
    /// Sale price per m² of lot area.
    pub price: f64,
    /// Cost per m² of road (site area outside every block).
    pub road_cost: f64,
    /// Cost per m of lot frontage at the curb.
    pub utility_cost: f64,
    /// Fixed cost per lot.
    pub lot_cost: f64,
    /// Cost per m² of reserved open space.
    pub open_cost: f64,
    /// Exponent γ of lot value on lot area (1 = linear).
    pub size_exponent: f64,
    /// Lot area worth exactly `price` per m²; 0 = the plan's `lot_area_min`.
    pub ref_lot_area: f64,
    /// Develop and sell built floor area (true, the default) instead of
    /// selling serviced lots. Developing buys the site at `price` per m² of
    /// gross site area, builds every building at `build_cost` and sells its
    /// GFA at `sale_price`.
    pub develop: bool,
    /// Sale price per m² of built floor area (GFA).
    pub sale_price: f64,
    /// Construction cost per m² of GFA for a one-storey building.
    pub build_cost: f64,
    /// Construction cost rise per storey above the first, as a fraction
    /// (structure, lifts, fire safety): a 5-storey building costs
    /// `build_cost × (1 + 4 × height_cost)` per m².
    pub height_cost: f64,
    /// Price uplift for lots / buildings within `park_distance` of a park.
    pub park_premium: f64,
    /// Walking distance to a park that earns the premium, m (straight line).
    pub park_distance: f64,
}

impl Default for Economics {
    fn default() -> Self {
        Economics {
            price: 120.0,
            road_cost: 90.0,
            utility_cost: 600.0,
            lot_cost: 3000.0,
            open_cost: 15.0,
            size_exponent: 0.85,
            ref_lot_area: 0.0,
            develop: true,
            sale_price: 2000.0,
            build_cost: 1200.0,
            height_cost: 0.10,
            park_premium: 0.10,
            park_distance: 400.0,
        }
    }
}

/// Depth : frontage where the soft shape penalty starts, and the value lost
/// per unit of ratio above it (10 % at 3 : 1), capped at [`SHAPE_PENALTY_MAX`].
const SHAPE_SOFT_RATIO: f64 = 2.5;
const SHAPE_PENALTY_SLOPE: f64 = 0.2;
const SHAPE_PENALTY_MAX: f64 = 0.5;

/// The physical quantities a ledger prices, measured on one site plan.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Quantities {
    /// Gross site area, m².
    pub gross_area: f64,
    /// Area of every lot, m².
    pub lot_areas: Vec<f64>,
    /// Site area outside every block, m².
    pub road_area: f64,
    /// Σ lot frontage at the curb, m.
    pub frontage: f64,
    /// Reserved open-space area, m².
    pub open_area: f64,
    /// Depth : frontage of every lot (area / curb frontage²; infinite with
    /// no frontage), parallel to `lot_areas`.
    pub depth_ratios: Vec<f64>,
    /// Largest perimeter of a subdivided (not reserved) block, m.
    pub max_block_perimeter: f64,
    /// Whether each lot (centroid) is within `park_distance` of a park,
    /// parallel to `lot_areas`.
    pub lot_near_park: Vec<bool>,
    /// Every building: GFA, storeys, near a park.
    pub buildings: Vec<BuiltQ>,
    /// Share of facade area with the requested direct-sun hours, when it was
    /// measured.
    pub daylight: Option<f64>,
    /// Share of storeys with at least one such facade sample.
    pub daylight_floors: Option<f64>,
    /// Share of facade area overheating on the hottest month, when asked.
    pub overheat: Option<f64>,
    /// Where the daylight sun hours came from, when daylight was measured.
    #[serde(default)]
    pub daylight_source: Option<DaylightSource>,
    /// Each lot's walk to the nearest park along the streets, m.
    pub park_walk: Vec<f64>,
}

/// One building as the ledger prices it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct BuiltQ {
    pub gfa: f64,
    pub floors: usize,
    /// Dwellings, by the plan's unit-size rule.
    #[serde(default)]
    pub dwellings: usize,
    pub near_park: bool,
}

impl Quantities {
    /// Share of lots within walking distance of a park (1 with no lots).
    pub fn park_access(&self) -> f64 {
        if self.lot_near_park.is_empty() {
            return 1.0;
        }
        self.lot_near_park.iter().filter(|n| **n).count() as f64 / self.lot_near_park.len() as f64
    }

    /// Building storeys, sorted ascending.
    pub fn floors_sorted(&self) -> Vec<usize> {
        let mut f: Vec<usize> = self.buildings.iter().map(|b| b.floors).collect();
        f.sort_unstable();
        f
    }

    /// The largest finite lot depth : frontage (0 with no lots).
    pub fn max_depth_ratio(&self) -> f64 {
        self.depth_ratios
            .iter()
            .copied()
            .filter(|r| r.is_finite())
            .fold(0.0, f64::max)
    }
}

/// One plan's development ledger: revenue, each cost line, profit, and profit
/// per m² of gross site area (`value`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Ledger {
    /// Lot sales, or floor-area sales when developing.
    pub revenue: f64,
    /// Site acquisition (developing only).
    pub land: f64,
    /// Construction (developing only).
    pub construction: f64,
    pub roads: f64,
    pub utilities: f64,
    pub lots: f64,
    pub open_space: f64,
    pub profit: f64,
    pub value: f64,
}

impl Economics {
    /// Sale value of one lot of `lot_area` m² and `depth_ratio` depth :
    /// frontage: diminishing in size around the reference area, less the
    /// soft shape penalty.
    pub fn land_value(&self, lot_area: f64, depth_ratio: f64) -> f64 {
        let size = if self.ref_lot_area > 0.0 && lot_area > 0.0 {
            self.ref_lot_area * (lot_area / self.ref_lot_area).powf(self.size_exponent)
        } else {
            lot_area
        };
        let penalty = ((depth_ratio - SHAPE_SOFT_RATIO) * SHAPE_PENALTY_SLOPE)
            .clamp(0.0, SHAPE_PENALTY_MAX);
        self.price * size * (1.0 - penalty)
    }

    /// These prices with `ref_lot_area` resolved: 0 takes `lot_area_min`.
    pub fn resolved(&self, lot_area_min: f64) -> Economics {
        Economics {
            ref_lot_area: if self.ref_lot_area > 0.0 { self.ref_lot_area } else { lot_area_min },
            ..*self
        }
    }

    pub fn ledger(&self, q: &Quantities) -> Ledger {
        let ratio = |i: usize| q.depth_ratios.get(i).copied().unwrap_or(0.0);
        let uplift = |near: bool| if near { 1.0 + self.park_premium } else { 1.0 };
        let (revenue, land, construction) = if self.develop {
            let sales = q
                .buildings
                .iter()
                .map(|b| b.gfa * self.sale_price * uplift(b.near_park))
                .sum::<f64>();
            let build = q
                .buildings
                .iter()
                .map(|b| {
                    let above = b.floors.saturating_sub(1) as f64;
                    b.gfa * self.build_cost * (1.0 + self.height_cost * above)
                })
                .sum::<f64>();
            (sales, q.gross_area * self.price, build)
        } else {
            let sales = q
                .lot_areas
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    let near = q.lot_near_park.get(i).copied().unwrap_or(false);
                    self.land_value(*a, ratio(i)) * uplift(near)
                })
                .sum::<f64>();
            (sales, 0.0, 0.0)
        };
        let roads = q.road_area * self.road_cost;
        let utilities = q.frontage * self.utility_cost;
        let lots = q.lot_areas.len() as f64 * self.lot_cost;
        let open_space = q.open_area * self.open_cost;
        let profit = revenue - land - construction - roads - utilities - lots - open_space;
        Ledger {
            revenue,
            land,
            construction,
            roads,
            utilities,
            lots,
            open_space,
            profit,
            value: if q.gross_area > 0.0 { profit / q.gross_area } else { 0.0 },
        }
    }

    /// Set one `key=value` price (the command form); false if `key` is not one.
    fn set(&mut self, key: &str, v: f64) -> bool {
        let slot = match key {
            "price" => &mut self.price,
            "road_cost" => &mut self.road_cost,
            "utility_cost" => &mut self.utility_cost,
            "lot_cost" => &mut self.lot_cost,
            "open_cost" => &mut self.open_cost,
            "size_exponent" => &mut self.size_exponent,
            "ref_lot_area" => &mut self.ref_lot_area,
            "sale_price" => &mut self.sale_price,
            "build_cost" => &mut self.build_cost,
            "height_cost" => &mut self.height_cost,
            "park_premium" => &mut self.park_premium,
            "park_distance" => &mut self.park_distance,
            "develop" => {
                self.develop = v != 0.0;
                return true;
            }
            _ => return false,
        };
        *slot = v;
        true
    }
}

impl Default for OptimizeRequest {
    fn default() -> Self {
        OptimizeRequest {
            objective: Objective::Value,
            weights: BTreeMap::new(),
            budget: 400,
            seed: 1,
            top: 5,
            lock: Vec::new(),
            patterns: WALKABLE_PATTERNS.iter().map(|p| p.to_string()).collect(),
            min_lot_area: None,
            min_lot_width: None,
            min_frontage: None,
            min_open: Some(0.10),
            max_far: None,
            max_coverage: None,
            max_lot_area: None,
            max_depth_ratio: Some(3.0),
            lot_rule_share: 0.05,
            access_share: 0.02,
            max_block_perimeter: Some(800.0),
            max_stories: Some(6),
            min_dwellings: None,
            min_density: None,
            dwelling_area: None,
            dwelling_efficiency: None,
            min_park_access: Some(DEFAULT_PARK_ACCESS),
            min_daylight: None,
            daylight_hours: 2.0,
            daylight_period: DaylightPeriod::Annual,
            max_overheat: None,
            overheat_hours: 6.0,
            location: None,
            use_climate: true,
            climate: None,
            economics: Economics::default(),
        }
    }
}

/// The params searched by default, in decode order (`lot_area_max` is a ratio
/// of `lot_area_min`, so it follows it).
/// The street patterns searched by default: connected networks without
/// cul-de-sacs (`culdesac`), spoke-and-ring (`radial`, also ~1 s a plan on a
/// large site) or meandering super-blocks (`organic`).
pub const WALKABLE_PATTERNS: &[&str] = &["orthogonal", "skewed", "hexagonal", "voronoi"];

/// Default least share of lots within walking distance of a park.
pub const DEFAULT_PARK_ACCESS: f64 = 0.9;

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
    "floors_min",
    "floors_max",
    "height_mix_low",
    "height_mix_mid",
    "height_mix_tower",
    "height_falloff",
    "falloff_strength",
    "height_seed",
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
                    format!("objective= must be value|lots|far_net|net_area|gfa|dwellings|weighted, got '{v}'")
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
                if self.objective == Objective::default() {
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
            "max_far" => self.max_far = Some(num()?),
            "max_coverage" => self.max_coverage = Some(num()?),
            // `off` lifts a default-on limit.
            "max_lot_area" | "max_depth_ratio" | "max_block_perimeter" => {
                let v = if v.eq_ignore_ascii_case("off") { None } else { Some(num()?) };
                match key {
                    "max_lot_area" => self.max_lot_area = v,
                    "max_depth_ratio" => self.max_depth_ratio = v,
                    _ => self.max_block_perimeter = v,
                }
            }
            "lot_rule_share" | "depth_ratio_share" => self.lot_rule_share = ratio(num()?),
            "access_share" => self.access_share = ratio(num()?),
            "max_stories" | "min_park_access" | "min_daylight" | "min_open" => {
                let off = v.eq_ignore_ascii_case("off");
                match key {
                    "max_stories" => self.max_stories = (!off).then(|| num().map(|n| n as usize)).transpose()?,
                    "min_park_access" => self.min_park_access = (!off).then(|| num().map(ratio)).transpose()?,
                    "min_open" => self.min_open = (!off).then(num).transpose()?,
                    _ => self.min_daylight = (!off).then(|| num().map(ratio)).transpose()?,
                }
            }
            "min_dwellings" | "min_density" | "dwelling_area" | "dwelling_efficiency" => {
                let v = (!v.eq_ignore_ascii_case("off")).then(num).transpose()?;
                match key {
                    "min_dwellings" => self.min_dwellings = v,
                    "min_density" => self.min_density = v,
                    "dwelling_area" => self.dwelling_area = v,
                    _ => self.dwelling_efficiency = v.map(ratio),
                }
            }
            "daylight_hours" => self.daylight_hours = num()?,
            "overheat_hours" => self.overheat_hours = num()?,
            "max_overheat" => {
                self.max_overheat = (!v.eq_ignore_ascii_case("off")).then(|| num().map(ratio)).transpose()?
            }
            // annual | worst | MM-DD
            "daylight_period" => {
                self.daylight_period = match v.to_ascii_lowercase().as_str() {
                    "annual" | "year" => DaylightPeriod::Annual,
                    "worst" | "worst_month" => DaylightPeriod::WorstMonth,
                    d => {
                        let (m, day) = d
                            .split_once('-')
                            .and_then(|(m, d)| Some((m.parse().ok()?, d.parse().ok()?)))
                            .ok_or("daylight_period= takes annual | worst | MM-DD")?;
                        DaylightPeriod::Custom { month: m, day }
                    }
                }
            }
            "location" => {
                let n: Vec<f64> = v.split(',').filter_map(|t| t.trim().parse().ok()).collect();
                let [lat, lon, rest @ ..] = n.as_slice() else {
                    return Err("location= takes lat,lon[,tz]".into());
                };
                self.location = Some(Location {
                    lat_deg: *lat,
                    lon_deg: *lon,
                    tz_hours: rest.first().copied().unwrap_or((lon / 15.0).round()),
                });
            }
            // climate=off: clear-sky sun hours even with a climate file.
            "climate" => {
                self.use_climate = !matches!(
                    v.to_ascii_lowercase().as_str(),
                    "off" | "0" | "false" | "clear_sky"
                );
            }
            k if self.economics.set(k, num()?) => {}
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
        if self.dwelling_area.is_some_and(|a| a <= 0.0) {
            return Err("siteoptimize: dwelling_area must be > 0".into());
        }
        if self
            .dwelling_efficiency
            .is_some_and(|e| e <= 0.0 || e > 1.0)
        {
            return Err("siteoptimize: dwelling_efficiency must be in (0, 1]".into());
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

    fn score(&self, r: &YieldReport, l: &Ledger, q: &Quantities) -> f64 {
        match self.objective {
            Objective::Weighted => self.weights.iter().map(|(k, w)| w * metric(r, l, q, k)).sum(),
            o => metric(r, l, q, o.token()),
        }
    }

    /// Whether daylight is part of this search (a constraint or a weight).
    pub fn wants_daylight(&self) -> bool {
        self.min_daylight.is_some()
            || self.max_overheat.is_some()
            || (self.objective == Objective::Weighted
                && (self.weights.contains_key("daylight") || self.weights.contains_key("overheat")))
    }

    /// The facade daylight analysis this request asks for.
    pub fn daylight_opts(&self) -> DaylightOpts {
        let overheat = self.max_overheat.is_some()
            || (self.objective == Objective::Weighted && self.weights.contains_key("overheat"));
        DaylightOpts {
            period: self.daylight_period,
            hours: self.daylight_hours,
            overheat_hours: overheat.then_some(self.overheat_hours),
            climate: self.climate.clone().filter(|_| self.use_climate),
        }
    }
}

/// `15` and `0.15` both mean 15 %.
fn ratio(v: f64) -> f64 {
    if v > 1.0 { v / 100.0 } else { v }
}

const METRICS: &[&str] = &[
    "lots",
    "far",
    "net_area",
    "gfa",
    "open",
    "value",
    "park",
    "daylight",
    "overheat",
    "dwellings",
    "density",
];

fn metric(r: &YieldReport, l: &Ledger, q: &Quantities, key: &str) -> f64 {
    match key {
        "park" => q.park_access(),
        "daylight" => q.daylight.unwrap_or(0.0),
        "overheat" => q.overheat.unwrap_or(0.0),
        "value" => l.value,
        "lots" => r.lot_count as f64,
        "far" | "far_net" => r.far_net.unwrap_or(0.0),
        "net_area" => r.total_lot_area,
        "gfa" => r.total_gfa,
        "open" => r.open_space_ratio,
        "dwellings" => r.dwellings.as_ref().map_or(0.0, |d| d.total as f64),
        "density" => r
            .dwellings
            .as_ref()
            .and_then(|d| d.per_ha_net)
            .unwrap_or(0.0),
        _ => 0.0,
    }
}

/// One evaluated site plan.
#[derive(Debug, Clone)]
pub struct Evaluation {
    pub report: YieldReport,
    /// Hard constraints this plan breaks (empty = feasible).
    pub violations: Vec<&'static str>,
    /// Σ of the broken checks' excess (shares of lots, relative overshoot);
    /// 0 when feasible. Guides the search toward feasibility.
    pub infeasibility: f64,
    pub score: f64,
    /// What the ledger prices.
    pub quantities: Quantities,
    /// The plan's ledger at the request's [`Economics`].
    pub ledger: Ledger,
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
/// edits and `edgerole` / `lotset` overrides. A boundary's existing plan lends its edits
/// to the search, so a scored plan is the plan `siteoptimize apply` bakes.
#[derive(Debug, Clone, Default)]
pub struct PlanEdits {
    pub streets: BTreeMap<u32, StreetEdit>,
    pub edge_roles: Vec<EdgeRoleOverride>,
    pub lot_overrides: Vec<LotOverride>,
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
    let mut heights = edits.lot_overrides.clone();
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
    // The lot holding each building (its footprint centroid).
    let lot_of = lot_locator(&plan.lots.polygons);
    let building_lots: Vec<Option<usize>> = plan.buildings.as_ref().map_or_else(Vec::new, |b| {
        b.buildings
            .iter()
            .map(|b| lot_of(b.footprint.centroid()))
            .collect()
    });
    let rule = settings.dwelling_rule();
    let report = YieldReport::compute(&YieldInputs {
        lot_areas: plan.lots.polygons.iter().map(|p| p.area()).collect(),
        lot_frontages: fronts.setback.clone(),
        gross_site_area: site.area(),
        open_space_feature_areas: Vec::new(),
        reserved_block_areas: plan.reserved.iter().map(|p| p.area()).collect(),
        building_gfas: gfas,
        building_footprints: footprints,
        building_lots: building_lots.clone(),
        dwelling_rule: Some(rule),
    });
    let depth_ratios: Vec<f64> = plan
        .lots
        .polygons
        .iter()
        .zip(&fronts.curb)
        .map(|(p, f)| if *f > 1e-6 { p.area() / (f * f) } else { f64::INFINITY })
        .collect();
    let max_block_perimeter = plan
        .site
        .blocks
        .iter()
        .filter(|b| !plan.reserved.contains(&b.polygon))
        .map(|b| b.polygon.perimeter())
        .fold(0.0, f64::max);
    // Each check's excess (> 0 = broken): a share of lots for per-lot rules,
    // else the relative overshoot. Their sum steers the search out of the
    // infeasible region (a count of broken rules is too coarse for that).
    let lot_areas: Vec<f64> = plan.lots.polygons.iter().map(|p| p.area()).collect();
    let n = lot_areas.len().max(1) as f64;
    let share = |v: &[f64], bad: &dyn Fn(f64) -> bool| v.iter().filter(|x| bad(**x)).count() as f64 / n;
    let over = |v: f64, m: f64| (v - m) / m.abs().max(1e-9);
    // Walking distance along the streets, from each lot's frontage.
    let park_walk = crate::site_metrics::park_walk(&plan);
    let lot_near_park: Vec<bool> =
        park_walk.iter().map(|d| *d <= req.economics.park_distance).collect();
    // A building earns its lot's park premium.
    let buildings: Vec<BuiltQ> = plan.buildings.as_ref().map_or_else(Vec::new, |b| {
        b.buildings
            .iter()
            .zip(&building_lots)
            .map(|(b, lot)| BuiltQ {
                gfa: b.gfa,
                floors: b.floor_areas.len(),
                dwellings: rule.dwellings(b.gfa),
                near_park: lot.is_some_and(|i| lot_near_park[i]),
            })
            .collect()
    });
    let park_access = if lot_near_park.is_empty() {
        1.0
    } else {
        lot_near_park.iter().filter(|n| **n).count() as f64 / lot_near_park.len() as f64
    };
    let tol = req.lot_rule_share + 1e-9;
    let (dwellings, density) = report.dwellings.as_ref().map_or((0.0, 0.0), |d| {
        (d.total as f64, d.per_ha_net.unwrap_or(0.0))
    });
    let checks = [
        ("min_park_access", req.min_park_access.map(|m| m - park_access)),
        // Daylight / overheating are measured after the search (see `rank`).
        ("min_lot_area", req.min_lot_area.map(|m| share(&lot_areas, &|a| a < m) - tol)),
        ("min_lot_width", req.min_lot_width.map(|m| share(&fronts.curb, &|w| w < m) - tol)),
        ("min_frontage", req.min_frontage.map(|m| share(&fronts.setback, &|w| w < m) - tol)),
        ("min_open", req.min_open.map(|m| m - report.open_space_ratio)),
        ("max_far", req.max_far.map(|m| over(report.far_net.unwrap_or(0.0), m))),
        ("max_coverage", req.max_coverage.map(|m| over(report.lot_coverage.unwrap_or(0.0), m))),
        ("max_lot_area", req.max_lot_area.map(|m| over(report.max_lot_area, m))),
        (
            "max_depth_ratio",
            req.max_depth_ratio
                .map(|m| share(&depth_ratios, &|r| r > m) - req.lot_rule_share - 1e-9),
        ),
        (
            "max_block_perimeter",
            req.max_block_perimeter
                .map(|m| over(max_block_perimeter, m)),
        ),
        (
            "min_dwellings",
            req.min_dwellings.map(|m| -over(dwellings, m)),
        ),
        ("min_density", req.min_density.map(|m| -over(density, m))),
        (
            "street_access",
            Some((report.lot_count - fronts.with_street.min(report.lot_count)) as f64 / n - req.access_share - 1e-9),
        ),
    ];
    let checks = checks.map(|(name, excess)| (name, excess.unwrap_or(0.0)));
    let infeasibility = checks.iter().map(|c| c.1.max(0.0)).sum();
    let violations = checks.iter().filter(|c| c.1 > 0.0).map(|c| c.0).collect();
    let block_area: f64 = plan.site.blocks.iter().map(|b| b.polygon.area()).sum();
    let quantities = Quantities {
        gross_area: site.area(),
        lot_areas,
        road_area: (site.area() - block_area).max(0.0),
        frontage: fronts.curb.iter().sum(),
        open_area: report.reserved_block_area,
        depth_ratios,
        max_block_perimeter,
        lot_near_park,
        buildings,
        daylight: None,
        daylight_floors: None,
        overheat: None,
        daylight_source: None,
        park_walk,
    };
    let ledger = req.economics.ledger(&quantities);
    let score = req.score(&report, &ledger, &quantities);
    Ok(Evaluation {
        report,
        violations,
        infeasibility,
        score,
        quantities,
        ledger,
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
    /// Ranks on the (score, [`OptimizeResult::pareto_axis`]) Pareto front.
    pub pareto: Vec<usize>,
    /// The front's second metric (`open`, `net_area`, `gfa` or `lots`): the
    /// first that varies across candidates and is not the objective.
    pub pareto_axis: &'static str,
    pub evaluated: usize,
    pub feasible: usize,
    /// The plan the search started from (the boundary's site plan, or the
    /// sticky settings), scored the same way. `None` if it failed to plan.
    pub current: Option<Evaluation>,
    /// How daylight was measured, when the search measured it.
    pub daylight: Option<DaylightInfo>,
}

/// Which sun hours a search's daylight used: clear-sky geometry or the site's
/// EPW climate, and the month overheating was measured on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaylightInfo {
    pub source: DaylightSource,
    /// The climate (`"<city> (<file>)"`) when `source` is `climate`.
    pub climate: Option<String>,
    /// The overheating month (1–12), when overheating was measured.
    pub overheat_month: Option<u32>,
}

impl DaylightInfo {
    /// What `opts` measures at `loc`.
    pub fn of(opts: &DaylightOpts, loc: &Location) -> DaylightInfo {
        let c = opts.climate.as_deref();
        DaylightInfo {
            source: if c.is_some() {
                DaylightSource::Climate
            } else {
                DaylightSource::ClearSky
            },
            climate: c.map(|c| format!("{} ({})", c.city, c.source)),
            overheat_month: opts
                .overheat_hours
                .map(|_| crate::site_metrics::overheat_month(loc, c)),
        }
    }

    /// One line for a report.
    pub fn describe(&self) -> String {
        let mut s = match &self.climate {
            Some(c) => format!("Daylight: sunshine hours from the EPW climate {c}"),
            None => {
                "Daylight: clear-sky sun hours (no climate file; `climate <file.epw>` sets one)"
                    .to_string()
            }
        };
        if let Some(m) = self.overheat_month {
            let how = if self.climate.is_some() {
                "hottest month"
            } else {
                "longest day"
            };
            s.push_str(&format!("; overheating on the 21st of month {m} ({how})"));
        }
        s.push('.');
        s
    }
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
            // Walkable blocks: an orthogonal block is up to ~2 depths a
            // side, so block faces of 60–200 m (and a perimeter within the
            // cap, ≈ 8 depths at most) need a depth of ~30–100 m. From 45 m
            // (smaller ones mostly add road and slow the hex / Voronoi
            // generators down) up to perimeter / 8, or half the site.
            "block_depth" => Range::Float {
                lo: 45.0_f64.min(short / 4.0).max(10.0),
                hi: req
                    .max_block_perimeter
                    .map_or(short / 2.0, |p| p / 8.0)
                    .min(short / 2.0)
                    .max(50.0),
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
                // CityEngine-style heights (#156), drawn within the storey cap
                // (`floors_cap` clamps every class range too).
                "floors_min" | "floors_max" => Range::Int {
                    lo: 1,
                    hi: req.max_stories.unwrap_or(12).max(1) as i64,
                },
                "height_mix_low" | "height_mix_mid" | "height_mix_tower" => Range::Int { lo: 0, hi: 3 },
                "height_falloff" => Range::Choice(vec!["none".into(), "center".into(), "street".into()]),
                "falloff_strength" => Range::Float { lo: 0.0, hi: 1.0, step: 0.05 },
                "height_seed" => Range::Int { lo: 1, hi: 9999 },
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
    /// GA selection key, smaller = better: less infeasible first (0 =
    /// feasible), then a higher score, then the [`TIE_BREAK`] metrics.
    fn fitness(&self) -> (f64, [f64; 4]) {
        match &self.eval {
            Ok(e) => (e.infeasibility, sort_key(e)),
            Err(_) => (f64::INFINITY, [0.0; 4]),
        }
    }
}

/// Metrics that order equal scores (higher first), so a lots objective prefers
/// the layout with more net area, then more GFA, then more open space.
const TIE_BREAK: [&str; 3] = ["net_area", "gfa", "open"];

/// Ascending sort key (smaller = better): −score, then −[`TIE_BREAK`].
fn sort_key(e: &Evaluation) -> [f64; 4] {
    let m = |k| -metric(&e.report, &e.ledger, &e.quantities, k);
    [-e.score, m(TIE_BREAK[0]), m(TIE_BREAK[1]), m(TIE_BREAK[2])]
}

fn cmp_keys(a: &[f64; 4], b: &[f64; 4]) -> std::cmp::Ordering {
    a.iter()
        .zip(b)
        .map(|(x, y)| x.total_cmp(y))
        .find(|o| o.is_ne())
        .unwrap_or(std::cmp::Ordering::Equal)
}

/// The Pareto front's second axis: the first of these that is not the
/// objective's own metric and varies across the candidates (open space is 0
/// for every plan when nothing reserves it). Falls back to `open`.
const PARETO_AXES: [&str; 4] = ["open", "net_area", "gfa", "lots"];

fn pareto_axis(objective: Objective, evals: &[&Evaluation]) -> &'static str {
    PARETO_AXES
        .into_iter()
        .filter(|k| *k != objective.token())
        .find(|k| {
            let (lo, hi) = evals
                .iter()
                .map(|e| metric(&e.report, &e.ledger, &e.quantities, k))
                .fold((f64::MAX, f64::MIN), |(lo, hi), v| (lo.min(v), hi.max(v)));
            hi - lo > 1e-6 * hi.abs().max(1.0)
        })
        .unwrap_or("open")
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
    optimize_ctl(site, base, edits, req, Control::default())
}

/// The error a cancelled search returns.
pub const CANCELLED: &str = "siteoptimize: cancelled";

/// Candidates evaluated between two cancel checks / progress reports. Chunks
/// only split the parallel batches; every genome is still drawn from the one
/// seeded RNG first, so a completed search is the same whatever the chunking.
const CHUNK: usize = 16;

/// Which part of the search is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Sampling and evolving site plans (`evaluated` of `budget`).
    #[default]
    Search,
    /// Measuring facade daylight on the best candidates (`evaluated` of the
    /// most that may be checked).
    Daylight,
}

/// Live progress of a running search.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Progress {
    pub phase: Phase,
    /// Plans evaluated so far in this phase.
    pub evaluated: usize,
    /// The phase's total.
    pub budget: usize,
    /// Feasible plans found so far (search phase).
    pub feasible: usize,
    /// Best feasible score so far.
    pub best: Option<f64>,
}

impl Progress {
    /// Done share of the current phase, in [0, 1].
    pub fn fraction(&self) -> f64 {
        if self.budget == 0 {
            0.0
        } else {
            (self.evaluated as f64 / self.budget as f64).min(1.0)
        }
    }
}

/// How a caller watches and stops a search: a cancel flag checked between
/// chunks of [`CHUNK`] plans (a cancelled search returns [`CANCELLED`]), and a
/// progress callback called after each chunk, from the calling thread.
#[derive(Clone, Copy, Default)]
pub struct Control<'a> {
    pub cancel: Option<&'a std::sync::atomic::AtomicBool>,
    pub progress: Option<&'a (dyn Fn(&Progress) + Sync)>,
}

impl Control<'_> {
    fn cancelled(&self) -> bool {
        self.cancel
            .is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
    }

    fn report(&self, p: Progress) {
        if let Some(f) = self.progress {
            f(&p);
        }
    }
}

/// [`optimize_with`] under a [`Control`]: cancellable, reporting progress.
pub fn optimize_ctl(
    site: &Polygon2d,
    base: &SubdivisionSettings,
    edits: &PlanEdits,
    req: &OptimizeRequest,
    ctl: Control,
) -> Result<OptimizeResult, String> {
    req.validate()?;
    if req.wants_daylight() && req.location.is_none() {
        return Err(NEEDS_LOCATION.into());
    }
    // The unit-size rule overrides, folded onto the base and every candidate.
    let rule_params: BTreeMap<String, String> = [
        ("dwelling_area", req.dwelling_area),
        ("dwelling_efficiency", req.dwelling_efficiency),
    ]
    .into_iter()
    .filter_map(|(k, v)| Some((k.to_string(), v?.to_string())))
    .collect();
    let base = &if rule_params.is_empty() {
        base.clone()
    } else {
        settings_with(base, &rule_params)?
    };
    let (area_lo, width_lo) = floors(base, req);
    let req = &OptimizeRequest {
        min_lot_area: Some(req.min_lot_area.unwrap_or(area_lo)),
        min_lot_width: Some(req.min_lot_width.unwrap_or(width_lo)),
        min_open: req.min_open.map(ratio),
        max_coverage: req.max_coverage.map(ratio),
        economics: req.economics.resolved(base.lot_area_min),
        ..req.clone()
    };
    let dims = dimensions(site, base, req);
    let d = dims.len();
    // Everything locked: one plan is all there is to evaluate.
    let budget = if d == 0 { 1 } else { req.budget };
    let mut rng = Rng(req.seed);
    let eval_one = |genome: Vec<f64>| -> Trial {
        let mut params = decode(&dims, &genome, base);
                params.insert(
                    "floors_cap".into(),
                    req.max_stories.unwrap_or(0).to_string(),
                );
                params.extend(rule_params.clone());
        // A cancelled search skips the rest of its chunk.
        let eval = if ctl.cancelled() {
            Err(CANCELLED.to_string())
        } else {
            settings_with(base, &params).and_then(|s| evaluate(site, &s, edits, req))
        };
        Trial {
            genome,
            params,
            eval,
        }
    };
    let mut progress = Progress {
        budget,
        ..Progress::default()
    };
    // Evaluate `genomes` in chunks onto `trials`, checking for a cancel and
    // reporting progress after each.
    let mut run = |trials: &mut Vec<Trial>, genomes: Vec<Vec<f64>>| -> Result<(), String> {
        let mut genomes = genomes.into_iter().peekable();
        while genomes.peek().is_some() {
            let chunk: Vec<Trial> = genomes
                .by_ref()
                .take(CHUNK)
                .collect::<Vec<_>>()
                .into_par_iter()
                .map(&eval_one)
                .collect();
            if ctl.cancelled() {
                return Err(CANCELLED.into());
            }
            for t in &chunk {
                if let Ok(e) = &t.eval
                    && e.violations.is_empty()
                {
                    progress.feasible += 1;
                    progress.best = Some(progress.best.map_or(e.score, |b| b.max(e.score)));
                }
            }
            progress.evaluated += chunk.len();
            trials.extend(chunk);
            ctl.report(progress);
        }
        Ok(())
    };

    let n_init = budget.div_ceil(2);
    let mut trials = Vec::with_capacity(budget);
    run(&mut trials, latin_hypercube(n_init, d, &mut rng))?;
    let (mu, lambda) = (8, ((budget - n_init) / 4).max(4));
    let mut sigma = 0.2;
    while trials.len() < budget {
        let mut order: Vec<usize> = (0..trials.len()).collect();
        order.sort_by(|&a, &b| {
            let (fa, fb) = (trials[a].fitness(), trials[b].fitness());
            fa.0.total_cmp(&fb.0).then(cmp_keys(&fa.1, &fb.1)).then(a.cmp(&b))
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
        run(&mut trials, children)?;
        sigma *= 0.8;
    }

    let mut current = evaluate(site, base, edits, req).ok();
    let daylight = req.location.filter(|_| req.wants_daylight());
    let opts = req.daylight_opts();
    let day = |p: &BTreeMap<String, String>| {
        let loc = daylight?;
        plan_daylight(site, &settings_with(base, p).ok()?, edits, &loc, &opts)
    };
    if let (Some(loc), Some(c)) = (daylight, current.as_mut()) {
        set_daylight(&mut c.quantities, plan_daylight(site, base, edits, &loc, &opts));
        c.score = req.score(&c.report, &c.ledger, &c.quantities);
    }
    let info = daylight.map(|loc| DaylightInfo::of(&opts, &loc));
    rank(trials, req, daylight.map(|_| &day as DaylightOf), ctl)
        .map(|r| OptimizeResult { current, daylight: info, ..r })
}

const NEEDS_LOCATION: &str = "siteoptimize: daylight needs a location — set one with \
                              `location <lat> <lon>` (or `sun …`) first";

/// Rank the feasible trials (score, then [`TIE_BREAK`]), drop layouts whose
/// yield duplicates a better one, and mark the (score, [`pareto_axis`]) Pareto
/// front. With nothing feasible, name
/// the constraint broken most often.
/// Daylight of a candidate's params (re-plans it), for the top-K pass.
type DaylightOf<'a> = &'a (dyn Fn(&BTreeMap<String, String>) -> Option<FacadeDaylight> + Sync);

/// Store a facade daylight measure on `q`.
pub fn set_daylight(q: &mut Quantities, d: Option<FacadeDaylight>) {
    q.daylight = d.map(|d| d.facade);
    q.daylight_floors = d.map(|d| d.floors);
    q.overheat = d.and_then(|d| d.overheat);
    q.daylight_source = d.map(|d| d.source);
}

fn rank(
    trials: Vec<Trial>,
    req: &OptimizeRequest,
    daylight: Option<DaylightOf>,
    ctl: Control,
) -> Result<OptimizeResult, String> {
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
            "max_lot_area" => req.max_lot_area,
            "max_depth_ratio" => req.max_depth_ratio,
            "max_block_perimeter" => req.max_block_perimeter,
            "min_dwellings" => req.min_dwellings,
            "min_density" => req.min_density,
            "min_park_access" => req.min_park_access,
            "min_daylight" => req.min_daylight,
            "max_overheat" => req.max_overheat,
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
        cmp_keys(&sort_key(&eval(a)), &sort_key(&eval(b))).then(ia.cmp(ib))
    });
    let mut seen = std::collections::BTreeSet::new();
    let mut unique: Vec<(BTreeMap<String, String>, Evaluation)> = feasible
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
        .collect();
    // Daylight is costly (a re-plan and a ray-cast grid), so it is measured
    // on the best candidates only, in batches of DAYLIGHT_TOP_K in rank order:
    // as a weight, on one batch; as a constraint, until `top` layouts pass
    // (it trades against profit, so the very best often fail) or
    // DAYLIGHT_MAX_CHECKED were tried.
    if let Some(day) = daylight {
        let limit = if req.min_daylight.is_some() || req.max_overheat.is_some() {
            DAYLIGHT_MAX_CHECKED
        } else {
            DAYLIGHT_TOP_K
        };
        let mut checked: Vec<(BTreeMap<String, String>, Evaluation)> = Vec::new();
        let mut best = 0.0_f64;
        let mut progress = Progress {
            phase: Phase::Daylight,
            budget: limit.min(unique.len()),
            ..Progress::default()
        };
        let mut rest = unique.into_iter();
        while checked.len() < limit {
            let batch: Vec<_> = rest.by_ref().take(DAYLIGHT_TOP_K).collect();
            if batch.is_empty() {
                break;
            }
            ctl.report(progress);
            // A cancel skips the batch's not-yet-started ray casts.
            let shares: Vec<Option<FacadeDaylight>> = batch
                .par_iter()
                .map(|(p, _)| if ctl.cancelled() { None } else { day(p) })
                .collect();
            if ctl.cancelled() {
                return Err(CANCELLED.into());
            }
            for ((p, mut e), d) in batch.into_iter().zip(shares) {
                set_daylight(&mut e.quantities, d);
                let d = e.quantities.daylight;
                best = best.max(d.unwrap_or(0.0));
                if req.min_daylight.is_some_and(|m| d.unwrap_or(0.0) < m) {
                    e.violations.push("min_daylight");
                }
                if req.max_overheat.is_some_and(|m| e.quantities.overheat.unwrap_or(0.0) > m) {
                    e.violations.push("max_overheat");
                }
                e.score = req.score(&e.report, &e.ledger, &e.quantities);
                if e.violations.is_empty() {
                    progress.feasible += 1;
                    progress.best = Some(progress.best.map_or(e.score, |b| b.max(e.score)));
                }
                checked.push((p, e));
            }
            progress.evaluated = checked.len();
            if checked.iter().filter(|(_, e)| e.violations.is_empty()).count() >= req.top.max(1) {
                break;
            }
        }
        checked.retain(|(_, e)| e.violations.is_empty());
        if checked.is_empty() {
            return Err(format!(
                "siteoptimize: none of the best {limit} layouts meets min_daylight={} \
                 (best {:.0} % of facade area averages {} h of direct sun over the period); \
                 lower it, the max stories or the max overheating",
                req.min_daylight.unwrap_or(0.0),
                best * 100.0,
                req.daylight_hours
            ));
        }
        checked.sort_by(|a, b| cmp_keys(&sort_key(&a.1), &sort_key(&b.1)));
        unique = checked;
    }
    let unique: Vec<Candidate> = unique
        .into_iter()
        .enumerate()
        .map(|(i, (params, eval))| Candidate {
            rank: i + 1,
            params,
            eval,
        })
        .collect();
    let axis = pareto_axis(
        req.objective,
        &unique.iter().map(|c| &c.eval).collect::<Vec<_>>(),
    );
    let dominated = |c: &Candidate| {
        unique.iter().any(|o| {
            let (s, os) = (c.eval.score, o.eval.score);
            let (p, op) = (
                metric(&c.eval.report, &c.eval.ledger, &c.eval.quantities, axis),
                metric(&o.eval.report, &o.eval.ledger, &o.eval.quantities, axis),
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
        pareto_axis: axis,
        evaluated,
        feasible: n_feasible,
        current: None,
        daylight: None,
    })
}

impl OptimizeResult {
    /// The candidate with `rank`, if listed.
    pub fn get(&self, rank: usize) -> Option<&Candidate> {
        self.candidates.iter().find(|c| c.rank == rank)
    }

    /// Whether the best candidate scores above the current plan (or there is
    /// no current plan to beat).
    pub fn beats_current(&self) -> bool {
        match (&self.current, self.candidates.first()) {
            (Some(cur), Some(best)) => {
                best.eval.score > cur.score + 1e-9 * cur.score.abs().max(1.0)
            }
            (None, best) => best.is_some(),
            (_, None) => false,
        }
    }

    /// A Markdown table of the candidates (the chat renders tables).
    pub fn to_markdown(&self) -> String {
        let mut s = format!(
            "**Site optimize — {}** ({} evaluated, {} feasible)\n\n\
             | # | score | lots | dwellings | lot area m² | FAR net | open % | params |\n|---|---|---|---|---|---|---|---|\n",
            self.objective.token(),
            self.evaluated,
            self.feasible
        );
        let current = self.current.iter().map(|e| ("Current".to_string(), e, None));
        let ranked = self
            .candidates
            .iter()
            .map(|c| (c.rank.to_string(), &c.eval, Some(&c.params)));
        for (rank, e, params) in current.chain(ranked) {
            let r = &e.report;
            let params: Vec<String> = params.map_or_else(Vec::new, |p| {
                SEARCHED
                    .iter()
                    .filter_map(|k| p.get(*k).map(|v| format!("{k}={v}")))
                    .collect()
            });
            s.push_str(&format!(
                "| {} | {:.2} | {} | {} | {:.0} | {} | {:.1} | {} |\n",
                rank,
                e.score + 0.0,
                r.lot_count,
                r.dwellings.as_ref().map_or(0, |d| d.total),
                r.total_lot_area,
                r.far_net.map_or("—".into(), |f| format!("{:.2}", f + 0.0)),
                r.open_space_ratio * 100.0 + 0.0,
                params.join(" ")
            ));
        }
        let front: Vec<String> = self.pareto.iter().map(|r| format!("#{r}")).collect();
        s.push_str(&format!(
            "\nPareto (score vs {}): {}. Bake one with `siteoptimize apply <rank>`.",
            self.pareto_axis,
            front.join(", ")
        ));
        if !self.beats_current() {
            s.push_str("\nThe current plan is already the best found.");
        }
        if let Some(d) = &self.daylight {
            s.push('\n');
            s.push_str(&d.describe());
        }
        s
    }

    /// `{"candidates":[{"rank","score","params":{…},"yield":{…},"ledger":{…}}],"pareto":[…],
    /// "pareto_axis","evaluated","feasible","current":{"score","yield","ledger","violations"}|null,
    /// "beats_current"}`.
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
                serde_json::json!({
                    "rank": c.rank, "score": c.eval.score, "params": params, "yield": c.eval.report,
                    "ledger": c.eval.ledger, "park_access": c.eval.quantities.park_access(),
                    "floors": c.eval.quantities.floors_sorted(), "daylight": c.eval.quantities.daylight,
                    "dwellings": c.eval.report.dwellings.as_ref().map(|d| d.total),
                })
            })
            .collect();
        serde_json::json!({
            "candidates": candidates,
            "pareto": self.pareto,
            "pareto_axis": self.pareto_axis,
            "evaluated": self.evaluated,
            "feasible": self.feasible,
            "current": self.current.as_ref().map(|e| serde_json::json!({
                "score": e.score, "yield": e.report, "ledger": e.ledger, "violations": e.violations,
                "park_access": e.quantities.park_access(), "floors": e.quantities.floors_sorted(),
                "daylight": e.quantities.daylight, "dwellings": e.report.dwellings.as_ref().map(|d| d.total),
            })),
            "beats_current": self.beats_current(),
            "daylight_source": self.daylight,
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
                lot_overrides: l.lot_overrides.clone(),
            },
        ),
        _ => (doc.subdivision_settings.clone(), PlanEdits::default()),
    }
}

/// Everything a search of a document's site boundary needs, owned, so it can
/// run off the document (a worker thread, a host job).
#[derive(Debug, Clone)]
pub struct Prepared {
    pub source: ObjectId,
    pub site: Polygon2d,
    pub base: SubdivisionSettings,
    pub edits: PlanEdits,
    /// The request, with the document's location when it sets none.
    pub req: OptimizeRequest,
}

impl Prepared {
    /// Run the search under `ctl`.
    pub fn run(&self, ctl: Control) -> Result<OptimizeResult, String> {
        optimize_ctl(&self.site, &self.base, &self.edits, &self.req, ctl)
    }
}

/// The search of the site boundary among `ids` in `doc`, ready to run.
pub fn prepare_doc(
    doc: &Document,
    ids: &[ObjectId],
    req: &OptimizeRequest,
) -> Result<Prepared, String> {
    let (source, site) = site_boundary(doc, ids)
        .ok_or("siteoptimize needs a closed site boundary curve (draw or select one first)")?;
    let (base, edits) = base_plan(doc, source);
    let req = OptimizeRequest {
        location: req.location.or(doc.location.map(|l| Location {
            lat_deg: l.lat_deg,
            lon_deg: l.lon_deg,
            tz_hours: l.tz_hours,
        })),
        climate: req
            .climate
            .clone()
            .or_else(|| doc.climate.clone().map(std::sync::Arc::new))
            .filter(|_| req.use_climate),
        ..req.clone()
    };
    Ok(Prepared {
        source,
        site,
        base,
        edits,
        req,
    })
}

/// Optimize the site boundary among `ids` in `doc`.
pub fn optimize_doc(
    doc: &Document,
    ids: &[ObjectId],
    req: &OptimizeRequest,
) -> Result<(ObjectId, OptimizeResult), String> {
    let p = prepare_doc(doc, ids, req)?;
    Ok((p.source, p.run(Control::default())?))
}

// ── parks and daylight ──────────────────────────────────────────────────────

/// Lookup of the lot polygon holding a point (a 50 m bucket grid).
fn lot_locator(lots: &[Polygon2d]) -> impl Fn(DVec2) -> Option<usize> + '_ {
    const CELL: f64 = 50.0;
    let key = |p: DVec2| ((p.x / CELL).floor() as i64, (p.y / CELL).floor() as i64);
    let mut grid: std::collections::HashMap<(i64, i64), Vec<usize>> = Default::default();
    for (i, l) in lots.iter().enumerate() {
        let (lo, hi) = l.aabb();
        let (a, b) = (key(lo), key(hi));
        for x in a.0..=b.0 {
            for y in a.1..=b.1 {
                grid.entry((x, y)).or_default().push(i);
            }
        }
    }
    move |p| grid.get(&key(p))?.iter().copied().find(|i| lots[*i].contains(p))
}

/// How many best candidates a daylight batch measures, and the most a
/// daylight constraint may try.
pub const DAYLIGHT_TOP_K: usize = 10;
pub const DAYLIGHT_MAX_CHECKED: usize = 60;

/// Plan `settings` on `site` with the hand edits and measure its facade
/// daylight.
pub fn plan_daylight(
    site: &Polygon2d,
    settings: &SubdivisionSettings,
    edits: &PlanEdits,
    loc: &Location,
    opts: &DaylightOpts,
) -> Option<FacadeDaylight> {
    let mut roles = edits.edge_roles.clone();
    let mut heights = edits.lot_overrides.clone();
    let plan = lot::plan_site(site, 0.0, settings, &edits.streets, &mut roles, &mut heights).ok()?;
    facade_daylight(&plan, settings.floor_height, loc, opts)
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
            objective: Objective::Lots,
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

    /// Equal scores are ordered by net area, then GFA (a lots objective must
    /// not hand back five indistinguishable 16-lot plans), and the Pareto
    /// front uses an axis that varies: open space is 0 without a reserve, so
    /// it falls back to net area.
    #[test]
    fn ties_break_on_net_area_and_pareto_axis_varies() {
        let site = rect(200.0, 150.0);
        // No green space (it would make open space vary): flat open space.
        let flat = OptimizeRequest { min_open: None, min_park_access: None, ..req(120) };
        let r = optimize(&site, &SubdivisionSettings::default(), &flat).unwrap();
        for w in r.candidates.windows(2) {
            let (a, b) = (&w[0].eval, &w[1].eval);
            assert!(a.score >= b.score);
            if a.score == b.score {
                let key = |e: &Evaluation| (e.report.total_lot_area, e.report.total_gfa);
                assert!(key(a) >= key(b), "#{} vs #{}", w[0].rank, w[1].rank);
            }
        }
        assert!(r.candidates.iter().all(|c| c.eval.report.open_space_ratio.abs() < 1e-9));
        assert_eq!(r.pareto_axis, "net_area");
        assert!(r.pareto.len() > 1, "a varying axis gives a real front: {:?}", r.pareto);
        assert!(r.to_markdown().contains("Pareto (score vs net_area)"));
        assert_eq!(r.to_json()["pareto_axis"], "net_area");
    }

    #[test]
    fn ledger_lines_add_up() {
        let q = Quantities {
            gross_area: 10_000.0,
            lot_areas: vec![500.0, 500.0],
            road_area: 1000.0,
            frontage: 40.0,
            open_area: 200.0,
            ..Quantities::default()
        };
        // Selling serviced lots.
        let sell = Economics { develop: false, ..Economics::default() };
        let l = sell.ledger(&q);
        assert_eq!(
            (l.revenue, l.land, l.construction, l.roads, l.utilities, l.lots, l.open_space),
            (120_000.0, 0.0, 0.0, 90_000.0, 24_000.0, 6000.0, 3000.0)
        );
        assert_eq!(l.profit, -3000.0);
        assert!((l.value + 0.3).abs() < 1e-12);
        // A park within walking distance lifts that lot's price.
        let near = Quantities { lot_near_park: vec![true, false], ..q.clone() };
        assert_eq!(sell.ledger(&near).revenue, 60_000.0 * 1.1 + 60_000.0);
        // Developing: buy the site, build, sell floor area.
        let built = Quantities {
            buildings: vec![
                BuiltQ {
                    gfa: 1000.0,
                    floors: 1,
                    near_park: false,
                    dwellings: 0,
                },
                BuiltQ {
                    gfa: 2000.0,
                    floors: 3,
                    near_park: true,
                    dwellings: 0,
                },
            ],
            ..q.clone()
        };
        let l = Economics::default().ledger(&built);
        assert_eq!(l.land, 10_000.0 * 120.0, "site acquisition");
        assert!((l.revenue - (1000.0 * 2000.0 + 2000.0 * 2000.0 * 1.1)).abs() < 1e-6);
        assert!((l.construction - (1000.0 * 1200.0 + 2000.0 * 1200.0 * 1.2)).abs() < 1e-6, "+10 %/storey");
        assert_eq!(Economics::default().ledger(&Quantities::default()).value, 0.0);
    }

    /// `value` is the default objective; the current plan is scored like a
    /// candidate; the command sets the unit prices.
    #[test]
    fn value_objective_scores_the_current_plan() {
        let site = rect(200.0, 150.0);
        let r = optimize(&site, &SubdivisionSettings::default(), &OptimizeRequest { budget: 24, ..OptimizeRequest::default() }).unwrap();
        assert_eq!(r.objective, Objective::Value);
        for c in &r.candidates {
            let (l, q) = (&c.eval.ledger, &c.eval.quantities);
            assert_eq!(c.eval.score, l.value);
            assert_eq!(q.lot_areas.len(), c.eval.report.lot_count);
            assert!(q.road_area > 0.0 && q.road_area < q.gross_area, "{q:?}");
            assert!(q.frontage > 0.0);
            let costs = l.land + l.construction + l.roads + l.utilities + l.lots + l.open_space;
            assert!((l.profit - (l.revenue - costs)).abs() < 1e-6);
        }
        let cur = r.current.as_ref().expect("the start plan is scored");
        assert_eq!(cur.score, cur.ledger.value);
        let j = r.to_json();
        assert_eq!(j["current"]["score"], cur.score);
        assert!(j["candidates"][0]["ledger"]["profit"].is_number());
        assert_eq!(j["beats_current"], r.beats_current());
        assert!(r.to_markdown().contains("| Current |"));

        let mut worse = r.clone();
        worse.current.as_mut().unwrap().score = f64::MAX;
        assert!(!worse.beats_current());
        assert!(worse.to_markdown().contains("already the best found"));

        let crate::Command::SiteOptimize { request, .. } = parse(
            "siteoptimize last price=50 road_cost=10 utility_cost=1 lot_cost=2 open_cost=3",
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(request.objective, Objective::Value);
        assert_eq!(
            request.economics,
            Economics {
                price: 50.0,
                road_cost: 10.0,
                utility_cost: 1.0,
                lot_cost: 2.0,
                open_cost: 3.0,
                ..Economics::default()
            }
        );
        let r: OptimizeRequest = serde_json::from_str(r#"{"economics":{"price":80}}"#).unwrap();
        assert_eq!(r.economics, Economics { price: 80.0, ..Economics::default() });
    }

    #[test]
    fn lot_value_diminishes_with_size_and_penalises_strips() {
        let e = Economics { ref_lot_area: 1000.0, ..Economics::default() };
        assert_eq!(e.land_value(1000.0, 1.0), 120_000.0, "the reference lot sells at price");
        let big = e.land_value(4000.0, 1.0);
        assert!((big - 120.0 * 1000.0 * 4f64.powf(0.85)).abs() < 1e-6);
        assert!(big / 4000.0 < 120.0, "bigger lots earn less per m²");
        assert_eq!(e.land_value(1000.0, 2.5), 120_000.0, "no penalty up to 2.5 : 1");
        assert!((e.land_value(1000.0, 3.0) - 108_000.0).abs() < 1e-6, "−10 % at 3 : 1");
        let linear = Economics { size_exponent: 1.0, ..e };
        assert_eq!(linear.land_value(4000.0, 1.0), 480_000.0);
        assert_eq!(Economics::default().resolved(500.0).ref_lot_area, 500.0, "0 = the plan's");
        assert_eq!(e.resolved(500.0).ref_lot_area, 1000.0);
    }

    /// No strip lots, no super-blocks by default; `off` lifts a limit.
    #[test]
    fn realistic_lot_and_block_limits_hold_by_default() {
        let d = OptimizeRequest::default();
        assert_eq!((d.max_depth_ratio, d.max_block_perimeter, d.lot_rule_share), (Some(3.0), Some(800.0), 0.05));
        let r = optimize(&rect(200.0, 150.0), &SubdivisionSettings::default(), &req(24)).unwrap();
        for c in &r.candidates {
            let q = &c.eval.quantities;
            let deep = q.depth_ratios.iter().filter(|x| **x > 3.0).count() as f64;
            assert!(deep <= 0.05 * q.depth_ratios.len() as f64, "#{} {:?}", c.rank, q.depth_ratios);
            assert!(q.max_block_perimeter <= 800.0, "#{}", c.rank);
            assert_eq!(c.eval.infeasibility, 0.0);
        }
        let crate::Command::SiteOptimize { request, .. } = parse(
            "siteoptimize last max_depth_ratio=off max_block_perimeter=600 max_lot_area=9000 lot_rule_share=5 size_exponent=0.9",
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(request.max_depth_ratio, None);
        assert_eq!(request.max_block_perimeter, Some(600.0));
        assert_eq!(request.max_lot_area, Some(9000.0));
        assert_eq!(request.lot_rule_share, 0.05);
        assert_eq!(request.economics.size_exponent, 0.9);
    }

    /// `lotsettings preset=walkable` loads the walkable neighbourhood (later
    /// keys refine it); a walkable site plan meets the optimizer's default
    /// walkability limits, heights stay within the cap and vary.
    #[test]
    fn walkable_preset_plans_a_walkable_neighbourhood() {
        let mut s = Session::default();
        run(&mut s, "lotsettings preset=walkable floors_cap=5");
        let want = SubdivisionSettings { floors_cap: 5, ..SubdivisionSettings::walkable() };
        assert_eq!(s.doc.subdivision_settings, want);
        let site = rect(400.0, 300.0);
        let e = evaluate(&site, &want, &PlanEdits::default(), &OptimizeRequest::default()).unwrap();
        let q = &e.quantities;
        let avg = e.report.avg_lot_area;
        assert!((300.0..=900.0).contains(&avg), "residential lots: {avg}");
        assert!(q.max_block_perimeter <= 800.0, "{}", q.max_block_perimeter);
        assert!(e.report.open_space_ratio >= 0.05, "parks reserved");
        let f = q.floors_sorted();
        assert!(f.iter().all(|n| (1..=5).contains(n)) && f.first() != f.last(), "{f:?}");
    }

    /// Facade daylight at 40° N and in the tropics (5° N, about Bogotá): the
    /// period is never a hemisphere assumption — the worst month is found
    /// from the sun; taller buildings shade more; overheating is measured on
    /// the hottest month; daylight constraints need a location.
    #[test]
    fn facade_daylight_by_latitude_and_period() {
        use crate::site_metrics::{hottest_month, worst_month};
        assert_eq!((worst_month(40.0), hottest_month(40.0)), (12, 6));
        assert_eq!((worst_month(-40.0), hottest_month(-40.0)), (6, 12));
        let site = rect(200.0, 150.0);
        let s = SubdivisionSettings::walkable();
        let plan = |s: &SubdivisionSettings| {
            lot::plan_site(&site, 0.0, s, &BTreeMap::new(), &mut Vec::new(), &mut Vec::new()).unwrap()
        };
        let (low, tall) = (plan(&s), plan(&SubdivisionSettings { floors_min: 6, floors_max: 6, floors_cap: 0, ..s.clone() }));
        let madrid = Location { lat_deg: 40.4, lon_deg: -3.7, tz_hours: 1.0 };
        let bogota = Location { lat_deg: 4.7, lon_deg: -74.1, tz_hours: -5.0 };
        let opts = |period, hours| DaylightOpts { period, hours, overheat_hours: Some(6.0), climate: None };
        let fd = |p: &SitePlanBake, loc: &Location, o: &DaylightOpts| facade_daylight(p, s.floor_height, loc, o).unwrap();
        for loc in [madrid, bogota] {
            let annual = fd(&low, &loc, &opts(DaylightPeriod::Annual, 2.0));
            let worst = fd(&low, &loc, &opts(DaylightPeriod::WorstMonth, 2.0));
            let high = fd(&tall, &loc, &opts(DaylightPeriod::Annual, 2.0));
            eprintln!("lat {}: annual {annual:?} worst {worst:?} tall {high:?}", loc.lat_deg);
            for v in [annual.facade, annual.floors, worst.facade, annual.overheat.unwrap()] {
                assert!((0.0..=1.0).contains(&v));
            }
            assert!(annual.facade > 0.2, "a low neighbourhood is mostly daylit");
            assert!(high.facade <= annual.facade, "taller shades more");
            assert!(annual.floors >= annual.facade - 1e-9, "a storey needs one daylit sample");
        }
        let req = OptimizeRequest { budget: 8, min_daylight: Some(0.1), ..OptimizeRequest::default() };
        let err = optimize(&site, &s, &req).unwrap_err();
        assert!(err.contains("needs a location"), "{err}");
        let r = optimize(&site, &s, &OptimizeRequest { location: Some(madrid), ..req }).unwrap();
        assert!(r.candidates.iter().all(|c| c.eval.quantities.daylight.is_some_and(|d| d >= 0.1)));
        let crate::Command::SiteOptimize { request, .. } =
            parse("siteoptimize last daylight_period=03-15 max_overheat=30 overheat_hours=5").unwrap()
        else {
            panic!()
        };
        assert_eq!(request.daylight_period, DaylightPeriod::Custom { month: 3, day: 15 });
        assert_eq!((request.max_overheat, request.overheat_hours), (Some(0.3), 5.0));
    }

    #[test]
    fn pareto_axis_skips_the_objective_and_flat_metrics() {
        let y = |lots: usize, area: f64, open: f64| Evaluation {
            report: YieldReport::compute(&YieldInputs {
                lot_areas: vec![area / lots as f64; lots],
                gross_site_area: 1000.0,
                reserved_block_areas: vec![open * 1000.0],
                ..YieldInputs::default()
            }),
            violations: Vec::new(),
            infeasibility: 0.0,
            score: 0.0,
            quantities: Quantities::default(),
            ledger: Ledger::default(),
        };
        let (a, b) = (y(10, 100.0, 0.0), y(12, 90.0, 0.0));
        assert_eq!(pareto_axis(Objective::Lots, &[&a, &b]), "net_area");
        assert_eq!(pareto_axis(Objective::NetArea, &[&a, &b]), "lots");
        let (c, d) = (y(10, 100.0, 0.1), y(12, 90.0, 0.2));
        assert_eq!(pareto_axis(Objective::Lots, &[&c, &d]), "open");
        assert_eq!(pareto_axis(Objective::Lots, &[&a]), "open", "nothing varies");
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

    /// #160: a `lotset` override is scored by the search, survives apply when
    /// its lot did not move, and is dropped (and reported) when it did; undo
    /// brings it back.
    #[test]
    fn apply_keeps_matching_lot_overrides_and_reports_dropped() {
        use crate::Command;
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 200 150");
        let source = s.doc.objects().next().unwrap().id;
        run(&mut s, "siteplan last");
        let lot = s
            .doc
            .objects()
            .find(|o| o.layer == lot::LOTS_LAYER)
            .unwrap()
            .id;
        run(&mut s, &format!("lotset id:{lot} floors=9"));
        let before = s.doc.subdivision_links[&source].lot_overrides.clone();
        assert_eq!(before.len(), 1);

        // Only the typology and heights are searched, so the lots stay put
        // (no parks: the green-space defaults are lifted).
        run(
            &mut s,
            &format!(
                "siteoptimize id:{source} budget=4 objective=gfa lock=seed,street_pattern,\
                 road_width,block_depth,method,lot_area_min,lot_area_max,lot_width_min,\
                 open_space_reserve_frac min_open=off min_park_access=off"
            ),
        );
        let chosen = s.last_site_optimize.as_ref().unwrap().1.get(1).unwrap().clone();
        let site = site_boundary(&s.doc, &[source]).unwrap().1;
        let settings =
            settings_with(&s.doc.subdivision_links[&source].settings, &chosen.params).unwrap();
        let bare = evaluate(&site, &settings, &PlanEdits::default(), &OptimizeRequest::default())
            .unwrap();
        assert!(
            chosen.eval.report.total_gfa > bare.report.total_gfa,
            "the pinned floors are scored"
        );
        let out = run(&mut s, "siteoptimize apply 1");
        assert!(!out.message.contains("dropped"), "{}", out.message);
        assert_eq!(s.doc.subdivision_links[&source].lot_overrides, before, "override kept");

        // A candidate that moves every lot drops the pin and says so.
        let area = s.doc.subdivision_links[&source].settings.lot_area_min * 10.0;
        let out = s
            .run(Command::SiteOptimizeApply {
                rank: None,
                source: Some(source),
                params: Some(BTreeMap::from([("lot_area_min".into(), area.to_string())])),
                ids: None,
            })
            .unwrap();
        assert!(
            out.message.contains("1 lot override(s) no longer match"),
            "{}",
            out.message
        );
        assert!(s.doc.subdivision_links[&source].lot_overrides.is_empty());
        run(&mut s, "undo");
        assert_eq!(s.doc.subdivision_links[&source].lot_overrides, before, "undo restores it");
    }

    /// #170: dwellings come from GFA through the unit-size rule, per building
    /// and in total; they can be the objective and a constraint, and a rule
    /// override is folded into the applied params.
    #[test]
    fn dwellings_objective_constraint_and_rule_override() {
        let site = rect(200.0, 150.0);
        let base = SubdivisionSettings::walkable();
        let e = evaluate(
            &site,
            &base,
            &PlanEdits::default(),
            &OptimizeRequest::default(),
        )
        .unwrap();
        let d = e.report.dwellings.as_ref().unwrap();
        let rule = base.dwelling_rule();
        assert_eq!(
            d.total,
            e.quantities
                .buildings
                .iter()
                .map(|b| b.dwellings)
                .sum::<usize>()
        );
        assert!(
            e.quantities
                .buildings
                .iter()
                .all(|b| b.dwellings == rule.dwellings(b.gfa))
        );
        assert!(
            d.total > e.report.building_count,
            "multi-storey buildings hold several dwellings"
        );
        assert!(d.per_ha_net.unwrap() > 0.0);

        let r = optimize(
            &site,
            &base,
            &OptimizeRequest {
                objective: Objective::Dwellings,
                budget: 24,
                ..OptimizeRequest::default()
            },
        )
        .unwrap();
        for c in &r.candidates {
            assert_eq!(
                c.eval.score,
                c.eval.report.dwellings.as_ref().unwrap().total as f64
            );
        }
        assert_eq!(
            r.to_json()["candidates"][0]["dwellings"],
            r.candidates[0].eval.score
        );
        assert!(r.to_markdown().contains("| dwellings |"));

        // A bigger unit halves the count; it rides along in the params.
        let req = OptimizeRequest {
            objective: Objective::Dwellings,
            budget: 24,
            dwelling_area: Some(160.0),
            ..OptimizeRequest::default()
        };
        let big = optimize(&site, &base, &req).unwrap();
        assert_eq!(
            big.candidates[0]
                .params
                .get("dwelling_area")
                .map(String::as_str),
            Some("160")
        );
        let cur = |r: &OptimizeResult| {
            r.current
                .as_ref()
                .unwrap()
                .report
                .dwellings
                .as_ref()
                .unwrap()
                .total
        };
        assert!(cur(&big) < cur(&r), "{} vs {}", cur(&big), cur(&r));

        // As a constraint: the floor holds for every candidate, or the search
        // names it.
        let need = (cur(&r) as f64 * 0.8).floor();
        let c = optimize(
            &site,
            &base,
            &OptimizeRequest {
                min_dwellings: Some(need),
                budget: 24,
                ..OptimizeRequest::default()
            },
        )
        .unwrap();
        assert!(
            c.candidates
                .iter()
                .all(|c| c.eval.report.dwellings.as_ref().unwrap().total as f64 >= need)
        );
        let err = optimize(
            &site,
            &base,
            &OptimizeRequest {
                min_density: Some(1e6),
                budget: 8,
                ..OptimizeRequest::default()
            },
        )
        .unwrap_err();
        assert!(err.contains("binding constraint: min_density"), "{err}");

        let crate::Command::SiteOptimize { request, .. } =
            parse("siteoptimize last objective=dwellings min_dwellings=40 min_density=off dwelling_area=90 dwelling_efficiency=75").unwrap()
        else {
            panic!()
        };
        assert_eq!(request.objective, Objective::Dwellings);
        assert_eq!(
            (request.min_dwellings, request.min_density),
            (Some(40.0), None)
        );
        assert_eq!(
            (request.dwelling_area, request.dwelling_efficiency),
            (Some(90.0), Some(0.75))
        );
        let bad = OptimizeRequest {
            dwelling_efficiency: Some(1.5),
            ..req
        };
        assert!(
            optimize(&site, &base, &bad)
                .unwrap_err()
                .contains("dwelling_efficiency")
        );
    }

    /// The unit-size rule is a lot setting; the document yield (`lotreport`,
    /// `ijc_subdivision_yield_json`) reports dwellings per building, per lot
    /// (buildings matched to their lot) and in total.
    #[test]
    fn lotsettings_rule_drives_the_document_yield() {
        let mut s = Session::default();
        run(
            &mut s,
            "lotsettings preset=walkable dwelling_area=100 dwelling_efficiency=75",
        );
        assert_eq!(
            (
                s.doc.subdivision_settings.dwelling_area,
                s.doc.subdivision_settings.dwelling_efficiency
            ),
            (100.0, 0.75)
        );
        run(&mut s, "rect 0,0,0 200 150");
        let source = s.doc.objects().next().unwrap().id;
        run(&mut s, "siteplan last");
        let link = s.doc.subdivision_links[&source].clone();
        assert_eq!(link.settings.dwelling_area, 100.0);
        let mut ids = vec![source];
        ids.extend(link.produced.iter().copied());
        let r = lot::yield_report(&s.doc, &ids, &link.settings);
        let d = r.dwellings.as_ref().unwrap();
        let site = site_boundary(&s.doc, &[source]).unwrap().1;
        let e = evaluate(
            &site,
            &link.settings,
            &PlanEdits::default(),
            &OptimizeRequest::default(),
        )
        .unwrap();
        assert_eq!(
            d.total,
            e.report.dwellings.as_ref().unwrap().total,
            "baked = scored"
        );
        let per_lot = d.per_lot.unwrap();
        assert!(
            per_lot.min < per_lot.max,
            "buildings matched to lots: {per_lot:?}"
        );
        assert!((per_lot.avg - d.total as f64 / r.lot_count as f64).abs() < 1e-9);
        let out = run(&mut s, "lotreport");
        assert!(out.message.contains("**Dwellings**"), "{}", out.message);
    }

    /// #170: progress after every chunk, a cancel stops the search, and a
    /// watched search that completes ranks exactly like an unwatched one.
    #[test]
    fn progress_and_cancel() {
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicBool, Ordering};
        let site = rect(200.0, 150.0);
        let base = SubdivisionSettings::default();
        let seen = Mutex::new(Vec::<Progress>::new());
        let log = |p: &Progress| seen.lock().unwrap().push(*p);
        let ctl = Control {
            cancel: None,
            progress: Some(&log),
        };
        let watched = optimize_ctl(&site, &base, &PlanEdits::default(), &req(60), ctl).unwrap();
        assert_eq!(
            ranking(&watched),
            ranking(&optimize(&site, &base, &req(60)).unwrap()),
            "same result"
        );
        let seen = seen.into_inner().unwrap();
        assert!(seen.len() >= 60 / CHUNK, "{seen:?}");
        assert!(seen.windows(2).all(|w| w[0].evaluated < w[1].evaluated));
        let last = seen.last().unwrap();
        assert_eq!(
            (last.phase, last.evaluated, last.budget, last.feasible),
            (Phase::Search, 60, 60, watched.feasible)
        );
        assert_eq!(
            last.best,
            watched
                .candidates
                .iter()
                .map(|c| c.eval.score)
                .reduce(f64::max)
        );
        assert_eq!(last.fraction(), 1.0);

        // Cancel from the progress callback after the first chunk.
        let cancel = AtomicBool::new(false);
        let calls = Mutex::new(0);
        let stop = |_: &Progress| {
            *calls.lock().unwrap() += 1;
            cancel.store(true, Ordering::Relaxed);
        };
        let ctl = Control {
            cancel: Some(&cancel),
            progress: Some(&stop),
        };
        let err = optimize_ctl(&site, &base, &PlanEdits::default(), &req(400), ctl).unwrap_err();
        assert_eq!(err, CANCELLED);
        assert_eq!(*calls.lock().unwrap(), 1, "stopped at the next chunk");
        // Cancelled up front: nothing is evaluated.
        let ctl = Control {
            cancel: Some(&cancel),
            progress: None,
        };
        assert_eq!(
            optimize_ctl(&site, &base, &PlanEdits::default(), &req(400), ctl).unwrap_err(),
            CANCELLED
        );

        // The daylight pass reports its own phase.
        let phases = Mutex::new(Vec::new());
        let log = |p: &Progress| phases.lock().unwrap().push(p.phase);
        let madrid = Location {
            lat_deg: 40.4,
            lon_deg: -3.7,
            tz_hours: 1.0,
        };
        let sunny = OptimizeRequest {
            budget: 16,
            min_daylight: Some(0.05),
            location: Some(madrid),
            ..OptimizeRequest::default()
        };
        let ctl = Control {
            cancel: None,
            progress: Some(&log),
        };
        optimize_ctl(
            &site,
            &SubdivisionSettings::walkable(),
            &PlanEdits::default(),
            &sunny,
            ctl,
        )
        .unwrap();
        assert!(phases.into_inner().unwrap().contains(&Phase::Daylight));
    }

    /// #170: the park walk follows the plan's one street graph — the graph
    /// `ijc_street_graph_json` draws from the baked roads is the graph the
    /// optimizer walks, and editing a street node changes the walk.
    #[test]
    fn editing_a_street_node_changes_the_walk() {
        use crate::lot::street_net::StreetNetwork;
        let mut s = Session::default();
        run(&mut s, "lotsettings preset=walkable");
        run(&mut s, "rect 0,0,0 400 300");
        let source = s.doc.objects().next().unwrap().id;
        run(&mut s, "siteplan last");
        let site = site_boundary(&s.doc, &[source]).unwrap().1;
        let walk = |s: &Session| {
            let (base, edits) = base_plan(&s.doc, source);
            let plan = lot::plan_site(
                &site,
                0.0,
                &base,
                &edits.streets,
                &mut edits.edge_roles.clone(),
                &mut edits.lot_overrides.clone(),
            )
            .unwrap();
            let (doc_net, _) = StreetNetwork::from_doc(&s.doc, source);
            let plan_net = StreetNetwork::from_plan(&plan.site);
            assert_eq!(
                doc_net.nodes.len(),
                plan_net.nodes.len(),
                "one street graph"
            );
            assert!((doc_net.length() - plan_net.length()).abs() < 1e-6);
            evaluate(&site, &base, &edits, &OptimizeRequest::default())
                .unwrap()
                .quantities
                .park_walk
        };
        let before = walk(&s);
        assert!(
            before.iter().any(|d| d.is_finite()),
            "parks reachable: {before:?}"
        );
        let graph = lot::streets::street_graph_json(&s.doc, source).unwrap();
        // Bend the first street with an interior-free span: move its middle.
        // The road object of street `id` (a re-derive re-creates them).
        let road = |s: &Session, id: u64| {
            s.doc
                .objects()
                .find(|o| o.street.as_ref().is_some_and(|i| u64::from(i.id) == id))
                .unwrap()
                .id
        };
        let moved = graph["edges"].as_array().unwrap().iter().any(|e| {
            let pts = e["points"].as_array().unwrap();
            let (a, b) = (&pts[0], &pts[pts.len() - 1]);
            let mid = |i: usize| (a[i].as_f64().unwrap() + b[i].as_f64().unwrap()) / 2.0;
            let street = e["street"].as_u64().unwrap();
            // An inserted node changes no geometry; moving it bends the street.
            let insert = format!(
                "streetnode id:{} insert at={},{}",
                road(&s, street),
                mid(0),
                mid(1)
            );
            if s.run(parse(&insert).unwrap()).is_err() {
                return false;
            }
            let bend = format!(
                "streetnode id:{} 1 to={},{}",
                road(&s, street),
                mid(0) + 9.0,
                mid(1) + 9.0
            );
            s.run(parse(&bend).unwrap()).is_ok()
        });
        assert!(moved, "some street takes a node edit");
        let t = &s;
        let after = walk(t);
        assert_ne!(before, after, "the walk follows the edited street");
        assert_ne!(
            lot::streets::street_graph_json(&t.doc, source).unwrap()["edges"],
            graph["edges"]
        );
    }

    /// The tiny synthetic EPW (Guayaquil-like: overcast wet-season
    /// afternoons, March hottest), copied to a temp file the test may delete.
    fn climate_fixture(name: &str) -> std::path::PathBuf {
        let src = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/guayaquil_synthetic.epw"
        );
        let dir = std::env::temp_dir().join(format!("ijc_climate_{}_{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("site.epw");
        std::fs::copy(src, &path).unwrap();
        path
    }

    /// #170: `climate <file.epw>` stores the parsed climate (and the location
    /// when there is none) in one logged, undoable op that replays without
    /// the file; `climate off` clears it.
    #[test]
    fn climate_command_logs_replays_and_undoes() {
        let path = climate_fixture("cmd");
        let mut s = Session::default();
        let out = run(&mut s, &format!("climate {}", path.display()));
        assert!(
            out.message.contains("hottest month Mar (28.5 °C)"),
            "{}",
            out.message
        );
        assert!(
            out.message.contains("location set from the EPW"),
            "{}",
            out.message
        );
        let c = s.doc.climate.clone().unwrap();
        assert_eq!(
            (c.source.as_str(), c.city.as_str(), c.hottest_month()),
            ("site.epw", "Guayaquil Synthetic", Some(3))
        );
        assert_eq!(s.doc.location.unwrap().tz_hours, -5.0);
        let sun = c.sunshine_hours();
        assert_eq!(
            (sun[2], sun[7]),
            (5.0, 12.0),
            "wet-season afternoons are overcast"
        );
        let log = s.save_log();
        std::fs::remove_file(&path).unwrap();
        let replayed = Session::replay(log).unwrap();
        assert_eq!(
            replayed.doc.climate, s.doc.climate,
            "replays without the EPW"
        );
        run(&mut s, "undo");
        assert_eq!(
            (s.doc.climate.is_none(), s.doc.location.is_none()),
            (true, true)
        );
        run(&mut s, "redo");
        assert!(s.doc.climate.is_some());
        run(&mut s, "climate off");
        assert!(s.doc.climate.is_none() && s.doc.location.is_some());
        assert!(parse("climate").is_err());
        let missing = s
            .run(parse("climate /nope/none.epw").unwrap())
            .unwrap_err()
            .to_string();
        assert!(missing.contains("none.epw"), "{missing}");
    }

    /// #170: with a climate, facade daylight counts sunshine (overcast hours
    /// drop out) and overheating uses the hottest month by temperature, not
    /// the longest day; the search report names its source.
    #[test]
    fn facade_daylight_uses_the_climate_when_present() {
        use crate::site_metrics::hottest_month;
        let mut s = Session::default();
        run(&mut s, "lotsettings preset=walkable");
        run(
            &mut s,
            &format!("climate {}", climate_fixture("daylight").display()),
        );
        let climate = std::sync::Arc::new(s.doc.climate.clone().unwrap());
        let l = s.doc.location.unwrap();
        let loc = Location {
            lat_deg: l.lat_deg,
            lon_deg: l.lon_deg,
            tz_hours: l.tz_hours,
        };
        let site = rect(200.0, 150.0);
        let base = SubdivisionSettings::walkable();
        let opts = DaylightOpts {
            period: DaylightPeriod::Annual,
            hours: 3.0,
            overheat_hours: Some(4.0),
            climate: None,
        };
        let clear = plan_daylight(&site, &base, &PlanEdits::default(), &loc, &opts).unwrap();
        let epw = DaylightOpts {
            climate: Some(climate),
            ..opts
        };
        let real = plan_daylight(&site, &base, &PlanEdits::default(), &loc, &epw).unwrap();
        eprintln!("clear {clear:?}\nclimate {real:?}");
        assert_eq!(
            (clear.source, real.source),
            (DaylightSource::ClearSky, DaylightSource::Climate)
        );
        assert!(
            real.facade < clear.facade,
            "overcast afternoons cut sun hours"
        );
        assert_eq!(real.overheat_month, Some(3), "hottest by dry-bulb");
        assert_eq!(clear.overheat_month, Some(hottest_month(loc.lat_deg)));
        assert_ne!(clear.overheat_month, Some(3));

        run(&mut s, "rect 0,0,0 200 150");
        let out = run(&mut s, "siteoptimize last budget=8 max_overheat=95");
        assert!(
            out.message
                .contains("sunshine hours from the EPW climate Guayaquil Synthetic (site.epw)"),
            "{}",
            out.message
        );
        assert!(
            out.message.contains("month 3 (hottest month)"),
            "{}",
            out.message
        );
        let r = &s.last_site_optimize.as_ref().unwrap().1;
        assert_eq!(r.to_json()["daylight_source"]["source"], "climate");
        assert!(
            r.candidates
                .iter()
                .all(|c| c.eval.quantities.daylight_source == Some(DaylightSource::Climate))
        );
        let out = run(
            &mut s,
            "siteoptimize last budget=8 max_overheat=95 climate=off",
        );
        assert!(
            out.message.contains("clear-sky sun hours"),
            "{}",
            out.message
        );
        let out = run(&mut s, "siteoptimize last budget=8");
        assert!(
            !out.message.contains("Daylight:"),
            "no daylight, no line: {}",
            out.message
        );
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
