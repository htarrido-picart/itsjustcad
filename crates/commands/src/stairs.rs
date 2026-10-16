// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Pure layout math for parametric ramps and stairs (issues #18 / #83).
//!
//! The actual math now lives in `kernel-mesh` (`kernel_mesh::stairs`) so the
//! `doc` crate's parametric `derive_mesh` — which depends on `kernel-mesh` but
//! NOT on `commands` — can re-run the SAME layout math on a param edit (issue
//! #83 made `ramp`/`stair` live parametric objects). This module re-exports it
//! so the verb-side call sites (`exec.rs`) and their `crate::stairs::*` imports
//! are unchanged.

pub use kernel_mesh::{
    ramp_centerline, ramp_layout, ramp_mesh, ramp_plan_length, ramp_surface, stair_layout,
    stair_mesh, stair_mesh_from_params, MeshBuild, RampLayout, RampSegment, RampSlope, StairLayout,
    RAMP_MAX_RUN_RISE, RAMP_MAX_SLOPE, RAMP_MIN_LANDING_LEN, STAIR_2R_PLUS_T_TARGET,
    STAIR_MAX_RISER, STAIR_MAX_RISER_VARIATION, STAIR_MAX_RISE_PER_FLIGHT, STAIR_MIN_RISER,
    STAIR_MIN_TREAD,
};
