// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Landscape-design math (M-landscape): pure, deterministic functions behind
//! the `contours`, `pad`, `cutfill`, `plant*`, `flowarrows`, `ponding` and
//! `sitepath` verbs. No document access here — exec.rs owns the scene; this
//! module owns the geometry so it can be tested against analytic terrains.

use glam::DVec3;

// ─── contours ────────────────────────────────────────────────────────────────

/// One extracted contour polyline at `level` (contour index `index` =
/// `level / interval`, used for the minor/major split).
#[derive(Debug, Clone)]
pub struct ContourLine {
    pub points: Vec<DVec3>,
    pub closed: bool,
    pub level: f64,
    pub index: i64,
}

/// Contour levels strictly inside `(zmin, zmax)`: every multiple of
/// `interval`. Levels that would land exactly on a vertex elevation are
/// handled per-triangle by the crossing test (strict sign change), so a level
/// equal to `zmin`/`zmax` simply yields nothing.
pub fn contour_levels(zmin: f64, zmax: f64, interval: f64) -> Vec<(i64, f64)> {
    if interval <= 0.0 || !interval.is_finite() || !zmin.is_finite() || !zmax.is_finite() || zmax <= zmin
    {
        return Vec::new();
    }
    let first = (zmin / interval).floor() as i64;
    let last = (zmax / interval).ceil() as i64;
    (first..=last)
        .map(|k| (k, k as f64 * interval))
        .filter(|(_, z)| *z > zmin && *z < zmax)
        .collect()
}

/// Marching triangles: intersection segments of the mesh with the horizontal
/// plane `z = level`. Vertices sitting exactly on the level are nudged down by
/// an infinitesimal (treated as below), which keeps the crossing test a strict
/// sign change and the chained loops watertight on gridded synthetic terrain.
fn contour_segments(
    positions: &[DVec3],
    faces: &[[u32; 3]],
    level: f64,
) -> Vec<(DVec3, DVec3)> {
    // "Above" is strict; a vertex exactly at the level counts as below.
    let above = |p: DVec3| p.z > level;
    let mut segs = Vec::new();
    for f in faces {
        let p = [
            positions[f[0] as usize],
            positions[f[1] as usize],
            positions[f[2] as usize],
        ];
        let mut hits: Vec<DVec3> = Vec::with_capacity(2);
        for e in 0..3 {
            let (a, b) = (p[e], p[(e + 1) % 3]);
            if above(a) != above(b) {
                // Guaranteed a.z != b.z when the side test differs, except the
                // degenerate a.z == b.z == level case which `above` maps to
                // (false, false) — never reaches here.
                let t = (level - a.z) / (b.z - a.z);
                hits.push(a + (b - a) * t);
            }
        }
        if hits.len() == 2 && hits[0].distance_squared(hits[1]) > 1e-18 {
            segs.push((hits[0], hits[1]));
        }
    }
    segs
}

/// Quantized XY key for endpoint matching while chaining (1 µm grid — far
/// below any drafting tolerance, far above f64 noise).
fn qkey(p: DVec3) -> (i64, i64) {
    ((p.x * 1e6).round() as i64, (p.y * 1e6).round() as i64)
}

/// Chain unordered segments into polylines. Deterministic: seeds walk in
/// segment order, adjacency uses ordered maps. Returns `(points, closed)`.
pub fn chain_segments(segs: &[(DVec3, DVec3)]) -> Vec<(Vec<DVec3>, bool)> {
    use std::collections::BTreeMap;
    // endpoint key → list of (segment index, which end is at this key)
    let mut at: BTreeMap<(i64, i64), Vec<(usize, bool)>> = BTreeMap::new();
    for (i, (a, b)) in segs.iter().enumerate() {
        at.entry(qkey(*a)).or_default().push((i, false));
        at.entry(qkey(*b)).or_default().push((i, true));
    }
    let mut used = vec![false; segs.len()];
    let mut out = Vec::new();
    for seed in 0..segs.len() {
        if used[seed] {
            continue;
        }
        used[seed] = true;
        let mut pts = vec![segs[seed].0, segs[seed].1];
        // Extend forward from the tail, then backward from the head.
        for forward in [true, false] {
            loop {
                let end = if forward { *pts.last().unwrap() } else { pts[0] };
                let Some(cands) = at.get(&qkey(end)) else { break };
                let next = cands.iter().find(|(i, _)| !used[*i]).copied();
                let Some((i, end_is_b)) = next else { break };
                used[i] = true;
                // The far endpoint of the found segment.
                let far = if end_is_b { segs[i].0 } else { segs[i].1 };
                if forward {
                    pts.push(far);
                } else {
                    pts.insert(0, far);
                }
            }
        }
        let closed = pts.len() > 2 && qkey(pts[0]) == qkey(*pts.last().unwrap());
        if closed {
            pts.pop(); // drop the duplicate closing point; `closed` flag carries it
        }
        out.push((pts, closed));
    }
    out
}

/// All contour polylines of a terrain mesh at multiples of `interval`.
pub fn contours(positions: &[DVec3], faces: &[[u32; 3]], interval: f64) -> Vec<ContourLine> {
    let (zmin, zmax) = positions.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| {
        (lo.min(p.z), hi.max(p.z))
    });
    let mut out = Vec::new();
    for (index, level) in contour_levels(zmin, zmax, interval) {
        let segs = contour_segments(positions, faces, level);
        for (points, closed) in chain_segments(&segs) {
            if points.len() >= 2 {
                out.push(ContourLine { points, closed, level, index });
            }
        }
    }
    out
}

// ─── grading & earthwork ─────────────────────────────────────────────────────

/// Grade a flat rectangular building pad into terrain vertex heights, with
/// side slopes to daylight. The pad is axis-aligned, centered at `(cx, cy)`,
/// `w`×`d`, at elevation `elev`. `slope_h` is the side-slope ratio expressed
/// as horizontal-run per unit rise (e.g. 2.0 = a 2:1 slope). Outside the pad
/// the graded surface may rise (cut) or fall (fill) by `dist / slope_h`
/// relative to `elev`; wherever the existing grade already sits inside that
/// envelope it is left untouched — that is the daylight line.
///
/// Returns `(pad_vertices, slope_vertices)` — how many vertices were set to
/// the pad elevation and how many were pulled onto a side slope.
pub fn grade_pad(
    positions: &mut [DVec3],
    cx: f64,
    cy: f64,
    w: f64,
    d: f64,
    elev: f64,
    slope_h: f64,
) -> (usize, usize) {
    let (mut on_pad, mut on_slope) = (0usize, 0usize);
    for p in positions.iter_mut() {
        let dx = ((p.x - cx).abs() - w / 2.0).max(0.0);
        let dy = ((p.y - cy).abs() - d / 2.0).max(0.0);
        let dist = dx.hypot(dy);
        if dist == 0.0 {
            if (p.z - elev).abs() > 1e-12 {
                on_pad += 1;
            }
            p.z = elev;
        } else {
            // Envelope the graded surface may occupy at this distance.
            let rise = dist / slope_h;
            let clamped = p.z.clamp(elev - rise, elev + rise);
            if (clamped - p.z).abs() > 1e-12 {
                p.z = clamped;
                on_slope += 1;
            }
        }
    }
    (on_pad, on_slope)
}

/// Cut and fill volumes (m³) between the current terrain and its pre-grading
/// vertex heights, over the same triangulation (grading only moves z).
///
/// Per triangle the signed prism volume is `area_xy · mean(Δz)` — exact for a
/// linearly interpolated surface when Δz does not change sign inside the
/// triangle; mixed-sign triangles are attributed by the sign of their mean
/// (a TIN prism estimate, second-order small on a reasonably dense mesh).
/// Returns `(cut, fill)`, both ≥ 0.
pub fn cut_fill(
    positions: &[DVec3],
    faces: &[[u32; 3]],
    original_z: &[f64],
) -> (f64, f64) {
    let (mut cut, mut fill) = (0.0f64, 0.0f64);
    for f in faces {
        let [a, b, c] = [f[0] as usize, f[1] as usize, f[2] as usize];
        let (pa, pb, pc) = (positions[a], positions[b], positions[c]);
        let area_xy = 0.5
            * ((pb.x - pa.x) * (pc.y - pa.y) - (pc.x - pa.x) * (pb.y - pa.y)).abs();
        let dz = (pa.z - original_z[a]) + (pb.z - original_z[b]) + (pc.z - original_z[c]);
        let vol = area_xy * dz / 3.0;
        if vol > 0.0 {
            fill += vol;
        } else {
            cut -= vol;
        }
    }
    (cut, fill)
}

// ─── planting ────────────────────────────────────────────────────────────────

