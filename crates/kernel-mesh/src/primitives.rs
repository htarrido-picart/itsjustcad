// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

use glam::{DVec2, DVec3};

use crate::earcut::{earcut, signed_area};
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

/// Extrude a simple closed XY profile from `base_z` upward by `height`.
/// The profile must not repeat its first point at the end.
pub fn extrude_profile(profile: &[DVec2], base_z: f64, height: f64) -> Mesh {
    let n = profile.len();
    assert!(n >= 3, "profile needs at least 3 points");

    // Normalize to CCW so side faces wind outward
    let pts: Vec<DVec2> = if signed_area(profile) >= 0.0 {
        profile.to_vec()
    } else {
        profile.iter().rev().copied().collect()
    };

    let top_z = base_z + height;
    let mut positions = Vec::with_capacity(n * 2);
    positions.extend(pts.iter().map(|p| DVec3::new(p.x, p.y, base_z)));
    positions.extend(pts.iter().map(|p| DVec3::new(p.x, p.y, top_z)));

    let mut faces = Vec::new();
    // Sides: bottom i -> i+1, quad split into two triangles
    for i in 0..n {
        let j = (i + 1) % n;
        let (b0, b1) = (i as u32, j as u32);
        let (t0, t1) = ((i + n) as u32, (j + n) as u32);
        faces.push([b0, b1, t1]);
        faces.push([b0, t1, t0]);
    }
    // Caps via ear clipping (indices refer to the CCW pts)
    let cap = earcut(&pts);
    for [a, b, c] in &cap {
        faces.push([*a + n as u32, *b + n as u32, *c + n as u32]); // top, CCW up
        faces.push([*a, *c, *b]); // bottom, flipped to face down
    }
    Mesh::new(positions, faces)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extrude_square_is_box() {
        let profile = vec![
            DVec2::new(0.0, 0.0),
            DVec2::new(2.0, 0.0),
            DVec2::new(2.0, 2.0),
            DVec2::new(0.0, 2.0),
        ];
        let m = extrude_profile(&profile, 0.0, 3.0);
        assert_eq!(m.positions().len(), 8);
        assert_eq!(m.faces().len(), 12);
        let bb = m.aabb();
        assert_eq!(bb.size(), DVec3::new(2.0, 2.0, 3.0));
    }
}
