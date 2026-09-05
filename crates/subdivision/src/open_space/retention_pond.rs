// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Retention-pond placement (plan §9 Phase 9).
//!
//! Place a rounded polygon (a near-circular basin) at a low/selected area. The
//! pond is sized to `area` when given, else a default fraction of the region,
//! and clipped to stay inside the region.
//!
//! **Advisory:** this is a design-intent basin FOOTPRINT, not a sized detention
//! volume. It carries no hydrology (storage, outflow, storm event) — the
//! commands layer surfaces the no-false-precision note.

use crate::geometry::clip_bridge;
use crate::geometry::polygon2d::Polygon2d;
use glam::DVec2;

/// Default share of the region a pond takes when no explicit area asked.
const DEFAULT_POND_FRAC: f64 = 0.20;

/// Vertices around the pond ring — enough to read as rounded, few enough to stay
/// a light polygon.
const POND_SEGMENTS: usize = 24;

/// Place a retention pond inside `region`. Sized to `area` (capped to fit) or
/// [`DEFAULT_POND_FRAC`] of the region. Returns a valid near-circular polygon
/// centred on the region centroid, clipped to the region.
pub fn retention_pond(region: &Polygon2d, area: Option<f64>) -> Option<Polygon2d> {
    let region_area = region.area();
    if region_area <= 1e-6 {
        return None;
    }
    let target = area
        .unwrap_or(region_area * DEFAULT_POND_FRAC)
        .min(region_area * 0.9)
        .max(1e-3);

    let c = region.centroid();
    // Radius for a regular n-gon of the target area:
    // A = 0.5 · n · r² · sin(2π/n) → r = sqrt(2A / (n·sin(2π/n))).
    let n = POND_SEGMENTS;
    let step = std::f64::consts::TAU / n as f64;
    let r = (2.0 * target / (n as f64 * step.sin())).sqrt();

    // Keep the disc inside the region bbox.
    let (lo, hi) = region.aabb();
    let max_r = ((hi.x - lo.x).min(hi.y - lo.y) / 2.0 * 0.95).max(1e-3);
    let r = r.min(max_r);

    let ring: Vec<DVec2> = (0..n)
        .map(|i| {
            let a = step * i as f64;
            DVec2::new(c.x + r * a.cos(), c.y + r * a.sin())
        })
        .collect();
    let pond = Polygon2d::new(ring)?;

    let clipped = clip_bridge::intersection(&pond, region);
    let best = clipped
        .into_iter()
        .max_by(|a, b| a.area().partial_cmp(&b.area()).unwrap());
    match best {
        Some(p) if super::is_valid_feature(&p) => Some(p),
        _ if super::is_valid_feature(&pond) => Some(pond),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(w: f64, h: f64) -> Polygon2d {
        Polygon2d::from_pairs([(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)]).unwrap()
    }

    #[test]
    fn pond_is_valid_and_rounded() {
        let region = rect(200.0, 200.0);
        let pond = retention_pond(&region, None).unwrap();
        assert!(super::super::is_valid_feature(&pond));
        assert!(pond.len() >= 12, "pond should read as rounded");
    }

    #[test]
    fn pond_honours_requested_area() {
        let region = rect(300.0, 300.0);
        let pond = retention_pond(&region, Some(5000.0)).unwrap();
        // Regular n-gon area is exact by construction.
        assert!((pond.area() - 5000.0).abs() / 5000.0 < 0.02, "area {}", pond.area());
    }

    #[test]
    fn pond_stays_inside_small_region() {
        let region = rect(40.0, 40.0);
        let pond = retention_pond(&region, Some(1_000_000.0)).unwrap();
        assert!(pond.area() < region.area());
    }
}
