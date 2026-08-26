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

/// Read a STEP (AP203/214/242) file through OCCT. STEP carries **exact** BREP
/// solids; OCCT reads the analytic geometry and we tessellate it into our
/// [`kernel_mesh::Mesh`] (with the exact volume from the boundary integral) so
/// it round-trips into the document like any other imported solid.
///
/// Returns `None` when the `occt` feature is not compiled in — the caller must
/// then report that STEP needs the exact-BREP tier (there is no pure-Rust STEP
/// reader here). `Some(Err(..))` is a genuine read/tessellation failure.
pub fn read_step(path: &str) -> Option<Result<ExactResult, String>> {
    #[cfg(not(feature = "occt"))]
    {
        let _ = path;
        None
    }
    #[cfg(feature = "occt")]
    {
        Some(imp::read_step(path))
    }
}

/// Write triangle-mesh geometry to a STEP file through OCCT as a **faceted**
/// shell (one planar BREP face per triangle). This is a valid STEP AP242 part,
/// but it is faceted, not analytic — the document stores meshes, so there is no
/// exact surface to emit. Callers must not present this as exact-BREP export.
///
/// Returns the number of faces written. `None` when the feature is off.
pub fn write_mesh_step(
    positions: &[DVec3],
    faces: &[[u32; 3]],
    path: &str,
) -> Option<Result<usize, String>> {
    #[cfg(not(feature = "occt"))]
    {
        let _ = (positions, faces, path);
        None
    }
    #[cfg(feature = "occt")]
    {
        Some(imp::write_mesh_step(positions, faces, path))
    }
}

/// Write an exact axis-aligned box solid to STEP (exact BREP). Used to prove the
/// exact STEP round-trip; `None` when the feature is off.
pub fn write_box_step(corner: DVec3, size: DVec3, path: &str) -> Option<Result<(), String>> {
    #[cfg(not(feature = "occt"))]
    {
        let _ = (corner, size, path);
        None
    }
    #[cfg(feature = "occt")]
    {
        Some(imp::write_box_step(corner, size, path))
    }
}

#[cfg(feature = "occt")]
mod imp {
    use super::{BoolOp, ExactResult};
    use glam::DVec3;
    use kernel_mesh::Mesh;
    use opencascade::primitives::{Compound, Shape, Wire};

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

    pub fn read_step(path: &str) -> Result<ExactResult, String> {
        let shape = Shape::read_step(path).map_err(|e| format!("STEP read failed: {e}"))?;
        let occ = shape
            .mesh_with_tolerance(0.01)
            .map_err(|e| format!("STEP tessellation failed: {e}"))?;
        let mesh = to_mesh(&occ);
        if mesh.faces().is_empty() {
            return Err("STEP file contained no tessellable solid/shell".to_string());
        }
        let volume = kernel_mesh::signed_volume(&mesh).abs();
        Ok(ExactResult { mesh, volume })
    }

    /// One planar BREP face per triangle, collected into a compound. A compound
    /// of faces is a valid STEP part; it is faceted (not analytic) geometry.
    pub fn write_mesh_step(
        positions: &[DVec3],
        faces: &[[u32; 3]],
        path: &str,
    ) -> Result<usize, String> {
        if faces.is_empty() {
            return Err("no triangles to write to STEP".to_string());
        }
        let mut face_shapes: Vec<Shape> = Vec::with_capacity(faces.len());
        for t in faces {
            let pts: Vec<DVec3> = t.iter().map(|&i| positions[i as usize]).collect();
            // Skip degenerate triangles (a wire from collapsed points can't face).
            if pts[0] == pts[1] || pts[1] == pts[2] || pts[0] == pts[2] {
                continue;
            }
            let wire = match Wire::from_ordered_points(pts.iter().map(|p| to_oc(*p))) {
                Ok(w) => w,
                Err(_) => continue,
            };
            face_shapes.push(wire.to_face().into());
        }
        if face_shapes.is_empty() {
            return Err("all triangles were degenerate; nothing to write".to_string());
        }
        let count = face_shapes.len();
        let compound = Compound::from_shapes(face_shapes.iter());
        let shape: Shape = compound.into();
        shape
            .write_step(path)
            .map_err(|e| format!("STEP write failed: {e}"))?;
        Ok(count)
    }

