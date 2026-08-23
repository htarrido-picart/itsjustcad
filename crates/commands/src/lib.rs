//! The command substrate: one language spoken by the human command line and
//! the LLM deck. A document is an op-log of `Command`s — undo, file format and
//! replay all derive from it.

mod command;
pub mod dxf;
mod error;
mod exec;
pub mod io;
pub mod mesh_export;
mod parse;
pub mod pdf;
mod registry;

pub use command::{Command, MirrorPlane, Selector};
pub use error::{ExecError, ParseError};
pub use exec::{ApplyOutcome, Session};
pub use parse::parse;
pub use registry::{registry, CommandSpec, SELECTOR_HELP};
