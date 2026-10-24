// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Road-network generators. The FOUR rectilinear generators,
//! each turning a site boundary + `SubdivisionSettings` into a
//! [`StreetGraph`]:
//!
//! - [`orthogonal`] — recursive-OBB split of the *site* down to ~2× lot depth;
//!   a spine centerline is emitted along each split.
//! - [`skewed`] — the same, with a global rotation applied to split directions.
//! - [`organic`] — spline spines fitted to the site long axis with controlled
//!   sinusoidal deviation, plus secondary connectors.
//! - [`culdesac`] — a spine road with perpendicular stubs ending in bulbs,
//!   spaced by block depth.
//!
//! **Three non-rectilinear (owner scope):**
//!
//! - [`radial`] — concentric ring roads at block-depth spacing + radial spokes
//!   from a center (or centers), clipped to the site; blocks are annular sectors.
//!   Its own polar layout — does NOT use OBB site-splitting.
//! - [`hexagonal`] — a hex lattice sized to block depth over the site bbox; hex
//!   cell edges become streets, hex cells become blocks (boundary cells clipped).
//! - [`voronoi`] — jittered-grid seeds (seeded from op data for replay) →
//!   Voronoi diagram as the **dual of `kernel_mesh::triangulate`** (circumcenters
//!   of adjacent Delaunay triangles are the Voronoi vertices); cell edges →
//!   streets, cells → blocks. Reuses the existing Delaunay — no new external dep.
//!
//! Determinism: each generator seeds a splitmix64 from a quantized hash of the
//! site + `settings.seed`, so the same input yields a byte-identical graph.

pub mod culdesac;
pub mod hexagonal;
pub mod organic;
pub mod orthogonal;
pub mod radial;
pub mod skewed;
pub mod voronoi;

use crate::geometry::polygon2d::Polygon2d;
use crate::settings::{StreetPattern, SubdivisionSettings};
use crate::streets::street_graph::StreetGraph;
use glam::DVec2;

/// A clipped piece shorter than this (metres) is a crumb, not a road: dropped.
const MIN_PIECE_M: f64 = 1e-3;
/// A point this close to the site boundary (metres) counts as on it.
const ON_BOUNDARY_EPS: f64 = 1e-9;

fn on_boundary(site: &Polygon2d, p: DVec2) -> bool {
    site.edges().any(|(a, b)| {
        let ab = b - a;
        let l2 = ab.length_squared();
        let t = if l2 > 0.0 { ((p - a).dot(ab) / l2).clamp(0.0, 1.0) } else { 0.0 };
        p.distance(a + ab * t) <= ON_BOUNDARY_EPS
    })
}

/// Parameters `t ∈ (0, 1)` where segment `a`→`b` meets the site boundary.
fn boundary_hits(site: &Polygon2d, a: DVec2, b: DVec2) -> Vec<f64> {
    let d = b - a;
    let mut ts = Vec::new();
    for (c, e) in site.edges() {
        let f = e - c;
        let den = d.perp_dot(f);
        if den.abs() < 1e-15 {
            // Parallel: if collinear, the edge endpoints bound an overlap.
            if (c - a).perp_dot(d).abs() < 1e-9 * d.length().max(1.0) {
                let l2 = d.length_squared();
                ts.extend([c, e].map(|q| (q - a).dot(d) / l2));
            }
            continue;
        }
        let t = (c - a).perp_dot(f) / den;
        let u = (c - a).perp_dot(d) / den;
        if (-1e-12..=1.0 + 1e-12).contains(&u) {
            ts.push(t);
        }
    }
    ts.retain(|t| *t > 1e-12 && *t < 1.0 - 1e-12);
    ts
}

/// Clip an open (or closed) polyline to the — possibly concave — site: the
/// pieces of it inside the site or on its boundary, in order. Each piece that
/// leaves the site ends exactly on the boundary; pieces shorter than
/// [`MIN_PIECE_M`] are dropped. A polyline wholly inside comes back unchanged.
pub(crate) fn clip_polyline_to_site(site: &Polygon2d, pts: &[DVec2]) -> Vec<Vec<DVec2>> {
    let mut runs: Vec<Vec<DVec2>> = Vec::new();
    let mut cur: Vec<DVec2> = Vec::new();
    let flush = |cur: &mut Vec<DVec2>, runs: &mut Vec<Vec<DVec2>>| {
        if cur.len() >= 2 && crate::geometry::polyline::PolylineTools::arc_length(cur) >= MIN_PIECE_M {
            runs.push(std::mem::take(cur));
        }
        cur.clear();
    };
    for w in pts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a.distance_squared(b) < 1e-24 {
            continue;
        }
        let mut ts = boundary_hits(site, a, b);
        ts.extend([0.0, 1.0]);
        ts.sort_by(f64::total_cmp);
        ts.dedup_by(|x, y| (*x - *y).abs() < 1e-12);
        for iv in ts.windows(2) {
            let mid = a.lerp(b, 0.5 * (iv[0] + iv[1]));
            if !(site.contains(mid) || on_boundary(site, mid)) {
                flush(&mut cur, &mut runs);
                continue;
            }
            let (p0, p1) = (a.lerp(b, iv[0]), a.lerp(b, iv[1]));
            if cur.is_empty() {
                cur.push(p0);
            }
            // Extend along the same segment instead of adding a collinear point.
            if iv[0] > 0.0 && cur.len() >= 2 {
                cur.pop();
            }
            cur.push(p1);
        }
    }
    flush(&mut cur, &mut runs);
    // A closed loop cut open: rejoin the piece through its start vertex.
    let closed = pts.len() >= 3 && pts[0].distance(pts[pts.len() - 1]) < 1e-9;
    if closed
        && runs.len() >= 2
        && runs[0][0] == pts[0]
        && runs.last().and_then(|r| r.last()) == pts.last()
    {
        let first = runs.remove(0);
        runs.last_mut().unwrap().extend_from_slice(&first[1..]);
    }
    runs
}

