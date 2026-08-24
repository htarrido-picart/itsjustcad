// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Structural modeling data stored on the [`crate::Document`]: named sections,
//! materials, grids, and stories/levels. This is interoperability-oriented
//! ("model here, analyze elsewhere"): material properties are recorded but never
//! analyzed. Frame and area members live as [`crate::Geometry`] variants; the
//! definitions here are the reusable named tables they reference.

use serde::{Deserialize, Serialize};

pub use kernel_mesh::StructSection as Section;

/// A named structural material. Elastic modulus and density are stored for
/// downstream exchange/analysis; nothing here performs analysis.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Material {
    /// Elastic (Young's) modulus, Pa.
    pub elastic_modulus_e: f64,
    /// Mass density, kg/m³.
    pub density: f64,
}

/// A labeled reference grid: named axes at fixed X and Y coordinates, plus
/// optional level lines (elevations). Rendered as reference lines and bubbles.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Grid {
    /// X-axis lines: (label, x-coordinate). Conventionally A, B, C…
    pub x_axes: Vec<(String, f64)>,
    /// Y-axis lines: (label, y-coordinate). Conventionally 1, 2, 3…
    pub y_axes: Vec<(String, f64)>,
    /// Optional level lines (elevations, meters) drawn as horizontal references.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub levels: Vec<f64>,
}

/// A building story / level: a name and its elevation in meters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Story {
    pub name: String,
    pub elevation: f64,
}
