//! Curve kernel: 2D/3D primitives (line, polyline, arc, ellipse) and NURBS curves.
//!
//! Rectangle, polygon and circle are constructed as closed polylines / full
//! arcs by the command layer — the enum stays minimal.

mod curve;
mod nurbs;

pub use curve::{clamped_uniform_knots, Curve};
pub use nurbs::nurbs_point;
