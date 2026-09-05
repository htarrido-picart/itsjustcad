// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Straight-skeleton interface + the Phase-7 approximate implementation.
//!
//! The plan (§5) builds the skeleton in two stages behind ONE trait:
//! - **Phase 7 (now):** [`offset_approx::OffsetApproxSkeleton`] — iterated small
//!   inward insets (`clip_bridge::offset`, i_overlay) with topology tracking, so
//!   the medial ridge is approximated to survey tolerance. Robust and fast; good
//!   at the shapes real parcels take.
//! - **Phase 12 (later):** a true Felkel & Obdržálek priority-queue skeleton
//!   (`felkel.rs`), swapped in behind the SAME [`StraightSkeleton`] trait. NOT
//!   built now — the trait is defined so it can slot in without touching callers.
//!
//! Output: one [`SkeletonFace`] per contour edge — the region of the block that
//! is "closest" (in the offset sense) to that edge. Skeleton subdivision
//! (`skeleton_sub`) groups these faces by street and slices them perpendicular
//! to their street edge.

pub mod felkel;
pub mod offset_approx;

pub use felkel::FelkelSkeleton;
pub use offset_approx::OffsetApproxSkeleton;

use crate::geometry::polygon2d::Polygon2d;
use glam::DVec2;

/// One face of the straight skeleton: the region of the block associated with a
/// single contour (boundary) edge. `base_a`/`base_b` are that edge's endpoints
/// (on the original contour, CCW), so the face's "outward" side is the segment
/// `base_a → base_b` and its lot lines run perpendicular to it.
#[derive(Debug, Clone)]
pub struct SkeletonFace {
    /// The polygon of this face (a wedge/trapezoid from the base edge inward to
    /// the medial ridge).
    pub polygon: Polygon2d,
    /// Index of the contour edge this face belongs to (edge `i` runs from
    /// `contour.verts()[i]` to `verts()[(i+1)%n]`).
    pub edge_index: usize,
    /// The base (contour) edge endpoints, in CCW order.
    pub base_a: DVec2,
    pub base_b: DVec2,
}

impl SkeletonFace {
    /// Outward-facing base edge direction (unit), `base_a → base_b`.
    pub fn base_dir(&self) -> DVec2 {
        let d = self.base_b - self.base_a;
        if d.length_squared() < 1e-18 {
            DVec2::X
        } else {
            d.normalize()
        }
    }

    /// Inward normal of the base edge (points into the block interior). For a CCW
    /// contour the interior is to the LEFT of `base_a → base_b`, i.e. the left
    /// normal `(-dir.y, dir.x)`.
    pub fn inward_normal(&self) -> DVec2 {
        let d = self.base_dir();
        DVec2::new(-d.y, d.x)
    }

    /// Length of the base (street-side) edge.
    pub fn base_len(&self) -> f64 {
        self.base_a.distance(self.base_b)
    }
}

/// The straight-skeleton interface. Phase 7 ships one implementor
/// ([`offset_approx::OffsetApproxSkeleton`]); Phase 12's Felkel skeleton slots in
/// behind the same trait.
pub trait StraightSkeleton {
    /// Compute the skeleton faces of `poly` — one face per contour edge (in
    /// contour edge order, `edge_index == i`). An implementation may return fewer
    /// faces than edges if a degenerate edge produces no interior region, but the
    /// union of the returned faces must cover the polygon to tolerance.
    fn faces(&self, poly: &Polygon2d) -> Vec<SkeletonFace>;
}
