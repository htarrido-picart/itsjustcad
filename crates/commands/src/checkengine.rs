// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Declarative compliance-check engine (M-checkengine) — the shared foundation
//! the IBC/ADA rule packs (M-ibc, M-ada) will ride on.
//!
//! A *rule* is data, never code: a JSON object naming a code section, a
//! severity, a target query over the typed model (object kind / layer / name),
//! a geometric predicate with a threshold, and a message. A *pack* is a named
//! list of rules — user-loadable from disk (`checkrules load`, like plugins)
//! and LLM-authorable (the deck can draft a pack JSON for the user to save).
//! The engine evaluates a pack against the document and emits a structured
//! [`ComplianceReport`] (per-rule verdict + object ids + locations + measured
//! vs required), stored on the document and served by the read-only `report`
//! verb — the same plumbing as the M-enviro [`itsjustcad_doc::AnalysisReport`].
//!
//! ADVISORY ONLY. These are geometric pre-checks, not a code review: every
//! report and command message carries [`ADVISORY_NOTE`]. Nothing here replaces
//! a licensed professional or the authority having jurisdiction.
//!
//! The geometry probes are pure functions, each unit-tested against known
//! geometry → known verdicts. Honest limits, by probe:
//! * slope — evaluated along *curves* (the drawn path/ramp centerline), not
//!   ribbon meshes; same rise-over-run math as the landscape sitepath advisory.
//! * door clear width — doors are block instances; width is the `width` param
//!   of a parametric door (pdoor family) times the instance scale, else the
//!   block definition's larger plan extent times scale. Frames wider than the
//!   leaf overestimate; there is no hardware/stop modeling.
//! * riser extraction — clusters the z levels of upward-facing mesh faces;
//!   works on stepped (boxy) stair meshes, not ramped/smoothed ones.
//! * headroom / clear width — vertical / perpendicular ray-casts against the
//!   scene's triangle meshes, sampled along a curve; curves and annotations
//!   never obstruct (they have no surface).
//! * turning circle — a simplified planar disc (default 60 in) tested against
//!   triangles in a person-height band; no door-swing maneuvering geometry.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use glam::DVec3;
use itsjustcad_doc::{
    AreaKind, ComplianceReport, Document, FrameKind, Geometry, Room, RuleOutcome, SceneObject,
    Story,
};
use kernel_mesh::{Mesh, TriBvh};
use serde::{Deserialize, Serialize};

/// Layer the failure markers land on (analysis-layer precedent).
pub const COMPLIANCE_LAYER: &str = "compliance";

/// The disclaimer every compliance report and codecheck message carries.
pub const ADVISORY_NOTE: &str =
    "advisory pre-check, not a code review — verify with a licensed professional / AHJ";

/// Rule severity. Drives the violation verdict ("fail"/"warn"/"info") and the
/// marker color.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warn,
    Info,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warn => "warn",
            Severity::Info => "info",
        }
    }

    /// The verdict string a violation of this severity produces.
    pub fn verdict(self) -> &'static str {
        match self {
            Severity::Error => "fail",
            Severity::Warn => "warn",
            Severity::Info => "info",
        }
    }

    /// Marker color on the compliance layer (red / orange / blue).
    pub fn marker_color(self) -> [f32; 3] {
        match self {
            Severity::Error => [0.85, 0.15, 0.15],
            Severity::Warn => [0.95, 0.55, 0.10],
            Severity::Info => [0.15, 0.45, 0.85],
        }
    }
}

/// Which objects a rule applies to. All present filters must match (AND);
/// an empty query matches every object.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TargetQuery {
    /// Object kinds, any-of: "curve"|"path", "mesh", "block", "door", "stair",
    /// "slab", "wall", "beam", "column". Empty = any kind.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Exact layer name.
    #[serde(default)]
    pub layer: Option<String>,
    /// Case-insensitive substring of the object name.
    #[serde(default)]
    pub name_contains: Option<String>,
}

/// The geometric predicate + threshold of one rule. Thresholds are meters
/// (slope is rise-over-run; count is a count).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CheckKind {
    /// Path/ramp slope along target curves must not exceed `limit` (rise/run).
    MaxSlope { limit: f64 },
    /// Door clear width (see module docs for how width is derived) ≥ `min`.
    MinDoorWidth { min: f64 },
    /// Every riser extracted from a stair-like mesh ≤ `max`.
    MaxRiser { max: f64 },
    /// Vertical clearance sampled along target curves ≥ `min`.
    MinHeadroom { min: f64 },
    /// Perpendicular clear width sampled along target curves ≥ `min`.
    MinClearWidth { min: f64 },
    /// A planar circle of `diameter` must fit at each target curve's endpoints.
    TurningCircle { diameter: f64 },
    /// At least `min` matching objects per story (whole doc when no stories).
    MinCountPerStory { min: usize },
    /// Walking surfaces whose top sits more than `drop` above z=0 get flagged
    /// (typically severity "info": verify guards).
    GuardDrop { drop: f64 },
    /// Occupant load per tagged room = area ÷ the per-occupancy load factor
    /// (IBC Table 1004.5, m²/occupant, carried as DATA here — never hard-coded).
    /// Informational: reports the computed load; the `report` shows the count.
    /// The rule "passes" as long as every matched room has a factor; a room
    /// whose occupancy has no factor entry is flagged. `unknown_factor` is the
    /// fallback (gross m²/occupant) when an occupancy is missing from the table.
    OccupantLoad {
        /// occupancy family → gross area (m²) per occupant.
        factors: BTreeMap<String, f64>,
    },
    /// Required number of exits from a room, from its occupant load via the
    /// pack's `factors` and the exit-count `thresholds` (IBC 1006.3.2: each
    /// entry is `[max_occupant_load, exits_required]`, ascending). Flags rooms
    /// that (by the exit objects nearby) appear to provide fewer than required.
    /// Exits are objects whose name contains `exit_name` (default "exit").
    ExitCount {
        factors: BTreeMap<String, f64>,
        /// Ascending `[max_load, required_exits]` bands (IBC 1006.3.2).
        thresholds: Vec<[f64; 2]>,
        #[serde(default = "default_exit_name")]
        exit_name: String,
    },
    /// Straight-line travel distance from each room's centroid to the nearest
    /// exit-tagged object ≤ `max` (IBC 1017.2; the sprinklered assumption is
    /// noted in the rule message). Honest limit: straight-line, NOT the routed
    /// path — see the module docs.
    TravelDistance {
        max: f64,
        #[serde(default = "default_exit_name")]
        exit_name: String,
    },
    /// Stair tread depth = plan run of the stair mesh ÷ its riser count ≥ `min`
    /// (IBC 1011.5.2). Plan run is the stair's horizontal extent along its long
    /// plan axis; riser count comes from the same clustering as `max_riser`.
    TreadDepth { min: f64 },
    /// Ramp landings along a ramp centerline curve (ADA 405.7): flags a run
    /// that rises more than `max_rise` (30 in) without an intervening landing —
    /// a maximal near-flat stretch (slope ≤ `flat_limit`, default 1:48) at
    /// least `landing_len` (60 in) long. See [`missing_landings`] for the
    /// honest limits (reads the drawn centerline; no landing-width geometry).
    RampLanding {
        #[serde(default = "default_flat_limit")]
        flat_limit: f64,
        landing_len: f64,
        max_rise: f64,
    },
    /// Threshold/step height at a door ≤ `max` (ADA 404.2.5). Read from the
    /// door block's `threshold` param; a door with no such param is flagged as
    /// "not modelably detectable" (honest — the model carries no small-step
    /// geometry), NOT as a pass or a fake measurement. See [`threshold_height`].
    ThresholdHeight { max: f64 },
    /// Cross-slope of a walking-surface MESH (ADA 403.3 / 405.3) ≤ `limit`
    /// (default 1:48). Distinct from `max_slope` (which is the running grade of
    /// a centerline CURVE); this measures the perpendicular grade of a ramp/
    /// route surface via up-facing face normals. See [`mesh_cross_slope`].
    MaxCrossSlope { limit: f64 },
    /// Accessible count vs total (ADA Table 208.2 parking, etc.): for `total`
    /// matched objects at least the table's `min_accessible` must ALSO be named
    /// `accessible_name`. Total = objects matching the rule target; accessible
    /// = those whose name additionally contains `accessible_name`. `thresholds`
    /// is ascending `[max_total, min_accessible]` bands (ADA Table 208.2).
    /// Honest: a pure name/count heuristic — no stall/aisle geometry.
    CountRatio {
        /// Ascending `[max_total, min_accessible]` bands (ADA Table 208.2).
        thresholds: Vec<[f64; 2]>,
        #[serde(default = "default_accessible_name")]
        accessible_name: String,
    },
}

fn default_flat_limit() -> f64 {
    1.0 / 48.0
}

fn default_accessible_name() -> String {
    "accessible".to_string()
}

