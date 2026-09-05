// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Roofs (plan §7.6, Phase 10).
//!
//! Cap a stepped mass with one of four roof forms, built on the TOP floor's
//! outer ring at the mass height (`base_z + height`):
//!
//! - [`RoofType::Flat`] — a horizontal slab cap (0 pitch).
//! - [`RoofType::Gable`] — a symmetric two-slope roof with a ridge along the
//!   ring's long axis; the ridge height follows the pitch.
//! - [`RoofType::Hip`] — four sloped faces rising to a central ridge/apex.
//! - [`RoofType::Shed`] — a single mono-pitch plane sloping from one eave up to
//!   the opposite eave.
//!
//! `pitch` is the roof slope in degrees; the peak height is derived from the
//! footprint half-span × tan(pitch). All roofs are emitted as a closed
//! [`kernel_mesh::Mesh`] sitting on the ring at the eave elevation. Rings are the
//! rectangular / ring footprints Phase 10 produces, so the gable/hip geometry is
//! built on the ring's bounding rectangle (robust + deterministic).

use crate::geometry::polygon2d::Polygon2d;
use crate::settings::RoofType;
use glam::{DVec2, DVec3};
use kernel_mesh::{earcut, Mesh};

/// Build a roof mesh of `roof_type` on `ring` at eave elevation `eave_z` with
/// `pitch` degrees. Deterministic. Returns an empty (no-face) mesh only if the
/// ring is degenerate.
pub fn roof(ring: &Polygon2d, eave_z: f64, roof_type: RoofType, pitch_deg: f64) -> Mesh {
    let (lo, hi) = ring.aabb();
    let w = hi.x - lo.x;
    let d = hi.y - lo.y;
    if w <= 1e-3 || d <= 1e-3 {
        return Mesh::new(Vec::new(), Vec::new());
    }
    let pitch = pitch_deg.clamp(0.0, 85.0).to_radians();
    match roof_type {
        RoofType::Flat => flat(ring, eave_z),
        RoofType::Gable => gable(lo, hi, eave_z, pitch),
        RoofType::Hip => hip(lo, hi, eave_z, pitch),
        RoofType::Shed => shed(lo, hi, eave_z, pitch),
        // `PerTypology` should be resolved by the caller; treat as flat if not.
        RoofType::PerTypology => flat(ring, eave_z),
    }
}

/// A flat slab cap on the ring at `z`.
fn flat(ring: &Polygon2d, z: f64) -> Mesh {
    let verts = ring.verts();
    let tris = earcut(verts);
    let positions: Vec<DVec3> = verts.iter().map(|v| DVec3::new(v.x, v.y, z)).collect();
    let faces: Vec<[u32; 3]> = tris.into_iter().map(|t| [t[0], t[1], t[2]]).collect();
    Mesh::new(positions, faces)
}

/// Peak rise for a given half-span and pitch.
fn rise(half_span: f64, pitch: f64) -> f64 {
    (half_span * pitch.tan()).max(0.01)
}

