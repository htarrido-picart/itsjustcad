// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Site-plan metrics the optimizer (#145) scores beyond the yield report:
//!
//! - **Park walk distance** along the street network: the plan's street graph
//!   ([`StreetNetwork`], the graph `ijc_street_graph_json` draws — so edited
//!   streets change the walk), multi-source Dijkstra
//!   from the parks' street-facing access points, read at each lot's
//!   frontage midpoint.
//! - **Facade daylight**: sample points on the buildings' vertical faces (a
//!   ~5 m × one-storey grid, at mid-storey), each daylit when its direct sun
//!   from in front of its face, ray-cast against every building mass (a
//!   triangle BVH) every 30 min, averages enough hours a day over the chosen
//!   period — the whole year by default (the 21st of each month), the
//!   site's worst month, or a date; never a hemisphere assumption. Optional
//!   overheating: the share with too many sun hours on the hottest month.

use std::collections::HashMap;

use glam::{DVec2, DVec3};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use subdivision::Polygon2d;

use crate::lot::SitePlanBake;
use crate::lot::street_net::{NetEdge, StreetNetwork};
use crate::site_optimize::Location;

// ── street network ──────────────────────────────────────────────────────────

/// How far a park edge may sit from a centerline and still open onto it, m
/// (half a wide road plus a verge).
const PARK_REACH: f64 = 20.0;
/// How far a lot's frontage may sit from its street's centerline, m.
const LOT_REACH: f64 = 40.0;
/// Spacing of the access points sampled along a park's boundary, m.
const PARK_STEP: f64 = 10.0;

/// Network walking distance from each of `lots` (access points) to the
/// nearest park, along the street network `net`. Parks open
/// onto every centerline within reach of their boundary. A lot or park with
/// no street within reach is unreachable (infinite).
pub fn walk_distances(net: &StreetNetwork, parks: &[Polygon2d], lots: &[DVec2]) -> Vec<f64> {
    let mut sources = Vec::new();
    for park in parks {
        for (a, b) in park.edges() {
            let n = (a.distance(b) / PARK_STEP).ceil().max(1.0) as usize;
            for k in 0..=n {
                let p = a + (b - a) * (k as f64 / n as f64);
                sources.extend(net.nearest(p, PARK_REACH));
            }
        }
    }
    if sources.is_empty() {
        return vec![f64::INFINITY; lots.len()];
    }
    let dist = net.distances(&sources);
    lots.iter()
        .map(|p| {
            let Some((e, q, off)) = net.nearest(*p, LOT_REACH) else {
                return f64::INFINITY;
            };
            let NetEdge { from: u, to: v, .. } = net.edges[e];
            let via_nodes = (dist[u] + q.distance(net.nodes[u])).min(dist[v] + q.distance(net.nodes[v]));
            // A park opening onto the same edge is reached along it.
            let same_edge = sources
                .iter()
                .filter(|s| s.0 == e)
                .map(|s| s.2 + s.1.distance(q))
                .fold(f64::INFINITY, f64::min);
            off + via_nodes.min(same_edge)
        })
        .collect()
}

/// A lot's access point: the midpoint of its longest street edge (its
/// frontage), else its centroid.
pub fn lot_access_point(lot: &subdivision::Block) -> DVec2 {
    lot.edges
        .iter()
        .filter(|e| e.is_street)
        .max_by(|x, y| x.a.distance(x.b).total_cmp(&y.a.distance(y.b)))
        .map_or_else(|| lot.polygon.centroid(), |e| (e.a + e.b) / 2.0)
}

/// Each lot's walk to the nearest park along the plan's streets (parallel to
/// `plan.lots`).
pub fn park_walk(plan: &SitePlanBake) -> Vec<f64> {
    let access: Vec<DVec2> = plan.lots.lots.iter().map(lot_access_point).collect();
    walk_distances(
        &StreetNetwork::from_plan(&plan.site),
        &plan.reserved,
        &access,
    )
}

// ── facade daylight ─────────────────────────────────────────────────────────

/// Horizontal spacing of facade samples, m.
pub const FACADE_STEP: f64 = 5.0;