fn default_exit_name() -> String {
    "exit".to_string()
}

impl CheckKind {
    /// (threshold, unit) for the report's measured-vs-required columns. For
    /// table-driven kinds (occupant load, exit count) there is no single scalar
    /// threshold, so `required` is `None` and only the unit is meaningful.
    fn required(&self) -> (Option<f64>, &'static str) {
        match self {
            CheckKind::MaxSlope { limit } => (Some(*limit), "rise/run"),
            CheckKind::MinDoorWidth { min }
            | CheckKind::MinHeadroom { min }
            | CheckKind::MinClearWidth { min }
            | CheckKind::TreadDepth { min } => (Some(*min), "m"),
            CheckKind::MaxRiser { max } => (Some(*max), "m"),
            CheckKind::TurningCircle { diameter } => (Some(*diameter), "m"),
            CheckKind::MinCountPerStory { min } => (Some(*min as f64), "count"),
            CheckKind::GuardDrop { drop } => (Some(*drop), "m"),
            CheckKind::OccupantLoad { .. } => (None, "occupants"),
            CheckKind::ExitCount { .. } => (None, "exits"),
            CheckKind::TravelDistance { max, .. } => (Some(*max), "m"),
            CheckKind::RampLanding { max_rise, .. } => (Some(*max_rise), "m rise/run"),
            CheckKind::ThresholdHeight { max } => (Some(*max), "m"),
            CheckKind::MaxCrossSlope { limit } => (Some(*limit), "rise/run"),
            CheckKind::CountRatio { .. } => (None, "count"),
        }
    }

    /// Validate the kind's own data (thresholds and, for table-driven kinds,
    /// the embedded factor/threshold tables). Called by [`CheckPack::from_json`].
    fn validate(&self) -> Result<(), String> {
        // Scalar threshold, when present, must be positive + finite.
        if let (Some(t), _) = self.required()
            && (!t.is_finite() || t <= 0.0)
        {
            return Err(format!("non-positive/non-finite threshold {t}"));
        }
        let check_factors = |factors: &BTreeMap<String, f64>| -> Result<(), String> {
            if factors.is_empty() {
                return Err("occupant-load factor table is empty".into());
            }
            for (occ, f) in factors {
                if !f.is_finite() || *f <= 0.0 {
                    return Err(format!("occupancy '{occ}' has a non-positive factor {f}"));
                }
            }
            Ok(())
        };
        match self {
            CheckKind::OccupantLoad { factors } => check_factors(factors)?,
            CheckKind::ExitCount { factors, thresholds, .. } => {
                check_factors(factors)?;
                if thresholds.is_empty() {
                    return Err("exit-count thresholds table is empty".into());
                }
                for t in thresholds {
                    if !t[0].is_finite() || t[1] < 1.0 {
                        return Err(format!("bad exit-count band {t:?}"));
                    }
                }
            }
            CheckKind::RampLanding { flat_limit, landing_len, .. } => {
                if !flat_limit.is_finite() || *flat_limit < 0.0 {
                    return Err(format!("ramp-landing flat_limit {flat_limit} is negative/non-finite"));
                }
                if !landing_len.is_finite() || *landing_len <= 0.0 {
                    return Err(format!("ramp-landing landing_len {landing_len} is non-positive"));
                }
            }
            CheckKind::CountRatio { thresholds, .. } => {
                if thresholds.is_empty() {
                    return Err("count-ratio thresholds table is empty".into());
                }
                for t in thresholds {
                    if !t[0].is_finite() || t[1] < 0.0 {
                        return Err(format!("bad count-ratio band {t:?}"));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Required exit count for `load` occupants from ascending `[max_load, exits]`
/// bands (IBC 1006.3.2). The first band whose `max_load` covers `load` wins;
/// a load beyond the last band takes the last band's exit count.
fn required_exits(load: f64, thresholds: &[[f64; 2]]) -> usize {
    let mut sorted: Vec<[f64; 2]> = thresholds.to_vec();
    sorted.sort_by(|a, b| a[0].total_cmp(&b[0]));
    for band in &sorted {
        if load <= band[0] {
            return band[1] as usize;
        }
    }
    sorted.last().map(|b| b[1] as usize).unwrap_or(1)
}

/// Minimum accessible count required for `total` items from ascending
/// `[max_total, min_accessible]` bands (ADA Table 208.2). The first band whose
/// `max_total` covers `total` wins; beyond the last band the last band applies.
fn min_accessible(total: f64, thresholds: &[[f64; 2]]) -> usize {
    let mut sorted: Vec<[f64; 2]> = thresholds.to_vec();
    sorted.sort_by(|a, b| a[0].total_cmp(&b[0]));
    for band in &sorted {
        if total <= band[0] {
            return band[1] as usize;
        }
    }
    sorted.last().map(|b| b[1] as usize).unwrap_or(0)
}

/// One declarative rule. Pure data — see the module docs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CheckRule {
    pub id: String,
    #[serde(default)]
    pub code_ref: String,
    pub severity: Severity,
    #[serde(default)]
    pub target: TargetQuery,
    pub check: CheckKind,
    #[serde(default)]
    pub message: String,
}

/// A named rule pack (JSON file). Loadable from disk like plugins.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CheckPack {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub rules: Vec<CheckRule>,
}

impl CheckPack {
    /// Parse + validate a pack from JSON. Rejects malformed JSON, unknown
    /// severities/check kinds (serde), empty packs, empty/duplicate rule ids,
    /// unsafe names, and non-finite/non-positive thresholds.
    pub fn from_json(s: &str) -> Result<Self, String> {
        let pack: CheckPack = serde_json::from_str(s).map_err(|e| e.to_string())?;
        if pack.name.trim().is_empty() || pack.name.contains(['/', '\\', '.']) {
            return Err(format!("pack name {:?} is empty or unsafe", pack.name));
        }
        if pack.rules.is_empty() {
            return Err(format!("pack '{}' has no rules", pack.name));
        }
        let mut seen = std::collections::BTreeSet::new();
        for r in &pack.rules {
            if r.id.trim().is_empty() {
                return Err(format!("pack '{}' has a rule with an empty id", pack.name));
            }
            if !seen.insert(r.id.as_str()) {
                return Err(format!("pack '{}' has a duplicate rule id '{}'", pack.name, r.id));
            }
            r.check
                .validate()
                .map_err(|e| format!("rule '{}': {e}", r.id))?;
        }
        Ok(pack)
    }
}

/// The embedded demo pack (assets/checks-demo.json): ~6 rules exercising the
/// probes. Proves the engine; the real IBC/ADA packs are separate phases.
pub fn demo_pack() -> CheckPack {
    CheckPack::from_json(include_str!("../../../assets/checks-demo.json"))
        .expect("embedded demo pack is valid")
}

/// The embedded IBC 2021 pack (assets/checks-ibc2021.json): ~12 rules covering
/// stairs, egress, occupant load, exit count, travel distance, and habitable
/// ceiling height. ADVISORY ONLY — every message carries the disclaimer.
pub fn ibc_pack() -> CheckPack {
    CheckPack::from_json(include_str!("../../../assets/checks-ibc2021.json"))
        .expect("embedded IBC 2021 pack is valid")
}

/// The embedded ADA 2010 / A117.1 pack (assets/checks-ada.json): ~12 rules
/// covering accessible ramps (running + cross slope + landings), routes, doors
/// (clear width + threshold), turning + clear floor space, handrail height, and
/// accessible parking count. ADVISORY ONLY. Reach ranges and door maneuvering
/// clearances are DELIBERATELY OMITTED — the model has no fixture-height or
/// door-swing geometry, so faking them would be dishonest (see PHASES M-ada).
pub fn ada_pack() -> CheckPack {
    CheckPack::from_json(include_str!("../../../assets/checks-ada.json"))
        .expect("embedded ADA 2010 pack is valid")
}

/// Default on-disk location for user check packs:
/// `~/.config/itsjustcad/checks/<name>.checks.json`.
pub fn default_dir() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".config").join("itsjustcad").join("checks"))
}

/// Load every `*.checks.json` pack under `dir` (absent dir = empty). Malformed
/// files are skipped with the error collected, plugin-loader style.
pub fn load_dir(dir: &Path) -> (BTreeMap<String, CheckPack>, Vec<String>) {
    let mut packs = BTreeMap::new();
    let mut warnings = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (packs, warnings);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.to_string_lossy().ends_with(".checks.json") {
            continue;
        }
        match std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|s| CheckPack::from_json(&s))
        {
            Ok(p) => {
                packs.insert(p.name.clone(), p);
            }
            Err(e) => warnings.push(format!("{}: {e}", path.display())),
        }
    }
    (packs, warnings)
}

// ── Geometry probes (pure functions) ─────────────────────────────────────────

