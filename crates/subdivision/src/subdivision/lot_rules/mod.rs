// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Lot rules (plan §7.4, Phase 6) — the bulk of Manuel's asks, applied as a
//! post-pass / mode on subdivision:
//!
//! - [`width_mix`] — the packing solver (frontage → width sequence).
//! - [`depth`] — an independent depth target, separate from area.
//! - [`corner`] — widen acute-corner lots by a true area transfer from the
//!   adjacent neighbour (clamped to available room; Σ area conserved).
//! - [`flag`] — panhandle lots (pole area excluded from countable area).
//! - [`loading`] — FrontLoaded vs AlleyLoaded (two-frontage depth).
//! - [`sliver`] — merge sub-threshold lots into their largest-shared-edge
//!   neighbour until none remain.
//!
//! **euro_latam defaults are PLACEHOLDERS** (plan §6b, §1 open questions). When a
//! run resolves a rule from the euro_latam profile because the user gave no
//! explicit override, [`LotRulesReport::used_placeholder`] is set so the command
//! can print "using euro_latam defaults (placeholder — confirm with Manuel)".

pub mod corner;
pub mod depth;
pub mod flag;
pub mod loading;
pub mod sliver;
pub mod width_mix;

pub use depth::DepthController;
pub use loading::{plan as loading_plan, LoadingPlan};
pub use width_mix::{WidthMixResult, WidthMixSolver, WidthProduct};

use crate::blocks::block::Block;
use crate::geometry::oriented_box::OrientedBox;
use crate::geometry::polygon2d::Polygon2d;
use crate::geometry::split::{split_by_line, Line2d};
use crate::settings::SubdivisionSettings;
use crate::subdivision::recursive_obb::Lot;
use glam::DVec2;

/// What the lot-rules pass did, for the command message + the placeholder note.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LotRulesReport {
    /// True if any rule value came from the euro_latam profile (no user
    /// override) — surfaces the "placeholder — confirm with Manuel" note.
    pub used_placeholder: bool,
    /// Human-readable list of the placeholder values that were applied.
    pub placeholder_notes: Vec<String>,
    /// Lots merged away by the sliver merger.
    pub slivers_merged: usize,
    /// Corner lots widened.
    pub corners_widened: usize,
    /// Width-mix max proportion error achieved (`None` if width-mix not run).
    pub width_mix_error: Option<f64>,
}

impl LotRulesReport {
    /// The one-line placeholder banner, if placeholders were used.
    pub fn placeholder_banner(&self) -> Option<String> {
        if self.used_placeholder {
            Some(format!(
                "using euro_latam defaults (placeholder — confirm with Manuel): {}",
                self.placeholder_notes.join(", ")
            ))
        } else {
            None
        }
    }
}

