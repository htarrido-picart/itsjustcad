// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! `subdivision` — pure-Rust 2D site-planning geometry.
//!
//! ZERO `egui` / `itsjustcad-doc` / `itsjustcad-commands` deps: plain geometry
//! in, plain geometry out, unit-tested headless. The commands crate owns the
//! bridge from document curves to these types (dependency direction:
//! `subdivision` (leaf) ← `commands` ← `app`).
//!
//! Features: geometry foundation (`Polygon2d`, OBB, `i_overlay` bridge);
//! recursive-OBB (`method=grid`), offset/perimeter (`method=perimeter`) and
//! skeleton / street-following (`method=streetfollowing`) subdivision; road
//! generation (rectilinear, radial, hexagonal, Voronoi) + block extraction +
//! street tagging; lot rules (width mix, depth, corner, flag, loading, sliver
//! merge, on euro_latam placeholder defaults); setbacks + buildable envelopes;
//! open space (pocket park, greenway corridor, retention pond, tree-save, plus
//! blind %-reserve of whole central blocks); buildings (footprints, stepped
//! massing, roofs); yield reporting + option comparison; consistent indexing.
//! The verb/settings plumbing lives in the commands crate but the
//! `SubdivisionSettings` type is here.
//!
//! Provenance: an independent Rust implementation written for ItsJustCAD.
//! Algorithms are implemented from published descriptions; no third-party
//! source was copied (`i_overlay`, MIT OR Apache-2.0, is a declared
//! dependency). The dated works below are cited as background for each
//! technique, not as code sources.
//! - Street patterns: grid, radial and terrain-led streets, Parish & Müller,
//!   SIGGRAPH 2001; Voronoi/raster/radial templates, Sun, Yu, Baciu & Green,
//!   VRST 2002; tensor-field streets, Chen et al., SIGGRAPH 2008; cul-de-sac
//!   turnarounds, US FHA "Planning Profitable Neighborhoods", 1938. Voronoi as
//!   the Delaunay dual: Bowyer, and Watson, Computer Journal 24(2), 1981.
//! - Recursive OBB lots (`method=grid`): minimum-area rectangle, Freeman &
//!   Shapira, CACM 18(7), 1975; automated lot layout, Erskine & Rink,
//!   Computers & Structures 7(6), 1977; Parish & Müller 2001; Kelly & McCabe,
//!   "Citygen", GDTW 2007; Weber, Müller, Wonka & Gross, Eurographics 2009;
//!   Vanegas, Kelly, Weber, Halatsch, Aliaga & Müller, CGF 31(2), 2012.
//! - Street-facing lots (`method=perimeter`, `method=streetfollowing`): side
//!   lot lines perpendicular or radial to the street, FHA 1938; Aliaga,
//!   Vanegas & Beneš, SIGGRAPH Asia 2008; Esri CityEngine 2011 skeleton
//!   subdivision; Vanegas et al. 2012. Straight skeleton: Aichholzer, Alberts,
//!   Aurenhammer & Gärtner, J.UCS 1(12), 1995; Felkel & Obdržálek, SCCG 1998;
//!   Eppstein & Erickson, DCG 22, 1999.
//! - Lot rules: corner-lot extra width, H. Bartholomew, Memphis city plan,
//!   1924; flag lots, S. Eisner & Assoc. model ordinance, 1969 (pole excluded
//!   from lot area, Poway, CA, 1983); min area/frontage/depth with remainder
//!   handling, Autodesk Civil 3D and Carlson parcel tools; sliver merge by
//!   longest shared border, Esri ARC/INFO ELIMINATE.
//! - Open space: reserve land in land readjustment (Frankfurt Lex Adickes,
//!   1902; Japan 1919/1954); R. Arendt, "Conservation Design for
//!   Subdivisions", 1996.
//! - Setbacks, stepped envelopes and FAR: NYC Building Zone Resolution, 1916;
//!   FAR in the 1949-51 NYC rezoning debate; Martin & March, "Urban Space and
//!   Structures", 1972; Berghauser Pont & Haupt, "Spacemate", 2004.
//! - Buildings and roofs: Müller, Wonka, Haegler, Ulmer & Van Gool, SIGGRAPH
//!   2006; straight-skeleton roofs, Laycock & Day, WSCG 2003.

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
    CornerAlignment, FootprintMode, LoadingType, LotWidthMix, RegionProfile, RoofType,
    SkeletonImpl, StreetPattern, SubdivisionMethod, SubdivisionSettings, Typology,
};
pub use subdivision::lot_rules::{
    apply_lot_rules, LotRulesReport, WidthMixResult, WidthMixSolver,
};
pub use straight_skeleton::{
    felkel::FelkelSkeleton, offset_approx::OffsetApproxSkeleton, SkeletonFace, StraightSkeleton,
};
pub use streets::{extract_blocks, generate_streets, Street, StreetGraph, StreetTier};
pub use subdivision::offset_sub::{
    subdivide as subdivide_offset, subdivide_block as subdivide_offset_block,
};
pub use subdivision::setbacks::{
    EdgeRole, Envelope, FrontageAt, buildable_envelope, classify_edges, frontage,
};
pub use subdivision::skeleton_sub::{
    subdivide as subdivide_skeleton, subdivide_block as subdivide_skeleton_block,
};
pub use subdivision::{subdivide, subdivide_block, Lot};

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
