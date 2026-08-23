//! Document model: pure scene state. The op-log and undo live in the
//! `commands` crate (which depends on this one); the document knows nothing
//! about how it is mutated.

mod document;
mod object;
mod sheet;
mod units;

pub use document::Document;
pub use object::{
    Annotation, Geometry, HatchPattern, LayerStyle, ObjectId, SceneObject, DEFAULT_LAYER,
};
pub use sheet::{PaperSize, Sheet, SheetView, ViewDirection};
pub use units::{format_length, Units, METERS_PER_FOOT, METERS_PER_INCH};