/// One species in the embedded plant catalog (`assets/plants.json`). Real
/// nursery-guide figures: mature height, canopy spread, typical growth rate.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PlantSpecies {
    pub id: String,
    pub common: String,
    pub binomial: String,
    pub mature_height_m: f64,
    pub canopy_diameter_m: f64,
    pub growth_m_per_year: f64,
    pub deciduous: bool,
    /// Canopy silhouette: "round" (deciduous shade tree), "cone" (conifer /
    /// pyramidal evergreen), "column" (fastigiate), "palm" (bare trunk column
    /// with a small crown fan atop — for low tropical-sun shadow studies).
    pub form: String,
    /// Köppen climate codes the species tolerates (e.g. "Af", "Am", "Aw",
    /// "Cfa", "Cwa", "Cfb", "Dfb"). Empty in untagged legacy catalogs.
    #[serde(default)]
    pub climate_zones: Vec<String>,
    /// Native / naturalized region tags ("caribbean", "valle-del-cauca",
    /// "guayaquil", "europe", "temperate"…). Empty in legacy catalogs.
    #[serde(default)]
    pub native_regions: Vec<String>,
    /// Miyawaki stratification layer: "canopy", "tree", "subtree" or "shrub".
    /// `None` when the species is not classified for mini-forest use.
    #[serde(default)]
    pub layer: Option<String>,
    /// Plan (top-view) drafting symbol style, independent of the 3D `form`:
    /// "round" (circle + radiating branches), "conifer" (spiky star),
    /// "palm" (radiating frond spokes), "shrub" (small stipple circle) or
    /// "clump" (cluster of dots — bamboo / guadua). Absent in legacy catalogs;
    /// [`PlantSpecies::plan_symbol_style`] then derives it from `form`.
    #[serde(default)]
    pub plan_symbol: Option<String>,
}

/// The 2D plan-drawing symbol styles a species can render as, top-down. This is
/// the standard landscape-drafting glyph vocabulary — deliberately distinct
/// from the 3D `form` silhouette so a fastigiate column can still read as a
/// shrub stipple in plan, or a clumping bamboo as a dot cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanSymbol {
    /// Deciduous shade tree: circle with radiating branch lines.
    Round,
    /// Conifer / pyramidal evergreen: spiky star (scalloped points).
    Conifer,
    /// Palm: radiating frond spokes from the centre (the iconic palm glyph).
    Palm,
    /// Shrub / small column: a small stippled circle (dotted ring).
    Shrub,
    /// Clumping bamboo (guadua): a cluster of small dots.
    Clump,
}

impl PlanSymbol {
    /// Parse an explicit `plan_symbol` catalog string; unknown → `None`.
    pub fn parse(s: &str) -> Option<PlanSymbol> {
        match s {
            "round" => Some(PlanSymbol::Round),
            "conifer" => Some(PlanSymbol::Conifer),
            "palm" => Some(PlanSymbol::Palm),
            "shrub" => Some(PlanSymbol::Shrub),
            "clump" => Some(PlanSymbol::Clump),
            _ => None,
        }
    }

    /// Default symbol for a 3D `form` when a species has no explicit
    /// `plan_symbol`. Guadua bamboo is the notable case: it is stored as a
    /// "column" form (a tall pole) but drafts as a "clump" — so catalog authors
    /// set `plan_symbol` explicitly; the generic "column" falls back to a shrub
    /// stipple.
    pub fn from_form(form: &str) -> PlanSymbol {
        match form {
            "cone" => PlanSymbol::Conifer,
            "palm" => PlanSymbol::Palm,
            "column" => PlanSymbol::Shrub,
            _ => PlanSymbol::Round,
        }
    }
}

impl PlantSpecies {
    /// The plan-drawing symbol for this species: the explicit `plan_symbol`
    /// field when present and valid, otherwise derived from `form`.
    pub fn plan_symbol_style(&self) -> PlanSymbol {
        self.plan_symbol
            .as_deref()
            .and_then(PlanSymbol::parse)
            .unwrap_or_else(|| PlanSymbol::from_form(&self.form))
    }
}

/// Rough Köppen climate band derived from the absolute latitude of the doc's
/// georeference. Coarse — a stand-in for a proper climate raster, enough to
/// advise on species suitability and to gate the Miyawaki generator.
///
/// | \|lat\|       | band        | representative zones |
/// |---------------|-------------|----------------------|
/// | < 10°         | equatorial  | Af, Am, Aw           |
/// | 10°–23.5°     | tropical    | Am, Aw               |
/// | 23.5°–35°     | subtropical | Cfa, Cwa             |
/// | > 35°         | temperate   | Cfb, Dfb             |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClimateBand {
    Equatorial,
    Tropical,
    Subtropical,
    Temperate,
}

impl ClimateBand {
    /// Derive the band from a latitude in degrees (sign ignored).
    pub fn from_latitude(lat_deg: f64) -> ClimateBand {
        let a = lat_deg.abs();
        if a < 10.0 {
            ClimateBand::Equatorial
        } else if a < 23.5 {
            ClimateBand::Tropical
        } else if a < 35.0 {
            ClimateBand::Subtropical
        } else {
            ClimateBand::Temperate
        }
    }

    /// Köppen codes representative of this band, for the mismatch advisory and
    /// `plantcatalog zone`. Overlapping on purpose (e.g. Am spans equatorial &
    /// tropical) so a species tagged for either matches.
    pub fn zones(self) -> &'static [&'static str] {
        match self {
            ClimateBand::Equatorial => &["Af", "Am", "Aw"],
            ClimateBand::Tropical => &["Am", "Aw"],
            ClimateBand::Subtropical => &["Cfa", "Cwa", "Csa"],
            ClimateBand::Temperate => &["Cfb", "Dfb", "Dfc"],
        }
    }

    /// Human label used in advisories.
    pub fn label(self) -> &'static str {
        match self {
            ClimateBand::Equatorial => "equatorial tropical",
            ClimateBand::Tropical => "tropical/monsoon",
            ClimateBand::Subtropical => "subtropical",
            ClimateBand::Temperate => "temperate",
        }
    }

    /// Does this species tolerate the band? True when any of its climate_zones
    /// is one of the band's representative codes.
    pub fn suits(self, sp: &PlantSpecies) -> bool {
        let zones = self.zones();
        sp.climate_zones.iter().any(|z| zones.contains(&z.as_str()))
    }
}

/// The embedded plant catalog, parsed once. The JSON is a compile-time asset,
/// so `plant` stays a pure command (no filesystem read at run time).
pub fn plant_catalog() -> &'static [PlantSpecies] {
    static CATALOG: std::sync::OnceLock<Vec<PlantSpecies>> = std::sync::OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("../assets/plants.json"))
            .expect("embedded plants.json must parse")
    })
}

/// Find a species by exact id, then by case-insensitive substring of the id,
/// common or botanical name ("oak" → Quercus robur). First match in catalog
/// order wins, so lookups are deterministic.
pub fn find_species(query: &str) -> Option<&'static PlantSpecies> {
    let q = query.to_lowercase();
    let cat = plant_catalog();
    cat.iter().find(|s| s.id == q).or_else(|| {
        cat.iter().find(|s| {
            s.id.contains(&q)
                || s.common.to_lowercase().contains(&q)
                || s.binomial.to_lowercase().contains(&q)
        })
    })
}

/// Species in the catalog filtered by an optional region tag and/or an
/// optional climate band. Both filters are ANDed when present; `None`/`None`
/// returns the whole catalog. Deterministic (catalog order). Query only.
pub fn catalog_filtered(
    region: Option<&str>,
    band: Option<ClimateBand>,
) -> Vec<&'static PlantSpecies> {
    let r = region.map(str::to_lowercase);
    plant_catalog()
        .iter()
        .filter(|s| r.as_deref().is_none_or(|r| s.native_regions.iter().any(|t| t == r)))
        .filter(|s| band.is_none_or(|b| b.suits(s)))
        .collect()
}

/// Native, Miyawaki-classified species available for a climate band: those
/// whose `climate_zones` suit the band (or whose `native_regions` names the
/// band's region) AND that carry a stratification `layer`. This is the pool
/// the Miyawaki generator draws from. Deterministic.
pub fn miyawaki_pool(band: ClimateBand) -> Vec<&'static PlantSpecies> {
    plant_catalog()
        .iter()
        .filter(|s| s.layer.is_some() && band.suits(s))
        .collect()
}

