//! Mesh kernel: indexed face-vertex meshes, primitives, extrusion.
//!
//! Document space is f64 (`DVec3`); GPU data is produced by [`Mesh::to_render`].

mod aabb;
mod csg;
mod delaunay;
mod earcut;
mod edges;
mod mesh;
mod primitives;
mod section;
mod solids;

pub use aabb::Aabb;
pub use csg::{csg_difference, csg_intersection, csg_union, signed_volume, weld};
pub use delaunay::triangulate;
pub use earcut::{earcut, signed_area};
pub use edges::{feature_edges, project_edges_behind, project_edges_onto};
pub use mesh::{Mesh, RenderMesh};
pub use primitives::{extrude_profile, make_box};
pub use section::slice;
pub use solids::{
    loft_profiles, pipe_curve, rail_revolve_profile, revolve_profile, sweep2_profile,
    sweep_profile,
};
