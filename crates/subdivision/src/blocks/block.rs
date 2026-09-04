// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! `Block` — a developable polygon carved out of the site by the road network,
//! plus one [`BlockEdge`] per polygon edge carrying its street tag (plan §5).
//! The `edges` are in the same order and winding (CCW) as the polygon, so
//! `edges[i]` runs from `polygon.verts()[i]` to `verts()[(i+1)%n]`.
//!
//! Downstream `lotsubdivide` consumes the plain `polygon`; the tags survive
//! into the bake so a later subdivide can honour `force_street_access`.

use crate::blocks::block_edge::BlockEdge;
use crate::geometry::polygon2d::Polygon2d;
use glam::DVec2;

/// A tagged block: the polygon + its per-edge street tags.
#[derive(Debug, Clone)]
pub struct Block {
    pub polygon: Polygon2d,
    /// One tag per polygon edge, in CCW edge order.
    pub edges: Vec<BlockEdge>,
}

impl Block {
    /// Build a block from a polygon with all edges initially tagged as plain
    /// (non-street) boundary edges. The street tagger overwrites these.
    pub fn untagged(polygon: Polygon2d) -> Block {
        let edges = polygon
            .edges()
            .map(|(a, b)| BlockEdge::boundary(a, b))
            .collect();
        Block { polygon, edges }
    }

    /// Area of the block polygon.
    pub fn area(&self) -> f64 {
        self.polygon.area()
    }

    /// Count of edges tagged `is_street`.
    pub fn street_edge_count(&self) -> usize {
        self.edges.iter().filter(|e| e.is_street).count()
    }

    /// Count of edges tagged `is_alley`.
    pub fn alley_edge_count(&self) -> usize {
        self.edges.iter().filter(|e| e.is_alley).count()
    }

    /// True if any edge is a street edge (block has frontage).
    pub fn has_street(&self) -> bool {
        self.edges.iter().any(|e| e.is_street)
    }

    /// The street ids this block fronts (deduplicated, in first-seen order).
    pub fn fronting_streets(&self) -> Vec<u32> {
        let mut out = Vec::new();
        for e in &self.edges {
            if let Some(id) = e.street_id
                && !out.contains(&id)
            {
                out.push(id);
            }
        }
        out
    }

    /// Centroid of the block polygon.
    pub fn centroid(&self) -> DVec2 {
        self.polygon.centroid()
    }
}