/// Height and canopy diameter at `age_years` (None = mature): a linear
/// height-growth model capped at maturity, canopy scaled proportionally.
/// Floored at 5% so a newly planted whip still shows up.
pub fn plant_size(sp: &PlantSpecies, age_years: Option<f64>) -> (f64, f64) {
    let f = match age_years {
        None => 1.0,
        Some(a) => (a * sp.growth_m_per_year / sp.mature_height_m).clamp(0.05, 1.0),
    };
    (f * sp.mature_height_m, f * sp.canopy_diameter_m)
}

/// Append a UV ellipsoid (canopy ball) to a mesh under construction.
fn push_ellipsoid(
    pos: &mut Vec<DVec3>,
    faces: &mut Vec<[u32; 3]>,
    center: DVec3,
    rxy: f64,
    rz: f64,
) {
    const SEG: usize = 8; // longitudes
    const STACK: usize = 6; // latitudes
    let base = pos.len() as u32;
    pos.push(center + DVec3::new(0.0, 0.0, rz)); // north pole
    for j in 1..STACK {
        let phi = std::f64::consts::PI * j as f64 / STACK as f64;
        for i in 0..SEG {
            let th = std::f64::consts::TAU * i as f64 / SEG as f64;
            pos.push(center + DVec3::new(
                rxy * phi.sin() * th.cos(),
                rxy * phi.sin() * th.sin(),
                rz * phi.cos(),
            ));
        }
    }
    pos.push(center - DVec3::new(0.0, 0.0, rz)); // south pole
    let ring = |j: usize, i: usize| base + 1 + ((j - 1) * SEG + i % SEG) as u32;
    for i in 0..SEG {
        faces.push([base, ring(1, i + 1), ring(1, i)]);
    }
    for j in 1..STACK - 1 {
        for i in 0..SEG {
            let (a, b, c, d) = (ring(j, i), ring(j, i + 1), ring(j + 1, i + 1), ring(j + 1, i));
            faces.push([a, b, c]);
            faces.push([a, c, d]);
        }
    }
    let south = base + 1 + ((STACK - 1) * SEG) as u32;
    for i in 0..SEG {
        faces.push([south, ring(STACK - 1, i), ring(STACK - 1, i + 1)]);
    }
}

/// Append a closed cone (conifer canopy) to a mesh under construction.
fn push_cone(
    pos: &mut Vec<DVec3>,
    faces: &mut Vec<[u32; 3]>,
    base_center: DVec3,
    radius: f64,
    height: f64,
) {
    const SEG: usize = 8;
    let b = pos.len() as u32;
    pos.push(base_center + DVec3::new(0.0, 0.0, height)); // apex
    pos.push(base_center); // base center
    for i in 0..SEG {
        let th = std::f64::consts::TAU * i as f64 / SEG as f64;
        pos.push(base_center + DVec3::new(radius * th.cos(), radius * th.sin(), 0.0));
    }
    for i in 0..SEG as u32 {
        let (p, q) = (b + 2 + i, b + 2 + (i + 1) % SEG as u32);
        faces.push([b, p, q]); // side
        faces.push([b + 1, q, p]); // base
    }
}

/// Append a hexagonal prism (trunk) to a mesh under construction.
fn push_prism(
    pos: &mut Vec<DVec3>,
    faces: &mut Vec<[u32; 3]>,
    base_center: DVec3,
    radius: f64,
    height: f64,
) {
    const SEG: usize = 6;
    let b = pos.len() as u32;
    for ring in 0..2 {
        let z = height * ring as f64;
        for i in 0..SEG {
            let th = std::f64::consts::TAU * i as f64 / SEG as f64;
            pos.push(base_center + DVec3::new(radius * th.cos(), radius * th.sin(), z));
        }
    }
    let s = SEG as u32;
    for i in 0..s {
        let j = (i + 1) % s;
        faces.push([b + i, b + j, b + s + j]);
        faces.push([b + i, b + s + j, b + s + i]);
    }
    for i in 1..s - 1 {
        faces.push([b, b + i + 1, b + i]); // bottom cap
        faces.push([b + s, b + s + i, b + s + i + 1]); // top cap
    }
}

/// Append a palm crown atop a bare trunk: a shallow disc of the canopy radius
/// with a small hemispherical cap — a crown-on-trunk silhouette, NOT a full
/// ellipsoid. This is what makes a palm's shadow correct at low tropical sun
/// angles (a thin trunk casting a long thin shadow, the crown a small blob on
/// top) rather than the fat ovoid a shade tree throws.
fn push_palm_crown(
    pos: &mut Vec<DVec3>,
    faces: &mut Vec<[u32; 3]>,
    center: DVec3,
    radius: f64,
    cap: f64,
) {
    const SEG: usize = 8;
    let b = pos.len() as u32;
    pos.push(center + DVec3::new(0.0, 0.0, cap)); // crown apex
    pos.push(center); // hub
    for i in 0..SEG {
        let th = std::f64::consts::TAU * i as f64 / SEG as f64;
        pos.push(center + DVec3::new(radius * th.cos(), radius * th.sin(), 0.0));
    }
    for i in 0..SEG as u32 {
        let (p, q) = (b + 2 + i, b + 2 + (i + 1) % SEG as u32);
        faces.push([b, p, q]); // upper cone shell (frond fan)
        faces.push([b + 1, q, p]); // underside
    }
}

/// Trunk + canopy mesh for a species at `base` (ground point), scaled by age.
/// Deterministic; participates in shadow/sun analyses like any scene mesh.
pub fn plant_mesh(
    sp: &PlantSpecies,
    base: DVec3,
    age_years: Option<f64>,
) -> (Vec<DVec3>, Vec<[u32; 3]>) {
    let (h, canopy_d) = plant_size(sp, age_years);
    let trunk_frac = match sp.form.as_str() {
        "cone" => 0.15,
        "column" => 0.10,
        // A palm is nearly all bare trunk — the crown is a thin fan on top.
        "palm" => 0.85,
        _ => 0.35,
    };
    let trunk_h = trunk_frac * h;
    // Palms have slender trunks relative to height; other forms scale as before.
    let trunk_r = if sp.form == "palm" {
        (0.012 * h).clamp(0.05, 0.35)
    } else {
        (0.02 * h).clamp(0.05, 0.5)
    };
    let mut pos = Vec::new();
    let mut faces = Vec::new();
    push_prism(&mut pos, &mut faces, base, trunk_r, trunk_h);
    let canopy_base = base + DVec3::new(0.0, 0.0, trunk_h);
    match sp.form.as_str() {
        "cone" => push_cone(&mut pos, &mut faces, canopy_base, canopy_d / 2.0, h - trunk_h),
        "palm" => push_palm_crown(&mut pos, &mut faces, canopy_base, canopy_d / 2.0, h - trunk_h),
        _ => {
            let rz = (h - trunk_h) / 2.0;
            push_ellipsoid(
                &mut pos,
                &mut faces,
                canopy_base + DVec3::new(0.0, 0.0, rz),
                canopy_d / 2.0,
                rz,
            );
        }
    }
    (pos, faces)
}

