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

/// Orthographic projection of `p` onto the plane through `point` with unit
/// `normal`, sliding along the normal: `p - (n·(p-point)) n`.
fn project_point(p: DVec3, point: DVec3, normal: DVec3) -> DVec3 {
    p - normal * normal.dot(p - point)
}

/// Feature edges lying entirely on the negative side of the plane (`n·(p-point)
/// < -tol` for both endpoints), projected onto the plane along `normal`.
///
/// This is the "edges below/beyond a cut" case: for a plan cut (normal = +Z)
/// it flattens the geometry below the slice onto z = the cut height; for a
/// vertical section it flattens everything on the far side (viewing direction
/// = -normal) onto the cut plane. Edges straddling the plane are dropped
/// (their cut portion is the section loop itself).
pub fn project_edges_behind(
    mesh: &Mesh,
    point: DVec3,
    normal: DVec3,
    tol: f64,
) -> Vec<(DVec3, DVec3)> {
    let n = normal.normalize_or_zero();
    if n == DVec3::ZERO {
        return Vec::new();
    }
    feature_edges(mesh)
        .into_iter()
        .filter(|(a, b)| n.dot(*a - point) < -tol && n.dot(*b - point) < -tol)
        .map(|(a, b)| (project_point(a, point, n), project_point(b, point, n)))
        .collect()
}

/// All feature edges projected orthographically onto the plane through `point`
/// with `normal` (the elevation / pure-projection case: no side filter, no
/// cutting). Zero-length projected edges (edges parallel to the view
/// direction) are dropped.
pub fn project_edges_onto(
    mesh: &Mesh,
    point: DVec3,
    normal: DVec3,
    tol: f64,
) -> Vec<(DVec3, DVec3)> {
    let n = normal.normalize_or_zero();
    if n == DVec3::ZERO {
        return Vec::new();
    }
    feature_edges(mesh)
        .into_iter()
        .map(|(a, b)| (project_point(a, point, n), project_point(b, point, n)))
        .filter(|(a, b)| a.distance(*b) > tol)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::make_box;

    const TOL: f64 = 1e-6;

    #[test]
    fn project_below_flattens_to_cut_height() {
        // Box from z=0..3; cut plane at z=2. All 12 edges have at least one
        // endpoint below z=2, but only edges fully below survive: the 4 bottom
        // edges (z=0) plus... the 4 verticals straddle (0 and 3) → dropped;
        // the 4 top edges (z=3) are above → dropped. So 4 bottom edges remain.
        let b = make_box(DVec3::ZERO, DVec3::new(2.0, 1.0, 3.0));
        let proj = project_edges_behind(&b, DVec3::new(0.0, 0.0, 2.0), DVec3::Z, TOL);
        assert_eq!(proj.len(), 4, "{proj:?}");
        for (a, c) in &proj {
            assert!((a.z - 2.0).abs() < TOL && (c.z - 2.0).abs() < TOL, "flattened to z=2");
        }
    }

    #[test]
    fn project_below_empty_when_all_above() {
        let b = make_box(DVec3::new(0.0, 0.0, 5.0), DVec3::new(2.0, 1.0, 3.0));
        assert!(project_edges_behind(&b, DVec3::new(0.0, 0.0, 2.0), DVec3::Z, TOL).is_empty());
    }

    #[test]
    fn project_onto_vertical_plane_outline() {
        // Elevation looking along -Y onto the y=0 plane: a box projects to its
        // XZ outline. 12 edges → the 4 y-parallel edges collapse to points and
        // are dropped; the front and back faces' 8 edges overlap in projection
        // but we don't dedup, so we keep 8 non-degenerate edges.
        let b = make_box(DVec3::ZERO, DVec3::new(2.0, 1.0, 3.0));
        let proj = project_edges_onto(&b, DVec3::ZERO, DVec3::Y, TOL);
        assert_eq!(proj.len(), 8, "{proj:?}");
        for (a, c) in &proj {
            assert!(a.y.abs() < TOL && c.y.abs() < TOL, "flattened to y=0");
        }
    }

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