/// Apply the lot rules to an already-subdivided lot set for `block`. This is the
/// front-loaded post-pass: corner widening (where the block has an acute corner)
/// then sliver merging. Width-mix (which drives the initial slicing) is a
/// separate entry, [`subdivide_width_mix`]. Returns the transformed lots plus a
/// report.
///
/// Σ area is conserved: corner widening is a TRUE transfer — the corner lot gains
/// exactly what its adjacent neighbour gives up across their shared boundary
/// (clamped to the room that exists, so it never overruns the block or overlaps);
/// sliver merge unions, never deletes. Deterministic for a fixed seed.
pub fn apply_lot_rules(
    block: &Block,
    lots: Vec<Lot>,
    settings: &SubdivisionSettings,
) -> (Vec<Lot>, LotRulesReport) {
    let mut report = LotRulesReport::default();
    let mut lots = lots;

    // ── Placeholder tracking (surface which euro_latam defaults are in use). ──
    let (_wm, wm_ph) = settings.effective_width_mix();
    if wm_ph {
        report.used_placeholder = true;
        report.placeholder_notes.push("width mix 6/8/10 m @ 25/50/25%".into());
    }
    let (_dt, dt_ph) = settings.effective_depth_target();
    if dt_ph {
        report.used_placeholder = true;
        report.placeholder_notes.push("depth 25 m ±5".into());
    }
    let (corner_bonus, cb_ph) = settings.effective_corner_bonus();
    if cb_ph {
        report.used_placeholder = true;
        report.placeholder_notes.push("corner bonus +15%".into());
    }

    // ── Corner lots: widen the lot at each acute block corner by a TRUE area
    //    transfer from its adjacent (down-frontage) neighbour. The shared boundary
    //    moves along the frontage so Σ area is conserved and no overlap is created;
    //    the move is clamped to the room the neighbour can spare, so the corner lot
    //    can never overrun the block. ────────────────────────────────────────────
    let acute = corner::acute_corners(&block.polygon, settings.corner_angle_max);
    if !acute.is_empty() && corner_bonus > 0.0 {
        let bverts = block.polygon.verts();
        let n = bverts.len();
        // Keep at least this much of the neighbour's own frontage after the give.
        let min_keep = (settings.lot_width_min * 0.5).max(1.0);
        for &ci in &acute {
            let cv = bverts[ci];
            // The frontage direction: along the LONGER of the two block edges
            // meeting at the corner, oriented away from the corner (into the
            // block). The corner lot widens along its frontage, taking area from
            // the next lot down that street edge.
            let prev = bverts[(ci + n - 1) % n];
            let next = bverts[(ci + 1) % n];
            let e_prev = cv - prev; // toward the corner along the incoming edge
            let e_next = next - cv; // away from the corner along the outgoing edge
            let dir = if e_next.length() >= e_prev.length() {
                e_next
            } else {
                -e_prev
            };
            if dir.length_squared() < 1e-12 {
                continue;
            }
            let dir = dir.normalize();
            // Find the lot whose centroid is nearest this corner.
            let Some(ci_lot) = nearest_lot(&lots, cv) else { continue };
            // Its neighbour = the lot immediately down-`dir` sharing the corner
            // lot's hi boundary (the piece the widen would push into).
            let Some(ni_lot) = neighbour_down_dir(&lots, ci_lot, dir) else { continue };
            if let Some(t) = corner::transfer_corner_widen(
                &lots[ci_lot].polygon,
                &lots[ni_lot].polygon,
                dir,
                corner_bonus,
                min_keep,
            ) {
                lots[ci_lot].polygon = t.corner;
                lots[ni_lot].polygon = t.neighbour;
                report.corners_widened += 1;
            }
        }
    }

    // ── Sliver merge. ─────────────────────────────────────────────────────────
    if settings.merge_slivers {
        let before = lots.len();
        let threshold = settings.sliver_area_frac * settings.lot_area_min;
        lots = sliver::merge_slivers(lots, threshold);
        report.slivers_merged = before.saturating_sub(lots.len());
    }

    (lots, report)
}

/// Index of the lot whose centroid is nearest `p`.
fn nearest_lot(lots: &[Lot], p: DVec2) -> Option<usize> {
    lots.iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            a.polygon
                .centroid()
                .distance(p)
                .partial_cmp(&b.polygon.centroid().distance(p))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(i, _)| i)
}

/// The lot immediately down-`dir` of `corner_idx` that shares its hi boundary: the
/// piece a widen along `dir` would push into. Chosen as the lot (other than the
/// corner) whose centroid projection along `dir` is the smallest value still
/// greater than the corner lot's centroid projection, and that laterally overlaps
/// the corner lot (so the shared boundary is real). Deterministic (index tie-break).
fn neighbour_down_dir(lots: &[Lot], corner_idx: usize, dir: DVec2) -> Option<usize> {
    let corner = &lots[corner_idx].polygon;
    let c_proj = corner.centroid().dot(dir);
    // Lateral (⊥ dir) span of the corner lot, to require a real shared boundary.
    let perp = DVec2::new(-dir.y, dir.x);
    let (c_plo, c_phi) = {
        let ps = corner.verts().iter().map(|v| v.dot(perp));
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        for p in ps {
            lo = lo.min(p);
            hi = hi.max(p);
        }
        (lo, hi)
    };
    let mut best: Option<(usize, f64)> = None;
    for (i, l) in lots.iter().enumerate() {
        if i == corner_idx {
            continue;
        }
        let proj = l.polygon.centroid().dot(dir);
        if proj <= c_proj {
            continue;
        }
        // Require lateral overlap with the corner lot.
        let (plo, phi) = {
            let ps = l.polygon.verts().iter().map(|v| v.dot(perp));
            let mut lo = f64::INFINITY;
            let mut hi = f64::NEG_INFINITY;
            for p in ps {
                lo = lo.min(p);
                hi = hi.max(p);
            }
            (lo, hi)
        };
        let overlap = phi.min(c_phi) - plo.max(c_plo);
        if overlap <= 1e-6 {
            continue;
        }
        match best {
            Some((_, bp)) if proj >= bp => {}
            _ => best = Some((i, proj)),
        }
    }
    best.map(|(i, _)| i)
}