/// 2D plan (top-view) drafting symbol for a plant, as a soup of line segments
/// lying flat in the horizontal plane at `center` (`center.z` is honored so the
/// glyph drapes on the ground). `canopy_d` is the mature/aged canopy diameter;
/// the symbol scales to it. Pure and deterministic — the same species + size
/// always yields the same segments, so it renders identically in the live top
/// view, the sketch/pencil NPR path, and the PDF/SVG plan exports.
///
/// Segment counts by style (radius `r = canopy_d/2`):
///   * Round   — a `CIRCLE_SEG`-gon canopy ring + `ROUND_BRANCHES` radial
///     branch stubs from a small hub (reads as a shade tree).
///   * Conifer — a spiky star: `CONIFER_POINTS` outer points alternating with
///     inner notches (`2·CONIFER_POINTS` ring segments).
///   * Palm    — `PALM_FRONDS` frond spokes radiating from the centre, no ring
///     (the iconic palm plan glyph).
///   * Shrub   — a small stippled circle: `STIPPLE_DOTS` short dashes around the
///     ring (a dotted outline).
///   * Clump   — a cluster of `CLUMP_DOTS` small dot-crosses scattered on a
///     deterministic ring inside the canopy (bamboo / guadua).
///
/// The color is applied by the caller (the plant's layer color); this function
/// only owns geometry.
pub fn plan_symbol_segments(
    style: PlanSymbol,
    center: DVec3,
    canopy_d: f64,
) -> Vec<(DVec3, DVec3)> {
    use std::f64::consts::TAU;
    let r = (canopy_d * 0.5).max(0.0);
    if r < 1e-9 {
        return Vec::new();
    }
    let z = center.z;
    let at = |ang: f64, rad: f64| {
        DVec3::new(center.x + rad * ang.cos(), center.y + rad * ang.sin(), z)
    };
    // Ring polygon of `n` sides at radius `rad`.
    let ring = |n: usize, rad: f64| -> Vec<(DVec3, DVec3)> {
        (0..n)
            .map(|i| {
                let a0 = TAU * i as f64 / n as f64;
                let a1 = TAU * (i + 1) as f64 / n as f64;
                (at(a0, rad), at(a1, rad))
            })
            .collect()
    };
    let mut segs = Vec::new();
    match style {
        PlanSymbol::Round => {
            segs.extend(ring(CIRCLE_SEG, r));
            // Radial branch stubs from a small central hub outward to ~85% of
            // the canopy — a stylized branching structure.
            let hub = r * 0.12;
            for i in 0..ROUND_BRANCHES {
                let a = TAU * i as f64 / ROUND_BRANCHES as f64;
                segs.push((at(a, hub), at(a, r * 0.85)));
            }
        }
        PlanSymbol::Conifer => {
            // Star: outer points at r, inner notches at 0.62·r.
            let n = CONIFER_POINTS;
            let inner = r * 0.62;
            let mut prev = at(0.0, r);
            for i in 1..=2 * n {
                let a = TAU * i as f64 / (2 * n) as f64;
                let rad = if i % 2 == 0 { r } else { inner };
                let p = at(a, rad);
                segs.push((prev, p));
                prev = p;
            }
        }
        PlanSymbol::Palm => {
            // Frond spokes radiating from the centre; no enclosing ring.
            for i in 0..PALM_FRONDS {
                let a = TAU * i as f64 / PALM_FRONDS as f64;
                segs.push((DVec3::new(center.x, center.y, z), at(a, r)));
            }
        }
        PlanSymbol::Shrub => {
            // Dotted / stippled ring: short dashes every other segment.
            let n = STIPPLE_DOTS;
            for i in 0..n {
                if i % 2 == 1 {
                    continue; // gap → stipple
                }
                let a0 = TAU * i as f64 / n as f64;
                let a1 = TAU * (i as f64 + 0.55) / n as f64;
                segs.push((at(a0, r), at(a1, r)));
            }
        }
        PlanSymbol::Clump => {
            // Cluster of small dot-crosses on a deterministic inner ring.
            let dot = (r * 0.14).max(0.02);
            for i in 0..CLUMP_DOTS {
                // Two interleaved radii so the cluster looks scattered, not a
                // perfect circle, while staying fully deterministic.
                let a = TAU * i as f64 / CLUMP_DOTS as f64;
                let rad = if i % 2 == 0 { r * 0.55 } else { r * 0.8 };
                let c = at(a, rad);
                segs.push((c - DVec3::new(dot, 0.0, 0.0), c + DVec3::new(dot, 0.0, 0.0)));
                segs.push((c - DVec3::new(0.0, dot, 0.0), c + DVec3::new(0.0, dot, 0.0)));
            }
        }
    }
    segs
}

/// Plan symbol for a planted mesh object, recovered purely from its object
/// `name` (`"plant:<species-id>"`) and mesh vertex positions. Returns `None`
/// for any object that is not a recognized plant. This is the single bridge the
/// live viewport, PDF and SVG plan exporters all call, so the symbol is
/// identical everywhere.
///
/// The center is the XY centroid of the vertices at the trunk-base elevation
/// (the mesh's minimum z, i.e. ground); the canopy diameter is twice the
/// maximum horizontal distance from that centroid to any vertex — recovering
/// the aged/scaled canopy actually planted, not the catalog mature figure.
pub fn plant_object_symbol(name: &str, positions: &[DVec3]) -> Option<Vec<(DVec3, DVec3)>> {
    let id = name.strip_prefix("plant:")?;
    let sp = find_species(id)?;
    if positions.is_empty() {
        return None;
    }
    let n = positions.len() as f64;
    let cx = positions.iter().map(|p| p.x).sum::<f64>() / n;
    let cy = positions.iter().map(|p| p.y).sum::<f64>() / n;
    let zmin = positions.iter().map(|p| p.z).fold(f64::INFINITY, f64::min);
    let canopy_d = 2.0
        * positions
            .iter()
            .map(|p| ((p.x - cx).powi(2) + (p.y - cy).powi(2)).sqrt())
            .fold(0.0f64, f64::max);
    let center = DVec3::new(cx, cy, zmin);
    Some(plan_symbol_segments(sp.plan_symbol_style(), center, canopy_d))
}

/// Canopy ring resolution for the round plan symbol.
const CIRCLE_SEG: usize = 24;
/// Radial branch stubs on the round symbol.
const ROUND_BRANCHES: usize = 8;
/// Outer points on the conifer star.
const CONIFER_POINTS: usize = 8;
/// Frond spokes on the palm symbol.
const PALM_FRONDS: usize = 8;
/// Dash slots around the shrub stipple ring (half are gaps).
const STIPPLE_DOTS: usize = 16;
/// Dot-crosses in the clump (bamboo) cluster.
const CLUMP_DOTS: usize = 7;

/// Planting positions for a row from `a` to `b` at `spacing`: every multiple
/// of `spacing` along the segment starting at `a` (b included only when the
/// length is an exact multiple).
pub fn row_positions(a: DVec3, b: DVec3, spacing: f64) -> Vec<DVec3> {
    if spacing <= 0.0 || !spacing.is_finite() {
        return Vec::new();
    }
    let len = (b - a).length();
    if len < 1e-12 {
        return vec![a];
    }
    let dir = (b - a) / len;
    let n = (len / spacing + 1e-9).floor() as usize + 1;
    (0..n).map(|i| a + dir * (i as f64 * spacing)).collect()
}

/// Terrain elevation at `(x, y)` by barycentric interpolation over the first
/// XY-containing triangle; `None` when outside the terrain footprint.
pub fn terrain_z_at(positions: &[DVec3], faces: &[[u32; 3]], x: f64, y: f64) -> Option<f64> {
    for f in faces {
        let (a, b, c) = (
            positions[f[0] as usize],
            positions[f[1] as usize],
            positions[f[2] as usize],
        );
        let det = (b.x - a.x) * (c.y - a.y) - (c.x - a.x) * (b.y - a.y);
        if det.abs() < 1e-18 {
            continue;
        }
        let u = ((x - a.x) * (c.y - a.y) - (c.x - a.x) * (y - a.y)) / det;
        let v = ((b.x - a.x) * (y - a.y) - (x - a.x) * (b.y - a.y)) / det;
        let w = 1.0 - u - v;
        let eps = -1e-9;
        if u >= eps && v >= eps && w >= eps {
            return Some(w * a.z + u * b.z + v * c.z);
        }
    }
    None
}

// ─── drainage visualization ──────────────────────────────────────────────────

/// Steepest-descent data for one terrain face.
#[derive(Debug, Clone, Copy)]
pub struct FaceFlow {
    pub centroid: DVec3,
    /// Unit vector down the face plane (has a negative z on any real slope).
    pub downhill: DVec3,
    /// Projected (plan) area of the face — used to pick the biggest faces.
    pub area_xy: f64,
    /// Slope as rise-over-run (tan of the slope angle).
    pub slope: f64,
}

/// Per-face steepest-descent flow: the gradient direction of each non-
/// horizontal, non-vertical face. Order follows the face list (deterministic).
pub fn face_flows(positions: &[DVec3], faces: &[[u32; 3]]) -> Vec<FaceFlow> {
    let mut out = Vec::new();
    for f in faces {
        let (a, b, c) = (
            positions[f[0] as usize],
            positions[f[1] as usize],
            positions[f[2] as usize],
        );
        let mut n = (b - a).cross(c - a);
        if n.z < 0.0 {
            n = -n;
        }
        let horiz2 = n.x * n.x + n.y * n.y;
        // Vertical face (no plan area) or flat face (no descent) → no arrow.
        if n.z <= 1e-12 || horiz2 < 1e-18 * n.z * n.z || horiz2 == 0.0 {
            continue;
        }
        // Steepest descent within the plane: d ⟂ n, d·(n.xy) < 0 in plan.
        let d = DVec3::new(n.x * n.z, n.y * n.z, -horiz2).normalize();
        out.push(FaceFlow {
            centroid: (a + b + c) / 3.0,
            downhill: d,
            area_xy: 0.5 * ((b.x - a.x) * (c.y - a.y) - (c.x - a.x) * (b.y - a.y)).abs(),
            slope: horiz2.sqrt() / n.z,
        });
    }
    out
}

