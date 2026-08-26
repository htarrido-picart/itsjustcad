// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Opt-in exact-BREP tier over OpenCASCADE (OCCT).
//!
//! This crate is a thin, dependency-clean boundary around the exact solid
//! kernel. It is **feature-gated**: with the default feature set it compiles to
//! a stub with NO OCCT, NO CMake, and NO C++ toolchain requirement, so the
//! normal build and the Linux build are unaffected. Enable the `occt` feature
//! to statically link OCCT (built from source by `occt-sys`) and get real
//! exact booleans and fillets on box solids.
//!
//! The document's mesh kernel (`kernel-mesh`) stays the default everywhere;
//! this tier is only consulted when a caller explicitly opts in and the feature
//! is compiled. Results are tessellated back into our [`kernel_mesh::Mesh`] so
//! they round-trip into the document and render like any other solid. For
//! polyhedral results (box booleans) the tessellation of the exact planar faces
//! is geometrically exact, so the reported volume matches the analytic value to
//! floating-point precision.

use glam::DVec3;
use kernel_mesh::Mesh;

/// The three exact solid booleans.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoolOp {
    /// A ∪ B.
    Union,
    /// A \ B (B is the tool).
    Difference,
    /// A ∩ B.
    Intersection,
}

/// The tessellated exact-BREP result plus its exact volume.
#[derive(Clone, Debug)]
pub struct ExactResult {
    /// Triangulated boundary of the exact solid, in f64 document space.
    pub mesh: Mesh,
    /// Exact volume of the result (analytic for polyhedral inputs).
    pub volume: f64,
}

/// Is the exact-BREP tier compiled in? `false` on the default build.
pub const fn available() -> bool {
    cfg!(feature = "occt")
}

/// Exact boolean of two axis-aligned boxes, each given by a corner and a size
/// vector (edge lengths along x/y/z). Returns `None` when the `occt` feature is
/// not compiled in — the caller should then fall back to the mesh kernel.
///
/// The inputs are exact (corner + size), so the result is an exact BREP solid;
/// we tessellate it into our mesh type and also report the exact volume.
pub fn box_boolean(
    a_corner: DVec3,
    a_size: DVec3,
    b_corner: DVec3,
    b_size: DVec3,
    op: BoolOp,
) -> Option<ExactResult> {
    #[cfg(not(feature = "occt"))]
    {
        let _ = (a_corner, a_size, b_corner, b_size, op);
        None
    }
    #[cfg(feature = "occt")]
    {
        Some(imp::box_boolean(a_corner, a_size, b_corner, b_size, op))
    }
}

/// Exact difference of a box minus a box, then fillet **all** the edges of the
/// result with `radius`. Returns `None` when the feature is off. This exercises
/// a genuine exact fillet (rolling-ball blend) on a solid, not a mesh
/// approximation. The result is no longer polyhedral, so its volume is the
/// exact OCCT tessellation's volume at the given triangulation tolerance.
pub fn box_difference_filleted(
    a_corner: DVec3,
    a_size: DVec3,
    b_corner: DVec3,
    b_size: DVec3,
    radius: f64,
) -> Option<ExactResult> {
    #[cfg(not(feature = "occt"))]
    {
        let _ = (a_corner, a_size, b_corner, b_size, radius);
        None
    }
    #[cfg(feature = "occt")]
    {
        Some(imp::box_difference_filleted(
            a_corner, a_size, b_corner, b_size, radius,
        ))
    }
}

#[cfg(feature = "occt")]
mod imp {
    use super::{BoolOp, ExactResult};
    use glam::DVec3;
    use kernel_mesh::Mesh;
    use opencascade::primitives::Shape;

    // opencascade 0.3's API speaks glam 0.24; convert at the boundary.
    fn to_oc(v: DVec3) -> oc_glam::DVec3 {
        oc_glam::dvec3(v.x, v.y, v.z)
    }
    fn from_oc(v: oc_glam::DVec3) -> DVec3 {
        DVec3::new(v.x, v.y, v.z)
    }

    fn shape_box(corner: DVec3, size: DVec3) -> Shape {
        Shape::box_from_corners(to_oc(corner), to_oc(corner + size))
    }

    /// Convert an OCCT mesh (per-face vertices, usize indices) into our indexed
    /// `Mesh`. Box booleans yield planar faces, so this loses no geometry.
    fn to_mesh(m: &opencascade::mesh::Mesh) -> Mesh {
        let positions: Vec<DVec3> = m.vertices.iter().map(|v| from_oc(*v)).collect();
        let faces: Vec<[u32; 3]> = m
            .indices
            .as_chunks::<3>()
            .0
            .iter()
            .map(|t| [t[0] as u32, t[1] as u32, t[2] as u32])
            .collect();
        Mesh::new(positions, faces)
    }

