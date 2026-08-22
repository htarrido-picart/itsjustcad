use glam::DVec3;

use crate::Mesh;

/// Axis-aligned box from a corner and (positive) size, Z-up.
pub fn make_box(corner: DVec3, size: DVec3) -> Mesh {
    let (x0, y0, z0) = (corner.x, corner.y, corner.z);
    let (x1, y1, z1) = (x0 + size.x, y0 + size.y, z0 + size.z);
    let positions = vec![
        DVec3::new(x0, y0, z0), // 0
        DVec3::new(x1, y0, z0), // 1
        DVec3::new(x1, y1, z0), // 2
        DVec3::new(x0, y1, z0), // 3
        DVec3::new(x0, y0, z1), // 4
        DVec3::new(x1, y0, z1), // 5
        DVec3::new(x1, y1, z1), // 6
        DVec3::new(x0, y1, z1), // 7
    ];
    // CCW seen from outside
    let faces = vec![
        [0, 2, 1],
        [0, 3, 2], // bottom (z0, normal -z)
        [4, 5, 6],
        [4, 6, 7], // top (z1, normal +z)
        [0, 1, 5],
        [0, 5, 4], // front (y0, normal -y)
        [2, 3, 7],
        [2, 7, 6], // back (y1, normal +y)
        [3, 0, 4],
        [3, 4, 7], // left (x0, normal -x)
        [1, 2, 6],
        [1, 6, 5], // right (x1, normal +x)
    ];
    Mesh::new(positions, faces)
}
