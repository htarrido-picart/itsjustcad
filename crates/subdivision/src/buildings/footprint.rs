// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Building footprints (plan §7.6, Phase 10).
//!
//! A footprint is a 2D polygon that lives INSIDE the Phase-8 buildable envelope
//! (`setbacks::buildable_envelope`). The typology drives the shape; the fill
//! mode drives how much of the envelope the footprint claims:
//!
//! - [`FootprintMode::FullEnvelope`] — the footprint IS the envelope.
//! - [`FootprintMode::CoverageRatio`] — a centred rectangle whose area is
//!   `coverage_frac × lot_area`, clamped to the envelope (lot-coverage %).
//! - [`FootprintMode::Inset`] — the envelope inset by a fixed margin.
//! - [`FootprintMode::TypologyDriven`] — the typology picks the shape (DEFAULT).
//!
//! Typologies (all owner-scoped):
//! - [`Typology::Detached`] — a compact mass centred in the envelope.
//! - [`Typology::Row`] — a terrace / party-wall block that FILLS the lot width
//!   (side = 0, matching euro_latam medianería) with a front/rear margin.
//! - [`Typology::Courtyard`] — a ring footprint with an interior void (a hole).
//! - [`Typology::Slab`] — a single large apartment mass ~= the envelope.
//!
//! A [`Footprint`] carries the outer polygon plus an optional inner void
//! (courtyard). Massing (Phase 10, `massing.rs`) extrudes it; roofs
//! (`roof.rs`) cap it.
//!
//! Everything here is deterministic: shape depends only on the envelope + lot +
//! settings, never on a random seed. A collapsed envelope yields `None`.

use crate::geometry::polygon2d::Polygon2d;
use crate::settings::{FootprintMode, SubdivisionSettings, Typology};
use glam::DVec2;

/// A building footprint: an outer polygon, optionally with an interior void
/// (courtyard). Both rings live inside the buildable envelope.
#[derive(Debug, Clone)]
pub struct Footprint {
    /// The outer footprint ring (CCW).
    pub outer: Polygon2d,
    /// The interior void for a courtyard typology (a hole), if any.
    pub void: Option<Polygon2d>,
    /// The typology this footprint was built for (drives roof defaults).
    pub typology: Typology,
}

impl Footprint {
    /// Net footprint area = outer area minus any interior void.
    pub fn area(&self) -> f64 {
        self.outer.area() - self.void.as_ref().map(|v| v.area()).unwrap_or(0.0)
    }
}

/// Build a footprint for one lot given its buildable `envelope` and `settings`.
/// Returns `None` when the envelope is degenerate or the requested shape
/// collapses (e.g. a courtyard whose ring is too thin). Deterministic.
pub fn footprint(
    envelope: &Polygon2d,
    lot: &Polygon2d,
    settings: &SubdivisionSettings,
) -> Option<Footprint> {
    if envelope.area() < 1e-6 {
        return None;
    }
    match settings.footprint_mode {
        FootprintMode::FullEnvelope => Some(Footprint {
            outer: envelope.clone(),
            void: None,
            typology: settings.typology,
        }),
        FootprintMode::CoverageRatio => coverage_footprint(envelope, lot, settings),
        FootprintMode::Inset => inset_footprint(envelope, settings),
        FootprintMode::TypologyDriven => typology_footprint(envelope, settings),
    }
}

/// Coverage-ratio mode: a rectangle centred in the envelope whose area is
/// `coverage_frac × lot_area`, clamped to the envelope's bounding box so it
/// never exceeds the envelope. This is the lot-coverage (%) planning control.
fn coverage_footprint(
    envelope: &Polygon2d,
    lot: &Polygon2d,
    settings: &SubdivisionSettings,
) -> Option<Footprint> {
    let frac = settings.coverage_frac.clamp(0.01, 1.0);
    let target = frac * lot.area();
    let (lo, hi) = envelope.aabb();
    let ew = (hi.x - lo.x).max(1e-6);
    let eh = (hi.y - lo.y).max(1e-6);
    let env_area = ew * eh;
    // Scale the envelope's bbox down uniformly to hit the target area, but never
    // above the envelope bbox itself.
    let s = (target / env_area).clamp(1e-6, 1.0).sqrt();
    let w = ew * s;
    let h = eh * s;
    let c = envelope.centroid();
    centered_rect(c, w, h, settings.typology)
}