/// Arrow glyph polyline (open, 5 points): shaft from the centroid down the
/// slope, then two barbs. Lies in the face plane; the caller lifts it.
pub fn arrow_points(centroid: DVec3, downhill: DVec3, len: f64) -> Vec<DVec3> {
    let tip = centroid + downhill * len;
    let back = tip - downhill * (0.3 * len);
    // Lateral: perpendicular to the arrow in plan.
    let plan = (downhill.x * downhill.x + downhill.y * downhill.y).sqrt().max(1e-12);
    let lat = DVec3::new(-downhill.y / plan, downhill.x / plan, 0.0) * (0.15 * len);
    vec![centroid, tip, back + lat, tip, back - lat]
}

/// Local-minima vertices (sinks): interior vertices strictly lower than every
/// edge-connected neighbor. Boundary vertices (on an edge used by only one
/// face) are excluded — water leaves the mesh there. Returned in vertex-index
/// order (deterministic).
pub fn find_sinks(positions: &[DVec3], faces: &[[u32; 3]]) -> Vec<usize> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut neighbors: Vec<BTreeSet<u32>> = vec![BTreeSet::new(); positions.len()];
    let mut edge_uses: BTreeMap<(u32, u32), u32> = BTreeMap::new();
    for f in faces {
        for e in 0..3 {
            let (u, v) = (f[e], f[(e + 1) % 3]);
            neighbors[u as usize].insert(v);
            neighbors[v as usize].insert(u);
            let key = (u.min(v), u.max(v));
            *edge_uses.entry(key).or_insert(0) += 1;
        }
    }
    let mut boundary = vec![false; positions.len()];
    for ((u, v), uses) in &edge_uses {
        if *uses == 1 {
            boundary[*u as usize] = true;
            boundary[*v as usize] = true;
        }
    }
    (0..positions.len())
        .filter(|&i| {
            !boundary[i]
                && !neighbors[i].is_empty()
                && neighbors[i]
                    .iter()
                    .all(|&j| positions[i].z < positions[j as usize].z)
        })
        .collect()
}

// ─── hardscape (site paths) ──────────────────────────────────────────────────

/// Resample a polyline at (close to) `step` spacing: `ceil(len/step)` equal
/// arclength intervals including both endpoints (closed curves get the seam
/// point at both ends). Deterministic.
pub fn sample_polyline(points: &[DVec3], closed: bool, step: f64) -> Vec<DVec3> {
    if points.len() < 2 || step <= 0.0 || !step.is_finite() {
        return points.to_vec();
    }
    let mut pts: Vec<DVec3> = points.to_vec();
    if closed {
        pts.push(points[0]);
    }
    let seg_lens: Vec<f64> = pts.windows(2).map(|w| (w[1] - w[0]).length()).collect();
    let total: f64 = seg_lens.iter().sum();
    if total < 1e-12 {
        return vec![pts[0]];
    }
    let n = (total / step).ceil().max(1.0) as usize;
    let mut out = Vec::with_capacity(n + 1);
    for i in 0..=n {
        let mut target = total * i as f64 / n as f64;
        let mut p = *pts.last().unwrap();
        for (k, len) in seg_lens.iter().enumerate() {
            if target <= *len || k == seg_lens.len() - 1 {
                let t = if *len > 1e-12 { (target / len).min(1.0) } else { 0.0 };
                p = pts[k] + (pts[k + 1] - pts[k]) * t;
                break;
            }
            target -= len;
        }
        out.push(p);
    }
    out
}

/// Constant-width ribbon along a sampled centerline: ±width/2 offsets
/// perpendicular (in plan) to the local direction, stitched into a triangle
/// strip. Returns `(positions, faces)`; positions alternate left/right.
pub fn ribbon(samples: &[DVec3], width: f64) -> (Vec<DVec3>, Vec<[u32; 3]>) {
    let n = samples.len();
    if n < 2 || width <= 0.0 {
        return (Vec::new(), Vec::new());
    }
    let mut pos = Vec::with_capacity(2 * n);
    let mut last_perp = DVec3::new(0.0, 1.0, 0.0);
    for i in 0..n {
        // Central difference in plan; falls back to the previous direction on
        // a degenerate (vertical / duplicate) step.
        let d = samples[(i + 1).min(n - 1)] - samples[i.saturating_sub(1)];
        let plan = (d.x * d.x + d.y * d.y).sqrt();
        let perp = if plan > 1e-12 {
            DVec3::new(-d.y / plan, d.x / plan, 0.0)
        } else {
            last_perp
        };
        last_perp = perp;
        pos.push(samples[i] + perp * (width / 2.0));
        pos.push(samples[i] - perp * (width / 2.0));
    }
    let mut faces = Vec::with_capacity(2 * (n - 1));
    for i in 0..(n as u32 - 1) {
        let (l0, r0, l1, r1) = (2 * i, 2 * i + 1, 2 * i + 2, 2 * i + 3);
        faces.push([l0, r0, r1]);
        faces.push([l0, r1, l1]);
    }
    (pos, faces)
}

