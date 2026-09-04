// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Recursive OBB subdivision (plan §7.1, Phase 3) — `method=grid`.
//!
//! Compute the minimum-area OBB of the block. Cut it with a line along the OBB
//! **short** direction, pivoted on the **long** axis midpoint (so the cut runs
//! across the short direction and halves the long dimension). Recurse on each
//! child while its area > `lot_area_min`. Terminate when `area < lot_area_min`
//! OR any child side would fall below `lot_width_min`.
//!
//! Four modifiers per the plan:
//! - **Street access** — if a child would lose its street edge, cut along the
//!   orthogonal (long-axis) direction instead. With `force_street_access == 1.0`
//!   a split that still orphans a child is rejected (the parent stays a lot).
//!   For Phase 3 there is no street graph, so **every original block boundary
//!   edge counts as frontage**.
//! - **Snap to contour vertices** — if the pivot lands near an original block
//!   vertex, move it onto that vertex.
//! - **Edge alignment** — the OBB already aligns cuts to a hull edge direction.
//! - **Seeding** — child RNG seeds are derived *before* recursing, so output is
//!   deterministic for a fixed seed and stable under replay.
//!
//! Determinism: all randomness comes from a splitmix64 stream seeded from a hash
//! of the block geometry + `settings.seed`. Same input → byte-identical output.

use crate::geometry::oriented_box::OrientedBox;
use crate::geometry::polygon2d::Polygon2d;
use crate::geometry::split::{split_by_line, Line2d};
use crate::settings::SubdivisionSettings;
use glam::DVec2;

/// One resulting lot plus which of its edges lie on the original block frontage.
#[derive(Debug, Clone)]
pub struct Lot {
    pub polygon: Polygon2d,
    /// True if the lot touches the original block boundary (its "street" in
    /// Phase 3, where any block edge counts as frontage).
    pub has_street: bool,
}

/// A deterministic splitmix64 stream. Seeded once from the block + settings.
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        SplitMix64 { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A float in `[-1, 1]`.
    fn signed_unit(&mut self) -> f64 {
        (self.next_u64() as f64 / u64::MAX as f64) * 2.0 - 1.0
    }
}

