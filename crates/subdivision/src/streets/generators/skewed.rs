// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Skewed generator (plan §7.3): identical to [`orthogonal`](super::orthogonal)
//! but with a global rotation applied to the split directions, giving a
//! diagonal / rotated grid. The rotation is a fixed diagonal offset (a quarter
//! of a right angle), so the graph stays deterministic for a fixed seed — the
//! skew is a property of the pattern, not random.

use crate::geometry::polygon2d::Polygon2d;
use crate::settings::SubdivisionSettings;
use crate::streets::generators::{effective_block_depth, effective_road_width, orthogonal};
use crate::streets::street_graph::StreetGraph;
use std::f64::consts::FRAC_PI_8;

/// The global skew applied to the grid (22.5°) — enough to read as diagonal
/// without collapsing block shapes.
pub const SKEW_ANGLE: f64 = FRAC_PI_8;

/// Generate a skewed street grid over `site`.
pub fn generate(site: &Polygon2d, settings: &SubdivisionSettings) -> StreetGraph {
    let mut graph = StreetGraph::new();
    let depth = effective_block_depth(settings);
    let width = effective_road_width(settings);
    orthogonal::recurse(site, depth, width, Some(SKEW_ANGLE), &mut graph, 0);
    graph
}