/// Accessible-slope check along a sampled path: `(max_slope, steep_segments)`
/// where slope is rise-over-run between consecutive samples and segments
/// steeper than `limit` (e.g. 1/12) are counted. Advisory only.
pub fn path_slope_check(samples: &[DVec3], limit: f64) -> (f64, usize) {
    let mut max_slope = 0.0f64;
    let mut steep = 0usize;
    for w in samples.windows(2) {
        let run = (w[1] - w[0]).truncate().length();
        let rise = (w[1].z - w[0].z).abs();
        if run < 1e-12 {
            continue;
        }
        let s = rise / run;
        max_slope = max_slope.max(s);
        if s > limit {
            steep += 1;
        }
    }
    (max_slope, steep)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regular grid over [0,size]² with `n`×`n` cells, z = f(x,y).
    fn grid_terrain(
        n: usize,
        size: f64,
        f: impl Fn(f64, f64) -> f64,
    ) -> (Vec<DVec3>, Vec<[u32; 3]>) {
        let mut pos = Vec::new();
        for j in 0..=n {
            for i in 0..=n {
                let x = size * i as f64 / n as f64;
                let y = size * j as f64 / n as f64;
                pos.push(DVec3::new(x, y, f(x, y)));
            }
        }
        let mut faces = Vec::new();
        let idx = |i: usize, j: usize| (j * (n + 1) + i) as u32;
        for j in 0..n {
            for i in 0..n {
                faces.push([idx(i, j), idx(i + 1, j), idx(i + 1, j + 1)]);
                faces.push([idx(i, j), idx(i + 1, j + 1), idx(i, j + 1)]);
            }
        }
        (pos, faces)
    }

    #[test]
    fn levels_inside_range_only() {
        let lv = contour_levels(0.3, 3.2, 1.0);
        assert_eq!(lv, vec![(1, 1.0), (2, 2.0), (3, 3.0)]);
        assert!(contour_levels(0.0, 1.0, 0.0).is_empty());
        assert!(contour_levels(2.0, 1.0, 0.5).is_empty());
    }

    #[test]
    fn plane_contours_are_straight_open_lines() {
        // z = x on [0,10]²: contours are vertical lines x = level.
        let (pos, faces) = grid_terrain(10, 10.0, |x, _| x);
        let lines = contours(&pos, &faces, 2.5);
        // Levels 2.5, 5.0, 7.5 (0 and 10 are the exact min/max → excluded).
        let mut levels: Vec<f64> = lines.iter().map(|c| c.level).collect();
        levels.sort_by(f64::total_cmp);
        levels.dedup();
        assert_eq!(levels, vec![2.5, 5.0, 7.5]);
        for c in &lines {
            assert!(!c.closed, "plane contours must be open");
            // Every point sits on x = level and z = level.
            for p in &c.points {
                assert!((p.x - c.level).abs() < 1e-9, "x={} level={}", p.x, c.level);
                assert!((p.z - c.level).abs() < 1e-9);
            }
            // Chained into ONE polyline spanning the full y range.
            let (ymin, ymax) = c
                .points
                .iter()
                .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| {
                    (lo.min(p.y), hi.max(p.y))
                });
            assert!((ymin - 0.0).abs() < 1e-9 && (ymax - 10.0).abs() < 1e-9);
        }
        // One polyline per level — chaining must not fragment a straight cut.
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn cone_contours_are_closed_rings_at_predicted_radii() {
        // Cone peak z=5 at center (5,5), slope 1 → contour at level z is a
        // circle of radius 5−z. Grid is fine enough that rings stay closed.
        let (pos, faces) = grid_terrain(40, 10.0, |x, y| {
            (5.0 - ((x - 5.0).powi(2) + (y - 5.0).powi(2)).sqrt()).max(0.0)
        });
        let lines = contours(&pos, &faces, 1.0);
        for want in [1.0, 2.0, 3.0, 4.0] {
            let rings: Vec<_> = lines.iter().filter(|c| c.level == want).collect();
            assert_eq!(rings.len(), 1, "level {want} must chain to one ring");
            let ring = rings[0];
            assert!(ring.closed, "cone contour at {want} must be a closed loop");
            let r_want = 5.0 - want;
            for p in &ring.points {
                let r = ((p.x - 5.0).powi(2) + (p.y - 5.0).powi(2)).sqrt();
                // Linear interpolation on a curved surface: tolerance scales
                // with cell size (0.25 m cells).
                assert!(
                    (r - r_want).abs() < 0.05,
                    "level {want}: radius {r} vs {r_want}"
                );
            }
        }
    }

    #[test]
    fn vertex_exactly_on_level_does_not_break_chaining() {
        // z = x on integer grid: level 2.0 passes exactly through a whole
        // column of vertices. Strict `above` still yields one clean open line.
        let (pos, faces) = grid_terrain(4, 4.0, |x, _| x);
        let lines = contours(&pos, &faces, 2.0);
        assert_eq!(lines.len(), 1);
        assert!((lines[0].level - 2.0).abs() < 1e-12);
        assert!(!lines[0].closed);
        for p in &lines[0].points {
            assert!((p.x - 2.0).abs() < 1e-9);
        }
    }

    #[test]
    fn pad_at_grade_on_flat_plane_changes_nothing() {
        // Flat plane at z=2, pad at elev 2 → no vertex moves, zero cut/fill.
        let (mut pos, faces) = grid_terrain(20, 20.0, |_, _| 2.0);
        let orig: Vec<f64> = pos.iter().map(|p| p.z).collect();
        let (on_pad, on_slope) = grade_pad(&mut pos, 10.0, 10.0, 4.0, 4.0, 2.0, 2.0);
        assert_eq!((on_pad, on_slope), (0, 0));
        let (cut, fill) = cut_fill(&pos, &faces, &orig);
        assert_eq!((cut, fill), (0.0, 0.0));
    }

    #[test]
    fn sunk_pad_in_flat_plane_matches_analytic_cut() {
        // Flat plane z=0, 4×4 pad sunk to −1 with 2:1 slopes. Analytic cut:
        //   pad:      4·4·1                       = 16
        //   edges:    perimeter 16 · ∫₀²(1−t/2)dt = 16·1 = 16
        //   corners:  4·¼-annulus = 2π·∫₀²(r−r²/2)dr = 2π·(2−4/3) = 4π/3
        // total ≈ 36.19 m³, fill = 0. 0.25 m grid cells → small TIN error.
        let (mut pos, faces) = grid_terrain(80, 20.0, |_, _| 0.0);
        let orig: Vec<f64> = pos.iter().map(|p| p.z).collect();
        let (on_pad, on_slope) = grade_pad(&mut pos, 10.0, 10.0, 4.0, 4.0, -1.0, 2.0);
        assert!(on_pad > 0 && on_slope > 0);
        let (cut, fill) = cut_fill(&pos, &faces, &orig);
        let want = 16.0 + 16.0 + 4.0 * std::f64::consts::PI / 3.0;
        assert!((cut - want).abs() < 0.5, "cut {cut} vs analytic {want}");
        assert_eq!(fill, 0.0);
    }

    #[test]
    fn raised_pad_is_pure_fill_and_symmetric() {
        // Same pad raised +1 on the same flat plane → the mirrored volume as
        // fill, zero cut.
        let (mut pos, faces) = grid_terrain(80, 20.0, |_, _| 0.0);
        let orig: Vec<f64> = pos.iter().map(|p| p.z).collect();
        grade_pad(&mut pos, 10.0, 10.0, 4.0, 4.0, 1.0, 2.0);
        let (cut, fill) = cut_fill(&pos, &faces, &orig);
        let want = 32.0 + 4.0 * std::f64::consts::PI / 3.0;
        assert!((fill - want).abs() < 0.5, "fill {fill} vs analytic {want}");
        assert_eq!(cut, 0.0);
    }

    #[test]
    fn pad_in_sloped_plane_daylights_and_balances() {
        // Plane z = x/2 on [0,20]², pad 4×4 at (10,10) elev 5 — exactly the
        // existing grade at pad center x=10. Pad cut (x>10 side) mirrors pad
        // fill (x<10 side): ∫ over 4×4 of (x/2−5) splits into ±2·... cut in
        // pad = fill in pad = 4·∫₁₀¹²(x/2−5)dx = 4·1 = 4 m³ each, plus equal
        // slope-band volumes by symmetry → cut ≈ fill overall.
        let (mut pos, faces) = grid_terrain(80, 20.0, |x, _| x / 2.0);
        let orig: Vec<f64> = pos.iter().map(|p| p.z).collect();
        grade_pad(&mut pos, 10.0, 10.0, 4.0, 4.0, 5.0, 2.0);
        // Far corner untouched: envelope daylights before reaching it.
        assert!((pos[0].z - 0.0).abs() < 1e-12, "corner (0,0) must be untouched");
        let (cut, fill) = cut_fill(&pos, &faces, &orig);
        assert!(cut > 4.0 && fill > 4.0, "pad body alone is 4 m³ each way");
        assert!(
            (cut - fill).abs() < 0.2,
            "symmetric grading must balance: cut {cut} vs fill {fill}"
        );
    }

    #[test]
    fn plant_catalog_parses_all_real_species() {
        let cat = plant_catalog();
        // 12 legacy temperate + 9 Caribbean + 6 Valle del Cauca + 6 Guayaquil.
        assert_eq!(cat.len(), 33);
        for sp in cat {
            assert!(sp.mature_height_m > 0.0 && sp.canopy_diameter_m > 0.0);
            assert!(sp.growth_m_per_year > 0.0);
            assert!(
                matches!(sp.form.as_str(), "round" | "cone" | "column" | "palm"),
                "{}",
                sp.id
            );
            assert!(sp.binomial.contains(' '), "binomial has genus + species");
            // Every tagged species carries climate zones and a Miyawaki layer.
            assert!(!sp.climate_zones.is_empty(), "{} untagged climate", sp.id);
            assert!(!sp.native_regions.is_empty(), "{} untagged region", sp.id);
            assert!(sp.layer.is_some(), "{} missing layer", sp.id);
        }
        // Conifers in the catalog are evergreen.
        assert!(!find_species("picea-abies").unwrap().deciduous);
        assert!(find_species("quercus-robur").unwrap().deciduous);
    }

    #[test]
    fn legacy_untagged_catalog_still_loads_via_serde_default() {
        // A pre-M-plants-tropical catalog entry with no climate/region/layer.
        let legacy = r#"[{"id":"old-oak","common":"oak","binomial":"Quercus x",
            "mature_height_m":20.0,"canopy_diameter_m":15.0,
            "growth_m_per_year":0.5,"deciduous":true,"form":"round"}]"#;
        let parsed: Vec<PlantSpecies> = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].climate_zones.is_empty());
        assert!(parsed[0].native_regions.is_empty());
        assert!(parsed[0].layer.is_none());
    }

    #[test]
    fn koppen_band_from_latitude() {
        assert_eq!(ClimateBand::from_latitude(-2.2), ClimateBand::Equatorial); // Guayaquil
        assert_eq!(ClimateBand::from_latitude(3.4), ClimateBand::Equatorial); // Cali
        assert_eq!(ClimateBand::from_latitude(18.5), ClimateBand::Tropical); // Santo Domingo
        assert_eq!(ClimateBand::from_latitude(25.76), ClimateBand::Subtropical); // Miami
        assert_eq!(ClimateBand::from_latitude(42.36), ClimateBand::Temperate); // Boston
        // Sign-agnostic.
        assert_eq!(ClimateBand::from_latitude(-42.0), ClimateBand::Temperate);
    }

    #[test]
    fn climate_suitability_matches_zones() {
        let oak = find_species("quercus-robur").unwrap(); // Cfb/Dfb
        let palm = find_species("roystonea-regia").unwrap(); // Af/Am/Aw
        assert!(ClimateBand::Temperate.suits(oak));
        assert!(!ClimateBand::Equatorial.suits(oak));
        assert!(ClimateBand::Equatorial.suits(palm));
        assert!(!ClimateBand::Temperate.suits(palm));
    }

    #[test]
    fn palm_shape_is_bare_trunk_with_small_crown_not_ellipsoid() {
        let palm = find_species("roystonea-regia").unwrap(); // 25 m, palm
        let oak = find_species("quercus-robur").unwrap(); // 30 m, round
        let base = DVec3::new(0.0, 0.0, 0.0);
        let (ppos, _) = plant_mesh(palm, base, None);
        // The palm's crown sits high on a bare trunk: the lowest canopy vertex
        // (any vertex above the trunk radius footprint) is near the top.
        let zmax = ppos.iter().map(|p| p.z).fold(f64::NEG_INFINITY, f64::max);
        assert!((zmax - 25.0).abs() < 0.5, "palm crown near mature height");
        // Trunk fraction 0.85 → the widest points (crown, r=3) appear only high
        // up; below 0.5·h the only geometry is the slender trunk (r<0.4).
        let low_r = ppos
            .iter()
            .filter(|p| p.z < 12.0)
            .map(|p| (p.x * p.x + p.y * p.y).sqrt())
            .fold(0.0f64, f64::max);
        assert!(low_r < 0.5, "palm trunk is slender low down, got {low_r}");
        // A round tree of the same height carries canopy width well below 12 m.
        let (opos, _) = plant_mesh(oak, base, None);
        let oak_low_r = opos
            .iter()
            .filter(|p| p.z < 12.0)
            .map(|p| (p.x * p.x + p.y * p.y).sqrt())
            .fold(0.0f64, f64::max);
        assert!(oak_low_r > 5.0, "shade tree is fat low down, got {oak_low_r}");
    }

    #[test]
    fn find_species_by_id_common_and_binomial_substring() {
        assert_eq!(find_species("quercus-robur").unwrap().id, "quercus-robur");
        assert_eq!(find_species("oak").unwrap().id, "quercus-robur");
        assert_eq!(find_species("Betula").unwrap().id, "betula-pendula");
        assert_eq!(find_species("SPRUCE").unwrap().id, "picea-abies");
        assert!(find_species("triffid").is_none());
    }

    #[test]
    fn canopy_scales_with_age_and_caps_at_maturity() {
        let oak = find_species("quercus-robur").unwrap(); // 30 m at 0.5 m/yr
        let (h_mature, d_mature) = plant_size(oak, None);
        assert_eq!((h_mature, d_mature), (30.0, 25.0));
        let (h10, d10) = plant_size(oak, Some(10.0)); // 10yr·0.5 = 5 m → f=1/6
        assert!((h10 - 5.0).abs() < 1e-9);
        assert!((d10 - 25.0 / 6.0).abs() < 1e-9);
        // Past maturity the cap holds; a seedling gets the 5% floor.
        assert_eq!(plant_size(oak, Some(500.0)), (30.0, 25.0));
        let (h0, _) = plant_size(oak, Some(0.0));
        assert!((h0 - 1.5).abs() < 1e-9, "5% floor");
    }

    #[test]
    fn plant_mesh_spans_ground_to_height_and_canopy_width() {
        let pine = find_species("pinus-sylvestris").unwrap();
        let base = DVec3::new(3.0, 4.0, 1.0);
        let (pos, faces) = plant_mesh(pine, base, None);
        assert!(!faces.is_empty());
        let zmin = pos.iter().map(|p| p.z).fold(f64::INFINITY, f64::min);
        let zmax = pos.iter().map(|p| p.z).fold(f64::NEG_INFINITY, f64::max);
        assert!((zmin - 1.0).abs() < 1e-9, "trunk starts at ground");
        assert!((zmax - (1.0 + 25.0)).abs() < 1e-9, "apex at mature height");
        let rmax = pos
            .iter()
            .map(|p| ((p.x - 3.0).powi(2) + (p.y - 4.0).powi(2)).sqrt())
            .fold(0.0f64, f64::max);
        assert!((rmax - 4.5).abs() < 1e-9, "canopy radius = 9/2");
        // Age-scaled mesh is proportionally smaller.
        let (pos_y, _) = plant_mesh(pine, base, Some(10.0)); // f = 4/25
        let zmax_y = pos_y.iter().map(|p| p.z).fold(f64::NEG_INFINITY, f64::max);
        assert!((zmax_y - (1.0 + 4.0)).abs() < 1e-9);
    }

    #[test]
    fn row_positions_count_and_spacing() {
        let a = DVec3::ZERO;
        let b = DVec3::new(10.0, 0.0, 0.0);
        let row = row_positions(a, b, 2.5);
        assert_eq!(row.len(), 5, "0, 2.5, 5, 7.5, 10");
        assert_eq!(*row.last().unwrap(), b, "exact multiple includes b");
        for (i, p) in row.iter().enumerate() {
            assert!((p.x - i as f64 * 2.5).abs() < 1e-9);
        }
        assert_eq!(row_positions(a, DVec3::new(9.9, 0.0, 0.0), 2.5).len(), 4);
        assert!(row_positions(a, b, 0.0).is_empty(), "bad spacing → empty");
        assert_eq!(row_positions(a, a, 2.0).len(), 1, "degenerate row = one plant");
    }

    #[test]
    fn terrain_z_interpolates_and_rejects_outside() {
        let (pos, faces) = grid_terrain(10, 10.0, |x, y| x + 2.0 * y);
        // Interior, off-vertex point: exact for a piecewise-linear plane.
        let z = terrain_z_at(&pos, &faces, 3.3, 4.7).unwrap();
        assert!((z - (3.3 + 9.4)).abs() < 1e-9);
        assert!(terrain_z_at(&pos, &faces, -1.0, 5.0).is_none());
        assert!(terrain_z_at(&pos, &faces, 5.0, 11.0).is_none());
    }

    #[test]
    fn flows_on_sloped_plane_point_downslope() {
        // z = x/2 → downhill is −x everywhere, slope 0.5, dz < 0.
        let (pos, faces) = grid_terrain(8, 8.0, |x, _| x / 2.0);
        let flows = face_flows(&pos, &faces);
        assert_eq!(flows.len(), faces.len(), "every face slopes");
        for fl in &flows {
            assert!(fl.downhill.x < -0.85, "points −x, got {:?}", fl.downhill);
            assert!(fl.downhill.y.abs() < 1e-9);
            assert!(fl.downhill.z < 0.0, "descends");
            assert!((fl.slope - 0.5).abs() < 1e-9);
            assert!((fl.downhill.length() - 1.0).abs() < 1e-12);
        }
    }

    #[test]
    fn flat_terrain_has_no_flow_arrows() {
        let (pos, faces) = grid_terrain(4, 4.0, |_, _| 1.0);
        assert!(face_flows(&pos, &faces).is_empty());
    }

    #[test]
    fn arrow_glyph_is_five_points_at_the_tip() {
        let c = DVec3::new(1.0, 2.0, 3.0);
        let d = DVec3::new(-1.0, 0.0, 0.0);
        let pts = arrow_points(c, d, 2.0);
        assert_eq!(pts.len(), 5);
        assert_eq!(pts[0], c);
        assert_eq!(pts[1], DVec3::new(-1.0, 2.0, 3.0), "tip 2 m downhill");
        assert_eq!(pts[3], pts[1], "polyline returns to the tip between barbs");
        assert!((pts[2].y - pts[4].y).abs() > 0.1, "barbs straddle the shaft");
    }

    #[test]
    fn bowl_has_one_sink_at_center_slope_has_none() {
        // Paraboloid bowl centered on the (5,5) grid vertex.
        let (pos, faces) = grid_terrain(10, 10.0, |x, y| {
            ((x - 5.0).powi(2) + (y - 5.0).powi(2)) / 10.0
        });
        let sinks = find_sinks(&pos, &faces);
        assert_eq!(sinks.len(), 1, "one sink at the bowl bottom");
        let p = pos[sinks[0]];
        assert!((p.x - 5.0).abs() < 1e-9 && (p.y - 5.0).abs() < 1e-9);
        // A uniform slope drains off the edge — no interior minima.
        let (pos2, faces2) = grid_terrain(10, 10.0, |x, _| x / 2.0);
        assert!(find_sinks(&pos2, &faces2).is_empty());
    }

    #[test]
    fn sample_polyline_even_spacing_and_endpoints() {
        let pts = vec![DVec3::ZERO, DVec3::new(10.0, 0.0, 0.0)];
        let s = sample_polyline(&pts, false, 0.5);
        assert_eq!(s.len(), 21, "10 m at 0.5 m → 20 intervals + both ends");
        assert_eq!(s[0], pts[0]);
        assert_eq!(*s.last().unwrap(), pts[1]);
        for w in s.windows(2) {
            assert!(((w[1] - w[0]).length() - 0.5).abs() < 1e-9);
        }
        // Closed square: seam included, corners survive resampling arclength.
        let sq = vec![
            DVec3::ZERO,
            DVec3::new(4.0, 0.0, 0.0),
            DVec3::new(4.0, 4.0, 0.0),
            DVec3::new(0.0, 4.0, 0.0),
        ];
        let s2 = sample_polyline(&sq, true, 1.0);
        assert_eq!(s2.len(), 17, "16 m perimeter at 1 m");
        assert_eq!(s2[0], *s2.last().unwrap(), "closed seam");
    }

    #[test]
    fn ribbon_width_and_face_count() {
        let center = vec![
            DVec3::ZERO,
            DVec3::new(5.0, 0.0, 0.0),
            DVec3::new(10.0, 0.0, 1.0),
        ];
        let (pos, faces) = ribbon(&center, 2.0);
        assert_eq!(pos.len(), 6, "left/right pair per sample");
        assert_eq!(faces.len(), 4, "two triangles per span");
        // Straight-in-plan centerline → edges at y = ±1.
        for pair in pos.chunks(2) {
            assert!((pair[0].y - 1.0).abs() < 1e-9 && (pair[1].y + 1.0).abs() < 1e-9);
        }
    }

    #[test]
    fn path_slope_check_flags_only_steep_runs() {
        // 12 m of run rising 1 m → exactly 1:12, NOT flagged (limit is >).
        let ok = vec![DVec3::ZERO, DVec3::new(12.0, 0.0, 1.0)];
        let (max_s, steep) = path_slope_check(&ok, 1.0 / 12.0);
        assert!((max_s - 1.0 / 12.0).abs() < 1e-12);
        assert_eq!(steep, 0, "exactly at the limit passes");
        // 5 m rising 1 m → 1:5, flagged.
        let bad = vec![DVec3::ZERO, DVec3::new(5.0, 0.0, 1.0), DVec3::new(10.0, 0.0, 1.0)];
        let (max_b, steep_b) = path_slope_check(&bad, 1.0 / 12.0);
        assert!((max_b - 0.2).abs() < 1e-12);
        assert_eq!(steep_b, 1, "only the first leg is steep");
    }

    /// Every vertex of a segment soup lies within `tol` of the expected radius
    /// band and on the given z-plane. Returns the max radius found.
    fn seg_extent(segs: &[(DVec3, DVec3)], cx: f64, cy: f64, z: f64) -> f64 {
        let mut rmax = 0.0f64;
        for (a, b) in segs {
            for p in [a, b] {
                assert!((p.z - z).abs() < 1e-9, "symbol must be planar at z={z}");
                rmax = rmax.max(((p.x - cx).powi(2) + (p.y - cy).powi(2)).sqrt());
            }
        }
        rmax
    }

    #[test]
    fn plan_symbol_default_from_form_and_explicit_override() {
        // Guadua is a "column" form but drafts as a clump via explicit field.
        let guadua = find_species("guadua-angustifolia").unwrap();
        assert_eq!(guadua.form, "column");
        assert_eq!(guadua.plan_symbol_style(), PlanSymbol::Clump);
        // Forms without an explicit symbol derive from the 3D form.
        let oak = find_species("quercus-robur").unwrap(); // round
        let pine = find_species("pinus-sylvestris").unwrap(); // cone
        let palm = find_species("roystonea-regia").unwrap(); // palm
        let cypress = find_species("cupressus-sempervirens").unwrap(); // column
        assert_eq!(oak.plan_symbol_style(), PlanSymbol::Round);
        assert_eq!(pine.plan_symbol_style(), PlanSymbol::Conifer);
        assert_eq!(palm.plan_symbol_style(), PlanSymbol::Palm);
        assert_eq!(cypress.plan_symbol_style(), PlanSymbol::Shrub);
    }

    #[test]
    fn plan_symbol_geometry_per_style_counts_and_extent() {
        let c = DVec3::new(10.0, 20.0, 3.0);
        let d = 8.0; // canopy diameter → radius 4
        let r = d / 2.0;

        let round = plan_symbol_segments(PlanSymbol::Round, c, d);
        // 24-gon ring + 8 branch stubs.
        assert_eq!(round.len(), CIRCLE_SEG + ROUND_BRANCHES);
        let rr = seg_extent(&round, c.x, c.y, c.z);
        assert!((rr - r).abs() < 1e-6, "round ring reaches canopy radius");

        let conifer = plan_symbol_segments(PlanSymbol::Conifer, c, d);
        assert_eq!(conifer.len(), 2 * CONIFER_POINTS, "closed star ring");
        assert!((seg_extent(&conifer, c.x, c.y, c.z) - r).abs() < 1e-6);

        let palm = plan_symbol_segments(PlanSymbol::Palm, c, d);
        assert_eq!(palm.len(), PALM_FRONDS, "one segment per frond spoke");
        // Every frond starts at the exact center.
        for (a, _) in &palm {
            assert!((a.x - c.x).abs() < 1e-9 && (a.y - c.y).abs() < 1e-9);
        }
        assert!((seg_extent(&palm, c.x, c.y, c.z) - r).abs() < 1e-6);

        let shrub = plan_symbol_segments(PlanSymbol::Shrub, c, d);
        assert_eq!(shrub.len(), STIPPLE_DOTS / 2, "half the ring slots are gaps");
        assert!(seg_extent(&shrub, c.x, c.y, c.z) <= r + 1e-9);

        let clump = plan_symbol_segments(PlanSymbol::Clump, c, d);
        assert_eq!(clump.len(), 2 * CLUMP_DOTS, "two crossing dashes per dot");
        // The clump stays strictly inside the canopy.
        assert!(seg_extent(&clump, c.x, c.y, c.z) < r);
    }

    #[test]
    fn plan_symbol_scales_linearly_with_canopy() {
        let c = DVec3::ZERO;
        let small = plan_symbol_segments(PlanSymbol::Round, c, 4.0);
        let big = plan_symbol_segments(PlanSymbol::Round, c, 8.0);
        assert_eq!(small.len(), big.len());
        let rs = seg_extent(&small, 0.0, 0.0, 0.0);
        let rb = seg_extent(&big, 0.0, 0.0, 0.0);
        assert!((rb - 2.0 * rs).abs() < 1e-6, "double diameter → double extent");
        // Degenerate canopy → no geometry.
        assert!(plan_symbol_segments(PlanSymbol::Round, c, 0.0).is_empty());
    }

    #[test]
    fn plan_symbol_is_deterministic() {
        let c = DVec3::new(1.0, 2.0, 0.5);
        for style in [
            PlanSymbol::Round,
            PlanSymbol::Conifer,
            PlanSymbol::Palm,
            PlanSymbol::Shrub,
            PlanSymbol::Clump,
        ] {
            let a = plan_symbol_segments(style, c, 6.0);
            let b = plan_symbol_segments(style, c, 6.0);
            assert_eq!(a, b);
        }
    }

    #[test]
    fn plant_object_symbol_recovers_center_and_canopy_from_mesh() {
        // A real planted mesh: royal palm (palm symbol) at a known base.
        let palm = find_species("roystonea-regia").unwrap();
        let base = DVec3::new(5.0, 7.0, 2.0);
        let (pos, _) = plant_mesh(palm, base, None);
        let segs = plant_object_symbol("plant:roystonea-regia", &pos).unwrap();
        assert_eq!(segs.len(), PALM_FRONDS, "palm plan symbol");
        // Fronds radiate from the trunk axis at ground elevation.
        for (a, _) in &segs {
            assert!((a.x - 5.0).abs() < 1e-6 && (a.y - 7.0).abs() < 1e-6);
            assert!((a.z - 2.0).abs() < 1e-6, "symbol drapes at ground z");
        }
        // Non-plant / unknown names yield nothing.
        assert!(plant_object_symbol("wall", &pos).is_none());
        assert!(plant_object_symbol("plant:triffid", &pos).is_none());
        assert!(plant_object_symbol("plant:roystonea-regia", &[]).is_none());
    }

    #[test]
    fn legacy_catalog_without_plan_symbol_derives_from_form() {
        // A pre-plan-symbol entry: no `plan_symbol` field at all.
        let legacy = r#"[{"id":"old-fir","common":"fir","binomial":"Abies x",
            "mature_height_m":20.0,"canopy_diameter_m":6.0,
            "growth_m_per_year":0.4,"deciduous":false,"form":"cone"}]"#;
        let parsed: Vec<PlantSpecies> = serde_json::from_str(legacy).unwrap();
        assert!(parsed[0].plan_symbol.is_none());
        assert_eq!(parsed[0].plan_symbol_style(), PlanSymbol::Conifer);
    }

    #[test]
    fn chaining_is_deterministic() {
        let (pos, faces) = grid_terrain(20, 10.0, |x, y| {
            (5.0 - ((x - 5.0).powi(2) + (y - 5.0).powi(2)).sqrt()).max(0.0)
        });
        let a = contours(&pos, &faces, 1.0);
        let b = contours(&pos, &faces, 1.0);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(&b) {
            assert_eq!(x.points, y.points);
            assert_eq!(x.closed, y.closed);
        }
    }
}
