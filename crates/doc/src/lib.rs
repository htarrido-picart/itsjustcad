//! Document model: pure scene state. The op-log and undo live in the
//! `commands` crate (which depends on this one); the document knows nothing
//! about how it is mutated.

mod document;
mod object;

pub use document::Document;
pub use object::{Geometry, ObjectId, SceneObject};