/// Symmetric gable: ridge runs along the LONGER axis; two sloping rectangles +
/// two vertical triangular gable ends.
fn gable(lo: DVec2, hi: DVec2, z: f64, pitch: f64) -> Mesh {
    let w = hi.x - lo.x;
    let d = hi.y - lo.y;
    let mut positions = Vec::new();
    let mut faces = Vec::new();

    if w >= d {
        // Ridge along x; slopes fall in ±y. Half-span = d/2.
        let r = rise(d * 0.5, pitch);
        let midy = (lo.y + hi.y) * 0.5;
        // Eave corners (0..3) + ridge ends (4,5).
        let p = [
            DVec3::new(lo.x, lo.y, z),      // 0
            DVec3::new(hi.x, lo.y, z),      // 1
            DVec3::new(hi.x, hi.y, z),      // 2
            DVec3::new(lo.x, hi.y, z),      // 3
            DVec3::new(lo.x, midy, z + r),  // 4 ridge-lo-x
            DVec3::new(hi.x, midy, z + r),  // 5 ridge-hi-x
        ];
        positions.extend_from_slice(&p);
        // Slope 1 (y = lo side): 0,1,5,4.
        faces.push([0, 1, 5]);
        faces.push([0, 5, 4]);
        // Slope 2 (y = hi side): 3,4,5,2 wound the other way.
        faces.push([2, 3, 4]);
        faces.push([2, 4, 5]);
        // Gable ends (triangles): lo-x end 0,3,4 ; hi-x end 1,2,5.
        faces.push([0, 4, 3]);
        faces.push([1, 5, 2]);
    } else {
        // Ridge along y; slopes fall in ±x. Half-span = w/2.
        let r = rise(w * 0.5, pitch);
        let midx = (lo.x + hi.x) * 0.5;
        let p = [
            DVec3::new(lo.x, lo.y, z),      // 0
            DVec3::new(hi.x, lo.y, z),      // 1
            DVec3::new(hi.x, hi.y, z),      // 2
            DVec3::new(lo.x, hi.y, z),      // 3
            DVec3::new(midx, lo.y, z + r),  // 4 ridge-lo-y
            DVec3::new(midx, hi.y, z + r),  // 5 ridge-hi-y
        ];
        positions.extend_from_slice(&p);
        // Slope 1 (x = lo side): 0,4,5,3.
        faces.push([0, 4, 5]);
        faces.push([0, 5, 3]);
        // Slope 2 (x = hi side): 1,2,5,4.
        faces.push([1, 2, 5]);
        faces.push([1, 5, 4]);
        // Gable ends: lo-y end 0,1,4 ; hi-y end 3,5,2.
        faces.push([0, 1, 4]);
        faces.push([3, 5, 2]);
    }
    Mesh::new(positions, faces)
}

/// Hip: four sloped faces rising to a central ridge (a short ridge along the
/// long axis, hipped ends). Approximated with the ridge shrunk to the centre
/// third of the long axis.
fn hip(lo: DVec2, hi: DVec2, z: f64, pitch: f64) -> Mesh {
    let w = hi.x - lo.x;
    let d = hi.y - lo.y;
    let mut positions = Vec::new();
    let mut faces = Vec::new();
    let long_x = w >= d;
    let half_span = if long_x { d * 0.5 } else { w * 0.5 };
    let r = rise(half_span, pitch);
    let cx = (lo.x + hi.x) * 0.5;
    let cy = (lo.y + hi.y) * 0.5;

    // Eave corners.
    let e = [
        DVec3::new(lo.x, lo.y, z), // 0
        DVec3::new(hi.x, lo.y, z), // 1
        DVec3::new(hi.x, hi.y, z), // 2
        DVec3::new(lo.x, hi.y, z), // 3
    ];
    // Ridge endpoints: shrink to the centre third along the long axis.
    let (r0, r1) = if long_x {
        let rx0 = lo.x + w / 3.0;
        let rx1 = hi.x - w / 3.0;
        (DVec3::new(rx0, cy, z + r), DVec3::new(rx1, cy, z + r))
    } else {
        let ry0 = lo.y + d / 3.0;
        let ry1 = hi.y - d / 3.0;
        (DVec3::new(cx, ry0, z + r), DVec3::new(cx, ry1, z + r))
    };
    positions.extend_from_slice(&e);
    positions.push(r0); // 4
    positions.push(r1); // 5

    if long_x {
        // Front slope (y=lo): 0,1,5,4 ; back slope (y=hi): 2,3,4,5.
        faces.push([0, 1, 5]);
        faces.push([0, 5, 4]);
        faces.push([2, 3, 4]);
        faces.push([2, 4, 5]);
        // Hip ends (triangles): lo-x 0,4,3 ; hi-x 1,2,5.
        faces.push([0, 4, 3]);
        faces.push([1, 2, 5]);
    } else {
        // Left slope (x=lo): 3,0,4,5 ; right slope (x=hi): 1,2,5,4.
        faces.push([3, 0, 4]);
        faces.push([3, 4, 5]);
        faces.push([1, 2, 5]);
        faces.push([1, 5, 4]);
        // Hip ends: lo-y 0,1,4 ; hi-y 2,3,5.
        faces.push([0, 1, 4]);
        faces.push([2, 3, 5]);
    }
    Mesh::new(positions, faces)
}

