// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Curve kernel: 2D/3D primitives (line, polyline, arc, ellipse) and NURBS curves.
//!
//! Rectangle, polygon and circle are constructed as closed polylines / full
//! arcs by the command layer — the enum stays minimal.

mod analysis;
mod biarc;
mod build;
mod curve;
mod nurbs;
mod ops;

pub use analysis::{CurvatureSample, curvature_profile};
pub use biarc::{BiarcFit, Seg2, fit_biarcs};
pub use build::{helix, interpolate_curve, rebuild};
pub use curve::{Curve, clamped_uniform_knots};
pub use nurbs::{insert_knot, nurbs_point};
pub use ops::{
    JOIN_TOL, chamfer_curves, chamfer_lines, closest_point, extend, fillet_curves,
    fillet_curves_at, fillet_lines, intersections, join_curves, split_at_points,
};
