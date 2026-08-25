// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Document model: pure scene state. The op-log and undo live in the
//! `commands` crate (which depends on this one); the document knows nothing
//! about how it is mutated.

mod document;
pub mod hatch;
pub mod hershey;
pub mod loads;
mod object;
mod sheet;
mod structure;
mod underlay;
mod units;
mod view;

pub use document::Document;
pub use loads::{LoadGeometry, RestraintKind, StructLoad, StructSupport};
pub use object::{
    Annotation, AreaKind, BlockGeometry, FrameKind, Geometry, HatchPattern, LayerStyle,
    MaterialPreset, ObjectId, ObjectMaterial, SceneObject, DEFAULT_LAYER,
};
pub use sheet::{PaperSize, ScheduleRow, Sheet, SheetDim, SheetTable, SheetView, ViewDirection};
pub use structure::{Grid, Material, Section, Story};
pub use underlay::Underlay;
pub use units::{
    format_area, format_length, format_volume, Units, METERS_PER_FOOT, METERS_PER_INCH,
};
pub use view::{NamedView, PanoView};

use serde::{Deserialize, Serialize};

/// Solar position recorded by the `sun` command. Stored as azimuth + altitude
/// (NOAA simplified SPA output) so the value is self-contained in the op-log.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SunPosition {
    /// Clockwise from North, degrees [0, 360).
    pub azimuth_deg: f64,
    /// Above the horizon, degrees (negative = below horizon).
    pub altitude_deg: f64,
}

/// Observer location on Earth, recorded by the `sun` command or an EPW import.
/// Needed by environmental analyses (`shadowstudy`, `sunhours`) to recompute
/// sun positions over a day. Stored in the op-log via a `location` op so saved
/// files replay identically.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GeoLocation {
    /// Latitude, degrees (north positive).
    pub lat_deg: f64,
    /// Longitude, degrees (east positive).
    pub lon_deg: f64,
    /// Time-zone offset from UTC in hours (east positive). Sun-position math is
    /// UTC-based; this lets analyses interpret local clock times on a date.
    pub tz_hours: f64,
}
