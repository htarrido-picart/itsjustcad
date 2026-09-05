// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! `subdivision` — pure-Rust 2D site-planning geometry (intemfit / M-intemfit).
//!
//! ZERO `egui` / `itsjustcad-doc` / `itsjustcad-commands` deps: plain geometry
//! in, plain geometry out, unit-tested headless. The commands crate owns the
//! bridge from document curves to these types (dependency direction:
//! `subdivision` (leaf) ← `commands` ← `app`).
//!
//! Phases shipped: 1 (geometry foundation + `i_overlay` bridge), 3 (recursive
//! OBB subdivision, `method=grid`), 4 (offset/perimeter subdivision,
//! `method=perimeter`), 5 + 5b (road generation + block extraction + street
//! tagging), 6 (lot rules — width mix, depth, corner, flag, loading, sliver
//! merge, on euro_latam placeholder defaults), 7 (skeleton / street-following
//! subdivision, `method=streetfollowing`, via the approximate straight skeleton
//! in `straight_skeleton`), 9 (open space — feature placement in `open_space/`:
//! pocket park, greenway corridor, retention pond, tree-save; plus blind
//! %-reserve pulling whole central blocks out as open space). Phase 2 (verb/
//! settings plumbing) lives in the commands crate but the `SubdivisionSettings`
//! type is here.

pub mod blocks;
pub mod geometry;
pub mod indexing;
pub mod open_space;
pub mod buildings;
pub mod reporting;
pub mod settings;
pub mod straight_skeleton;
pub mod streets;
pub mod subdivision;

pub use blocks::{Block, BlockEdge};
pub use buildings::{
    build_on_envelope, default_roof_for, footprint as building_footprint, massing as building_massing,
    roof as building_roof, BuildingResult, Floor, Footprint, Massing,
};
pub use geometry::clip_bridge;
pub use indexing::ConsistentIndexing;
pub use geometry::oriented_box::{convex_hull, OrientedBox};
pub use open_space::{
    greenway, pocket_park, reserve_blocks, retention_pond, tree_save, OpenSpaceFeature,
    ReservedBlock,
};
pub use geometry::polygon2d::Polygon2d;
pub use reporting::{FrontageStats, YieldComparison, YieldInputs, YieldReport};
pub use geometry::polyline::PolylineTools;
pub use geometry::split::{split_by_line, Line2d};
pub use settings::{
    FootprintMode, LoadingType, LotWidthMix, RegionProfile, RoofType, SkeletonImpl,
    StreetPattern, SubdivisionMethod, SubdivisionSettings, Typology,
};
pub use subdivision::lot_rules::{
    apply_lot_rules, LotRulesReport, WidthMixResult, WidthMixSolver,
};
pub use straight_skeleton::{
    felkel::FelkelSkeleton, offset_approx::OffsetApproxSkeleton, SkeletonFace, StraightSkeleton,
};
pub use streets::{extract_blocks, generate_streets, Street, StreetGraph, StreetTier};
pub use subdivision::offset_sub::subdivide as subdivide_offset;
pub use subdivision::setbacks::{
    buildable_envelope, frontage, EdgeRole, Envelope, FrontageAt,
};
pub use subdivision::skeleton_sub::{
    subdivide as subdivide_skeleton, subdivide_block as subdivide_skeleton_block,
};
pub use subdivision::{subdivide, Lot};

use glam::DVec2;

/// A validation block loaded from a `samples/blocks/*.json` file. The JSON is a
/// simple `{"name": "...", "boundary": [[x,y], ...]}` object.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SampleBlock {
    pub name: String,
    pub boundary: Vec<[f64; 2]>,
}

impl SampleBlock {
    /// Parse a sample block from a JSON string.
    pub fn from_json(s: &str) -> Result<SampleBlock, serde_json::Error> {
        serde_json::from_str(s)
    }

    /// Convert the boundary into a `Polygon2d` (CCW-normalised).
    pub fn polygon(&self) -> Option<Polygon2d> {
        Polygon2d::new(self.boundary.iter().map(|p| DVec2::new(p[0], p[1])).collect())
    }
}