/// Hash a block + seed into a deterministic 64-bit RNG seed. Quantizes coords to
/// the mm grid so f64 noise never perturbs the seed (replay stability).
fn block_seed(block: &Polygon2d, seed: u64) -> u64 {
    // FNV-1a over quantized vertex coordinates + the settings seed.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ seed;
    let mix = |h: &mut u64, x: i64| {
        for b in x.to_le_bytes() {
            *h ^= b as u64;
            *h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for v in block.verts() {
        mix(&mut h, (v.x * 1000.0).round() as i64);
        mix(&mut h, (v.y * 1000.0).round() as i64);
    }
    // Avoid a zero state degenerating splitmix.
    h | 1
}

/// Whether `lot` touches the original block boundary. A lot is on the frontage
/// if any of its edges lies (mostly) on a block boundary edge.
fn touches_boundary(lot: &Polygon2d, block_edges: &[(DVec2, DVec2)]) -> bool {
    // Sample edge midpoints of the lot; if a midpoint sits on any block edge,
    // the lot has frontage. Robust to clip-introduced extra vertices.
    for (a, b) in lot.edges() {
        let mid = (a + b) * 0.5;
        for &(ea, eb) in block_edges {
            if point_on_segment(mid, ea, eb, 1e-6) {
                return true;
            }
        }
    }
    false
}

/// Is `p` within `tol` of the segment `a→b`?
fn point_on_segment(p: DVec2, a: DVec2, b: DVec2, tol: f64) -> bool {
    let ab = b - a;
    let len2 = ab.length_squared();
    if len2 < 1e-18 {
        return p.distance(a) < tol;
    }
    let t = (p - a).dot(ab) / len2;
    if !(-1e-9..=1.0 + 1e-9).contains(&t) {
        return false;
    }
    let proj = a + ab * t.clamp(0.0, 1.0);
    p.distance(proj) < tol
}

/// The minimum edge length of a polygon (approx "narrowest side"). Uses the OBB
/// short extent as the true min-width proxy for a lot.
fn min_side(poly: &Polygon2d) -> f64 {
    OrientedBox::of_polygon(poly)
        .map(|ob| ob.short_len())
        .unwrap_or(0.0)
}

/// Snap `pivot` to the nearest original block vertex if within `snap_dist`.
fn snap_pivot(pivot: DVec2, block: &Polygon2d, snap_dist: f64) -> DVec2 {
    let mut best = pivot;
    let mut best_d = snap_dist;
    for &v in block.verts() {
        let d = pivot.distance(v);
        if d < best_d {
            best_d = d;
            best = v;
        }
    }
    best
}

/// Public entry: subdivide `block` into lots using the Recursive OBB method.
/// Deterministic for a fixed `settings.seed`.
pub fn subdivide(block: &Polygon2d, settings: &SubdivisionSettings) -> Vec<Lot> {
    let block_edges: Vec<(DVec2, DVec2)> = block.edges().collect();
    let mut rng = SplitMix64::new(block_seed(block, settings.seed));
    let irregularity = settings.clamped_irregularity();
    let mut out = Vec::new();
    recurse(block, settings, &block_edges, irregularity, &mut rng, &mut out, 0);
    out
}

/// Recursion depth guard: even a pathological block cannot exceed this many
/// levels (2^40 lots is far past any real site).
const MAX_DEPTH: u32 = 40;

fn recurse(
    poly: &Polygon2d,
    settings: &SubdivisionSettings,
    block_edges: &[(DVec2, DVec2)],
    irregularity: f64,
    rng: &mut SplitMix64,
    out: &mut Vec<Lot>,
    depth: u32,
) {
    let area = poly.area();
    // Terminal: small enough, or too narrow to split further, or depth guard.
    let stop_area = area < settings.lot_area_min;
    if stop_area || depth >= MAX_DEPTH {
        emit(poly, block_edges, out);
        return;
    }

    let Some(ob) = OrientedBox::of_polygon(poly) else {
        emit(poly, block_edges, out);
        return;
    };

    // A cut is only worthwhile if BOTH children can clear lot_width_min. If the
    // block is already at/below 2× min width in its short dimension, splitting it
    // further would create sub-width lots — leave it whole. This also produces
    // the CORRECT behaviour when a high lot_width_min forces lots above
    // lot_area_max (plan §7.1): we simply stop.
    if ob.short_len() < settings.lot_width_min * 2.0 && ob.long_len() < settings.lot_width_min * 2.0
    {
        emit(poly, block_edges, out);
        return;
    }

    // Primary cut: line along the SHORT axis direction, pivoted at the LONG-axis
    // midpoint (i.e. the cut plane's normal is the long axis; it slices the long
    // dimension in two). `irregularity` jitters the pivot off center; seed the
    // jitter BEFORE recursing (deterministic).
    let jitter = rng.signed_unit() * irregularity * ob.half_long * 0.9;
    let mut pivot = ob.center + ob.long_axis * jitter;
    // Snap to a nearby original vertex if close (stops lot lines landing inches
    // off a bend). Snap radius scales with the short extent.
    pivot = snap_pivot(pivot, poly, ob.short_len() * 0.1);

    // Try the primary (short-direction) cut first, then the orthogonal fallback
    // for street access.
    let primary = Line2d::new(pivot, ob.short_axis);
    let orthogonal = Line2d::new(ob.center, ob.long_axis);

    let want_street = settings.force_street_access >= 1.0;

    if let Some((a, b)) = try_split(poly, &primary, settings, block_edges, want_street) {
        recurse(&a, settings, block_edges, irregularity, rng, out, depth + 1);
        recurse(&b, settings, block_edges, irregularity, rng, out, depth + 1);
        return;
    }
    // Street-access fallback: orthogonal split (keeps both children spanning the
    // long dimension, so each keeps a length of frontage).
    if let Some((a, b)) = try_split(poly, &orthogonal, settings, block_edges, want_street) {
        recurse(&a, settings, block_edges, irregularity, rng, out, depth + 1);
        recurse(&b, settings, block_edges, irregularity, rng, out, depth + 1);
        return;
    }

    // No admissible split — this polygon is a terminal lot.
    emit(poly, block_edges, out);
}

/// Attempt a split; return the two children only if both are valid (min side ≥
/// `lot_width_min`, positive area) and, when `want_street`, both retain frontage.
fn try_split(
    poly: &Polygon2d,
    line: &Line2d,
    settings: &SubdivisionSettings,
    block_edges: &[(DVec2, DVec2)],
    want_street: bool,
) -> Option<(Polygon2d, Polygon2d)> {
    let (a, b) = split_by_line(poly, line);
    let (a, b) = (a?, b?);
    // Both children must be non-degenerate and clear the min width.
    if a.area() < 1e-9 || b.area() < 1e-9 {
        return None;
    }
    if min_side(&a) < settings.lot_width_min || min_side(&b) < settings.lot_width_min {
        return None;
    }
    if want_street && (!touches_boundary(&a, block_edges) || !touches_boundary(&b, block_edges)) {
        return None;
    }
    Some((a, b))
}

fn emit(poly: &Polygon2d, block_edges: &[(DVec2, DVec2)], out: &mut Vec<Lot>) {
    let has_street = touches_boundary(poly, block_edges);
    out.push(Lot {
        polygon: poly.clone(),
        has_street,
    });
}
