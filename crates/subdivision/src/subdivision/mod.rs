// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Subdivision algorithms. Phase 3 ships `recursive_obb` (`method=grid`);
//! `offset_sub` (Phase 4) and `skeleton_sub` (Phase 7) land later.

pub mod recursive_obb;

pub use recursive_obb::{subdivide, Lot};