/// The days a daylight analysis covers. Never a hemisphere assumption: the
/// default averages the whole year, the others are computed or chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaylightPeriod {
    /// One representative day per month (the 21st); the metric is the average
    /// daily direct-sun hours across the year.
    #[default]
    Annual,
    /// The 21st of the month with the shortest day at the site's latitude
    /// (found from the sun, not assumed).
    WorstMonth,
    /// One chosen date.
    Custom { month: u32, day: u32 },
}

/// What a facade daylight analysis measures.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DaylightOpts {
    pub period: DaylightPeriod,
    /// Direct-sun hours (per day, averaged over the period) a facade sample
    /// needs to count as daylit.
    pub hours: f64,
    /// When set, also measure the share of facade area with MORE than this
    /// many direct-sun hours on the hottest month's 21st (overheating).
    pub overheat_hours: Option<f64>,
}

const YEAR: i32 = 2025;

/// The month (1–12) with the shortest day at `lat_deg` — the sunniest is
/// the longest; ties go to the earlier month.
fn extreme_month(lat_deg: f64, longest: bool) -> u32 {
    let hours = |m: u32| itsjustcad_solar::daylight_hours(YEAR, m, 21, lat_deg);
    (1..=12)
        .reduce(|a, b| {
            let (ha, hb) = (hours(a), hours(b));
            if (longest && hb > ha + 1e-9) || (!longest && hb < ha - 1e-9) { b } else { a }
        })
        .unwrap_or(1)
}

/// The hottest month as sun geometry sees it: the longest day at the site.
/// (A climate file's monthly temperatures would be better; the document keeps
/// none, so this is the proxy.)
pub fn hottest_month(lat_deg: f64) -> u32 {
    extreme_month(lat_deg, true)
}

/// The month with the fewest sun hours at the site.
pub fn worst_month(lat_deg: f64) -> u32 {
    extreme_month(lat_deg, false)
}

/// Sun directions (unit, z up) every 30 min of `month`/`day` at `loc`, sun
/// above the horizon only.
pub fn day_sun(loc: &Location, month: u32, day: u32) -> Vec<DVec3> {
    (0..48)
        .filter_map(|slot| {
            let utc = (slot as f64 * 30.0 - loc.tz_hours * 60.0).rem_euclid(1440.0);
            let p = itsjustcad_solar::solar_position(
                YEAR,
                month,
                day,
                (utc / 60.0) as u32,
                (utc % 60.0) as u32,
                loc.lat_deg,
                loc.lon_deg,
            );
            (p.altitude_deg > 0.0).then(|| {
                let d = itsjustcad_solar::sun_direction(p.azimuth_deg, p.altitude_deg);
                DVec3::new(d[0] as f64, d[1] as f64, d[2] as f64)
            })
        })
        .collect()
}

/// The analysis days of `period` at `loc`.
pub fn period_days(period: DaylightPeriod, loc: &Location) -> Vec<(u32, u32)> {
    match period {
        DaylightPeriod::Annual => (1..=12).map(|m| (m, 21)).collect(),
        DaylightPeriod::WorstMonth => vec![(worst_month(loc.lat_deg), 21)],
        DaylightPeriod::Custom { month, day } => vec![(month.clamp(1, 12), day.clamp(1, 28))],
    }
}

/// Facade daylight of a plan.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FacadeDaylight {
    /// Share of facade area with at least the threshold of direct sun.
    pub facade: f64,
    /// Share of storeys (per building) with at least one such facade sample.
    pub floors: f64,
    /// Share of facade area over the overheating threshold on the hottest
    /// month, when asked.
    pub overheat: Option<f64>,
}

/// One facade sample: where, facing, weight (m²), building and storey.
struct Sample {
    p: DVec3,
    n: DVec3,
    w: f64,
    storey: (usize, usize),
}