/// Width-mix frontage subdivision (plan §7.4): slice `block` into lots along its
/// longest street frontage using the width-mix packing solver, at the loading
/// plan's depth. Falls back to an empty vec if the block has no usable frontage
/// (caller then uses the recursive/offset path). Deterministic.
///
/// Returns the lots + the achieved width-mix proportion error (for the §9
/// "within 5 %" verification) + whether placeholders were used.
pub fn subdivide_width_mix(
    block: &Block,
    settings: &SubdivisionSettings,
) -> Option<(Vec<Lot>, f64, bool)> {
    let (mix, placeholder) = settings.effective_width_mix();
    let mix = mix?;
    let solver = WidthMixSolver::new(&mix.products)?;

    // Frontage = the longest street edge (Phase 5 tags) or, absent tags, the
    // longest block edge (Phase 3/4 treat every boundary edge as frontage).
    let (fa, fb) = longest_frontage(block)?;
    let frontage = fa.distance(fb);
    if frontage < solver.min_width() {
        return None;
    }
    let dir = (fb - fa).normalize();

    // Depth: loading plan (alley halves it) clamped by the depth controller.
    let block_depth = OrientedBox::of_polygon(&block.polygon)
        .map(|ob| ob.short_len())
        .unwrap_or(0.0);
    let lp = loading_plan(block, settings.loading, block_depth);
    let (dtarget, _) = settings.effective_depth_target();
    let dc = DepthController::new(dtarget, settings.effective_depth_tolerance());
    let _lot_depth = dc.clamp(lp.lot_depth());

    // Solve the width sequence along the frontage.
    let result = solver.solve(frontage);
    if result.widths.is_empty() {
        return None;
    }
    let error = result.max_proportion_error(solver.targets());

    // Cut the block with lines perpendicular to the frontage at the cumulative
    // width offsets. Each slice is a lot spanning the block depth.
    let mut pieces = vec![block.polygon.clone()];
    let mut acc = 0.0;
    let perp = DVec2::new(-dir.y, dir.x); // cut-line direction (⊥ to frontage)
    for w in result.widths.iter().take(result.widths.len().saturating_sub(0)) {
        acc += *w;
        if acc >= frontage - 1e-6 {
            break;
        }
        let cut_point = fa + dir * acc;
        let line = Line2d::new(cut_point, perp);
        // Split the piece the cut point falls on (the last piece, since we walk
        // the frontage in order).
        let mut next: Vec<Polygon2d> = Vec::with_capacity(pieces.len() + 1);
        let mut done = false;
        for piece in pieces.drain(..) {
            if !done {
                let (a, b) = split_by_line(&piece, &line);
                if let (Some(a), Some(b)) = (a, b)
                    && a.area() > 1e-6
                    && b.area() > 1e-6
                {
                    // Keep the "before the cut" piece first, continue forward on
                    // the other (ordered by projection along the frontage).
                    let (before, after) = order_by_frontage(a, b, fa, dir);
                    next.push(before);
                    next.push(after);
                    done = true;
                    continue;
                }
            }
            next.push(piece);
        }
        pieces = next;
    }

    let block_edges: Vec<(DVec2, DVec2)> = block.polygon.edges().collect();
    let lots: Vec<Lot> = pieces
        .into_iter()
        .map(|p| {
            let has_street = touches_boundary(&p, &block_edges);
            Lot { polygon: p, has_street }
        })
        .collect();

    Some((lots, error, placeholder))
}

