// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Subdivision algorithms. Phase 3 ships `recursive_obb` (`method=grid`); Phase 4
//! ships `offset_sub` (`method=perimeter`); Phase 7 ships `skeleton_sub`
//! (`method=streetfollowing`) — perpendicular-to-curve lot lines around cul-de-sac
//! bulbs and curved streets, via the approximate straight skeleton.

pub mod lot_rules;
pub mod offset_sub;
pub mod recursive_obb;
pub mod setbacks;
pub mod skeleton_sub;

pub use recursive_obb::{subdivide, Lot};
pub use setbacks::{buildable_envelope, frontage, EdgeRole, Envelope, FrontageAt};
