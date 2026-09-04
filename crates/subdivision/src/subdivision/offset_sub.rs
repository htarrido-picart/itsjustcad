// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Offset / perimeter subdivision (plan §7.2, Phase 4) — `method=perimeter`.
//!
//! Inward-offset the block by `offset_width` to get an interior **core**; the
//! ring between the block boundary and the core is the **perimeter strip**. Walk
//! the block boundary, sampling at a spacing derived from the target lot area /
//! depth (jittered by `irregularity`, seeded-deterministic like `recursive_obb`),
//! and cut the block with lines **orthogonal to the boundary** at each sample.
//! Each resulting wedge, intersected with the strip, is a perimeter lot. When
//! `subdivide_core` is set, the interior core is subdivided by `recursive_obb`;
//! otherwise it stays a single hollow-ring interior lot.
//!
//! **Degenerate fallbacks (plan §9 Phase 4):** if `offset_width ≈ 0`, or the
//! inward offset collapses / empties (a deep inset on a small or thin block), we
//! fall back to plain `recursive_obb` on the whole block. Never panic, never emit
//! garbage. Area is conserved either way.
//!
//! Determinism: all jitter comes from a splitmix64 stream seeded from a quantized
//! block hash + `settings.seed`, identical to `recursive_obb` — same input →
//! byte-identical output, so op-log replay is stable.

use crate::geometry::clip_bridge;
use crate::geometry::oriented_box::OrientedBox;
use crate::geometry::polygon2d::Polygon2d;
use crate::geometry::polyline::PolylineTools;
use crate::geometry::split::{split_by_line, Line2d};
use crate::settings::SubdivisionSettings;
use crate::subdivision::recursive_obb::{self, Lot};
use glam::DVec2;

/// Below this the `offset_width` counts as "≈ 0" → straight to the fallback.
const OFFSET_EPS: f64 = 1e-6;

/// A deterministic splitmix64 stream (same construction as `recursive_obb`).
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
/// the mm grid so f64 noise never perturbs the seed (replay stability). Salted
/// differently from `recursive_obb` so the two methods do not share a stream.
fn block_seed(block: &Polygon2d, seed: u64) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ seed ^ 0x0ff5_e700_0ff5_e700;
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
    h | 1
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

/// Whether `lot` touches the original block boundary (its Phase-4 street edge —
/// like Phase 3, any original block boundary edge counts as frontage).
fn touches_boundary(lot: &Polygon2d, block_edges: &[(DVec2, DVec2)]) -> bool {
    for (a, b) in lot.edges() {
        let mid = (a + b) * 0.5;
        for &(ea, eb) in block_edges {
            if point_on_segment(mid, ea, eb, 1e-4) {
                return true;
            }
        }
    }
    false
}

/// Public entry: subdivide `block` into perimeter lots (+ optional core lots).
/// Deterministic for a fixed `settings.seed`. Falls back to `recursive_obb` on
/// any degenerate offset.
pub fn subdivide(block: &Polygon2d, settings: &SubdivisionSettings) -> Vec<Lot> {
    let width = settings.offset_width;

    // Fallback 1: offset_width ≈ 0 → nothing to inset, plain recursive OBB.
    if width.abs() < OFFSET_EPS {
        return recursive_obb::subdivide(block, settings);
    }

    // Inward offset (negative delta shrinks). If it collapses to empty or a
    // degenerate ring, the inset is deeper than the block is wide → fallback.
    let core = pick_core(block, width);
    let Some(core) = core else {
        return recursive_obb::subdivide(block, settings);
    };

    // The perimeter strip = block − core. If the difference is empty or the strip
    // is essentially the whole block (core vanished), fall back.
    let strip_area = block.area() - core.area();
    if core.area() < 1e-9 || strip_area < 1e-9 {
        return recursive_obb::subdivide(block, settings);
    }

    let block_edges: Vec<(DVec2, DVec2)> = block.edges().collect();
    let mut rng = SplitMix64::new(block_seed(block, settings.seed));
    let irregularity = settings.clamped_irregularity();

    let mut out: Vec<Lot> = Vec::new();

    // ── Perimeter lots: cut the block into wedges with orthogonal-to-boundary
    // lines at ring samples, then keep each wedge ∩ strip. ─────────────────────
    let wedges = cut_into_wedges(block, settings, irregularity, &mut rng);
    for wedge in &wedges {
        // The perimeter portion of this wedge is what lies OUTSIDE the core.
        for peri in clip_bridge::difference(wedge, &core) {
            if peri.area() < 1e-6 {
                continue;
            }
            let has_street = touches_boundary(&peri, &block_edges);
            out.push(Lot { polygon: peri, has_street });
        }
    }

    // ── Core: subdivide with recursive OBB, or keep as one hollow-ring lot. ────
    if settings.subdivide_core {
        // Run recursive OBB on the core polygon. Its lots do not touch the block
        // boundary (they are interior), so `has_street` is computed against the
        // original block edges and will generally be false — correct for an
        // interior core.
        let core_settings = SubdivisionSettings {
            method: crate::settings::SubdivisionMethod::Recursive,
            ..settings.clone()
        };
        for lot in recursive_obb::subdivide(&core, &core_settings) {
            let has_street = touches_boundary(&lot.polygon, &block_edges);
            out.push(Lot { polygon: lot.polygon, has_street });
        }
    } else {
        let has_street = touches_boundary(&core, &block_edges);
        out.push(Lot { polygon: core, has_street });
    }

    // Safety net: if for any reason the wedge cutting produced nothing usable,
    // fall back rather than emit an under-covered block.
    let covered: f64 = out.iter().map(|l| l.polygon.area()).sum();
    if out.is_empty() || (block.area() - covered).abs() / block.area() > 1e-3 {
        return recursive_obb::subdivide(block, settings);
    }

    out
}

