// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Orthogonal generator (plan §7.3): recursive minimum-area-OBB split of the
//! *site* down to ~block depth. At each split we emit the split line — clipped
//! to the current sub-polygon — as a **spine** street centerline, then recurse
//! on both halves while the short OBB extent still exceeds `block_depth`.
//!
//! The result is a rectilinear grid of centerlines aligned to the site's
//! dominant axis. Block extraction (`block_extractor`) turns the streets +
//! site into tagged blocks.

use crate::geometry::oriented_box::OrientedBox;
use crate::geometry::polygon2d::Polygon2d;
use crate::geometry::split::{split_by_line, Line2d};
use crate::settings::SubdivisionSettings;
use crate::streets::generators::{effective_block_depth, effective_road_width};
use crate::streets::street_graph::{StreetGraph, StreetTier};
use glam::DVec2;

/// Recursion depth guard (2^32 splits is far past any real site).
const MAX_DEPTH: u32 = 32;

/// Clip the infinite `line` to `poly`, returning the two intersection points of
/// the line with the polygon boundary (the centerline segment inside the poly).
/// Returns `None` if the line does not cross the interior (fewer than 2 hits).
pub(crate) fn clip_line_to_polygon(poly: &Polygon2d, line: &Line2d) -> Option<(DVec2, DVec2)> {
    let mut hits: Vec<DVec2> = Vec::new();
    for (a, b) in poly.edges() {
        let da = line.signed(a);
        let db = line.signed(b);
        // Edge crosses the line if the endpoints are on opposite sides.
        if (da > 0.0) != (db > 0.0) {
            let denom = da - db;
            if denom.abs() < 1e-15 {
                continue;
            }
            let t = da / denom;
            if (0.0..=1.0).contains(&t) {
                hits.push(a + (b - a) * t);
            }
        }
    }
    if hits.len() < 2 {
        return None;
    }
    // Sort along the line direction and take the extreme pair (spans the poly).
    hits.sort_by(|p, q| {
        line.dir
            .dot(*p)
            .partial_cmp(&line.dir.dot(*q))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let first = *hits.first().unwrap();
    let last = *hits.last().unwrap();
    if first.distance(last) < 1e-9 {
        return None;
    }
    Some((first, last))
}

/// Generate an orthogonal street grid over `site`.
pub fn generate(site: &Polygon2d, settings: &SubdivisionSettings) -> StreetGraph {
    let mut graph = StreetGraph::new();
    let depth = effective_block_depth(settings);
    let width = effective_road_width(settings);
    recurse(site, depth, width, None, &mut graph, 0);
    graph
}

/// `rot`: optional global rotation (radians) applied to split directions — used
/// by the skewed generator, which shares this recursion.
pub(crate) fn recurse(
    poly: &Polygon2d,
    block_depth: f64,
    road_width: f64,
    rot: Option<f64>,
    graph: &mut StreetGraph,
    depth: u32,
) {
    if depth >= MAX_DEPTH {
        return;
    }
    let Some(ob) = OrientedBox::of_polygon(poly) else {
        return;
    };
    // Stop when the long extent is at/below 2× block depth: a further split
    // would create sub-block-depth strips. (We split the LONG dimension.)
    if ob.long_len() < block_depth * 2.0 {
        return;
    }

    // Split direction: the OBB short axis (the cut plane runs across the short
    // direction, halving the long dimension), optionally globally rotated.
    let short_axis = match rot {
        Some(theta) => {
            let (s, c) = theta.sin_cos();
            DVec2::new(
                ob.short_axis.x * c - ob.short_axis.y * s,
                ob.short_axis.x * s + ob.short_axis.y * c,
            )
        }
        None => ob.short_axis,
    };
    let line = Line2d::new(ob.center, short_axis);

    // Emit the split line, clipped to this sub-polygon, as a spine centerline.
    if let Some((p0, p1)) = clip_line_to_polygon(poly, &line) {
        graph.add(vec![p0, p1], road_width, StreetTier::Spine);
    }

    let (a, b) = split_by_line(poly, &line);
    if let Some(a) = a {
        recurse(&a, block_depth, road_width, rot, graph, depth + 1);
    }
    if let Some(b) = b {
        recurse(&b, block_depth, road_width, rot, graph, depth + 1);
    }
}
