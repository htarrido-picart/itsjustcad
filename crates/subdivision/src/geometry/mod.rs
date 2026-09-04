// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! 2D geometry foundation for subdivision: polygons, minimum-area oriented
//! boxes, half-plane splitting, polyline tools, and the sole `i_overlay` bridge.

pub mod clip_bridge;
pub mod oriented_box;
pub mod polygon2d;
pub mod polyline;
pub mod split;
