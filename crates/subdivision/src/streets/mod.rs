// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Road-network generation + block extraction (plan §7.3, Phase 5).
//!
//! - [`street_graph`] — the `StreetGraph` output type (centerlines + widths +
//!   hierarchy + adjacency).
//! - [`generators`] — the four rectilinear generators (orthogonal, skewed,
//!   organic, cul-de-sac), each producing a `StreetGraph`.
//! - [`block_extractor`] — ROW/2 offset → boolean-subtract from the site →
//!   tagged blocks (with an optional alley tier).

pub mod block_extractor;
pub mod generators;
pub mod street_graph;

pub use block_extractor::extract as extract_blocks;
pub use generators::generate as generate_streets;
pub use street_graph::{Street, StreetGraph, StreetTier};