/// Densify a polyline: keep every vertex and insert extra samples so no two
/// consecutive samples are further than `step` apart. Probes sample these.
pub fn resample_polyline(points: &[DVec3], step: f64) -> Vec<DVec3> {
    let mut out = Vec::new();
    if points.is_empty() || step <= 0.0 {
        return out;
    }
    out.push(points[0]);
    for w in points.windows(2) {
        let len = (w[1] - w[0]).length();
        let n = (len / step).ceil().max(1.0) as usize;
        for i in 1..=n {
            out.push(w[0] + (w[1] - w[0]) * (i as f64 / n as f64));
        }
    }
    out
}

/// Slope probe: max rise-over-run between consecutive samples plus the
/// midpoints of every segment steeper than `limit`. Same math as the
/// M-landscape sitepath advisory (`landscape::path_slope_check`).
pub fn steep_segments(samples: &[DVec3], limit: f64) -> (f64, Vec<DVec3>) {
    let mut max_slope = 0.0f64;
    let mut mids = Vec::new();
    for w in samples.windows(2) {
        let run = (w[1] - w[0]).truncate().length();
        if run < 1e-12 {
            continue;
        }
        let s = (w[1].z - w[0].z).abs() / run;
        max_slope = max_slope.max(s);
        if s > limit {
            mids.push((w[0] + w[1]) * 0.5);
        }
    }
    (max_slope, mids)
}

/// Riser extraction from a stair-like mesh: the z levels of upward-facing face
/// centroids, clustered within 30 mm, sorted; risers are the consecutive level
/// differences over 40 mm (sub-40 mm steps are modeling noise, not risers).
pub fn stair_risers(mesh: &Mesh) -> Vec<f64> {
    let pos = mesh.positions();
    let mut zs: Vec<f64> = Vec::new();
    for f in mesh.faces() {
        let (a, b, c) = (pos[f[0] as usize], pos[f[1] as usize], pos[f[2] as usize]);
        let n = (b - a).cross(c - a);
        let len = n.length();
        if len < 1e-12 || n.z / len < 0.7 {
            continue; // not an upward-facing (tread-like) face
        }
        zs.push((a.z + b.z + c.z) / 3.0);
    }
    zs.sort_by(f64::total_cmp);
    // Cluster into levels (means of groups within 30 mm).
    let mut levels: Vec<f64> = Vec::new();
    let mut group: Vec<f64> = Vec::new();
    for z in zs {
        if let Some(&last) = group.last()
            && z - last > 0.03
        {
            levels.push(group.iter().sum::<f64>() / group.len() as f64);
            group.clear();
        }
        group.push(z);
    }
    if !group.is_empty() {
        levels.push(group.iter().sum::<f64>() / group.len() as f64);
    }
    levels.windows(2).map(|w| w[1] - w[0]).filter(|d| *d > 0.04).collect()
}

/// Headroom probe: vertical clearance above each sample (ray-cast up from
/// 0.1 m above the sample against the scene meshes). `f64::INFINITY` when
/// nothing is overhead.
pub fn headroom(samples: &[DVec3], bvh: &TriBvh) -> Vec<f64> {
    samples
        .iter()
        .map(|p| match bvh.ray_hit(*p + DVec3::new(0.0, 0.0, 0.1), DVec3::Z) {
            Some(t) => t + 0.1,
            None => f64::INFINITY,
        })
        .collect()
}

/// Clear-width probe: at each sample, cast horizontal rays perpendicular to the
/// local path direction (both sides, probed 1 m above the sample) and sum the
/// two clearances, each capped at `cap` so open space reads as `2*cap`, not
/// infinity. Returns one width per sample.
pub fn clear_widths(samples: &[DVec3], bvh: &TriBvh, cap: f64) -> Vec<f64> {
    let n = samples.len();
    (0..n)
        .map(|i| {
            // Central-difference tangent, degrading to one-sided at the ends.
            let a = samples[i.saturating_sub(1)];
            let b = samples[(i + 1).min(n - 1)];
            let t = (b - a).truncate();
            if t.length() < 1e-12 {
                return 2.0 * cap; // degenerate segment: no direction, no verdict
            }
            let t = t.normalize();
            let perp = DVec3::new(-t.y, t.x, 0.0);
            let origin = samples[i] + DVec3::new(0.0, 0.0, 1.0);
            let side = |dir: DVec3| bvh.ray_hit(origin, dir).unwrap_or(cap).min(cap);
            side(perp) + side(-perp)
        })
        .collect()
}

/// Unsigned distance in the XY plane from point `p` to triangle `abc`
/// (0 inside). Used by the turning-circle probe.
fn tri_dist_2d(p: DVec3, a: DVec3, b: DVec3, c: DVec3) -> f64 {
    let (p, a, b, c) = (p.truncate(), a.truncate(), b.truncate(), c.truncate());
    // Inside test via signs of edge cross products.
    let sign = |o: glam::DVec2, e: glam::DVec2, q: glam::DVec2| {
        (e.x - o.x) * (q.y - o.y) - (e.y - o.y) * (q.x - o.x)
    };
    let (d1, d2, d3) = (sign(a, b, p), sign(b, c, p), sign(c, a, p));
    let has_neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let has_pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    if !(has_neg && has_pos) {
        return 0.0;
    }
    // Outside: min distance to the three edges.
    let seg_dist = |a: glam::DVec2, b: glam::DVec2| {
        let ab = b - a;
        let t = ((p - a).dot(ab) / ab.length_squared()).clamp(0.0, 1.0);
        (a + ab * t - p).length()
    };
    seg_dist(a, b).min(seg_dist(b, c)).min(seg_dist(c, a))
}

/// Turning-circle probe (simplified, planar): a horizontal disc of `radius`
/// centered at `center` fits iff no scene triangle whose z range overlaps the
/// person band (`center.z + 0.1 .. center.z + 2.0`) comes within `radius` of
/// the center in plan. The floor underfoot (at or below `center.z`) never
/// blocks; no door-swing maneuvering geometry is modeled.
pub fn circle_fits(center: DVec3, radius: f64, tris: &[[DVec3; 3]]) -> bool {
    let (z_lo, z_hi) = (center.z + 0.1, center.z + 2.0);
    for t in tris {
        let tz_lo = t[0].z.min(t[1].z).min(t[2].z);
        let tz_hi = t[0].z.max(t[1].z).max(t[2].z);
        if tz_hi < z_lo || tz_lo > z_hi {
            continue;
        }
        if tri_dist_2d(center, t[0], t[1], t[2]) < radius {
            return false;
        }
    }
    true
}

/// Door clear-width probe: the parametric `width` param (times instance scale)
/// when present, else the block definition's larger plan extent times scale
/// (`None` when the block definition is unknown). Honest limits in module docs.
pub fn door_width(
    params: &BTreeMap<String, String>,
    def_extent_xy: Option<(f64, f64)>,
    scale: f64,
) -> Option<f64> {
    if let Some(w) = params.get("width").and_then(|s| s.parse::<f64>().ok())
        && w.is_finite()
        && w > 0.0
    {
        return Some(w * scale);
    }
    def_extent_xy.map(|(x, y)| x.max(y) * scale)
}

/// Count matched objects per story band. Bands span each story's elevation up
/// to the next-higher story (the top story is open-ended); an object belongs to
/// the band its AABB bottom falls in. With no stories the whole document is one
/// unnamed band. Returns `(story name, elevation, count)` per band.
pub fn count_per_story(stories: &[Story], bottoms: &[f64]) -> Vec<(String, f64, usize)> {
    if stories.is_empty() {
        return vec![("(document)".to_string(), 0.0, bottoms.len())];
    }
    let mut sorted: Vec<&Story> = stories.iter().collect();
    sorted.sort_by(|a, b| a.elevation.total_cmp(&b.elevation));
    sorted
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let hi = sorted.get(i + 1).map(|n| n.elevation).unwrap_or(f64::INFINITY);
            let count = bottoms
                .iter()
                .filter(|z| **z >= s.elevation - 1e-9 && **z < hi - 1e-9)
                .count();
            (s.name.clone(), s.elevation, count)
        })
        .collect()
}

// ── Room / occupancy probes (M-ibc) ──────────────────────────────────────────

/// Occupant load of a room = area ÷ the per-occupancy factor (m²/occupant),
/// rounded UP (a fractional occupant still counts — IBC 1004.5). `None` when
/// the occupancy has no factor in the table.
pub fn occupant_load(room: &Room, factors: &BTreeMap<String, f64>) -> Option<f64> {
    let f = *factors.get(&room.occupancy)?;
    if f <= 0.0 {
        return None;
    }
    Some((room.area / f).ceil())
}

/// A room's proximity radius for the exit-count check: half its longest
/// boundary chord (bounding-box diagonal), so exits on the room's edges count.
fn room_reach(room: &Room) -> f64 {
    let mut lo = [f64::INFINITY; 2];
    let mut hi = [f64::NEG_INFINITY; 2];
    for p in &room.boundary {
        lo[0] = lo[0].min(p[0]);
        lo[1] = lo[1].min(p[1]);
        hi[0] = hi[0].max(p[0]);
        hi[1] = hi[1].max(p[1]);
    }
    if !lo[0].is_finite() {
        return 0.0;
    }
    let diag = ((hi[0] - lo[0]).powi(2) + (hi[1] - lo[1]).powi(2)).sqrt();
    diag * 0.5
}

