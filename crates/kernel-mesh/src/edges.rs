//! Feature-edge extraction, shared by the DXF/PDF exporters and the viewport
//! wireframe/x-ray display modes.

use std::collections::BTreeMap;

use glam::DVec3;

use crate::Mesh;

/// Feature edges of a mesh: boundary edges plus edges where the adjacent face
/// normals differ. Diagonals across flat quads are skipped — a box yields its
/// 12 outline edges, not 18.
pub fn feature_edges(mesh: &Mesh) -> Vec<(DVec3, DVec3)> {
    let mut edges: BTreeMap<(u32, u32), Vec<DVec3>> = BTreeMap::new();
    let pos = mesh.positions();
    for face in mesh.faces() {
        let [a, b, c] = face.map(|i| pos[i as usize]);
        let n = (b - a).cross(c - a).normalize_or_zero();
        for (i, j) in [(face[0], face[1]), (face[1], face[2]), (face[2], face[0])] {
            edges.entry((i.min(j), i.max(j))).or_default().push(n);
        }
    }
    let mut out = Vec::new();
    for ((i, j), normals) in edges {
        let flat = normals.len() == 2 && normals[0].dot(normals[1]) > 1.0 - 1e-9;
        if !flat {
            out.push((pos[i as usize], pos[j as usize]));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::make_box;

    #[test]
    fn box_has_12_feature_edges_not_18() {
        // Policy: coplanar quad diagonals are not feature edges. A box is 12
        // triangle-pair faces = 18 unique edges, 6 of them flat diagonals.
        let b = make_box(DVec3::ZERO, DVec3::new(2.0, 1.0, 3.0));
        assert_eq!(feature_edges(&b).len(), 12);
    }

    #[test]
    fn open_surface_boundary_edges_are_features() {
        // A single triangle: all 3 edges are boundaries.
        let m = Mesh::new(vec![DVec3::ZERO, DVec3::X, DVec3::Y], vec![[0, 1, 2]]);
        assert_eq!(feature_edges(&m).len(), 3);
    }
}