/// Inset mode: the envelope's bounding rectangle inset by a fixed margin on all
/// sides (a simple, robust "pull the walls in" footprint).
fn inset_footprint(envelope: &Polygon2d, settings: &SubdivisionSettings) -> Option<Footprint> {
    let (lo, hi) = envelope.aabb();
    // A modest default inset if none is meaningful; reuse setback_side as a hint.
    let margin = settings.footprint_inset.max(0.0);
    let w = (hi.x - lo.x) - 2.0 * margin;
    let h = (hi.y - lo.y) - 2.0 * margin;
    if w <= 1e-3 || h <= 1e-3 {
        return None;
    }
    let c = envelope.centroid();
    centered_rect(c, w, h, settings.typology)
}

/// Typology-driven mode (DEFAULT): the typology dictates how the footprint fills
/// the envelope.
fn typology_footprint(envelope: &Polygon2d, settings: &SubdivisionSettings) -> Option<Footprint> {
    let (lo, hi) = envelope.aabb();
    let ew = (hi.x - lo.x).max(1e-6);
    let eh = (hi.y - lo.y).max(1e-6);
    let c = envelope.centroid();
    match settings.typology {
        // Detached: a compact mass centred in the envelope, claiming ~70% of the
        // envelope bbox in each dimension so it reads as a free-standing house.
        Typology::Detached => centered_rect(c, ew * 0.7, eh * 0.7, Typology::Detached),
        // Row/terrace: fills the FULL lot width (party-wall, side = 0) with a
        // front/rear margin. The envelope already spans the full width under
        // euro_latam side = 0, so claim full width and ~80% of the depth.
        Typology::Row => centered_rect(c, ew, eh * 0.8, Typology::Row),
        // Slab: a large single apartment mass ~= the envelope (95%).
        Typology::Slab => centered_rect(c, ew * 0.95, eh * 0.95, Typology::Slab),
        // Courtyard: a ring footprint (outer ~= envelope) with an interior void.
        Typology::Courtyard => courtyard_footprint(c, ew, eh),
    }
}

/// Build an axis-aligned rectangle centred at `c` with width `w`, height `h`,
/// tagged with `typology`. `None` if degenerate.
fn centered_rect(c: DVec2, w: f64, h: f64, typology: Typology) -> Option<Footprint> {
    if w <= 1e-3 || h <= 1e-3 {
        return None;
    }
    let hw = w * 0.5;
    let hh = h * 0.5;
    let outer = Polygon2d::from_pairs([
        (c.x - hw, c.y - hh),
        (c.x + hw, c.y - hh),
        (c.x + hw, c.y + hh),
        (c.x - hw, c.y + hh),
    ])?;
    Some(Footprint { outer, void: None, typology })
}