/// Inward-offset `block` by `width` and pick the core ring. `clip_bridge::offset`
/// with a negative delta insets; a collapsed inset yields an empty vec. When the
/// inset produces several islands (a concave block can split), keep the largest.
fn pick_core(block: &Polygon2d, width: f64) -> Option<Polygon2d> {
    let mut rings = clip_bridge::offset(block, -width);
    rings.retain(|r| r.area() > 1e-9);
    rings.into_iter().max_by(|a, b| {
        a.area()
            .partial_cmp(&b.area())
            .unwrap_or(std::cmp::Ordering::Equal)
    })
}

/// Cut the whole block into wedges using lines orthogonal to the boundary at
/// samples spaced by the target perimeter-lot width. Returns the wedge polygons
/// (their union == the block). Falls back to returning the whole block as one
/// wedge if the boundary is too short to sample.
fn cut_into_wedges(
    block: &Polygon2d,
    settings: &SubdivisionSettings,
    irregularity: f64,
    rng: &mut SplitMix64,
) -> Vec<Polygon2d> {
    let boundary: Vec<DVec2> = block.verts().to_vec();
    // Closed polyline for arc-length work (repeat the first point at the end).
    let mut closed = boundary.clone();
    closed.push(boundary[0]);
    let perim = PolylineTools::arc_length(&closed);

    // Target frontage per perimeter lot: derive from area/depth if available,
    // else from lot_width_min. Depth of the strip is offset_width.
    let depth = settings.offset_width.max(1.0);
    let target_width = if settings.lot_area_min > 0.0 {
        (settings.lot_area_min / depth).max(settings.lot_width_min)
    } else {
        settings.lot_width_min.max(1.0)
    };

    let n_cuts = (perim / target_width).floor() as i64;
    if n_cuts < 2 || perim < 1e-6 {
        // Too short to slice — one wedge = the whole block.
        return vec![block.clone()];
    }

    // Sample parameters around the boundary, jittered. The cut line at each sample
    // passes through the sample point along the boundary NORMAL (i.e. orthogonal
    // to the boundary tangent), so it drives inward toward the core.
    let mut pieces: Vec<Polygon2d> = vec![block.clone()];
    let n = n_cuts as usize;
    // Compute jittered sample fractions BEFORE cutting (seed-before-use).
    let mut samples: Vec<f64> = Vec::with_capacity(n);
    for i in 0..n {
        let base = i as f64 / n as f64;
        // Jitter within ±(irregularity * half the spacing) so cuts don't cross.
        let jitter = rng.signed_unit() * irregularity * (0.45 / n as f64);
        samples.push((base + jitter).rem_euclid(1.0));
    }

    // Apply each cut. A cut with an infinite line divides EVERY current piece it
    // crosses; we only want to divide the piece the sample point lies on, so we
    // clip against the piece containing that boundary point.
    for &frac in &samples {
        let pt = PolylineTools::point_at(&closed, frac);
        let tan = PolylineTools::tangent_at(&closed, frac);
        if tan.length_squared() < 1e-18 {
            continue;
        }
        // The cut line runs along the inward normal: direction = normal to the
        // boundary tangent. `split_by_line` takes a point + a direction along the
        // line, so the line direction IS the normal.
        let normal = DVec2::new(-tan.y, tan.x);
        let line = Line2d::new(pt, normal);

        // Find the piece whose boundary the sample sits on and split just that.
        let mut next: Vec<Polygon2d> = Vec::with_capacity(pieces.len() + 1);
        let mut cut_done = false;
        for piece in pieces.drain(..) {
            if !cut_done && on_piece_boundary(&piece, pt) {
                let (a, b) = split_by_line(&piece, &line);
                match (a, b) {
                    (Some(a), Some(b)) if a.area() > 1e-6 && b.area() > 1e-6 => {
                        next.push(a);
                        next.push(b);
                        cut_done = true;
                    }
                    _ => next.push(piece),
                }
            } else {
                next.push(piece);
            }
        }
        pieces = next;
    }

    pieces
}

