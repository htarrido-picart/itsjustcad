// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Open-space feature placement + blind %-reserve (plan §1, §9 Phase 9).
//!
//! Two modes, both baking onto the `openspace` layer so a future `lotreport`
//! (Phase 11) can subtract them from gross site area (net-of-open-space):
//!
//! - **Feature placement** (default, Manuel's questionnaire): the tool PLACES a
//!   marked amenity — a [`pocket_park`], a [`greenway`] trail corridor, a
//!   [`retention_pond`], or a [`tree_save`] area — as a valid, non-self-
//!   intersecting polygon at a selected region (or the largest empty block).
//! - **Blind %-reserve** (owner opt-in keyword, §1): [`reserve_blocks`] pulls
//!   whole blocks out of subdivision, biggest-and-most-central first, until the
//!   reserved area reaches ~pct of the site (CityEngine's model: open space = a
//!   block you chose not to subdivide). Reserved blocks are returned tagged so
//!   yield nets them out and a later `lotsubdivide` skips them.
//!
//! **Advisory (plan §9):** these are DESIGN-INTENT placements, not hydrology or
//! ecology engineering. A retention pond here is a rounded polygon of the
//! requested area, NOT a sized detention basin; a tree-save is a marked polygon,
//! not a surveyed canopy. The commands layer surfaces this as a no-false-
//! precision note; the geometry carries no engineering claim.

pub mod greenway;
pub mod pocket_park;
pub mod retention_pond;
pub mod reserve;
pub mod tree_save;

pub use greenway::greenway;
pub use pocket_park::pocket_park;
pub use reserve::{reserve_blocks, ReservedBlock};
pub use retention_pond::retention_pond;
pub use tree_save::tree_save;

use crate::geometry::polygon2d::Polygon2d;

/// The kind of placed open-space feature (feature-placement mode). Determines
/// the shape family and the `openspace` sub-label the commands layer bakes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenSpaceFeature {
    /// A small green polygon (a compact rounded rectangle).
    PocketPark,
    /// A linear buffered strip (trail corridor) along a path/edge.
    Greenway,
    /// A rounded polygon (a near-circular basin) at a low/selected area.
    RetentionPond,
    /// A preserved-vegetation polygon the layout works around.
    TreeSave,
}

impl OpenSpaceFeature {
    /// Parse a feature-type keyword → variant. `None` for an unknown keyword.
    pub fn parse(s: &str) -> Option<OpenSpaceFeature> {
        match s.to_lowercase().as_str() {
            "park" | "pocketpark" | "pocket_park" | "pocket-park" => {
                Some(OpenSpaceFeature::PocketPark)
            }
            "greenway" | "trail" | "corridor" => Some(OpenSpaceFeature::Greenway),
            "pond" | "retention" | "retentionpond" | "retention_pond" | "basin" => {
                Some(OpenSpaceFeature::RetentionPond)
            }
            "treesave" | "tree_save" | "tree-save" | "trees" | "vegetation" => {
                Some(OpenSpaceFeature::TreeSave)
            }
            _ => None,
        }
    }

    /// The short label the geometry is tagged with on the `openspace` layer.
    pub fn label(self) -> &'static str {
        match self {
            OpenSpaceFeature::PocketPark => "openspace:park",
            OpenSpaceFeature::Greenway => "openspace:greenway",
            OpenSpaceFeature::RetentionPond => "openspace:pond",
            OpenSpaceFeature::TreeSave => "openspace:treesave",
        }
    }
}

/// A validity check shared by every placement: at least a triangle, positive
/// area, and no repeated-adjacent vertices (a proxy for "simple" that the
/// constructors here guarantee by construction — they never create a bowtie).
pub(crate) fn is_valid_feature(poly: &Polygon2d) -> bool {
    if poly.len() < 3 || poly.area() <= 1e-9 {
        return false;
    }
    let v = poly.verts();
    let n = v.len();
    for i in 0..n {
        if v[i].distance_squared(v[(i + 1) % n]) < 1e-12 {
            return false;
        }
    }
    true
}
