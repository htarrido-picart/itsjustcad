// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Mesh kernel: indexed face-vertex meshes, primitives, extrusion.
//!
//! Document space is f64 (`DVec3`); GPU data is produced by [`Mesh::to_render`].

mod aabb;
mod bvh;
mod csg;
mod delaunay;
mod earcut;
mod edges;
mod expressive;
mod catalog;
mod formfind;
mod mesh;
mod primitives;
mod section;
mod solids;
mod stairs;
mod structsection;

pub use aabb::Aabb;
pub use bvh::{ray_triangle, Bvh, TriBvh};
pub use csg::{csg_difference, csg_intersection, csg_union, signed_volume, weld};
pub use delaunay::triangulate;
pub use earcut::{earcut, signed_area};
pub use edges::{feature_edges, project_edges_behind, project_edges_onto};
pub use expressive::{
    catenary_vault_surface, diagrid_segments, gaussvault_surface, geodesic_network, gridshell,
    gridshell_segments, hypar_surface, reciprocal_segments, schwedler_segments, spaceframe_struts,
    strut_lattice, voronoishell_segments, waffle_segments,
    GridshellSurface,
};
pub use formfind::{
    cable_net, cable_net_surface, dynamic_relaxation, force_density, funicular_chain,
    invert_funicular, max_residual,
    minimal_surface, minimal_surface_grid, tensegrity_prism, Link, Network, RelaxParams,
    RelaxReport, Tensegrity,
};
pub use mesh::{Mesh, RenderMesh};
pub use primitives::{extrude_profile, make_box};
pub use section::{slice, SliceCurve};
pub use stairs::{
    ramp_centerline, ramp_centerline_curved, ramp_centerline_turning, ramp_layout,
    ramp_layout_opts, ramp_mesh, ramp_plan_length, ramp_surface, ramp_surface_curved,
    ramp_surface_turning, stair_centerline_spiral, stair_layout, stair_layout_opts, stair_mesh,
    stair_mesh_from_params, stair_mesh_spiral, stair_mesh_turning, MeshBuild, RampLayout,
    RampSegment, RampSlope, StairLayout, TurnKind, RAMP_MAX_RUN_RISE, RAMP_MAX_SLOPE,
    RAMP_MIN_LANDING_LEN, STAIR_2R_PLUS_T_TARGET, STAIR_MAX_RISER, STAIR_MAX_RISER_VARIATION,
    STAIR_MAX_RISE_PER_FLIGHT, STAIR_MIN_RISER, STAIR_MIN_TREAD,
};
pub use catalog::{builtin_section, builtin_section_names};
pub use structsection::Section as StructSection;
pub use solids::{
    area_member, blend_curves, frame_member, loft_profiles, loft_profiles_guided, pipe_curve,
    rail_revolve_profile, revolve_profile, sweep2_profile, sweep_profile,
};
