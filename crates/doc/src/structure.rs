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

/// A tagged floor region (M-ibc): a closed boundary polygon with an occupancy
/// classification and a pre-computed plan area. Created by the logged `room`
/// verb from a closed curve selection; occupant-load / exit-count / travel-
/// distance compliance checks read these (the occupancy string keys the IBC
/// Table 1004.5 load factor carried as DATA in the check pack, not here).
///
/// Purely descriptive: the boundary polygon and area are recorded for the
/// checks and for annotation; nothing here is analyzed on its own.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Room {
    /// User label (defaults to `<occupancy>-N` when unnamed).
    pub name: String,
    /// IBC use-group family, simplified and lower-cased: "assembly",
    /// "business", "residential", "mercantile", "educational", "storage",
    /// "institutional". Keys the pack's occupant-load factor table.
    pub occupancy: String,
    /// Plan (XY) area of the boundary polygon, square meters (shoelace).
    pub area: f64,
    /// Boundary polygon vertices in world space, meters (not closed — the
    /// first vertex is not repeated at the end). Used for the centroid and as
    /// the travel-distance start point.
    pub boundary: Vec<[f64; 3]>,
}

impl Room {
    /// Plan centroid of the boundary polygon (mean of the vertices), meters.
    /// `[0,0,z]` for an empty boundary.
    pub fn centroid(&self) -> [f64; 3] {
        if self.boundary.is_empty() {
            return [0.0, 0.0, 0.0];
        }
        let n = self.boundary.len() as f64;
        let mut c = [0.0f64; 3];
        for p in &self.boundary {
            c[0] += p[0];
            c[1] += p[1];
            c[2] += p[2];
        }
        [c[0] / n, c[1] / n, c[2] / n]
    }
}
