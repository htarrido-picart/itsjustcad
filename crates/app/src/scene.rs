// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Scene helpers for the app shell. The LLM `digest` now lives in the `deck`
//! crate so the desktop app and the iOS FFI shell feed the model an identical,
//! identically-sanitized scene description; it is re-exported here so existing
//! `crate::scene::digest(...)` call sites keep working unchanged.

pub use itsjustcad_deck::digest;
pub use itsjustcad_render::{snapshot_with_mode, Theme};