/// World positions of exit-tagged objects: any object whose name contains
/// `exit_name` (case-insensitive), reported at its AABB center, optionally
/// restricted to the story `band`.
fn exit_positions(doc: &Document, exit_name: &str, band: Option<(f64, f64)>) -> Vec<DVec3> {
    let needle = exit_name.to_lowercase();
    doc.objects()
        .filter(|o| o.name.as_deref().is_some_and(|n| n.to_lowercase().contains(&needle)))
        .filter(|o| match band {
            Some((lo, hi)) => {
                let a = o.geometry.aabb();
                a.max.z >= lo - 1e-9 && a.min.z < hi - 1e-9
            }
            None => true,
        })
        .map(|o| {
            let a = o.geometry.aabb();
            (a.min + a.max) * 0.5
        })
        .collect()
}

/// Rooms matching a rule's target query (only `name_contains` applies to rooms
/// — they have no layer/kind), restricted to the story `band` by centroid z.
fn rooms_in_band<'a>(
    doc: &'a Document,
    q: &'a TargetQuery,
    band: Option<(f64, f64)>,
) -> impl Iterator<Item = &'a Room> + 'a {
    doc.rooms.iter().filter(move |r| {
        if let Some(needle) = &q.name_contains {
            let needle = needle.to_lowercase();
            if !r.name.to_lowercase().contains(&needle)
                && !r.occupancy.to_lowercase().contains(&needle)
            {
                return false;
            }
        }
        if let Some((lo, hi)) = band {
            let z = r.centroid()[2];
            if z < lo - 1e-9 || z >= hi - 1e-9 {
                return false;
            }
        }
        true
    })
}

/// Stair tread depth from a boxy stair mesh: the plan run along the stair's
/// longer horizontal axis divided by the riser count (`stair_risers`). `None`
/// when fewer than one riser is found (not a stair-like mesh).
pub fn tread_depth(mesh: &Mesh) -> Option<f64> {
    let risers = stair_risers(mesh);
    if risers.is_empty() {
        return None;
    }
    let a = mesh.aabb();
    // Plan run is the longer horizontal extent (stairs run along one plan axis).
    let run = (a.max.x - a.min.x).max(a.max.y - a.min.y);
    // Treads number one fewer than risers on a straight flight, but boxy meshes
    // vary; dividing the run by the riser count is the honest, stable estimate.
    Some(run / risers.len() as f64)
}

// ── ADA / A117.1 probes (M-ada) ───────────────────────────────────────────────

/// Ramp landing analysis along a ramp centerline polyline (ADA 405.7).
/// Walks the samples accumulating rise; a *landing* is a maximal run of
/// consecutive near-flat segments (running slope ≤ `flat_limit`, default the
/// 1:48 cross-slope threshold) whose horizontal length ≥ `landing_len`
/// (60 in). ADA requires a landing at the top and bottom of every ramp run and
/// between runs so that no single run rises more than `max_rise` (30 in) without
/// one. Returns the positions (segment midpoints) where the accumulated rise
/// since the last qualifying landing first exceeds `max_rise` — i.e. a missing
/// landing. Honest limits: this reads the DRAWN centerline; it cannot see
/// landing *width* (only length along the path), and complex switchback
/// geometry that shares samples between runs may under-count. Pure function.
pub fn missing_landings(
    samples: &[DVec3],
    flat_limit: f64,
    landing_len: f64,
    max_rise: f64,
) -> Vec<DVec3> {
    let mut flags = Vec::new();
    if samples.len() < 2 {
        return flags;
    }
    // Rise accumulated on the current (non-landing) run since the last landing.
    let mut run_rise = 0.0f64;
    // Horizontal length + rise of the current candidate flat stretch.
    let mut flat_len = 0.0f64;
    let mut flagged_this_run = false;
    for w in samples.windows(2) {
        let run = (w[1] - w[0]).truncate().length();
        if run < 1e-12 {
            continue;
        }
        let slope = (w[1].z - w[0].z).abs() / run;
        if slope <= flat_limit {
            // Extending a flat stretch; once it reaches a full landing length
            // it resets the run-rise accumulator (a landing was provided).
            flat_len += run;
            if flat_len >= landing_len - 1e-9 {
                run_rise = 0.0;
                flagged_this_run = false;
            }
        } else {
            // A sloped segment ends any flat stretch and adds to the run rise.
            flat_len = 0.0;
            run_rise += (w[1].z - w[0].z).abs();
            if run_rise > max_rise + 1e-9 && !flagged_this_run {
                flags.push((w[0] + w[1]) * 0.5);
                flagged_this_run = true;
            }
        }
    }
    flags
}

/// Threshold height at a door/opening (ADA 404.2.5): the vertical step at the
/// door read from the door block's `threshold` param (meters), else `None`
/// (not modelably detectable — the engine reports this honestly rather than
/// inventing a height). Mirrors [`door_width`]'s param-first honesty.
pub fn threshold_height(params: &BTreeMap<String, String>) -> Option<f64> {
    params
        .get("threshold")
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|h| h.is_finite() && *h >= 0.0)
}

/// Cross-slope of a walking-surface mesh (ADA 403.3 / 405.3): the grade
/// measured ACROSS the surface's dominant run direction. The run axis is taken
/// as the mesh's longer horizontal AABB extent; the cross axis is the shorter.
/// For each up-facing face (normal.z/|n| ≥ 0.7) the cross-slope is the tilt of
/// its normal projected onto the cross axis. Returns the worst (max) cross
/// slope over all up-facing faces, or `None` when the mesh has no walking
/// surface. Honest limit: run/cross axes are inferred from the AABB, so a ramp
/// drawn on a diagonal or a non-rectangular landing may mis-assign the axes;
/// this never substitutes for a measured field grade. Pure function.
pub fn mesh_cross_slope(mesh: &Mesh) -> Option<f64> {
    let a = mesh.aabb();
    // Cross axis = the SHORTER horizontal extent (unit vector in XY).
    let cross = if (a.max.x - a.min.x) >= (a.max.y - a.min.y) {
        DVec3::new(0.0, 1.0, 0.0)
    } else {
        DVec3::new(1.0, 0.0, 0.0)
    };
    let pos = mesh.positions();
    let mut worst: Option<f64> = None;
    for f in mesh.faces() {
        let (p0, p1, p2) = (pos[f[0] as usize], pos[f[1] as usize], pos[f[2] as usize]);
        let n = (p1 - p0).cross(p2 - p0);
        let len = n.length();
        if len < 1e-12 {
            continue;
        }
        let nn = n / len;
        if nn.z.abs() < 0.7 {
            continue; // not a walking (up-facing) surface
        }
        // Grade along the cross axis = |horizontal gradient · cross|. The
        // gradient of the plane z(x,y) is (-nx/nz, -ny/nz).
        let grad = DVec3::new(-nn.x / nn.z, -nn.y / nn.z, 0.0);
        let cross_grade = grad.dot(cross).abs();
        worst = Some(worst.map_or(cross_grade, |m: f64| m.max(cross_grade)));
    }
    worst
}

// ── Target matching ──────────────────────────────────────────────────────────

/// True when `obj` is one of the query kinds ("curve", "mesh", "block",
/// "door", "stair", "slab", "wall", "beam", "column"). Doors are block
/// instances whose block/source/object name contains "door"; stairs are any
/// mesh-backed object named like a stair.
fn matches_kind(obj: &SceneObject, kind: &str) -> bool {
    let name_has = |needle: &str| {
        obj.name.as_deref().is_some_and(|n| n.to_lowercase().contains(needle))
    };
    match kind {
        "curve" | "path" => matches!(obj.geometry, Geometry::Curve(_)),
        "mesh" => matches!(obj.geometry, Geometry::Mesh(_)),
        "block" => matches!(obj.geometry, Geometry::Instance { .. }),
        "door" => match &obj.geometry {
            Geometry::Instance { block, source, .. } => {
                block.to_lowercase().contains("door")
                    || source.as_deref().is_some_and(|s| s.to_lowercase().contains("door"))
                    || name_has("door")
            }
            _ => false,
        },
        "stair" => obj.geometry.mesh().is_some() && name_has("stair"),
        "slab" => matches!(obj.geometry, Geometry::Area { kind: AreaKind::Slab, .. }),
        "wall" => matches!(obj.geometry, Geometry::Area { kind: AreaKind::Wall, .. }),
        "beam" => matches!(obj.geometry, Geometry::Frame { kind: FrameKind::Beam, .. }),
        "column" => matches!(obj.geometry, Geometry::Frame { kind: FrameKind::Column, .. }),
        _ => false,
    }
}