/// Courtyard: outer rectangle at ~90% of the envelope with a concentric
/// interior void at ~40% (leaving a ~25% wall band). `None` if the ring would be
/// too thin to be a real building.
fn courtyard_footprint(c: DVec2, ew: f64, eh: f64) -> Option<Footprint> {
    let ow = ew * 0.9;
    let oh = eh * 0.9;
    let iw = ew * 0.4;
    let ih = eh * 0.4;
    // Wall band must be a real thickness on both axes.
    if (ow - iw) * 0.5 < 1.0 || (oh - ih) * 0.5 < 1.0 {
        return None;
    }
    let outer = centered_rect(c, ow, oh, Typology::Courtyard)?.outer;
    let void = Polygon2d::from_pairs([
        (c.x - iw * 0.5, c.y - ih * 0.5),
        (c.x + iw * 0.5, c.y - ih * 0.5),
        (c.x + iw * 0.5, c.y + ih * 0.5),
        (c.x - iw * 0.5, c.y + ih * 0.5),
    ])?;
    Some(Footprint { outer, void: Some(void), typology: Typology::Courtyard })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(w: f64, d: f64) -> Polygon2d {
        Polygon2d::from_pairs([(0.0, 0.0), (w, 0.0), (w, d), (0.0, d)]).unwrap()
    }

    fn settings(typ: Typology, mode: FootprintMode) -> SubdivisionSettings {
        SubdivisionSettings {
            typology: typ,
            footprint_mode: mode,
            coverage_frac: 0.5,
            footprint_inset: 2.0,
            ..SubdivisionSettings::default()
        }
    }

    fn inside(outer: &Polygon2d, env: &Polygon2d) -> bool {
        // Every footprint vertex is inside (or on) the envelope bbox + tol.
        let (lo, hi) = env.aabb();
        outer
            .verts()
            .iter()
            .all(|v| v.x >= lo.x - 1e-6 && v.x <= hi.x + 1e-6 && v.y >= lo.y - 1e-6 && v.y <= hi.y + 1e-6)
    }

    #[test]
    fn detached_is_centered_and_smaller_than_envelope() {
        let e = env(20.0, 30.0);
        let s = settings(Typology::Detached, FootprintMode::TypologyDriven);
        let fp = footprint(&e, &e, &s).unwrap();
        assert!(inside(&fp.outer, &e));
        assert!(fp.area() < e.area(), "detached should not fill the envelope");
        // Centred: footprint centroid ~= envelope centroid.
        assert!(fp.outer.centroid().distance(e.centroid()) < 1e-6);
        assert!(fp.void.is_none());
    }

    #[test]
    fn row_fills_full_width() {
        let e = env(12.0, 30.0);
        let s = settings(Typology::Row, FootprintMode::TypologyDriven);
        let fp = footprint(&e, &e, &s).unwrap();
        let (lo, hi) = fp.outer.aabb();
        // Row spans the full envelope width (party-wall side = 0).
        assert!((lo.x - 0.0).abs() < 1e-6 && (hi.x - 12.0).abs() < 1e-6, "row must fill lot width");
        // But not the full depth (front/rear margin).
        assert!((hi.y - lo.y) < 30.0);
    }

    #[test]
    fn courtyard_has_a_hole() {
        let e = env(40.0, 40.0);
        let s = settings(Typology::Courtyard, FootprintMode::TypologyDriven);
        let fp = footprint(&e, &e, &s).unwrap();
        assert!(fp.void.is_some(), "courtyard must have an interior void");
        let void = fp.void.as_ref().unwrap();
        assert!(void.area() > 0.0);
        // Net area (outer minus void) is strictly between void and outer areas.
        assert!(fp.area() < fp.outer.area());
        assert!(fp.area() > 0.0);
    }

    #[test]
    fn slab_nearly_fills_envelope() {
        let e = env(30.0, 50.0);
        let s = settings(Typology::Slab, FootprintMode::TypologyDriven);
        let fp = footprint(&e, &e, &s).unwrap();
        // Slab claims most of the envelope.
        assert!(fp.area() > 0.8 * e.area(), "slab area {} vs env {}", fp.area(), e.area());
    }

    #[test]
    fn full_envelope_equals_envelope() {
        let e = env(20.0, 30.0);
        let s = settings(Typology::Detached, FootprintMode::FullEnvelope);
        let fp = footprint(&e, &e, &s).unwrap();
        assert!((fp.area() - e.area()).abs() < 1e-6, "full = envelope area");
    }

    #[test]
    fn coverage_hits_target_fraction() {
        // coverage_frac 0.5 of the lot (== envelope here) → ~0.5 * area, ±tol.
        let e = env(40.0, 40.0); // 1600 m²
        let s = settings(Typology::Detached, FootprintMode::CoverageRatio);
        let fp = footprint(&e, &e, &s).unwrap();
        let want = 0.5 * 1600.0;
        assert!((fp.area() - want).abs() / want < 0.05, "coverage area {} vs {}", fp.area(), want);
    }

    #[test]
    fn inset_pulls_walls_in() {
        let e = env(20.0, 30.0);
        let s = settings(Typology::Detached, FootprintMode::Inset);
        let fp = footprint(&e, &e, &s).unwrap();
        let (lo, hi) = fp.outer.aabb();
        // Inset by 2 on all sides → 16 × 26.
        assert!((hi.x - lo.x - 16.0).abs() < 1e-6, "width {}", hi.x - lo.x);
        assert!((hi.y - lo.y - 26.0).abs() < 1e-6, "height {}", hi.y - lo.y);
    }

    #[test]
    fn degenerate_envelope_yields_none() {
        let tiny = Polygon2d::from_pairs([(0.0, 0.0), (0.001, 0.0), (0.001, 0.001), (0.0, 0.001)]).unwrap();
        let s = settings(Typology::Detached, FootprintMode::TypologyDriven);
        assert!(footprint(&tiny, &tiny, &s).is_none());
    }
}
