// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Road-network generators (plan §7.3). The FOUR rectilinear generators
//! (Phase 5), each turning a site boundary + `SubdivisionSettings` into a
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
//! **Three non-rectilinear (owner scope, Phase 5b):**
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

/// The effective block depth for road spacing: the verb-supplied value if
/// positive, else a fallback of ~2× the lot depth target (or a multiple of the
/// min lot width when no depth target is set). Never returns ≤ 0.
pub(crate) fn effective_block_depth(settings: &SubdivisionSettings) -> f64 {
    if settings.block_depth > 0.0 {
        settings.block_depth
    } else if settings.lot_depth_target > 0.0 {
        settings.lot_depth_target * 2.0
    } else {
        // ~2× lot depth; lacking a depth target, derive from min width.
        (settings.lot_width_min * 2.0).max(20.0)
    }
}

/// The effective road ROW width: the verb-supplied value if positive, else a
/// sane residential default. Never returns ≤ 0.
pub(crate) fn effective_road_width(settings: &SubdivisionSettings) -> f64 {
    if settings.road_width > 0.0 {
        settings.road_width
    } else {
        12.0
    }
}

/// Dispatch on `settings.street_pattern` to the matching generator. Phase 5
/// covers the four rectilinear patterns; Phase 5b adds the three non-rectilinear
/// generators (radial / hexagonal / Voronoi). All seven produce a `StreetGraph`
/// fed to the same `block_extractor`.
pub fn generate(site: &Polygon2d, settings: &SubdivisionSettings) -> StreetGraph {
    match settings.street_pattern {
        StreetPattern::Orthogonal => orthogonal::generate(site, settings),
        StreetPattern::Skewed => skewed::generate(site, settings),
        StreetPattern::Organic => organic::generate(site, settings),
        StreetPattern::CulDeSac => culdesac::generate(site, settings),
        // Phase 5b — owner scope, non-rectilinear.
        StreetPattern::Radial => radial::generate(site, settings),
        StreetPattern::Hexagonal => hexagonal::generate(site, settings),
        StreetPattern::Voronoi => voronoi::generate(site, settings),
    }
}
