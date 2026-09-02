// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Sketch-constraint records stored on the document. Pure data — the actual
//! solving lives in the `commands` crate (which maps these onto the
//! `itsjustcad-constraints` solver). Constraints reference document objects
//! (lines, circles) by id; point-level references are resolved once when the
//! constraint is created and stored here so repeated solves are stable.

use serde::{Deserialize, Serialize};

use crate::object::ObjectId;

/// A specific point on a document object: a line endpoint (`index` 0 = a,
/// 1 = b), or a circle center (`index` 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PointRef {
    pub object: ObjectId,
    pub index: u8,
}

/// One sketch constraint over document geometry. Dimensional values are in
/// document units (meters); angles in degrees.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SketchConstraint {
    /// Two points (line endpoints / circle centers) at the same location.
    Coincident { a: PointRef, b: PointRef },
    Horizontal { line: ObjectId },
    Vertical { line: ObjectId },
    /// Distance between two resolved points.
    Distance { a: PointRef, b: PointRef, value: f64 },
    /// Perpendicular distance from a point to a line.
    DistanceToLine { point: PointRef, line: ObjectId, value: f64 },
    /// Segment length of a line.
    Length { line: ObjectId, value: f64 },
    /// Angle between two lines, degrees.
    Angle { a: ObjectId, b: ObjectId, degrees: f64 },
    Parallel { a: ObjectId, b: ObjectId },
    Perpendicular { a: ObjectId, b: ObjectId },
    /// Equal segment lengths (two lines).
    EqualLength { a: ObjectId, b: ObjectId },
    /// Equal radii (two circles).
    EqualRadius { a: ObjectId, b: ObjectId },
    Radius { circle: ObjectId, value: f64 },
    /// Anchor an entire object (all its solver points + radius) where it is.
    Fixed { object: ObjectId },
    /// Line tangent to a circle.
    Tangent { line: ObjectId, circle: ObjectId },
    /// Two circles tangent.
    TangentCircles { a: ObjectId, b: ObjectId },
    /// Point lies on the (infinite) line.
    PointOnLine { point: PointRef, line: ObjectId },
    /// Point lies on the circle.
    PointOnCircle { point: PointRef, circle: ObjectId },
    /// Point at the midpoint of the line.
    Midpoint { point: PointRef, line: ObjectId },
}

impl SketchConstraint {
    /// Short verb-style name for listings.
    pub fn kind_name(&self) -> &'static str {
        match self {
            SketchConstraint::Coincident { .. } => "coincident",
            SketchConstraint::Horizontal { .. } => "horizontal",
            SketchConstraint::Vertical { .. } => "vertical",
            SketchConstraint::Distance { .. } => "distance",
            SketchConstraint::DistanceToLine { .. } => "distance",
            SketchConstraint::Length { .. } => "length",
            SketchConstraint::Angle { .. } => "angle",
            SketchConstraint::Parallel { .. } => "parallel",
            SketchConstraint::Perpendicular { .. } => "perpendicular",
            SketchConstraint::EqualLength { .. } => "equal",
            SketchConstraint::EqualRadius { .. } => "equal",
            SketchConstraint::Radius { .. } => "radius",
            SketchConstraint::Fixed { .. } => "fixed",
            SketchConstraint::Tangent { .. } => "tangent",
            SketchConstraint::TangentCircles { .. } => "tangent",
            SketchConstraint::PointOnLine { .. } => "on",
            SketchConstraint::PointOnCircle { .. } => "on",
            SketchConstraint::Midpoint { .. } => "midpoint",
        }
    }

    /// Every object this constraint references.
    pub fn objects(&self) -> Vec<ObjectId> {
        match self {
            SketchConstraint::Coincident { a, b } => vec![a.object, b.object],
            SketchConstraint::Horizontal { line } | SketchConstraint::Vertical { line } => {
                vec![*line]
            }
            SketchConstraint::Distance { a, b, .. } => vec![a.object, b.object],
            SketchConstraint::DistanceToLine { point, line, .. } => vec![point.object, *line],
            SketchConstraint::Length { line, .. } => vec![*line],
            SketchConstraint::Angle { a, b, .. }
            | SketchConstraint::Parallel { a, b }
            | SketchConstraint::Perpendicular { a, b }
            | SketchConstraint::EqualLength { a, b }
            | SketchConstraint::EqualRadius { a, b }
            | SketchConstraint::TangentCircles { a, b } => vec![*a, *b],
            SketchConstraint::Radius { circle, .. } => vec![*circle],
            SketchConstraint::Fixed { object } => vec![*object],
            SketchConstraint::Tangent { line, circle } => vec![*line, *circle],
            SketchConstraint::PointOnLine { point, line }
            | SketchConstraint::Midpoint { point, line } => vec![point.object, *line],
            SketchConstraint::PointOnCircle { point, circle } => vec![point.object, *circle],
        }
    }
}
