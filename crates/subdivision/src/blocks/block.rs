// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! `Block` — a developable polygon carved out of the site by the road network,
//! plus one [`BlockEdge`] per polygon edge carrying its street tag.
//! The `edges` are in the same order and winding (CCW) as the polygon, so
//! `edges[i]` runs from `polygon.verts()[i]` to `verts()[(i+1)%n]`.
//!
//! The tags are carried in memory through `siteplan` / `lotgeneratesite` into
//! subdivision, setbacks and frontage (lots inherit them via [`Block::inherit`]).
//! They are NOT stored on the baked curves: a later `lotsubdivide` /
//! `lotsetbacks` on a baked block or lot curve sees an untagged polygon.

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

    /// Tag a lot `polygon` cut from `parent`: each lot edge lying on a parent
    /// edge inherits that edge's tag (street id, width, alley); cut edges
    /// interior to the parent stay plain boundary edges.
    pub fn inherit(polygon: Polygon2d, parent: &Block) -> Block {
        let edges = polygon
            .edges()
            .map(|(a, b)| {
                parent
                    .edges
                    .iter()
                    .find(|e| on_segment(a, e.a, e.b) && on_segment(b, e.a, e.b))
                    .map_or(BlockEdge::boundary(a, b), |e| BlockEdge { a, b, ..*e })
            })
            .collect();
        Block { polygon, edges }
    }

    /// The edges that count as street frontage (`has_street`,
    /// `force_street_access`): the street-tagged edges when the block carries
    /// road-network tags, else every edge (an untagged block treats its whole
    /// boundary as frontage).
    pub fn frontage_edges(&self) -> Vec<(DVec2, DVec2)> {
        if self.has_street() {
            self.edges
                .iter()
                .filter(|e| e.is_street)
                .map(|e| (e.a, e.b))
                .collect()
        } else {
            self.polygon.edges().collect()
        }
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

/// Is `p` within 1e-4 of the segment `a→b`? (Same tolerance as the
/// subdividers' frontage test.)
fn on_segment(p: DVec2, a: DVec2, b: DVec2) -> bool {
    let ab = b - a;
    let len2 = ab.length_squared();
    if len2 < 1e-18 {
        return p.distance(a) < 1e-4;
    }
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    p.distance(a + ab * t) < 1e-4
}
