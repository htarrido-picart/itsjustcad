// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! `BlockEdge` — one edge of a block polygon carrying its street tag (plan §5,
//! the **hard dependency**). Every block edge records whether it lies on a
//! generated street ROW and, if so, which street (id + width + length). An
//! original-boundary edge that is not a street has `is_street == false`. The
//! rear-lane (alley) tier tags its edges `is_alley == true`.
//!
//! Skeleton subdivision, corner-lot assignment, front/alley loading, and
//! frontage measurement (all later phases) are unimplementable without this,
//! so it is built now and carried through the bake so a downstream
//! `lotsubdivide` on a generated block sees the same tags.

use glam::DVec2;

/// The street-adjacency tag of a single block edge (from vertex `a` to `b`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockEdge {
    /// Edge start vertex (matches the block polygon winding, CCW).
    pub a: DVec2,
    /// Edge end vertex.
    pub b: DVec2,
    /// True if this edge is coincident with a generated street ROW boundary.
    pub is_street: bool,
    /// The generating street's id when `is_street`; `None` for boundary edges.
    pub street_id: Option<u32>,
    /// The generating street's ROW width when `is_street`; `0.0` otherwise.
    pub street_width: f64,
    /// The generating street centerline length when `is_street`; `0.0` otherwise.
    pub street_length: f64,
    /// True if this edge is a rear-lane (alley) edge (§ alley tier).
    pub is_alley: bool,
}

impl BlockEdge {
    /// A plain (non-street) boundary edge.
    pub fn boundary(a: DVec2, b: DVec2) -> BlockEdge {
        BlockEdge {
            a,
            b,
            is_street: false,
            street_id: None,
            street_width: 0.0,
            street_length: 0.0,
            is_alley: false,
        }
    }

    /// Length of the edge.
    pub fn length(&self) -> f64 {
        self.a.distance(self.b)
    }

    /// Midpoint of the edge.
    pub fn midpoint(&self) -> DVec2 {
        (self.a + self.b) * 0.5
    }
}
