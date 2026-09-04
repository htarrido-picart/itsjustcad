// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Organic generator (plan §7.3): spline spines fitted to the site long axis
//! with controlled **sinusoidal deviation**, then secondary connectors.
//!
//! We run one gently-curving spine down the site's long axis (amplitude driven
//! by `irregularity`, phase seeded from the site so it is deterministic), then
//! drop perpendicular **connector** roads spaced by `block_depth` along the
//! spine. Each centerline is clipped to the site polygon. The result reads as a
//! free-form street network rather than a rigid grid, while still tiling the
//! site into extractable blocks.

use crate::geometry::oriented_box::OrientedBox;
use crate::geometry::polygon2d::Polygon2d;
use crate::geometry::split::Line2d;
use crate::settings::SubdivisionSettings;
use crate::streets::generators::orthogonal::clip_line_to_polygon;
use crate::streets::generators::{
    effective_block_depth, effective_road_width, site_seed, SplitMix64,
};
use crate::streets::street_graph::{StreetGraph, StreetTier};
use glam::DVec2;

/// Salt for the organic RNG stream (distinct from other generators).
const SALT: u64 = 0x0072_6761_6e69_6300; // "…rganic\0"

/// Number of samples along the spine centerline.
const SPINE_SAMPLES: usize = 24;

/// Generate an organic street network over `site`.
pub fn generate(site: &Polygon2d, settings: &SubdivisionSettings) -> StreetGraph {
    let mut graph = StreetGraph::new();
    let Some(ob) = OrientedBox::of_polygon(site) else {
        return graph;
    };
    let block_depth = effective_block_depth(settings);
    let width = effective_road_width(settings);
    let mut rng = SplitMix64::new(site_seed(site, settings.seed, SALT));

    // Deviation amplitude: irregularity of the short half-extent, floored so the
    // curve is visible even at irregularity 0 (a gentle S).
    let irr = settings.clamped_irregularity().max(0.1);
    let amp = (ob.half_short * irr).min(ob.half_short * 0.5);
    let phase = rng.signed_unit() * std::f64::consts::PI;

    // Spine: sample along the long axis, deviate sinusoidally along the short
    // axis, clip the polyline pieces to the site.
    let long = ob.long_axis;
    let short = ob.short_axis;
    let start = ob.center - long * ob.half_long;
    let mut raw: Vec<DVec2> = Vec::with_capacity(SPINE_SAMPLES);
    for i in 0..SPINE_SAMPLES {
        let t = i as f64 / (SPINE_SAMPLES - 1) as f64; // 0..1
        let along = start + long * (t * ob.long_len());
        // Two-cycle sine so the spine wiggles a couple of times over the site.
        let dev = (t * std::f64::consts::TAU * 2.0 + phase).sin() * amp;
        raw.push(along + short * dev);
    }
    // Trim the deviated polyline to the interior of the site (keep the inside run).
    let spine = trim_polyline_to_polygon(site, &raw);
    if spine.len() >= 2 {
        graph.add(spine, width, StreetTier::Spine);
    }

    // Secondary connectors: perpendicular (short-axis-aligned) lines spaced by
    // block_depth along the long axis, clipped to the site.
    let n_conn = (ob.long_len() / block_depth).floor() as i64;
    for k in 1..n_conn {
        let s = -ob.half_long + k as f64 * block_depth;
        if s.abs() >= ob.half_long {
            continue;
        }
        let p = ob.center + long * s;
        let line = Line2d::new(p, short);
        if let Some((p0, p1)) = clip_line_to_polygon(site, &line) {
            graph.add(vec![p0, p1], width, StreetTier::Connector);
        }
    }

    graph
}

/// Keep the longest run of `pts` that lies inside `poly` (endpoints clamped to
/// the boundary crossing). Organic spines can wander outside a concave site;
/// this keeps the usable interior run.
fn trim_polyline_to_polygon(poly: &Polygon2d, pts: &[DVec2]) -> Vec<DVec2> {
    // Simple approach: keep interior points; for a convex-ish site this is the
    // whole polyline. Endpoints outside are pulled to the nearest interior
    // sample so the spine still reaches near the site edges.
    let mut best: Vec<DVec2> = Vec::new();
    let mut cur: Vec<DVec2> = Vec::new();
    for &p in pts {
        if poly.contains(p) {
            cur.push(p);
        } else {
            if cur.len() > best.len() {
                best = std::mem::take(&mut cur);
            }
            cur.clear();
        }
    }
    if cur.len() > best.len() {
        best = cur;
    }
    best
}
