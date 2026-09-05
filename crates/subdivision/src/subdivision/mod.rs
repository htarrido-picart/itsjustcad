// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Subdivision algorithms. Phase 3 ships `recursive_obb` (`method=grid`); Phase 4
//! ships `offset_sub` (`method=perimeter`); `skeleton_sub` (Phase 7) lands later.

pub mod lot_rules;
pub mod offset_sub;
pub mod recursive_obb;

pub use recursive_obb::{subdivide, Lot};
