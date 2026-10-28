// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! The command substrate: one language spoken by the human command line and
//! the LLM deck. A document is an op-log of `Command`s — undo, file format and
//! replay all derive from it.

pub mod blocklib;
pub mod checkengine;
mod command;
pub mod csv;
pub mod cut;
pub mod cutcheck;
pub mod dwg;
pub mod dxf;
pub mod e57;
mod error;
mod exec;
pub mod gbnf;
pub mod geo;
pub mod ifc;
pub mod io;
pub mod landscape;
pub mod las;
pub mod lot;
pub mod osnap;
pub mod mesh_export;
pub mod mesh_import;
mod parse;
pub mod paths;
pub mod pat;
pub mod pdf;
pub mod plugin;
pub mod raster;
mod registry;
pub mod rhino3dm;
pub mod saf;
pub mod site_metrics;
pub mod site_optimize;
pub mod scs;
pub mod sketch;
pub mod stairs;
pub mod subdiv_params;
pub mod svg;
pub mod trace;
pub mod workdir;

pub use command::{
    BoolKind, CPlaneOp, Command, CompassDir, ConstrainKind, DimAnchorSpec, GridshellSurfaceSpec,
    LotEdit, LotField, MirrorPlane, OptionOp, PlotStyleOp, RegionMode, Selector, SheetSetOp, SheetViewSet, SimilarBy,
};
pub use error::{ExecError, ParseError};
pub use exec::{
    resolve_targets, trim_preview, trim_removed_piece, ApplyOutcome, ImportSummary, Session, MAIN_BRANCH,
};
pub use parse::{parse, parse_with_home};
pub use plugin::{Plugin, PluginError, PluginParam, PluginRegistry};
pub use registry::{registry, Category, CommandSpec, SELECTOR_HELP};

/// The user's config dir: [`home_config_dir`] of the home directory.
#[cfg(not(test))]
pub fn config_dir() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| home_config_dir(&h))
}

/// Tests never read or write the developer's real config: they get a throwaway
/// dir under the system temp dir instead, one per test thread (unit tests run in parallel threads of one process, and
/// a workdir grant persisted by one test must not leak into another).
#[cfg(test)]
pub fn config_dir() -> Option<std::path::PathBuf> {
    let thread =
        format!("{:?}", std::thread::current().id()).replace(|c: char| !c.is_ascii_digit(), "");
    Some(std::env::temp_dir().join(format!("ijc_test_config_{}_{thread}", std::process::id())))
}

/// `~/.config/itsjustcad` under `home`, on every platform (CLI-tool convention,
/// XDG default on Linux; never a macOS-only dir — GitHub issue #26).
pub fn home_config_dir(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".config").join("itsjustcad")
}
