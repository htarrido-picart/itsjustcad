//! Mesh kernel: indexed face-vertex meshes, primitives, extrusion.
//!
//! Document space is f64 (`DVec3`); GPU data is produced by [`Mesh::to_render`].

mod aabb;
mod csg;
mod earcut;
mod mesh;
mod primitives;
mod solids;

pub use aabb::Aabb;
pub use csg::{csg_difference, csg_intersection, csg_union, signed_volume, weld};
pub use earcut::{earcut, signed_area};
pub use mesh::{Mesh, RenderMesh};
pub use primitives::{extrude_profile, make_box};
pub use solids::{loft_profiles, revolve_profile, sweep_profile};