/// Is `pt` on (within tolerance of) any edge of `piece`?
fn on_piece_boundary(piece: &Polygon2d, pt: DVec2) -> bool {
    let tol = (OrientedBox::of_polygon(piece).map(|o| o.short_len()).unwrap_or(1.0) * 0.02).max(1e-3);
    for (a, b) in piece.edges() {
        if point_on_segment(pt, a, b, tol) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::SubdivisionMethod;

    fn rect(w: f64, h: f64) -> Polygon2d {
        Polygon2d::from_pairs([(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)]).unwrap()
    }

    fn base_settings() -> SubdivisionSettings {
        SubdivisionSettings {
            method: SubdivisionMethod::Offset,
            offset_width: 30.0,
            subdivide_core: true,
            lot_area_min: 4000.0,
            lot_width_min: 20.0,
            force_street_access: 0.0,
            irregularity: 0.0,
            seed: 7,
            ..SubdivisionSettings::default()
        }
    }

    fn area_conserved(block: &Polygon2d, lots: &[Lot]) {
        let sum: f64 = lots.iter().map(|l| l.polygon.area()).sum();
        let rel = (sum - block.area()).abs() / block.area();
        assert!(rel < 1e-3, "area not conserved: Σ {sum} vs {}", block.area());
    }

    #[test]
    fn offset_conserves_area_on_big_rect() {
        let block = rect(400.0, 300.0);
        let s = base_settings();
        let lots = subdivide(&block, &s);
        assert!(lots.len() > 1);
        area_conserved(&block, &lots);
    }

    #[test]
    fn zero_offset_falls_back_to_recursive() {
        let block = rect(400.0, 300.0);
        let mut s = base_settings();
        s.offset_width = 0.0;
        let via_offset = subdivide(&block, &s);
        let mut rec = s.clone();
        rec.method = SubdivisionMethod::Recursive;
        let via_rec = recursive_obb::subdivide(&block, &rec);
        assert_eq!(via_offset.len(), via_rec.len());
        area_conserved(&block, &via_offset);
    }

    #[test]
    fn huge_offset_collapses_falls_back_no_panic() {
        let block = rect(400.0, 300.0);
        let mut s = base_settings();
        s.offset_width = 500.0; // far larger than the block half-extent
        let lots = subdivide(&block, &s);
        assert!(!lots.is_empty());
        area_conserved(&block, &lots);
    }

    #[test]
    fn thin_rectangle_inset_collapses_falls_back() {
        // Case #1-like thin block: a 30 m inset collapses the 120 m-tall block’s
        // interior; must fall back cleanly.
        let block = rect(400.0, 40.0);
        let mut s = base_settings();
        s.offset_width = 30.0;
        let lots = subdivide(&block, &s);
        assert!(!lots.is_empty());
        area_conserved(&block, &lots);
    }

    #[test]
    fn subdivide_core_on_vs_off() {
        let block = rect(400.0, 300.0);
        let mut on = base_settings();
        on.subdivide_core = true;
        let mut off = base_settings();
        off.subdivide_core = false;
        let lots_on = subdivide(&block, &on);
        let lots_off = subdivide(&block, &off);
        area_conserved(&block, &lots_on);
        area_conserved(&block, &lots_off);
        // Core-on produces MORE lots (interior gets carved up) than core-off
        // (which keeps a single hollow-ring interior lot).
        assert!(
            lots_on.len() > lots_off.len(),
            "core-on {} should exceed core-off {}",
            lots_on.len(),
            lots_off.len()
        );
    }

    #[test]
    fn deterministic_for_fixed_seed() {
        let block = rect(500.0, 350.0);
        let mut s = base_settings();
        s.irregularity = 0.3;
        let a = subdivide(&block, &s);
        let b = subdivide(&block, &s);
        assert_eq!(a.len(), b.len());
        for (la, lb) in a.iter().zip(&b) {
            assert_eq!(la.polygon.verts(), lb.polygon.verts());
        }
    }
}