    pub fn write_box_step(corner: DVec3, size: DVec3, path: &str) -> Result<(), String> {
        let shape = shape_box(corner, size);
        shape
            .write_step(path)
            .map_err(|e| format!("STEP write failed: {e}"))
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

    #[cfg(not(feature = "occt"))]
    #[test]
    fn step_helpers_are_inert_without_feature() {
        assert!(read_step("/nonexistent.step").is_none());
        assert!(write_mesh_step(&[], &[], "/tmp/x.step").is_none());
        assert!(write_box_step(DVec3::ZERO, DVec3::splat(1.0), "/tmp/x.step").is_none());
    }

    // -- exact STEP round-trip (only with the exact kernel compiled in) --

    #[cfg(feature = "occt")]
    #[test]
    fn exact_box_step_round_trip_preserves_volume() {
        // Write an exact 2x3x4 box solid to STEP, read it back, and confirm the
        // exact-BREP volume survives the AP242 round-trip (= 24, analytic).
        let path = std::env::temp_dir().join("ijc_step_rt_box.step");
        let p = path.to_string_lossy().to_string();
        write_box_step(DVec3::new(1.0, 2.0, 3.0), DVec3::new(2.0, 3.0, 4.0), &p)
            .expect("feature on")
            .expect("write ok");
        let back = read_step(&p).expect("feature on").expect("read ok");
        assert!(
            (back.volume - 24.0).abs() < 1e-6,
            "STEP round-trip volume {} != 24",
            back.volume
        );
        assert!(!back.mesh.faces().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(feature = "occt")]
    #[test]
    fn exact_boolean_result_step_round_trip() {
        // The exact difference (volume 840) written to STEP and re-read keeps its
        // volume — a boolean BREP survives the exact handoff, not just a box.
        let path = std::env::temp_dir().join("ijc_step_rt_bool.step");
        let p = path.to_string_lossy().to_string();
        // Build the exact difference shape directly and write it.
        {
            use opencascade::primitives::Shape;
            let a = Shape::box_from_corners(
                oc_glam::dvec3(0.0, 0.0, 0.0),
                oc_glam::dvec3(10.0, 10.0, 10.0),
            );
            let b = Shape::box_from_corners(
                oc_glam::dvec3(3.0, 3.0, -5.0),
                oc_glam::dvec3(7.0, 7.0, 15.0),
            );
            a.subtract(&b).shape.write_step(&p).expect("write ok");
        }
        let back = read_step(&p).expect("feature on").expect("read ok");
        assert!(
            (back.volume - 840.0).abs() < 1e-6,
            "STEP boolean round-trip volume {} != 840",
            back.volume
        );
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(feature = "occt")]
    #[test]
    fn faceted_mesh_step_writes_and_reads_back() {
        // A single triangle mesh written as a faceted STEP part reads back with
        // faces (it is a shell of planar faces, faceted not analytic).
        let path = std::env::temp_dir().join("ijc_step_facet.step");
        let p = path.to_string_lossy().to_string();
        let positions = vec![
            DVec3::new(0.0, 0.0, 0.0),
            DVec3::new(1.0, 0.0, 0.0),
            DVec3::new(0.0, 1.0, 0.0),
        ];
        let faces = vec![[0u32, 1, 2]];
        let n = write_mesh_step(&positions, &faces, &p)
            .expect("feature on")
            .expect("write ok");
        assert_eq!(n, 1);
        assert!(std::path::Path::new(&p).exists());
        let _ = std::fs::remove_file(&path);
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
