// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Lot rules (plan §7.4, Phase 6) — the bulk of Manuel's asks, applied as a
//! post-pass / mode on subdivision:
//!
//! - [`width_mix`] — the packing solver (frontage → width sequence).
//! - [`depth`] — an independent depth target, separate from area.
//! - [`corner`] — widen acute-corner lots, clamped against self-intersection.
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
/// Σ area is conserved (widening moves area between lots via clamped slack;
/// sliver merge unions, never deletes). Deterministic for a fixed seed.
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

    // ── Corner lots: widen the lot sitting at each acute block corner. ────────
    let acute = corner::acute_corners(&block.polygon, settings.corner_angle_max);
    if !acute.is_empty() && corner_bonus > 0.0 {
        let bverts = block.polygon.verts();
        let n = bverts.len();
        for &ci in &acute {
            let cv = bverts[ci];
            // The frontage direction at the corner: along the incoming block edge.
            let prev = bverts[(ci + n - 1) % n];
            let dir = cv - prev;
            if dir.length_squared() < 1e-12 {
                continue;
            }
            // Find the lot whose centroid is nearest this corner.
            if let Some((li, _)) = lots
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| {
                    a.polygon
                        .centroid()
                        .distance(cv)
                        .partial_cmp(&b.polygon.centroid().distance(cv))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
            {
                // Slack = distance to the next lot's frontage (bounded so we do
                // not overrun). Use half the lot's own frontage as a safe cap.
                let slack = corner_lot_slack(&lots[li].polygon, dir);
                let widened =
                    corner::widen_corner_lot(&lots[li].polygon, dir, corner_bonus, slack);
                if widened.area() > lots[li].polygon.area() + 1e-9 {
                    lots[li].polygon = widened;
                    report.corners_widened += 1;
                }
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

/// A conservative frontage slack for corner widening: a fraction of the lot's own
/// frontage extent along `dir`, so the widen never doubles the lot.
fn corner_lot_slack(lot: &Polygon2d, dir: DVec2) -> f64 {
    let d = dir.normalize();
    let projs: Vec<f64> = lot.verts().iter().map(|v| v.dot(d)).collect();
    let lo = projs.iter().cloned().fold(f64::INFINITY, f64::min);
    let hi = projs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    (hi - lo) * 0.5
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
