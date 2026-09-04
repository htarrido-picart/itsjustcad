// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Tagged blocks (plan §5 hard dependency): a `Block` = polygon + per-edge
//! `BlockEdge` street tags, produced by `streets::block_extractor`.

pub mod block;
pub mod block_edge;

pub use block::Block;
pub use block_edge::BlockEdge;