fn matches_query(obj: &SceneObject, q: &TargetQuery, band: Option<(f64, f64)>) -> bool {
    if let Some(layer) = &q.layer
        && &obj.layer != layer
    {
        return false;
    }
    if let Some(needle) = &q.name_contains {
        let needle = needle.to_lowercase();
        if !obj.name.as_deref().is_some_and(|n| n.to_lowercase().contains(&needle)) {
            return false;
        }
    }
    if !q.kinds.is_empty() && !q.kinds.iter().any(|k| matches_kind(obj, k)) {
        return false;
    }
    if let Some((lo, hi)) = band {
        let aabb = obj.geometry.aabb();
        if aabb.max.z < lo - 1e-9 || aabb.min.z >= hi - 1e-9 {
            return false;
        }
    }
    true
}

/// Resolve a story name to its z band `[elevation, next-higher story)`.
fn story_band(stories: &[Story], name: &str) -> Result<(f64, f64), String> {
    let story = stories
        .iter()
        .find(|s| s.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            let known: Vec<&str> = stories.iter().map(|s| s.name.as_str()).collect();
            format!(
                "unknown story '{name}' (defined: {})",
                if known.is_empty() { "none".to_string() } else { known.join(", ") }
            )
        })?;
    let next = stories
        .iter()
        .map(|s| s.elevation)
        .filter(|e| *e > story.elevation + 1e-9)
        .fold(f64::INFINITY, f64::min);
    Ok((story.elevation, next))
}

// ── The engine ───────────────────────────────────────────────────────────────

/// How far apart the along-path probe samples are (meters).
const SAMPLE_STEP: f64 = 0.25;
/// Per-side cap for the clear-width probe: open space reads as 2×CAP.
const CLEAR_WIDTH_CAP: f64 = 5.0;

/// One violation marker: where, and how severe (drives the marker color).
pub type Marker = (DVec3, Severity);

