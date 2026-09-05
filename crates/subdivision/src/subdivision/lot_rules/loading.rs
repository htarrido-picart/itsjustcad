// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! `LoadingStrategy` (plan §7.4, Phase 6) — FrontLoaded (euro_latam default) vs
//! AlleyLoaded. Front-loaded lots front a street and back onto the interior of
//! the block. Alley-loaded lots front a street on one side and an ALLEY on the
//! other: a two-frontage arrangement, so each lot's depth runs street→alley (the
//! block half-depth), which requires the alley to already be in the street graph
//! (Phase 5's alley tier tags block edges `is_alley`).
//!
//! Uses the block-edge `is_street` / `is_alley` tags. This module reports the
//! per-side depth a downstream slicer should use; it does not itself cut lots.

use crate::blocks::block::Block;
use crate::settings::LoadingType;

/// The frontage geometry a block presents under a loading strategy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoadingPlan {
    pub loading: LoadingType,
    /// True when the block carries an alley edge (required for AlleyLoaded).
    pub has_alley: bool,
    /// The lot depth street→rear for a FRONT-loaded row (full block depth from a
    /// street edge to the opposite side).
    pub front_depth: f64,
    /// The lot depth street→alley for an ALLEY-loaded row (half the block, from a
    /// street edge to the central alley). 0 when no alley is present.
    pub alley_depth: f64,
}

/// Compute the loading plan for `block` under `loading`. `block_depth` is the
/// full front-to-rear depth of the block (its OBB short extent, typically).
pub fn plan(block: &Block, loading: LoadingType, block_depth: f64) -> LoadingPlan {
    let has_alley = block.alley_edge_count() > 0;
    let effective = match loading {
        // AlleyLoaded only takes effect if the alley tier is present; otherwise
        // it degrades gracefully to front-loading (plan: requires the alley in
        // the street graph).
        LoadingType::AlleyLoaded if has_alley => LoadingType::AlleyLoaded,
        LoadingType::AlleyLoaded => LoadingType::FrontLoaded,
        other => other,
    };
    // Under alley loading, the two rows share the block, so each row's depth is
    // half the block (street to central alley). Front loading uses the whole
    // depth (a single-loaded strip) unless the block is naturally double-loaded.
    let alley_depth = if has_alley { block_depth * 0.5 } else { 0.0 };
    LoadingPlan {
        loading: effective,
        has_alley,
        front_depth: block_depth,
        alley_depth,
    }
}

impl LoadingPlan {
    /// The depth a lot slicer should use for this block: alley half-depth when
    /// alley-loaded, else the full front depth.
    pub fn lot_depth(&self) -> f64 {
        match self.loading {
            LoadingType::AlleyLoaded if self.has_alley => self.alley_depth,
            _ => self.front_depth,
        }
    }

    /// True when lots are two-frontage (street + alley).
    pub fn is_two_frontage(&self) -> bool {
        matches!(self.loading, LoadingType::AlleyLoaded) && self.has_alley
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::block::Block;
    use crate::blocks::block_edge::BlockEdge;
    use crate::geometry::polygon2d::Polygon2d;

    fn block_with_alley(alley: bool) -> Block {
        let poly = Polygon2d::from_pairs([(0.0, 0.0), (100.0, 0.0), (100.0, 50.0), (0.0, 50.0)])
            .unwrap();
        let mut edges: Vec<BlockEdge> = poly.edges().map(|(a, b)| BlockEdge::boundary(a, b)).collect();
        // Tag bottom edge as a street.
        edges[0].is_street = true;
        if alley {
            // Pretend the top edge is an alley.
            edges[2].is_alley = true;
            edges[2].is_street = false;
        }
        Block { polygon: poly, edges }
    }

    #[test]
    fn front_loaded_uses_full_depth() {
        let b = block_with_alley(false);
        let p = plan(&b, LoadingType::FrontLoaded, 50.0);
        assert_eq!(p.loading, LoadingType::FrontLoaded);
        assert!((p.lot_depth() - 50.0).abs() < 1e-9);
        assert!(!p.is_two_frontage());
    }

    #[test]
    fn alley_loaded_halves_depth_when_alley_present() {
        let b = block_with_alley(true);
        let p = plan(&b, LoadingType::AlleyLoaded, 50.0);
        assert!(p.is_two_frontage());
        assert!((p.lot_depth() - 25.0).abs() < 1e-9, "expected half depth");
    }

    #[test]
    fn alley_loaded_without_alley_degrades_to_front() {
        let b = block_with_alley(false);
        let p = plan(&b, LoadingType::AlleyLoaded, 50.0);
        assert_eq!(p.loading, LoadingType::FrontLoaded);
        assert!((p.lot_depth() - 50.0).abs() < 1e-9);
    }
}
