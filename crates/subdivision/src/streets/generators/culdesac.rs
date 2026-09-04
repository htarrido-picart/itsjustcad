// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Cul-de-sac generator (plan §7.3): one **spine** road down the site long axis
//! plus perpendicular **stubs** spaced by block depth, each terminating in a
//! **bulb** (a turnaround) rather than reaching the far edge. Stubs alternate
//! sides of the spine so blocks form on both flanks.
//!
//! The bulb is emitted as a small closed loop of centerline points at the stub
//! end (radius derived from the ROW width), so block extraction offsets it into
//! the familiar teardrop turnaround.

use crate::geometry::oriented_box::OrientedBox;
use crate::geometry::polygon2d::Polygon2d;
use crate::settings::SubdivisionSettings;
use crate::streets::generators::orthogonal::clip_line_to_polygon;
use crate::streets::generators::{effective_block_depth, effective_road_width};
use crate::geometry::split::Line2d;
use crate::streets::street_graph::{StreetGraph, StreetTier};
use glam::DVec2;

/// Points used to draw a bulb turnaround loop.
const BULB_SEGMENTS: usize = 12;

/// Generate a cul-de-sac street network over `site`.
pub fn generate(site: &Polygon2d, settings: &SubdivisionSettings) -> StreetGraph {
    let mut graph = StreetGraph::new();
    let Some(ob) = OrientedBox::of_polygon(site) else {
        return graph;
    };
    let block_depth = effective_block_depth(settings);
    let width = effective_road_width(settings);

    let long = ob.long_axis;
    let short = ob.short_axis;

    // Spine down the long axis (clipped to the site).
    let spine_line = Line2d::new(ob.center, long);
    if let Some((p0, p1)) = clip_line_to_polygon(site, &spine_line) {
        graph.add(vec![p0, p1], width, StreetTier::Spine);
    }

    // Stub length: reach most of the way to the site edge but stop short so the
    // bulb sits inside a block, not on the boundary.
    let stub_len = (ob.half_short - block_depth * 0.25).max(block_depth * 0.5);
    let bulb_r = (width * 0.9).min(stub_len * 0.4).max(width * 0.5);

    // Stubs spaced by block_depth along the long axis, alternating sides.
    let n = (ob.long_len() / block_depth).floor() as i64;
    for k in 1..n {
        let s = -ob.half_long + k as f64 * block_depth;
        if s.abs() >= ob.half_long {
            continue;
        }
        let base = ob.center + long * s;
        // Alternate side by parity.
        let side = if k % 2 == 0 { 1.0 } else { -1.0 };
        let end = base + short * (side * stub_len);
        // Only keep the stub if its end is inside the site.
        if !site.contains(end) {
            continue;
        }
        // Stub centerline from spine to bulb center.
        graph.add(vec![base, end], width, StreetTier::Stub);
        // Bulb: a closed loop at the stub end.
        let bulb = bulb_loop(end, bulb_r);
        graph.add(bulb, width, StreetTier::Stub);
    }

    graph
}

/// A closed circular loop of centerline points for a turnaround bulb.
fn bulb_loop(center: DVec2, r: f64) -> Vec<DVec2> {
    let mut pts = Vec::with_capacity(BULB_SEGMENTS + 1);
    for i in 0..=BULB_SEGMENTS {
        let a = i as f64 / BULB_SEGMENTS as f64 * std::f64::consts::TAU;
        pts.push(center + DVec2::new(a.cos(), a.sin()) * r);
    }
    pts
}