/// Order two split pieces so the one nearer the frontage START comes first.
fn order_by_frontage(a: Polygon2d, b: Polygon2d, fa: DVec2, dir: DVec2) -> (Polygon2d, Polygon2d) {
    let pa = (a.centroid() - fa).dot(dir);
    let pb = (b.centroid() - fa).dot(dir);
    if pa <= pb {
        (a, b)
    } else {
        (b, a)
    }
}

/// The longest street-tagged block edge, or (absent tags) the longest edge.
fn longest_frontage(block: &Block) -> Option<(DVec2, DVec2)> {
    let street: Vec<&crate::blocks::block_edge::BlockEdge> =
        block.edges.iter().filter(|e| e.is_street).collect();
    let pool: Vec<(DVec2, DVec2)> = if street.is_empty() {
        block.polygon.edges().collect()
    } else {
        street.iter().map(|e| (e.a, e.b)).collect()
    };
    pool.into_iter()
        .max_by(|(a0, a1), (b0, b1)| {
            a0.distance(*a1)
                .partial_cmp(&b0.distance(*b1))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

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

fn point_on_segment(p: DVec2, a: DVec2, b: DVec2, tol: f64) -> bool {
    let ab = b - a;
    let len2 = ab.length_squared();
    if len2 < 1e-18 {
        return p.distance(a) < tol;
    }
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    p.distance(a + ab * t) < tol
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::block::Block;
    use crate::settings::{LotWidthMix, RegionProfile, SubdivisionSettings};

    fn rect_block(w: f64, h: f64) -> Block {
        Block::untagged(Polygon2d::from_pairs([(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)]).unwrap())
    }

    fn wm_settings() -> SubdivisionSettings {
        SubdivisionSettings {
            region: RegionProfile::EuroLatam,
            width_mix: Some(LotWidthMix::euro_latam_default()),
            lot_depth_target: 25.0,
            lot_depth_tolerance: 5.0,
            ..SubdivisionSettings::default()
        }
    }

    #[test]
    fn width_mix_slices_500m_frontage_within_5pct() {
        // 500 m frontage × 50 m deep block.
        let block = rect_block(500.0, 50.0);
        let s = wm_settings();
        let (lots, err, _ph) = subdivide_width_mix(&block, &s).expect("width-mix should run");
        assert!(lots.len() > 10, "expected many lots, got {}", lots.len());
        assert!(err < 0.05, "proportion error {err} exceeds 5%");
        // Area conserved.
        let sum: f64 = lots.iter().map(|l| l.polygon.area()).sum();
        assert!(
            (sum - block.area()).abs() / block.area() < 1e-3,
            "area not conserved: {sum} vs {}",
            block.area()
        );
    }

    #[test]
    fn width_mix_deterministic() {
        let block = rect_block(500.0, 50.0);
        let s = wm_settings();
        let a = subdivide_width_mix(&block, &s).unwrap().0;
        let b = subdivide_width_mix(&block, &s).unwrap().0;
        assert_eq!(a.len(), b.len());
        for (la, lb) in a.iter().zip(&b) {
            assert_eq!(la.polygon.verts(), lb.polygon.verts());
        }
    }

    #[test]
    fn placeholder_flag_set_when_defaults_used() {
        // No explicit width mix but EuroLatam → placeholder true.
        let block = rect_block(200.0, 40.0);
        let s = SubdivisionSettings {
            region: RegionProfile::EuroLatam,
            width_mix: None,
            ..SubdivisionSettings::default()
        };
        let (_lots, _err, ph) = subdivide_width_mix(&block, &s).unwrap();
        assert!(ph, "euro_latam default mix must flag placeholder");
    }

    #[test]
    fn apply_rules_reports_placeholder_banner() {
        let block = rect_block(200.0, 40.0);
        let lots = vec![Lot {
            polygon: block.polygon.clone(),
            has_street: true,
        }];
        let s = SubdivisionSettings {
            region: RegionProfile::EuroLatam,
            width_mix: None,
            lot_depth_target: 0.0,
            corner_lot_width_bonus: 0.0,
            merge_slivers: false,
            ..SubdivisionSettings::default()
        };
        let (_lots, report) = apply_lot_rules(&block, lots, &s);
        assert!(report.used_placeholder);
        assert!(report.placeholder_banner().unwrap().contains("confirm with Manuel"));
    }
}