/// Clip every street of `graph` to `site` (#166): generators that lay a
/// lattice / diagram over the site bbox (hexagonal, Voronoi) or step off a
/// concave site (cul-de-sac stubs) would otherwise leave centerlines outside.
/// Streets keep their width + tier; ids are reassigned in order.
pub(crate) fn clip_to_site(site: &Polygon2d, graph: StreetGraph) -> StreetGraph {
    let mut out = StreetGraph::new();
    for s in graph.streets {
        for piece in clip_polyline_to_site(site, &s.centerline) {
            out.add(piece, s.width, s.tier);
        }
    }
    out
}

/// A deterministic splitmix64 stream shared by the generators.
pub(crate) struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub(crate) fn new(seed: u64) -> Self {
        SplitMix64 { state: seed }
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A float in `[-1, 1]`.
    pub(crate) fn signed_unit(&mut self) -> f64 {
        (self.next_u64() as f64 / u64::MAX as f64) * 2.0 - 1.0
    }
}

/// Hash a site polygon + seed + a per-generator salt into a deterministic 64-bit
/// RNG seed. Quantizes coordinates to the mm grid so f64 noise never perturbs
/// the seed (replay stability).
pub(crate) fn site_seed(site: &Polygon2d, seed: u64, salt: u64) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ seed ^ salt;
    let mix = |h: &mut u64, x: i64| {
        for b in x.to_le_bytes() {
            *h ^= b as u64;
            *h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for v in site.verts() {
        mix(&mut h, (v.x * 1000.0).round() as i64);
        mix(&mut h, (v.y * 1000.0).round() as i64);
    }
    h | 1
}

/// A sane metre floor for any spacing/depth that becomes a loop denominator or a
/// lattice cell size. A hostile/typo value like `blockdepth=0.0001` would
/// otherwise spin up billions of road rows / lattice cells (DoS / hang). No real
/// urban block depth is below this.
pub(crate) const MIN_SPACING_M: f64 = 0.5;

/// The effective block depth for road spacing: the verb-supplied value if
/// positive, else a fallback of ~2× the lot depth target (or a multiple of the
/// min lot width when no depth target is set). Clamped to [`MIN_SPACING_M`] so a
/// tiny denominator can never generate an unbounded number of roads.
pub fn effective_block_depth(settings: &SubdivisionSettings) -> f64 {
    let d = if settings.block_depth > 0.0 {
        settings.block_depth
    } else if settings.lot_depth_target > 0.0 {
        settings.lot_depth_target * 2.0
    } else {
        // ~2× lot depth; lacking a depth target, derive from min width.
        (settings.lot_width_min * 2.0).max(20.0)
    };
    d.max(MIN_SPACING_M)
}

/// The effective road ROW width: the verb-supplied value if positive, else a
/// sane residential default. Never returns ≤ 0.
pub fn effective_road_width(settings: &SubdivisionSettings) -> f64 {
    if settings.road_width > 0.0 {
        settings.road_width
    } else {
        12.0
    }
}

/// Dispatch on `settings.street_pattern` to the matching generator: the
/// four rectilinear patterns plus the three non-rectilinear
/// generators (radial / hexagonal / Voronoi). All seven produce a `StreetGraph`
/// fed to the same `block_extractor`.
pub fn generate(site: &Polygon2d, settings: &SubdivisionSettings) -> StreetGraph {
    let graph = match settings.street_pattern {
        StreetPattern::Orthogonal => orthogonal::generate(site, settings),
        StreetPattern::Skewed => skewed::generate(site, settings),
        StreetPattern::Organic => organic::generate(site, settings),
        StreetPattern::CulDeSac => culdesac::generate(site, settings),
        // Owner scope, non-rectilinear.
        StreetPattern::Radial => radial::generate(site, settings),
        StreetPattern::Hexagonal => hexagonal::generate(site, settings),
        StreetPattern::Voronoi => voronoi::generate(site, settings),
    };
    clip_to_site(site, graph)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Security: a tiny (or zero) block depth must be clamped to the metre floor
    /// so road-spacing loops can't spin up billions of rows.
    #[test]
    fn effective_block_depth_clamps_tiny_value() {
        let s = SubdivisionSettings {
            block_depth: 0.0001,
            ..SubdivisionSettings::default()
        };
        assert!(
            effective_block_depth(&s) >= MIN_SPACING_M,
            "tiny block depth must clamp to {MIN_SPACING_M} m, got {}",
            effective_block_depth(&s)
        );
        // A realistic value passes through unchanged.
        let s2 = SubdivisionSettings { block_depth: 60.0, ..SubdivisionSettings::default() };
        assert!((effective_block_depth(&s2) - 60.0).abs() < 1e-9);
    }
}
