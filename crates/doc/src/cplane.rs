// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Construction plane (CPlane / UCS): a custom coordinate frame the user types
//! coordinates against. A CPlane is defined by an `origin` and an orthonormal
//! basis (`x_axis`, `y_axis`, `normal`); a typed CPlane-space point `p` maps to
//! world as `origin + p.x*x_axis + p.y*y_axis + p.z*normal`.
//!
//! Replay-stability note: the active CPlane is TRANSIENT session/input state,
//! never part of the op-log (see `Session::run` in the commands crate). Typed
//! points are resolved to WORLD at entry time and the logged op carries world
//! coords, so replay — which always starts from the world (identity) CPlane —
//! reproduces byte-identical geometry regardless of any later CPlane change.

use glam::{DMat4, DVec3};

/// An orthonormal construction plane. Default = world XY (identity).
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CPlane {
    /// World-space origin of the plane.
    pub origin: DVec3,
    /// Unit vector: the plane's local +X direction, in world space.
    pub x_axis: DVec3,
    /// Unit vector: the plane's local +Y direction, in world space.
    pub y_axis: DVec3,
    /// Unit vector: the plane's normal (local +Z), in world space.
    pub normal: DVec3,
}

impl Default for CPlane {
    /// The world XY plane: origin at 0, axes aligned with world X/Y/Z.
    fn default() -> Self {
        Self::world()
    }
}

impl CPlane {
    /// The world XY construction plane (identity transform).
    pub fn world() -> Self {
        Self {
            origin: DVec3::ZERO,
            x_axis: DVec3::X,
            y_axis: DVec3::Y,
            normal: DVec3::Z,
        }
    }

    /// True if this is (numerically) the world XY plane, so the CPlane transform
    /// can be skipped as a no-op.
    pub fn is_world(&self) -> bool {
        const EPS: f64 = 1e-12;
        self.origin.abs_diff_eq(DVec3::ZERO, EPS)
            && self.x_axis.abs_diff_eq(DVec3::X, EPS)
            && self.y_axis.abs_diff_eq(DVec3::Y, EPS)
            && self.normal.abs_diff_eq(DVec3::Z, EPS)
    }

    /// True if the basis is rotated relative to world — i.e. `x_axis` is not
    /// (anti)parallel to world +X or `y_axis` is not (anti)parallel to world +Y.
    /// A pure translation / +Z-offset plane (world-aligned axes, any origin) is
    /// NOT rotated. Verbs whose extents stay world-axis-aligned (rect/box/circle)
    /// are only correct on a non-rotated plane; they use this as a guard.
    pub fn is_rotated(&self) -> bool {
        const EPS: f64 = 1e-9;
        // |dot| ~ 1 means (anti)parallel to the world axis.
        let x_aligned = self.x_axis.normalize_or_zero().dot(DVec3::X).abs() > 1.0 - EPS;
        let y_aligned = self.y_axis.normalize_or_zero().dot(DVec3::Y).abs() > 1.0 - EPS;
        !(x_aligned && y_aligned)
    }

    /// Build a CPlane from an `origin` and a `normal`. The in-plane X axis is
    /// chosen to align as closely as possible with world +X (so an upright,
    /// +Z-normal plane keeps world-aligned axes — the intuitive "draw at
    /// elevation" case: typed (1,2) → world (1,2)); Y completes a right-handed
    /// frame. Returns `None` if `normal` is degenerate.
    pub fn from_origin_normal(origin: DVec3, normal: DVec3) -> Option<Self> {
        let n = normal.try_normalize()?;
        // Project world +X onto the plane; if the normal IS ±X (so world X has
        // no in-plane component), fall back to projecting world +Y instead.
        let mut x_ref = DVec3::X - n * DVec3::X.dot(n);
        if x_ref.length_squared() < 1e-12 {
            x_ref = DVec3::Y - n * DVec3::Y.dot(n);
        }
        let x = x_ref.try_normalize()?;
        let y = n.cross(x).normalize();
        Some(Self { origin, x_axis: x, y_axis: y, normal: n })
    }