/// Evaluate `rules` against the document. `story` optionally restricts targets
/// to one story's z band. `tris` are the scene's world-space mesh triangles
/// (obstruction set for the ray-cast probes). Pure with respect to the
/// document: returns the report + the failure-marker positions; the caller
/// (exec) draws markers and stores the report.
pub fn evaluate(
    doc: &Document,
    pack: &str,
    story: Option<&str>,
    rules: &[CheckRule],
    tris: &[[DVec3; 3]],
) -> Result<(ComplianceReport, Vec<Marker>), String> {
    let band = match story {
        Some(name) => Some(story_band(&doc.stories, name)?),
        None => None,
    };
    let bvh = TriBvh::build(tris.to_vec());
    let mut outcomes = Vec::with_capacity(rules.len());
    let mut markers: Vec<Marker> = Vec::new();

    for rule in rules {
        let targets: Vec<&SceneObject> = doc
            .objects()
            .filter(|o| matches_query(o, &rule.target, band))
            .collect();
        // Violations: (object short id, location, measured value).
        let mut violations: Vec<(String, DVec3, f64)> = Vec::new();
        // Worst measured value across ALL targets (also when passing).
        let mut measured: Option<f64> = None;
        // For max-limits worst = max, for min-limits worst = min.
        let worst_max = |v: f64, m: &mut Option<f64>| *m = Some(m.unwrap_or(v).max(v));
        let worst_min = |v: f64, m: &mut Option<f64>| *m = Some(m.unwrap_or(v).min(v));

        match &rule.check {
            CheckKind::MaxSlope { limit } => {
                for obj in &targets {
                    let Geometry::Curve(c) = &obj.geometry else { continue };
                    let (max, mids) = steep_segments(&c.tessellate(0.01), *limit);
                    worst_max(max, &mut measured);
                    for m in mids {
                        violations.push((obj.id.short(), m, max));
                    }
                }
            }
            CheckKind::MinDoorWidth { min } => {
                for obj in &targets {
                    let Geometry::Instance { block, scale, params, position, .. } =
                        &obj.geometry
                    else {
                        continue;
                    };
                    let extent = doc.blocks.get(block).map(|geos| {
                        let mut lo = DVec3::splat(f64::INFINITY);
                        let mut hi = DVec3::splat(f64::NEG_INFINITY);
                        for g in geos {
                            let a = g.aabb();
                            lo = lo.min(a.min);
                            hi = hi.max(a.max);
                        }
                        (hi.x - lo.x, hi.y - lo.y)
                    });
                    let Some(w) = door_width(params, extent, *scale) else { continue };
                    worst_min(w, &mut measured);
                    if w < *min {
                        violations.push((obj.id.short(), *position, w));
                    }
                }
            }
            CheckKind::MaxRiser { max } => {
                for obj in &targets {
                    let Some(mesh) = obj.geometry.mesh() else { continue };
                    let risers = stair_risers(mesh);
                    let Some(worst) =
                        risers.iter().copied().max_by(f64::total_cmp)
                    else {
                        continue;
                    };
                    worst_max(worst, &mut measured);
                    if worst > *max {
                        let a = mesh.aabb();
                        let c = (a.min + a.max) * 0.5;
                        violations.push((obj.id.short(), DVec3::new(c.x, c.y, a.max.z), worst));
                    }
                }
            }
            CheckKind::MinHeadroom { min } => {
                for obj in &targets {
                    let Geometry::Curve(c) = &obj.geometry else { continue };
                    let samples = resample_polyline(&c.tessellate(0.01), SAMPLE_STEP);
                    for (p, h) in samples.iter().zip(headroom(&samples, &bvh)) {
                        if h.is_finite() {
                            worst_min(h, &mut measured);
                        }
                        if h < *min {
                            violations.push((obj.id.short(), *p, h));
                        }
                    }
                }
            }
            CheckKind::MinClearWidth { min } => {
                for obj in &targets {
                    let Geometry::Curve(c) = &obj.geometry else { continue };
                    let samples = resample_polyline(&c.tessellate(0.01), SAMPLE_STEP);
                    for (p, w) in samples.iter().zip(clear_widths(&samples, &bvh, CLEAR_WIDTH_CAP))
                    {
                        if w < 2.0 * CLEAR_WIDTH_CAP - 1e-9 {
                            worst_min(w, &mut measured);
                        }
                        if w < *min {
                            violations.push((obj.id.short(), *p, w));
                        }
                    }
                }
            }
            CheckKind::TurningCircle { diameter } => {
                for obj in &targets {
                    let Geometry::Curve(c) = &obj.geometry else { continue };
                    let pts = c.tessellate(0.01);
                    let mut ends = Vec::new();
                    if let Some(first) = pts.first() {
                        ends.push(*first);
                    }
                    if pts.len() > 1 {
                        ends.push(*pts.last().unwrap());
                    }
                    for p in ends {
                        if !circle_fits(p, diameter / 2.0, tris) {
                            violations.push((obj.id.short(), p, *diameter));
                        }
                    }
                }
            }
            CheckKind::MinCountPerStory { min } => {
                let bottoms: Vec<f64> =
                    targets.iter().map(|o| o.geometry.aabb().min.z).collect();
                for (name, elev, count) in count_per_story(&doc.stories, &bottoms) {
                    if story.is_some_and(|s| !name.eq_ignore_ascii_case(s)) {
                        continue;
                    }
                    worst_min(count as f64, &mut measured);
                    if count < *min {
                        violations.push((name, DVec3::new(0.0, 0.0, elev), count as f64));
                    }
                }
            }
            CheckKind::GuardDrop { drop } => {
                for obj in &targets {
                    let a = obj.geometry.aabb();
                    if !a.max.z.is_finite() {
                        continue;
                    }
                    worst_max(a.max.z, &mut measured);
                    if a.max.z > *drop {
                        let c = (a.min + a.max) * 0.5;
                        violations.push((obj.id.short(), DVec3::new(c.x, c.y, a.max.z), a.max.z));
                    }
                }
            }
            CheckKind::OccupantLoad { factors } => {
                // Rooms, filtered by the target's story band + name_contains.
                for room in rooms_in_band(doc, &rule.target, band) {
                    let load = occupant_load(room, factors);
                    let c = room.centroid();
                    let at = DVec3::new(c[0], c[1], c[2]);
                    match load {
                        Some(n) => worst_max(n, &mut measured),
                        None => {
                            // No factor for this occupancy → flag it.
                            violations.push((room.name.clone(), at, f64::NAN));
                        }
                    }
                }
            }
            CheckKind::ExitCount { factors, thresholds, exit_name } => {
                let exits = exit_positions(doc, exit_name, band);
                for room in rooms_in_band(doc, &rule.target, band) {
                    let Some(load) = occupant_load(room, factors) else { continue };
                    let need = required_exits(load, thresholds);
                    // Count exits within a generous radius of the room (its
                    // longest boundary chord) — honest proximity heuristic.
                    let c = room.centroid();
                    let ctr = DVec3::new(c[0], c[1], c[2]);
                    let reach = room_reach(room);
                    let have = exits
                        .iter()
                        .filter(|e| (**e - ctr).truncate().length() <= reach)
                        .count();
                    worst_min(have as f64, &mut measured);
                    if have < need {
                        violations.push((room.name.clone(), ctr, have as f64));
                    }
                }
            }
            CheckKind::TravelDistance { max, exit_name } => {
                let exits = exit_positions(doc, exit_name, band);
                for room in rooms_in_band(doc, &rule.target, band) {
                    let c = room.centroid();
                    let ctr = DVec3::new(c[0], c[1], c[2]);
                    let nearest = exits
                        .iter()
                        .map(|e| (*e - ctr).length())
                        .min_by(f64::total_cmp);
                    match nearest {
                        Some(d) => {
                            worst_max(d, &mut measured);
                            if d > *max {
                                violations.push((room.name.clone(), ctr, d));
                            }
                        }
                        None => {
                            // No exits at all → the room can't reach one.
                            violations.push((room.name.clone(), ctr, f64::INFINITY));
                        }
                    }
                }
            }
            CheckKind::TreadDepth { min } => {
                for obj in &targets {
                    let Some(mesh) = obj.geometry.mesh() else { continue };
                    let Some(tread) = tread_depth(mesh) else { continue };
                    worst_min(tread, &mut measured);
                    if tread < *min {
                        let a = mesh.aabb();
                        let c = (a.min + a.max) * 0.5;
                        violations.push((obj.id.short(), DVec3::new(c.x, c.y, a.max.z), tread));
                    }
                }
            }
            CheckKind::RampLanding { flat_limit, landing_len, max_rise } => {
                for obj in &targets {
                    let Geometry::Curve(c) = &obj.geometry else { continue };
                    let samples = resample_polyline(&c.tessellate(0.01), SAMPLE_STEP);
                    for at in missing_landings(&samples, *flat_limit, *landing_len, *max_rise) {
                        violations.push((obj.id.short(), at, *max_rise));
                    }
                    // Worst measured = the total rise of the run (a coarse proxy
                    // when passing); use the curve's vertical extent.
                    if let (Some(lo), Some(hi)) = (
                        samples.iter().map(|p| p.z).min_by(f64::total_cmp),
                        samples.iter().map(|p| p.z).max_by(f64::total_cmp),
                    ) {
                        worst_max(hi - lo, &mut measured);
                    }
                }
            }
            CheckKind::ThresholdHeight { max } => {
                for obj in &targets {
                    let Geometry::Instance { params, position, .. } = &obj.geometry else {
                        continue;
                    };
                    match threshold_height(params) {
                        Some(h) => {
                            worst_max(h, &mut measured);
                            if h > *max {
                                violations.push((obj.id.short(), *position, h));
                            }
                        }
                        None => {
                            // Not modelably detectable: flag honestly (NaN
                            // measured) rather than pass or fake a height.
                            violations.push((obj.id.short(), *position, f64::NAN));
                        }
                    }
                }
            }
            CheckKind::MaxCrossSlope { limit } => {
                for obj in &targets {
                    let Some(mesh) = obj.geometry.mesh() else { continue };
                    let Some(cross) = mesh_cross_slope(mesh) else { continue };
                    worst_max(cross, &mut measured);
                    if cross > *limit {
                        let a = mesh.aabb();
                        let c = (a.min + a.max) * 0.5;
                        violations.push((obj.id.short(), DVec3::new(c.x, c.y, a.max.z), cross));
                    }
                }
            }
            CheckKind::CountRatio { thresholds, accessible_name } => {
                let needle = accessible_name.to_lowercase();
                let total = targets.len();
                let accessible = targets
                    .iter()
                    .filter(|o| {
                        o.name.as_deref().is_some_and(|n| n.to_lowercase().contains(&needle))
                    })
                    .count();
                let need = min_accessible(total as f64, thresholds);
                worst_min(accessible as f64, &mut measured);
                if total > 0 && accessible < need {
                    // Flag at the centroid of all matched objects.
                    let mut sum = DVec3::ZERO;
                    for o in &targets {
                        let a = o.geometry.aabb();
                        sum += (a.min + a.max) * 0.5;
                    }
                    let at = sum / total as f64;
                    violations.push((format!("{accessible}/{total} accessible"), at, accessible as f64));
                }
            }
        }

        let (required, unit) = rule.check.required();
        let verdict = if violations.is_empty() { "pass" } else { rule.severity.verdict() };
        for (_, at, _) in &violations {
            markers.push((*at, rule.severity));
        }
        // De-duplicate object ids, preserving order.
        let mut objects: Vec<String> = Vec::new();
        for (id, _, _) in &violations {
            if !objects.contains(id) {
                objects.push(id.clone());
            }
        }
        // Room-based checks are evaluated against tagged rooms, not scene
        // objects, so report the honest count for each.
        let checked = match &rule.check {
            CheckKind::OccupantLoad { .. }
            | CheckKind::ExitCount { .. }
            | CheckKind::TravelDistance { .. } => {
                rooms_in_band(doc, &rule.target, band).count()
            }
            _ => targets.len(),
        };
        outcomes.push(RuleOutcome {
            rule_id: rule.id.clone(),
            code_ref: rule.code_ref.clone(),
            severity: rule.severity.label().to_string(),
            verdict: verdict.to_string(),
            message: rule.message.clone(),
            objects,
            locations: violations.iter().map(|(_, p, _)| [p.x, p.y, p.z]).collect(),
            measured,
            required,
            unit: unit.to_string(),
            checked,
        });
    }

    let context = format!(
        "{} rule(s){} — {ADVISORY_NOTE}",
        rules.len(),
        story.map(|s| format!(", story '{s}'")).unwrap_or_default()
    );
    Ok((ComplianceReport { pack: pack.to_string(), context, rules: outcomes }, markers))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tri_box(min: DVec3, max: DVec3) -> Vec<[DVec3; 3]> {
        let m = kernel_mesh::make_box(min, max - min);
        let pos = m.positions();
        m.faces()
            .iter()
            .map(|f| [pos[f[0] as usize], pos[f[1] as usize], pos[f[2] as usize]])
            .collect()
    }

    // ── schema ───────────────────────────────────────────────────────────────

    #[test]
    fn demo_pack_parses_and_is_valid() {
        let p = demo_pack();
        assert_eq!(p.name, "demo");
        assert_eq!(p.rules.len(), 6);
        assert!(p.description.to_lowercase().contains("advisory"));
        // Round-trips.
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(CheckPack::from_json(&json).unwrap(), p);
    }

    #[test]
    fn malformed_packs_rejected() {
        // Bad JSON.
        assert!(CheckPack::from_json("{ nope").is_err());
        // Unknown check kind.
        let bad_kind = r#"{"name":"x","rules":[{"id":"a","severity":"error",
            "check":{"kind":"psychic_read","limit":1.0},"message":"m"}]}"#;
        assert!(CheckPack::from_json(bad_kind).is_err());
        // Unknown severity.
        let bad_sev = r#"{"name":"x","rules":[{"id":"a","severity":"catastrophic",
            "check":{"kind":"max_slope","limit":0.08},"message":"m"}]}"#;
        assert!(CheckPack::from_json(bad_sev).is_err());
        // Empty rules.
        assert!(CheckPack::from_json(r#"{"name":"x","rules":[]}"#).is_err());
        // Unsafe name (path traversal).
        let traversal = r#"{"name":"../evil","rules":[{"id":"a","severity":"info",
            "check":{"kind":"max_slope","limit":0.08},"message":"m"}]}"#;
        assert!(CheckPack::from_json(traversal).unwrap_err().contains("unsafe"));
        // Duplicate rule id.
        let dup = r#"{"name":"x","rules":[
            {"id":"a","severity":"info","check":{"kind":"max_slope","limit":0.08},"message":"m"},
            {"id":"a","severity":"info","check":{"kind":"max_slope","limit":0.08},"message":"m"}]}"#;
        assert!(CheckPack::from_json(dup).unwrap_err().contains("duplicate"));
        // Non-positive threshold.
        let neg = r#"{"name":"x","rules":[{"id":"a","severity":"info",
            "check":{"kind":"max_slope","limit":-1.0},"message":"m"}]}"#;
        assert!(CheckPack::from_json(neg).unwrap_err().contains("threshold"));
    }

    #[test]
    fn load_dir_reads_packs_and_reports_malformed() {
        let dir = std::env::temp_dir().join(format!("ijc-checks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let good = serde_json::to_string(&demo_pack()).unwrap()
            .replace("\"demo\"", "\"mine\""); // rename so it's distinct
        std::fs::write(dir.join("mine.checks.json"), good).unwrap();
        std::fs::write(dir.join("broken.checks.json"), "{ nope").unwrap();
        std::fs::write(dir.join("ignored.json"), "{}").unwrap();
        let (packs, warnings) = load_dir(&dir);
        assert!(packs.contains_key("mine"));
        assert_eq!(packs.len(), 1);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("broken.checks.json"));
        let _ = std::fs::remove_dir_all(&dir);
        // Absent dir: empty, no warnings.
        let (packs, warnings) = load_dir(Path::new("/no/such/dir/xyz"));
        assert!(packs.is_empty() && warnings.is_empty());
    }

    // ── probes ───────────────────────────────────────────────────────────────

    #[test]
    fn steep_segments_flags_only_over_limit() {
        // 12 m run rising 1 m = exactly 1:12 → passes a 1:12 limit (not >).
        let ok = [DVec3::ZERO, DVec3::new(12.0, 0.0, 1.0)];
        let (max, mids) = steep_segments(&ok, 1.0 / 12.0);
        assert!((max - 1.0 / 12.0).abs() < 1e-12);
        assert!(mids.is_empty());
        // 6 m run rising 1 m = 1:6 → flagged, midpoint at x=3.
        let steep = [DVec3::ZERO, DVec3::new(6.0, 0.0, 1.0)];
        let (max, mids) = steep_segments(&steep, 1.0 / 12.0);
        assert!((max - 1.0 / 6.0).abs() < 1e-12);
        assert_eq!(mids.len(), 1);
        assert!((mids[0].x - 3.0).abs() < 1e-12);
        // Matches the landscape sitepath advisory math exactly.
        let (land_max, land_n) = crate::landscape::path_slope_check(&steep, 1.0 / 12.0);
        assert_eq!((max, mids.len()), (land_max, land_n));
    }

    #[test]
    fn resample_polyline_densifies() {
        let pts = [DVec3::ZERO, DVec3::new(1.0, 0.0, 0.0)];
        let s = resample_polyline(&pts, 0.25);
        assert_eq!(s.len(), 5); // 0, .25, .5, .75, 1
        for w in s.windows(2) {
            assert!((w[1] - w[0]).length() <= 0.25 + 1e-12);
        }
        assert!(resample_polyline(&[], 0.25).is_empty());
    }

    #[test]
    fn stair_risers_extract_repeated_steps() {
        // Three stacked treads: tops at z = 0.15, 0.30, 0.45 → risers 0.15, 0.15.
        let mut mesh = kernel_mesh::make_box(DVec3::ZERO, DVec3::new(0.3, 1.0, 0.15));
        for i in 1..3 {
            let step = kernel_mesh::make_box(
                DVec3::new(0.3 * i as f64, 0.0, 0.0),
                DVec3::new(0.3, 1.0, 0.15 * (i + 1) as f64),
            );
            mesh.merge(&step);
        }
        let risers = stair_risers(&mesh);
        assert_eq!(risers.len(), 2, "risers: {risers:?}");
        for r in &risers {
            assert!((r - 0.15).abs() < 1e-9, "riser {r}");
        }
    }

    #[test]
    fn headroom_measures_ceiling() {
        // Ceiling slab from z=2.1 over the sample point.
        let tris = tri_box(DVec3::new(-1.0, -1.0, 2.1), DVec3::new(1.0, 1.0, 2.3));
        let bvh = TriBvh::build(tris);
        let h = headroom(&[DVec3::ZERO], &bvh);
        assert!((h[0] - 2.1).abs() < 1e-9, "headroom {h:?}");
        // Nothing overhead → infinite.
        let h = headroom(&[DVec3::new(50.0, 50.0, 0.0)], &bvh);
        assert!(h[0].is_infinite());
    }

    #[test]
    fn clear_widths_between_walls() {
        // Corridor along X between walls at y=-0.5 and y=+0.5 → 1.0 m clear.
        let mut tris = tri_box(DVec3::new(0.0, 0.5, 0.0), DVec3::new(10.0, 0.7, 3.0));
        tris.extend(tri_box(DVec3::new(0.0, -0.7, 0.0), DVec3::new(10.0, -0.5, 3.0)));
        let bvh = TriBvh::build(tris);
        let samples = [DVec3::new(5.0, 0.0, 0.0), DVec3::new(6.0, 0.0, 0.0)];
        let w = clear_widths(&samples, &bvh, 5.0);
        for wi in &w {
            assert!((wi - 1.0).abs() < 1e-9, "width {w:?}");
        }
        // Open field: both sides capped → 2*cap.
        let open = clear_widths(&[DVec3::new(5.0, 30.0, 0.0), DVec3::new(6.0, 30.0, 0.0)], &bvh, 5.0);
        assert!((open[0] - 10.0).abs() < 1e-9);
    }

    #[test]
    fn circle_fits_respects_obstacles_and_floor() {
        // A column 0.5 m from the center blocks a 60" (1.524 m ∅) circle.
        let column = tri_box(DVec3::new(0.5, -0.1, 0.0), DVec3::new(0.7, 0.1, 3.0));
        assert!(!circle_fits(DVec3::ZERO, 0.762, &column));
        // Far column doesn't.
        let far = tri_box(DVec3::new(5.0, -0.1, 0.0), DVec3::new(5.2, 0.1, 3.0));
        assert!(circle_fits(DVec3::ZERO, 0.762, &far));
        // The floor underfoot (top at z=0) never blocks.
        let floor = tri_box(DVec3::new(-5.0, -5.0, -0.2), DVec3::new(5.0, 5.0, 0.0));
        assert!(circle_fits(DVec3::ZERO, 0.762, &floor));
    }

    #[test]
    fn door_width_prefers_param_then_extent() {
        let mut params = BTreeMap::new();
        params.insert("width".to_string(), "0.9".to_string());
        assert_eq!(door_width(&params, Some((0.3, 0.2)), 1.0), Some(0.9));
        assert_eq!(door_width(&params, None, 2.0), Some(1.8));
        // No param: larger plan extent × scale.
        let none = BTreeMap::new();
        assert_eq!(door_width(&none, Some((0.7, 0.05)), 1.0), Some(0.7));
        assert_eq!(door_width(&none, None, 1.0), None);
        // Garbage param falls through to the extent.
        params.insert("width".to_string(), "wide".to_string());
        assert_eq!(door_width(&params, Some((0.7, 0.05)), 1.0), Some(0.7));
    }

    #[test]
    fn count_per_story_bands() {
        let stories = vec![
            Story { name: "L1".into(), elevation: 0.0, height: 0.0 },
            Story { name: "L2".into(), elevation: 3.0, height: 0.0 },
        ];
        // Bottoms: two on L1, one on L2.
        let counts = count_per_story(&stories, &[0.0, 1.0, 3.5]);
        assert_eq!(counts, vec![("L1".to_string(), 0.0, 2), ("L2".to_string(), 3.0, 1)]);
        // No stories: one document-wide band.
        let counts = count_per_story(&[], &[0.0, 9.0]);
        assert_eq!(counts, vec![("(document)".to_string(), 0.0, 2)]);
    }

    // ── IBC (M-ibc) probes ───────────────────────────────────────────────────

    fn room(occ: &str, area: f64, boundary: Vec<[f64; 3]>) -> Room {
        Room { name: format!("{occ}-1"), occupancy: occ.to_string(), area, boundary }
    }

    /// A tiny named mesh object whose AABB center sits at `at` — used as an
    /// "exit" marker in the egress tests.
    fn named_marker(name: &str, at: DVec3) -> SceneObject {
        use itsjustcad_doc::ObjectId;
        let mesh = kernel_mesh::make_box(at - DVec3::splat(0.05), DVec3::splat(0.1));
        SceneObject {
            visible: true,
            id: ObjectId::new(),
            name: Some(name.to_string()),
            layer: itsjustcad_doc::DEFAULT_LAYER.to_string(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Mesh(mesh),
        }
    }

    #[test]
    fn ibc_pack_parses_and_is_valid() {
        let p = ibc_pack();
        assert_eq!(p.name, "ibc2021");
        assert!(p.rules.len() >= 11, "rules: {}", p.rules.len());
        assert!(p.description.to_lowercase().contains("advisory"));
        // Every rule carries a real IBC section ref and the disclaimer.
        for r in &p.rules {
            assert!(r.code_ref.starts_with("IBC"), "rule '{}' code_ref '{}'", r.id, r.code_ref);
            assert!(
                r.message.contains("advisory pre-check"),
                "rule '{}' message missing disclaimer",
                r.id
            );
        }
        // Round-trips.
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(CheckPack::from_json(&json).unwrap(), p);
    }

    #[test]
    fn occupant_load_100m2_business_is_8() {
        // IBC Table 1004.5: business gross factor 13.94 m2/occ. 100 / 13.94 =
        // 7.17 → ceil → 8 occupants.
        let mut factors = BTreeMap::new();
        factors.insert("business".to_string(), 13.94);
        let r = room("business", 100.0, vec![]);
        assert_eq!(occupant_load(&r, &factors), Some(8.0));
        // Unknown occupancy → None.
        let other = room("assembly", 100.0, vec![]);
        assert_eq!(occupant_load(&other, &factors), None);
    }

    #[test]
    fn required_exits_thresholds() {
        // IBC 1006.3.2 bands: <=49 -> 1, 50-500 -> 2, 501-1000 -> 3, >1000 -> 4.
        let t = [[49.0, 1.0], [500.0, 2.0], [1000.0, 3.0], [1e9, 4.0]];
        assert_eq!(required_exits(1.0, &t), 1);
        assert_eq!(required_exits(49.0, &t), 1);
        assert_eq!(required_exits(50.0, &t), 2);
        assert_eq!(required_exits(500.0, &t), 2);
        assert_eq!(required_exits(501.0, &t), 3);
        assert_eq!(required_exits(1000.0, &t), 3);
        assert_eq!(required_exits(1001.0, &t), 4);
        assert_eq!(required_exits(50000.0, &t), 4);
    }

    #[test]
    fn tread_depth_from_synthetic_stair() {
        // Three treads 0.30 m deep (X) each, tops at z=0.15,0.30,0.45 (risers
        // 0.15). Plan run along the long X axis = 0.9 m (Y width only 0.5 m);
        // riser count = 2 → tread = 0.45 m. (The boxy estimate divides the run
        // by the riser count, an honest approximation.)
        let mut mesh = kernel_mesh::make_box(DVec3::ZERO, DVec3::new(0.3, 0.5, 0.15));
        for i in 1..3 {
            let step = kernel_mesh::make_box(
                DVec3::new(0.3 * i as f64, 0.0, 0.0),
                DVec3::new(0.3, 0.5, 0.15 * (i + 1) as f64),
            );
            mesh.merge(&step);
        }
        let t = tread_depth(&mesh).expect("stair tread");
        assert!((t - 0.45).abs() < 1e-9, "tread {t}");
        // A plain box (no risers) is not a stair.
        let plain = kernel_mesh::make_box(DVec3::ZERO, DVec3::new(1.0, 1.0, 1.0));
        assert_eq!(tread_depth(&plain), None);
    }

    // ── ADA (M-ada) probes ───────────────────────────────────────────────────

    #[test]
    fn missing_landings_flags_long_runs_without_a_landing() {
        // A ramp at 1:12 rising 0.4 m (well under the 0.762 m limit), no flag.
        let ok = resample_polyline(
            &[DVec3::ZERO, DVec3::new(4.8, 0.0, 0.4)],
            0.25,
        );
        assert!(missing_landings(&ok, 1.0 / 48.0, 1.524, 0.762).is_empty());

        // A ramp at 1:12 rising 1.0 m in one continuous run (> 0.762 m) with no
        // landing → flagged once.
        let steep = resample_polyline(
            &[DVec3::ZERO, DVec3::new(12.0, 0.0, 1.0)],
            0.25,
        );
        let flags = missing_landings(&steep, 1.0 / 48.0, 1.524, 0.762);
        assert_eq!(flags.len(), 1, "one missing landing: {flags:?}");

        // Same 1.0 m rise but split by a 2 m flat landing at mid-height → OK.
        // run 1: 0 → 0.5 m over 6 m; landing: 2 m flat; run 2: 0.5 → 1.0 m.
        let with_landing = resample_polyline(
            &[
                DVec3::new(0.0, 0.0, 0.0),
                DVec3::new(6.0, 0.0, 0.5),
                DVec3::new(8.0, 0.0, 0.5), // 2 m flat landing
                DVec3::new(14.0, 0.0, 1.0),
            ],
            0.25,
        );
        assert!(
            missing_landings(&with_landing, 1.0 / 48.0, 1.524, 0.762).is_empty(),
            "landing should clear the run: {:?}",
            missing_landings(&with_landing, 1.0 / 48.0, 1.524, 0.762)
        );
    }

    #[test]
    fn threshold_height_reads_param_or_none() {
        let mut params = BTreeMap::new();
        assert_eq!(threshold_height(&params), None); // not detectable
        params.insert("threshold".to_string(), "0.012".to_string());
        assert_eq!(threshold_height(&params), Some(0.012));
        // Negative / garbage rejected.
        params.insert("threshold".to_string(), "-1".to_string());
        assert_eq!(threshold_height(&params), None);
        params.insert("threshold".to_string(), "tall".to_string());
        assert_eq!(threshold_height(&params), None);
    }

    #[test]
    fn mesh_cross_slope_measures_lateral_tilt() {
        // A flat ramp surface (a level plate) has ~zero cross slope.
        let flat = kernel_mesh::make_box(DVec3::ZERO, DVec3::new(4.0, 1.0, 0.05));
        let c = mesh_cross_slope(&flat).expect("has an up-face");
        assert!(c < 1e-6, "flat cross slope {c}");

        // Build a plate tilted about the X axis (its LONGER run axis), so the
        // tilt is a CROSS slope. Two triangles, y from 0..1 rising 0.1 in z.
        let mesh = Mesh::new(
            vec![
                DVec3::new(0.0, 0.0, 0.0),
                DVec3::new(4.0, 0.0, 0.0),
                DVec3::new(4.0, 1.0, 0.1),
                DVec3::new(0.0, 1.0, 0.1),
            ],
            vec![[0, 1, 2], [0, 2, 3]],
        );
        let c = mesh_cross_slope(&mesh).expect("has an up-face");
        // Rise 0.1 over run 1.0 across the short (Y) axis = 1:10.
        assert!((c - 0.1).abs() < 1e-6, "cross slope {c}");
    }

    #[test]
    fn ada_pack_parses_and_is_valid() {
        let p = ada_pack();
        assert_eq!(p.name, "ada2010");
        assert!(p.rules.len() >= 11, "rules: {}", p.rules.len());
        assert!(p.description.to_lowercase().contains("advisory"));
        for r in &p.rules {
            assert!(r.code_ref.starts_with("ADA"), "rule '{}' code_ref '{}'", r.id, r.code_ref);
            assert!(
                r.message.contains("advisory pre-check"),
                "rule '{}' message missing disclaimer",
                r.id
            );
        }
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(CheckPack::from_json(&json).unwrap(), p);
    }

    #[test]
    fn min_accessible_thresholds() {
        // ADA Table 208.2 lower bands.
        let t = [[25.0, 1.0], [50.0, 2.0], [75.0, 3.0], [100.0, 4.0]];
        assert_eq!(min_accessible(1.0, &t), 1);
        assert_eq!(min_accessible(25.0, &t), 1);
        assert_eq!(min_accessible(26.0, &t), 2);
        assert_eq!(min_accessible(50.0, &t), 2);
        assert_eq!(min_accessible(76.0, &t), 4);
        assert_eq!(min_accessible(500.0, &t), 4); // beyond last band → last
    }

    #[test]
    fn travel_distance_and_exit_count_on_known_layout() {
        // Business room, 200 m2 (load ceil(200/13.94)=15 -> needs 1 exit under
        // 49). One exit 3 m from the room centroid, well within 76.2 m.
        let square =
            || vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [10.0, 20.0, 0.0], [0.0, 20.0, 0.0]];
        let mut doc = Document::default();
        doc.rooms.push(room("business", 200.0, square()));
        // Exit object named "exit-1" near the room centroid (5,10,0).
        doc.insert(named_marker("exit-1", DVec3::new(5.0, 8.0, 0.0)));

        let pack = ibc_pack();
        let (report, _) = evaluate(&doc, "ibc2021", None, &pack.rules, &[]).unwrap();
        let exitc = report.rules.iter().find(|r| r.rule_id == "exit-count").unwrap();
        assert_eq!(exitc.verdict, "pass", "exit-count: {exitc:?}");
        let travel = report.rules.iter().find(|r| r.rule_id == "travel-distance").unwrap();
        assert_eq!(travel.verdict, "pass", "travel: {travel:?}");
        let occ = report.rules.iter().find(|r| r.rule_id == "occupant-load").unwrap();
        assert!((occ.measured.unwrap() - 15.0).abs() < 1e-9, "occ load {occ:?}");

        // Move the exit 200 m away → travel distance fails (> 76.2 m).
        let mut far = Document::default();
        far.rooms.push(room("business", 200.0, square()));
        far.insert(named_marker("exit-far", DVec3::new(5.0, 210.0, 0.0)));
        let (report, _) = evaluate(&far, "ibc2021", None, &pack.rules, &[]).unwrap();
        let travel = report.rules.iter().find(|r| r.rule_id == "travel-distance").unwrap();
        assert_eq!(travel.verdict, "warn", "far travel: {travel:?}");
    }

    // ── engine (evaluate) unknown-story error ────────────────────────────────

    #[test]
    fn evaluate_unknown_story_errors() {
        let doc = Document::default();
        let err = evaluate(&doc, "demo", Some("penthouse"), &demo_pack().rules, &[])
            .unwrap_err();
        assert!(err.contains("unknown story"), "{err}");
    }
}
