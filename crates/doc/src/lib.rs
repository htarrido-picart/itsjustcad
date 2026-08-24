//! Document model: pure scene state. The op-log and undo live in the
//! `commands` crate (which depends on this one); the document knows nothing
//! about how it is mutated.

mod document;
pub mod hatch;
mod object;
mod sheet;
mod underlay;
mod units;
mod view;

pub use document::Document;
pub use object::{
    Annotation, BlockGeometry, Geometry, HatchPattern, LayerStyle, ObjectId, SceneObject,
    DEFAULT_LAYER,
};
pub use sheet::{PaperSize, ScheduleRow, Sheet, SheetDim, SheetTable, SheetView, ViewDirection};
pub use underlay::Underlay;
pub use units::{
    format_area, format_length, format_volume, Units, METERS_PER_FOOT, METERS_PER_INCH,
};
pub use view::NamedView;

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