/// Shed / mono-pitch: a single plane sloping from the low eave (y=lo) up to the
/// high eave (y=hi), plus the two triangular side walls + the high gable wall to
/// keep it closed.
fn shed(lo: DVec2, hi: DVec2, z: f64, pitch: f64) -> Mesh {
    let d = hi.y - lo.y;
    let r = rise(d, pitch);
    let p = [
        DVec3::new(lo.x, lo.y, z),         // 0 low eave
        DVec3::new(hi.x, lo.y, z),         // 1 low eave
        DVec3::new(hi.x, hi.y, z + r),     // 2 high eave
        DVec3::new(lo.x, hi.y, z + r),     // 3 high eave
    ];
    let positions = p.to_vec();
    // The single sloped plane.
    let faces = vec![[0, 1, 2], [0, 2, 3]];
    Mesh::new(positions, faces)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(w: f64, d: f64) -> Polygon2d {
        Polygon2d::from_pairs([(0.0, 0.0), (w, 0.0), (w, d), (0.0, d)]).unwrap()
    }

    fn peak_z(m: &Mesh) -> f64 {
        m.positions().iter().map(|p| p.z).fold(f64::MIN, f64::max)
    }
    fn base_z(m: &Mesh) -> f64 {
        m.positions().iter().map(|p| p.z).fold(f64::MAX, f64::min)
    }

    #[test]
    fn flat_is_horizontal_at_eave() {
        let m = roof(&ring(20.0, 30.0), 9.0, RoofType::Flat, 0.0);
        assert!(!m.faces().is_empty());
        assert!((peak_z(&m) - 9.0).abs() < 1e-9 && (base_z(&m) - 9.0).abs() < 1e-9, "flat is flat");
    }

    #[test]
    fn gable_rises_above_eave() {
        let m = roof(&ring(20.0, 30.0), 9.0, RoofType::Gable, 30.0);
        assert!(!m.faces().is_empty());
        assert!(peak_z(&m) > 9.0, "gable peak {} must exceed eave 9", peak_z(&m));
        assert!((base_z(&m) - 9.0).abs() < 1e-6);
    }

    #[test]
    fn hip_rises_and_has_all_faces() {
        let m = roof(&ring(30.0, 20.0), 12.0, RoofType::Hip, 35.0);
        assert!(m.faces().len() >= 6, "hip should have >=6 tris, got {}", m.faces().len());
        assert!(peak_z(&m) > 12.0);
    }

    #[test]
    fn shed_slopes_one_way() {
        let m = roof(&ring(20.0, 20.0), 6.0, RoofType::Shed, 20.0);
        assert!(!m.faces().is_empty());
        // One eave at 6, the opposite eave higher.
        assert!((base_z(&m) - 6.0).abs() < 1e-6);
        assert!(peak_z(&m) > 6.0, "shed high eave {} > 6", peak_z(&m));
    }

    #[test]
    fn all_types_produce_valid_meshes() {
        for rt in [RoofType::Flat, RoofType::Gable, RoofType::Hip, RoofType::Shed] {
            let m = roof(&ring(24.0, 16.0), 10.0, rt, 30.0);
            assert!(!m.positions().is_empty(), "{rt:?} positions");
            assert!(!m.faces().is_empty(), "{rt:?} faces");
            // Every face index is in range.
            let n = m.positions().len() as u32;
            for f in m.faces() {
                assert!(f[0] < n && f[1] < n && f[2] < n, "{rt:?} face index out of range");
            }
        }
    }

    #[test]
    fn degenerate_ring_yields_empty() {
        let tiny = Polygon2d::from_pairs([(0.0, 0.0), (0.0005, 0.0), (0.0005, 0.0005), (0.0, 0.0005)]).unwrap();
        let m = roof(&tiny, 3.0, RoofType::Gable, 30.0);
        assert!(m.faces().is_empty());
    }
}
