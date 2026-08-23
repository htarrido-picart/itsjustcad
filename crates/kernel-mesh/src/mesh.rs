use glam::{DMat4, DVec3};
use serde::{Deserialize, Serialize};

use crate::Aabb;

/// Indexed face-vertex triangle mesh in f64 document space.
///
/// The internal representation is triangles; the public API is designed so a
/// half-edge structure can replace the storage later without breaking callers.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Mesh {
    positions: Vec<DVec3>,
    faces: Vec<[u32; 3]>,
}

/// Flat-shaded GPU-ready copy: one vertex per face corner, f32.
pub struct RenderMesh {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub indices: Vec<u32>,
}

impl Mesh {
    pub fn new(positions: Vec<DVec3>, faces: Vec<[u32; 3]>) -> Self {
        Self { positions, faces }
    }

    pub fn positions(&self) -> &[DVec3] {
        &self.positions
    }

    pub fn faces(&self) -> &[[u32; 3]] {
        &self.faces
    }

    pub fn transform(&mut self, m: DMat4) {
        for p in &mut self.positions {
            *p = m.transform_point3(*p);
        }
        // Reflections (mirror, negative scale) flip triangle winding; reverse
        // the faces so normals keep pointing outward.
        if m.determinant() < 0.0 {
            for f in &mut self.faces {
                f.swap(1, 2);
            }
        }
    }

    pub fn aabb(&self) -> Aabb {
        Aabb::from_points(self.positions.iter().copied())
    }

    /// Flat normals: vertices are duplicated per face so each triangle is
    /// uniformly shaded (correct look for prismatic architecture massing).
    pub fn to_render(&self) -> RenderMesh {
        let mut positions = Vec::with_capacity(self.faces.len() * 3);
        let mut normals = Vec::with_capacity(self.faces.len() * 3);
        let mut indices = Vec::with_capacity(self.faces.len() * 3);
        for face in &self.faces {
            let [a, b, c] = face.map(|i| self.positions[i as usize]);
            let n = (b - a).cross(c - a).normalize_or_zero();
            for p in [a, b, c] {
                indices.push(positions.len() as u32);
                positions.push([p.x as f32, p.y as f32, p.z as f32]);
                normals.push([n.x as f32, n.y as f32, n.z as f32]);
            }
        }
        RenderMesh {
            positions,
            normals,
            indices,
        }
    }
}
