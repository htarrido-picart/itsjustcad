//! The command substrate: one language spoken by the human command line and
//! the LLM deck. A document is an op-log of `Command`s — undo, file format and
//! replay all derive from it.

pub mod blocklib;
mod command;
pub mod csv;
pub mod dxf;
mod error;
mod exec;
pub mod geo;
pub mod ifc;
pub mod io;
pub mod las;
pub mod mesh_export;
pub mod mesh_import;
mod parse;
pub mod pdf;
pub mod plugin;
mod registry;
pub mod svg;

pub use command::{Command, CompassDir, MirrorPlane, OptionOp, Selector};
pub use error::{ExecError, ParseError};
pub use exec::{ApplyOutcome, Session, MAIN_BRANCH};
pub use parse::parse;
pub use plugin::{Plugin, PluginError, PluginParam, PluginRegistry};
pub use registry::{registry, CommandSpec, SELECTOR_HELP};