/// Sample the vertical faces of `mass` (triangles with a horizontal normal):
/// a grid of `FACADE_STEP` along the wall × one storey, at mid-storey heights.
/// A face too small to hold a grid point gets one sample at its centroid,
/// weighted by its area.
fn facade_samples(mass: &kernel_mesh::Mesh, base_z: f64, floor_h: f64, building: usize, out: &mut Vec<Sample>) {
    let pos = mass.positions();
    for f in mass.faces() {
        let [a, b, c] = f.map(|i| pos[i as usize]);
        let n = (b - a).cross(c - a);
        let area = n.length() / 2.0;
        if area < 1e-6 {
            continue;
        }
        let n = n.normalize();
        if n.z.abs() > 0.2 {
            continue; // roof / floor
        }
        let h = DVec3::new(-n.y, n.x, 0.0).normalize(); // along the wall
        let st = |p: DVec3| DVec2::new((p - a).dot(h), p.z);
        let (sa, sb, sc) = (st(a), st(b), st(c));
        let inside = |q: DVec2| {
            let s = |p: DVec2, u: DVec2, v: DVec2| (v - u).perp_dot(p - u);
            let (d1, d2, d3) = (s(q, sa, sb), s(q, sb, sc), s(q, sc, sa));
            !((d1 < 0.0 || d2 < 0.0 || d3 < 0.0) && (d1 > 0.0 || d2 > 0.0 || d3 > 0.0))
        };
        let (lo, hi) = (sa.min(sb).min(sc), sa.max(sb).max(sc));
        let storey = |z: f64| ((z - base_z) / floor_h).floor().max(0.0) as usize;
        let before = out.len();
        let mut k = ((lo.x / FACADE_STEP) - 0.5).ceil() as i64;
        while (k as f64 + 0.5) * FACADE_STEP <= hi.x {
            let s = (k as f64 + 0.5) * FACADE_STEP;
            let mut j = (((lo.y - base_z) / floor_h) - 0.5).ceil().max(0.0) as i64;
            while base_z + (j as f64 + 0.5) * floor_h <= hi.y {
                let z = base_z + (j as f64 + 0.5) * floor_h;
                if inside(DVec2::new(s, z)) {
                    let p = a + h * s + DVec3::Z * (z - a.z);
                    out.push(Sample { p, n, w: FACADE_STEP * floor_h, storey: (building, j as usize) });
                }
                j += 1;
            }
            k += 1;
        }
        if out.len() == before {
            let p = (a + b + c) / 3.0;
            out.push(Sample { p, n, w: area, storey: (building, storey(p.z)) });
        }
    }
}