    pub fn box_boolean(
        a_corner: DVec3,
        a_size: DVec3,
        b_corner: DVec3,
        b_size: DVec3,
        op: BoolOp,
    ) -> ExactResult {
        let a = shape_box(a_corner, a_size);
        let b = shape_box(b_corner, b_size);
        let result: Shape = match op {
            BoolOp::Union => a.union(&b).shape,
            BoolOp::Difference => a.subtract(&b).shape,
            BoolOp::Intersection => a.intersect(&b).shape,
        };
        // Fine tolerance: for planar faces this only affects the number of
        // triangles, never their geometry, so the volume stays exact.
        let occ = result.mesh_with_tolerance(0.001).expect("triangulation");
        let mesh = to_mesh(&occ);
        let volume = kernel_mesh::signed_volume(&mesh).abs();
        ExactResult { mesh, volume }
    }

    pub fn box_difference_filleted(
        a_corner: DVec3,
        a_size: DVec3,
        b_corner: DVec3,
        b_size: DVec3,
        radius: f64,
    ) -> ExactResult {
        let a = shape_box(a_corner, a_size);
        let b = shape_box(b_corner, b_size);
        let cut = a.subtract(&b).shape;
        let filleted = cut.fillet(radius);
        let occ = filleted.mesh_with_tolerance(0.01).expect("triangulation");
        let mesh = to_mesh(&occ);
        let volume = kernel_mesh::signed_volume(&mesh).abs();
        ExactResult { mesh, volume }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_matches_feature() {
        assert_eq!(available(), cfg!(feature = "occt"));
    }

    #[cfg(not(feature = "occt"))]
    #[test]
    fn stub_returns_none_without_feature() {
        let r = box_boolean(
            DVec3::ZERO,
            DVec3::splat(1.0),
            DVec3::ZERO,
            DVec3::splat(1.0),
            BoolOp::Union,
        );
        assert!(r.is_none(), "exact tier must be inert when feature is off");
    }

    // -- exact-op tests (only when the exact kernel is compiled in) --

    #[cfg(feature = "occt")]
    #[test]
    fn exact_difference_volume_is_analytic() {
        // 10^3 box minus a 4x4 column punched all the way through in z.
        // overlap = 4*4*10 = 160, so result = 1000 - 160 = 840, exactly.
        let r = box_boolean(
            DVec3::ZERO,
            DVec3::new(10.0, 10.0, 10.0),
            DVec3::new(3.0, 3.0, -5.0),
            DVec3::new(4.0, 4.0, 20.0),
            BoolOp::Difference,
        )
        .expect("feature on");
        assert!(
            (r.volume - 840.0).abs() < 1e-6,
            "exact difference volume {} != 840",
            r.volume
        );
        assert!(!r.mesh.faces().is_empty());
    }

    #[cfg(feature = "occt")]
    #[test]
    fn exact_union_of_disjoint_boxes_is_sum() {
        // two disjoint 2^3 cubes -> 8 + 8 = 16.
        let r = box_boolean(
            DVec3::ZERO,
            DVec3::splat(2.0),
            DVec3::new(5.0, 0.0, 0.0),
            DVec3::splat(2.0),
            BoolOp::Union,
        )
        .expect("feature on");
        assert!(
            (r.volume - 16.0).abs() < 1e-6,
            "exact union volume {} != 16",
            r.volume
        );
    }

    #[cfg(feature = "occt")]
    #[test]
    fn exact_intersection_volume_is_overlap() {
        // intersection of the 10^3 box and the 4x4x20 column = 160.
        let r = box_boolean(
            DVec3::ZERO,
            DVec3::new(10.0, 10.0, 10.0),
            DVec3::new(3.0, 3.0, -5.0),
            DVec3::new(4.0, 4.0, 20.0),
            BoolOp::Intersection,
        )
        .expect("feature on");
        assert!(
            (r.volume - 160.0).abs() < 1e-6,
            "exact intersection volume {} != 160",
            r.volume
        );
    }

    #[cfg(feature = "occt")]
    #[test]
    fn exact_fillet_reduces_volume_below_the_sharp_cut() {
        // A 10^3 box with a 4x4 notch, all outer edges rounded by r=0.5.
        // Filleting a convex box removes material at every edge/corner, so the
        // filleted volume must be strictly less than the sharp 840, but close.
        let sharp = box_boolean(
            DVec3::ZERO,
            DVec3::new(10.0, 10.0, 10.0),
            DVec3::new(3.0, 3.0, -5.0),
            DVec3::new(4.0, 4.0, 20.0),
            BoolOp::Difference,
        )
        .expect("feature on");
        let filleted = box_difference_filleted(
            DVec3::ZERO,
            DVec3::new(10.0, 10.0, 10.0),
            DVec3::new(3.0, 3.0, -5.0),
            DVec3::new(4.0, 4.0, 20.0),
            0.5,
        )
        .expect("feature on");
        assert!(
            filleted.volume < sharp.volume,
            "fillet {} should remove material vs sharp {}",
            filleted.volume,
            sharp.volume
        );
        assert!(
            filleted.volume > sharp.volume - 20.0,
            "fillet {} removed implausibly much vs sharp {}",
            filleted.volume,
            sharp.volume
        );
        assert!(!filleted.mesh.faces().is_empty());
    }
}
