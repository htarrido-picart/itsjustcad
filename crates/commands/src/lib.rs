//! The command substrate: one language spoken by the human command line and
//! the LLM deck. A document is an op-log of `Command`s — undo, file format and
//! replay all derive from it.

mod command;
mod error;
mod exec;
mod parse;
mod registry;

pub use command::{Command, Selector};
pub use error::{ExecError, ParseError};
pub use exec::{ApplyOutcome, Session};
pub use parse::parse;
pub use registry::{registry, CommandSpec, SELECTOR_HELP};
