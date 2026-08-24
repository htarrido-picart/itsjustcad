//! Curve kernel: 2D/3D primitives (line, polyline, arc, ellipse) and NURBS curves.
//!
//! Rectangle, polygon and circle are constructed as closed polylines / full
//! arcs by the command layer — the enum stays minimal.

mod build;
mod curve;
mod nurbs;
mod ops;

pub use build::{helix, interpolate_curve, rebuild};
pub use curve::{clamped_uniform_knots, Curve};
pub use nurbs::nurbs_point;
pub use ops::{
    closest_point, extend, fillet_lines, intersections, join_curves, split_at_points, JOIN_TOL,
};