/// Facade daylight: the share of facade area (and of storeys) whose direct
/// sun — from in front of the face, unblocked by any building mass — averages
/// at least `opts.hours` a day over `opts.period`; plus, when asked, the share
/// over the overheating threshold on the hottest month. `None` without
/// buildings.
pub fn facade_daylight(plan: &SitePlanBake, floor_h: f64, loc: &Location, opts: &DaylightOpts) -> Option<FacadeDaylight> {
    let blds = plan.buildings.as_ref()?;
    if blds.buildings.is_empty() {
        return None;
    }
    let days: Vec<Vec<DVec3>> = period_days(opts.period, loc)
        .into_iter()
        .map(|(m, d)| day_sun(loc, m, d))
        .collect();
    let hot = opts.overheat_hours.map(|h| (day_sun(loc, hottest_month(loc.lat_deg), 21), h));
    let mut tris = Vec::new();
    let mut samples = Vec::new();
    for (i, b) in blds.buildings.iter().enumerate() {
        let pos = b.mass.positions();
        tris.extend(b.mass.faces().iter().map(|f| f.map(|k| pos[k as usize])));
        facade_samples(&b.mass, b.z, floor_h.max(1.0), i, &mut samples);
    }
    if samples.is_empty() {
        return None;
    }
    let bvh = kernel_mesh::TriBvh::build(tris);
    // Half-hour slots needed over the whole period (average ≥ hours a day).
    let need = (opts.hours * 2.0 * days.len() as f64).ceil() as usize;
    let lit = |o: DVec3, n: DVec3, d: &DVec3| n.dot(*d) > 0.0 && !bvh.ray_occluded(o, *d);
    let per: Vec<(bool, bool)> = samples
        .par_iter()
        .map(|s| {
            let o = s.p + s.n * 0.05;
            // Stop as soon as the threshold is met or can no longer be.
            let mut got = 0usize;
            let mut left: usize = days.iter().map(|d| d.iter().filter(|d| s.n.dot(**d) > 0.0).count()).sum();
            'days: for day in &days {
                for d in day.iter().filter(|d| s.n.dot(**d) > 0.0) {
                    if got >= need || got + left < need {
                        break 'days;
                    }
                    left -= 1;
                    if lit(o, s.n, d) {
                        got += 1;
                    }
                }
            }
            let over = hot.as_ref().is_some_and(|(sun, h)| {
                let limit = (h * 2.0).floor() as usize;
                sun.iter().filter(|d| lit(o, s.n, d)).take(limit + 1).count() > limit
            });
            (got >= need, over)
        })
        .collect();
    let total: f64 = samples.iter().map(|s| s.w).sum::<f64>().max(1e-9);
    let share = |f: &dyn Fn(&(bool, bool)) -> bool| {
        samples.iter().zip(&per).filter(|(_, x)| f(x)).map(|(s, _)| s.w).sum::<f64>() / total
    };
    let mut storeys: HashMap<(usize, usize), bool> = HashMap::new();
    for (s, (l, _)) in samples.iter().zip(&per) {
        *storeys.entry(s.storey).or_default() |= *l;
    }
    let lit_storeys = storeys.values().filter(|l| **l).count();
    Some(FacadeDaylight {
        facade: share(&|x| x.0),
        floors: lit_storeys as f64 / storeys.len().max(1) as f64,
        overheat: hot.map(|_| share(&|x| x.1)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lot::street_net::StreetLine;

    fn net(roads: &[Vec<DVec2>]) -> StreetNetwork {
        let lines: Vec<StreetLine> = roads
            .iter()
            .enumerate()
            .map(|(i, r)| StreetLine {
                id: i as u32,
                points: r.clone(),
                closed: false,
            })
            .collect();
        StreetNetwork::build(&lines)
    }

    fn poly(pts: &[(f64, f64)]) -> Polygon2d {
        Polygon2d::from_pairs(pts.iter().copied()).unwrap()
    }

    /// Two parallel streets joined only far away: a park just across the
    /// block is 100 m away in a straight line but over 2 km on foot.
    #[test]
    fn walk_follows_streets_not_straight_lines() {
        let south = vec![DVec2::new(0.0, 0.0), DVec2::new(1000.0, 0.0)];
        let north = vec![DVec2::new(0.0, 100.0), DVec2::new(1000.0, 100.0)];
        let link = vec![DVec2::new(1000.0, 0.0), DVec2::new(1000.0, 100.0)];
        // A park fronting the north street, on its far side from the lot.
        let park = poly(&[(0.0, 106.0), (60.0, 106.0), (60.0, 160.0), (0.0, 160.0)]);
        let lot = DVec2::new(30.0, 6.0); // frontage midpoint on the south street
        let straight = (lot - DVec2::new(30.0, 106.0)).length();
        assert!(straight < 400.0);
        let d = walk_distances(
            &net(&[south.clone(), north.clone(), link]),
            std::slice::from_ref(&park),
            &[lot],
        );
        assert!(d[0] > 2000.0 && d[0] < 2200.0, "{}", d[0]);
        // A through street at x = 30 makes it a short walk.
        let cut = vec![DVec2::new(30.0, 0.0), DVec2::new(30.0, 100.0)];
        let d = walk_distances(&net(&[south, north, cut]), &[park], &[lot]);
        assert!(d[0] < 130.0, "{}", d[0]);
    }

    #[test]
    fn unreachable_without_streets_or_parks() {
        let road = vec![DVec2::new(0.0, 0.0), DVec2::new(100.0, 0.0)];
        let lot = DVec2::new(50.0, 5.0);
        assert!(walk_distances(&net(std::slice::from_ref(&road)), &[], &[lot])[0].is_infinite());
        let far_park = poly(&[(0.0, 500.0), (10.0, 500.0), (10.0, 510.0), (0.0, 510.0)]);
        assert!(walk_distances(&net(&[road]), &[far_park], &[lot])[0].is_infinite());
    }
}
