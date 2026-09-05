// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Buildings (plan §7.6, M-intemfit Phase 10) — footprints + stepped massing +
//! roofs, generated INSIDE the Phase-8 buildable envelope.
//!
//! Pipeline, all deterministic:
//! 1. [`footprint::footprint`] — typology + fill mode → a 2D footprint polygon
//!    (optionally with a courtyard void) inside the envelope.
//! 2. [`massing::massing`] — extrude the footprint by floors, stepping upper
//!    floors back, recording per-floor areas for GFA / FAR (Phase 11).
//! 3. [`roof::roof`] — cap the top floor's ring with flat / gable / hip / shed.
//!
//! [`build_on_envelope`] runs the whole pipeline for one lot and returns a
//! [`BuildingResult`] carrying the footprint polygon (2D), the stepped mass mesh
//! (3D), the roof mesh (3D), and the per-floor GFA data.

pub mod footprint;
pub mod massing;
pub mod roof;

pub use footprint::{footprint, Footprint};
pub use massing::{massing, Floor, Massing};
pub use roof::roof;

use crate::geometry::polygon2d::Polygon2d;
use crate::settings::{RoofType, SubdivisionSettings, Typology};
use kernel_mesh::Mesh;

/// The full result of building on one lot: the 2D footprint + 3D stepped mass +
/// roof mesh + per-floor GFA data (for Phase 11 yield / FAR).
#[derive(Debug, Clone)]
pub struct BuildingResult {
    /// The footprint outer polygon (bake as a 2D curve).
    pub footprint: Polygon2d,
    /// The courtyard void, if the typology has one.
    pub void: Option<Polygon2d>,
    /// The stepped 3D mass (bake as a mesh).
    pub mass: Mesh,
    /// The roof mesh (bake as a mesh; a flat roof is a slab).
    pub roof: Mesh,
    /// Per-floor records (index, base z, area) for yield reporting (Phase 11).
    pub floors: Vec<Floor>,
    /// Fixed mass height = floors × floor_height.
    pub height: f64,
    /// Gross floor area = Σ per-floor net areas (for FAR = GFA / site area).
    pub gfa: f64,
    /// Roof type used (per settings or per typology default).
    pub roof_type: RoofType,
}

impl BuildingResult {
    /// Achieved floor count (may be < requested if a step-back collapsed floors).
    pub fn floor_count(&self) -> usize {
        self.floors.len()
    }
}

/// The default roof for a typology when the settings request the per-typology
/// default (`RoofType::PerTypology`): detached/row → gable, courtyard → flat
/// (roofs surround the void), slab → flat.
pub fn default_roof_for(typology: Typology) -> RoofType {
    match typology {
        Typology::Detached | Typology::Row => RoofType::Gable,
        Typology::Courtyard | Typology::Slab => RoofType::Flat,
    }
}

/// Run the whole footprint → massing → roof pipeline for one lot at elevation
/// `base_z`. Returns `None` if the envelope is unbuildable (collapsed / too
/// small), which the caller reports (never a panic). Deterministic.
pub fn build_on_envelope(
    envelope: &Polygon2d,
    lot: &Polygon2d,
    base_z: f64,
    settings: &SubdivisionSettings,
) -> Option<BuildingResult> {
    let fp = footprint::footprint(envelope, lot, settings)?;
    let mass = massing::massing(&fp, base_z, settings)?;

    // Roof sits on the TOP floor's outer ring at the mass top.
    let top = mass.floors.last()?;
    let eave_z = base_z + mass.height;
    let roof_type = match settings.roof_type {
        RoofType::PerTypology => default_roof_for(settings.typology),
        rt => rt,
    };
    let roof_mesh = roof::roof(&top.outer, eave_z, roof_type, settings.roof_pitch);

    let gfa = mass.gfa();
    Some(BuildingResult {
        footprint: fp.outer,
        void: fp.void,
        mass: mass.mesh,
        roof: roof_mesh,
        floors: mass.floors,
        height: mass.height,
        gfa,
        roof_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::FootprintMode;

    fn env(w: f64, d: f64) -> Polygon2d {
        Polygon2d::from_pairs([(0.0, 0.0), (w, 0.0), (w, d), (0.0, d)]).unwrap()
    }

    #[test]
    fn full_pipeline_detached_gable() {
        let e = env(20.0, 30.0);
        let s = SubdivisionSettings {
            typology: Typology::Detached,
            footprint_mode: FootprintMode::TypologyDriven,
            floor_height: 3.0,
            floor_count: 2,
            roof_type: RoofType::PerTypology,
            roof_pitch: 30.0,
            ..SubdivisionSettings::default()
        };
        let b = build_on_envelope(&e, &e, 0.0, &s).unwrap();
        assert_eq!(b.floor_count(), 2);
        assert!((b.height - 6.0).abs() < 1e-9);
        assert!(b.gfa > 0.0);
        assert!(!b.mass.faces().is_empty());
        assert!(!b.roof.faces().is_empty());
        // PerTypology → detached gets a gable.
        assert_eq!(b.roof_type, RoofType::Gable);
        // Roof peak above the eave.
        let peak = b.roof.positions().iter().map(|p| p.z).fold(f64::MIN, f64::max);
        assert!(peak > 6.0, "gable peak {peak} above eave 6");
    }

    #[test]
    fn courtyard_carries_void_and_flat_roof() {
        let e = env(40.0, 40.0);
        let s = SubdivisionSettings {
            typology: Typology::Courtyard,
            footprint_mode: FootprintMode::TypologyDriven,
            floor_count: 3,
            roof_type: RoofType::PerTypology,
            ..SubdivisionSettings::default()
        };
        let b = build_on_envelope(&e, &e, 0.0, &s).unwrap();
        assert!(b.void.is_some(), "courtyard has a void");
        assert_eq!(b.roof_type, RoofType::Flat);
        assert_eq!(b.floor_count(), 3);
    }

    #[test]
    fn gfa_matches_sum_of_floor_areas() {
        let e = env(30.0, 30.0);
        let s = SubdivisionSettings {
            typology: Typology::Slab,
            footprint_mode: FootprintMode::FullEnvelope,
            floor_count: 4,
            stepback_start_floor: 2,
            stepback_depth: 2.0,
            ..SubdivisionSettings::default()
        };
        let b = build_on_envelope(&e, &e, 0.0, &s).unwrap();
        let want: f64 = b.floors.iter().map(|f| f.area).sum();
        assert!((b.gfa - want).abs() < 1e-9);
    }

    #[test]
    fn collapsed_envelope_yields_none() {
        let tiny = env(0.5, 0.5);
        let s = SubdivisionSettings {
            typology: Typology::Courtyard,
            footprint_mode: FootprintMode::TypologyDriven,
            ..SubdivisionSettings::default()
        };
        assert!(build_on_envelope(&tiny, &tiny, 0.0, &s).is_none());
    }

    #[test]
    fn deterministic_same_inputs() {
        let e = env(25.0, 35.0);
        let s = SubdivisionSettings {
            typology: Typology::Detached,
            floor_count: 3,
            ..SubdivisionSettings::default()
        };
        let a = build_on_envelope(&e, &e, 0.0, &s).unwrap();
        let b = build_on_envelope(&e, &e, 0.0, &s).unwrap();
        assert_eq!(a.footprint.verts(), b.footprint.verts());
        assert_eq!(a.mass.positions(), b.mass.positions());
        assert_eq!(a.roof.positions(), b.roof.positions());
        assert!((a.gfa - b.gfa).abs() < 1e-12);
    }
}
