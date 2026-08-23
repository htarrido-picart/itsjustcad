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

#[cfg(test)]
mod tests {
    use super::*;

    /// CCW triangle in the XY plane; right-hand rule gives a +Z normal.
    fn xy_triangle() -> Mesh {
        Mesh::new(
            vec![
                DVec3::ZERO,
                DVec3::new(1.0, 0.0, 0.0),
                DVec3::new(0.0, 1.0, 0.0),
            ],
            vec![[0, 1, 2]],
        )
    }

    #[test]
    fn to_render_flat_normal_for_known_triangle() {
        let render = xy_triangle().to_render();
        assert_eq!(render.positions.len(), 3);
        assert_eq!(render.indices, vec![0, 1, 2]);
        for n in &render.normals {
            assert_eq!(*n, [0.0, 0.0, 1.0]);
        }
    }

    #[test]
    fn to_render_duplicates_shared_vertices_per_face() {
        // Two triangles sharing an edge: 4 mesh vertices, 6 render vertices.
        let mesh = Mesh::new(
            vec![
                DVec3::ZERO,
                DVec3::new(1.0, 0.0, 0.0),
                DVec3::new(1.0, 1.0, 0.0),
                DVec3::new(0.0, 1.0, 0.0),
            ],
            vec![[0, 1, 2], [0, 2, 3]],
        );
        let render = mesh.to_render();
        assert_eq!(render.positions.len(), 6);
        assert_eq!(render.normals.len(), 6);
        assert_eq!(render.indices, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn to_render_degenerate_face_gets_zero_normal() {
        let mesh = Mesh::new(vec![DVec3::ZERO, DVec3::ONE, DVec3::ZERO], vec![[0, 1, 2]]);
        let render = mesh.to_render();
        for n in &render.normals {
            assert_eq!(*n, [0.0, 0.0, 0.0]);
        }
    }

    #[test]
    fn transform_translation_moves_positions() {
        let mut mesh = xy_triangle();
        mesh.transform(DMat4::from_translation(DVec3::new(2.0, -1.0, 3.0)));
        assert_eq!(mesh.positions()[0], DVec3::new(2.0, -1.0, 3.0));
        assert_eq!(mesh.positions()[1], DVec3::new(3.0, -1.0, 3.0));
        assert_eq!(mesh.positions()[2], DVec3::new(2.0, 0.0, 3.0));
        assert_eq!(mesh.faces(), &[[0, 1, 2]], "translation keeps winding");
    }

    #[test]
    fn reflection_reverses_winding_so_normal_survives() {
        let mut mesh = xy_triangle();
        // Mirror across the XZ plane (negative determinant).
        mesh.transform(DMat4::from_scale(DVec3::new(1.0, -1.0, 1.0)));
        assert_eq!(mesh.faces(), &[[0, 2, 1]]);
        let render = mesh.to_render();
        for n in &render.normals {
            assert_eq!(*n, [0.0, 0.0, 1.0], "normal still points +Z after mirror");
        }
    }

    #[test]
    fn aabb_bounds_positions() {
        let aabb = xy_triangle().aabb();
        assert_eq!(aabb.min, DVec3::ZERO);
        assert_eq!(aabb.max, DVec3::new(1.0, 1.0, 0.0));
    }
}
