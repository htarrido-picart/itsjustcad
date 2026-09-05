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
//! merge, on euro_latam placeholder defaults). Phase 2 (verb/settings plumbing)
//! lives in the commands crate but the `SubdivisionSettings` type is here.

pub mod blocks;
pub mod geometry;
pub mod settings;
pub mod streets;
pub mod subdivision;

pub use blocks::{Block, BlockEdge};
pub use geometry::clip_bridge;
pub use geometry::oriented_box::{convex_hull, OrientedBox};
pub use geometry::polygon2d::Polygon2d;
pub use geometry::polyline::PolylineTools;
pub use geometry::split::{split_by_line, Line2d};
pub use settings::{
    LoadingType, LotWidthMix, RegionProfile, StreetPattern, SubdivisionMethod, SubdivisionSettings,
};
pub use subdivision::lot_rules::{
    apply_lot_rules, LotRulesReport, WidthMixResult, WidthMixSolver,
};
pub use streets::{extract_blocks, generate_streets, Street, StreetGraph, StreetTier};
pub use subdivision::offset_sub::subdivide as subdivide_offset;
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
