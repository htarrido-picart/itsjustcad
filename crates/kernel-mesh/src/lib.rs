//! Mesh kernel: indexed face-vertex meshes, primitives, extrusion.
//!
//! Document space is f64 (`DVec3`); GPU data is produced by [`Mesh::to_render`].

mod aabb;
mod mesh;
mod primitives;

pub use aabb::Aabb;
pub use mesh::{Mesh, RenderMesh};
pub use primitives::make_box;