    /// Build a CPlane from three points: `origin`, a point on the +X axis, and a
    /// point in the +XY half-plane (defines Y's side). Y is orthogonalized
    /// against X (Gram–Schmidt); the normal is X×Y. Returns `None` if the points
    /// are collinear/coincident.
    pub fn from_three_points(origin: DVec3, on_x: DVec3, on_xy: DVec3) -> Option<Self> {
        let x = (on_x - origin).try_normalize()?;
        let in_plane = on_xy - origin;
        // Remove the X component so Y is perpendicular to X but on `on_xy`'s side.
        let y_raw = in_plane - x * in_plane.dot(x);
        let y = y_raw.try_normalize()?;
        let normal = x.cross(y).try_normalize()?;
        Some(Self { origin, x_axis: x, y_axis: y, normal })
    }

    /// Map a CPlane-space point to world space.
    pub fn to_world(&self, p: DVec3) -> DVec3 {
        self.origin + self.x_axis * p.x + self.y_axis * p.y + self.normal * p.z
    }

    /// Map a world-space point back into CPlane space (inverse of `to_world`).
    /// The basis is orthonormal, so the inverse is the transpose applied to the
    /// offset from the origin.
    pub fn to_cplane(&self, world: DVec3) -> DVec3 {
        let d = world - self.origin;
        DVec3::new(d.dot(self.x_axis), d.dot(self.y_axis), d.dot(self.normal))
    }

    /// The `world_from_cplane` affine matrix, for callers (e.g. rendering the
    /// plane grid) that prefer a `DMat4`.
    pub fn world_from_cplane(&self) -> DMat4 {
        DMat4::from_cols(
            self.x_axis.extend(0.0),
            self.y_axis.extend(0.0),
            self.normal.extend(0.0),
            self.origin.extend(1.0),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn world_is_identity() {
        let c = CPlane::world();
        assert!(c.is_world());
        assert_eq!(c.to_world(DVec3::new(1.0, 2.0, 3.0)), DVec3::new(1.0, 2.0, 3.0));
    }

    #[test]
    fn elevated_z_plane_translates() {
        // Origin lifted 10 in Z, normal +Z -> a typed (1,2,0) lands at (1,2,10).
        let c = CPlane::from_origin_normal(DVec3::new(0.0, 0.0, 10.0), DVec3::Z).unwrap();
        let w = c.to_world(DVec3::new(1.0, 2.0, 0.0));
        assert!(w.abs_diff_eq(DVec3::new(1.0, 2.0, 10.0), 1e-12), "got {w}");
    }

    #[test]
    fn orthonormal_basis_from_normal() {
        let c = CPlane::from_origin_normal(DVec3::ZERO, DVec3::new(1.0, 1.0, 1.0)).unwrap();
        assert!((c.x_axis.length() - 1.0).abs() < 1e-12);
        assert!((c.y_axis.length() - 1.0).abs() < 1e-12);
        assert!(c.x_axis.dot(c.y_axis).abs() < 1e-12);
        assert!(c.x_axis.dot(c.normal).abs() < 1e-12);
        assert!(c.y_axis.dot(c.normal).abs() < 1e-12);
    }

    #[test]
    fn three_point_frame() {
        // origin, +X at (2,0,0), XY point at (0,3,0) -> world XY plane back.
        let c = CPlane::from_three_points(
            DVec3::ZERO,
            DVec3::new(2.0, 0.0, 0.0),
            DVec3::new(0.0, 3.0, 0.0),
        )
        .unwrap();
        assert!(c.x_axis.abs_diff_eq(DVec3::X, 1e-12));
        assert!(c.y_axis.abs_diff_eq(DVec3::Y, 1e-12));
        assert!(c.normal.abs_diff_eq(DVec3::Z, 1e-12));
    }

    #[test]
    fn to_cplane_round_trips() {
        let c = CPlane::from_origin_normal(DVec3::new(1.0, 2.0, 3.0), DVec3::new(0.0, 1.0, 1.0))
            .unwrap();
        let p = DVec3::new(4.0, -5.0, 6.0);
        let back = c.to_cplane(c.to_world(p));
        assert!(back.abs_diff_eq(p, 1e-10), "got {back}");
    }

    #[test]
    fn degenerate_inputs_return_none() {
        assert!(CPlane::from_origin_normal(DVec3::ZERO, DVec3::ZERO).is_none());
        assert!(CPlane::from_three_points(DVec3::ZERO, DVec3::ZERO, DVec3::Y).is_none());
        // Collinear: on_xy on the X axis -> no Y direction.
        assert!(CPlane::from_three_points(
            DVec3::ZERO,
            DVec3::new(1.0, 0.0, 0.0),
            DVec3::new(2.0, 0.0, 0.0)
        )
        .is_none());
    }
}
