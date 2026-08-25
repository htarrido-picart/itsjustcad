// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Structural loads and supports stored on the [`crate::Document`].
//!
//! This module is **data only** — "model here, analyze elsewhere".  Values are
//! recorded for exchange (IFC, JSON op-log) but no structural analysis is ever
//! performed here.

use glam::DVec3;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Loads
// ---------------------------------------------------------------------------

/// The three kinds of structural load ItsJustCAD can record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoadGeometry {
    /// Concentrated force at a single point.
    Point {
        /// World-space position (may coincide with a node or any coordinate).
        position: DVec3,
    },
    /// Distributed force along a frame member or arbitrary line.
    Line {
        /// Start point.
        a: DVec3,
        /// End point.
        b: DVec3,
    },
    /// Pressure on a planar area (slab or wall surface).
    Area {
        /// Closed boundary polygon (≥ 3 points).
        boundary: Vec<DVec3>,
    },
}

/// One structural load: a magnitude (N, N/m, or Pa depending on geometry kind),
/// a direction (unit vector, stored as given — the user is responsible for the
/// coordinate frame), and the geometry it acts on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StructLoad {
    /// Human label, e.g. "dead" or "live-floor".
    pub name: String,
    /// Force magnitude in SI units: N (point), N/m (line), Pa (area).
    pub magnitude: f64,
    /// Direction the load acts in world space (stored normalised).
    pub direction: DVec3,
    /// Where the load is applied.
    pub geometry: LoadGeometry,
}

// ---------------------------------------------------------------------------
// Supports / restraints
// ---------------------------------------------------------------------------

/// Restraint type at a point.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestraintKind {
    /// Pinned: all translations fixed, all rotations free.
    Pinned,
    /// Fixed: all 6 DOFs fixed.
    Fixed,
    /// Roller: one translation free (along `axis`), others fixed.
    Roller,
}

impl RestraintKind {
    pub fn label(self) -> &'static str {
        match self {
            RestraintKind::Pinned => "pinned",
            RestraintKind::Fixed => "fixed",
            RestraintKind::Roller => "roller",
        }
    }
}

/// A support / boundary condition at a single point.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StructSupport {
    /// World-space position of the support node.
    pub position: DVec3,
    /// What kind of restraint it is.
    pub kind: RestraintKind,
    /// For `Roller`, the free-translation axis (world space, normalised).
    /// Ignored for `Pinned` and `Fixed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roller_axis: Option<DVec3>,
}
