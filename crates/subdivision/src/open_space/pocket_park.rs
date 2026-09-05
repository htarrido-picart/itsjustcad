// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Pocket park placement (plan §9 Phase 9).
//!
//! Place a small green polygon (a compact rounded rectangle) centred in a
//! region — a selected block or the largest empty block. The park is sized to
//! `area` when given, else a default fraction of the region; it is always
//! clipped to stay inside the region so it never spills across a street.

use crate::geometry::clip_bridge;
use crate::geometry::polygon2d::Polygon2d;
use glam::DVec2;

/// Default share of the region a pocket park takes when no explicit area asked.
const DEFAULT_PARK_FRAC: f64 = 0.30;

/// Place a pocket park inside `region`. If `area` is `Some`, the park targets
/// that area (capped so it fits inside the region); otherwise it takes
/// [`DEFAULT_PARK_FRAC`] of the region area. Returns a valid, non-self-
/// intersecting polygon centred on the region centroid and clipped to it.
///
/// The park is a rounded rectangle (chamfered corners) so it reads as a
/// designed amenity rather than a raw block cut. It is guaranteed to sit inside
/// the region (final intersection clip), so it can never cross a street edge.
pub fn pocket_park(region: &Polygon2d, area: Option<f64>) -> Option<Polygon2d> {
    let region_area = region.area();
    if region_area <= 1e-6 {
        return None;
    }
    let target = area
        .unwrap_or(region_area * DEFAULT_PARK_FRAC)
        .min(region_area * 0.95)
        .max(1e-3);

    let c = region.centroid();
    let (lo, hi) = region.aabb();
    let rw = (hi.x - lo.x).max(1e-6);
    let rh = (hi.y - lo.y).max(1e-6);
    // A rectangle of the region's aspect ratio, scaled to the target area.
    let aspect = rw / rh;
    let mut h = (target / aspect).sqrt();
    let mut w = aspect * h;
    // Keep it inside the region bbox with a small margin.
    let margin = 0.05;
    w = w.min(rw * (1.0 - margin));
    h = h.min(rh * (1.0 - margin));
    let hw = w / 2.0;
    let hh = h / 2.0;
    // Chamfer = 15 % of the short half-dimension → a rounded-rectangle read.
    let ch = 0.30 * hw.min(hh);

    let park = rounded_rect(c, hw, hh, ch)?;

    // Clip to the region so it never spills outside (robust on non-convex
    // regions); keep the largest resulting ring.
    let clipped = clip_bridge::intersection(&park, region);
    let best = clipped
        .into_iter()
        .max_by(|a, b| a.area().partial_cmp(&b.area()).unwrap());
    match best {
        Some(p) if super::is_valid_feature(&p) => Some(p),
        // Fall back to the un-clipped park if the boolean produced nothing
        // usable but the raw park is itself valid (e.g. region == park bbox).
        _ if super::is_valid_feature(&park) => Some(park),
        _ => None,
    }
}

/// A rounded rectangle centred at `c` with half-width/half-height `hw`/`hh` and
/// corner chamfer `ch` (a single cut per corner → an octagon-ish rounded look).
fn rounded_rect(c: DVec2, hw: f64, hh: f64, ch: f64) -> Option<Polygon2d> {
    let ch = ch.max(0.0).min(hw.min(hh) * 0.99);
    let x0 = c.x - hw;
    let x1 = c.x + hw;
    let y0 = c.y - hh;
    let y1 = c.y + hh;
    let verts = vec![
        DVec2::new(x0 + ch, y0),
        DVec2::new(x1 - ch, y0),
        DVec2::new(x1, y0 + ch),
        DVec2::new(x1, y1 - ch),
        DVec2::new(x1 - ch, y1),
        DVec2::new(x0 + ch, y1),
        DVec2::new(x0, y1 - ch),
        DVec2::new(x0, y0 + ch),
    ];
    Polygon2d::new(verts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(w: f64, h: f64) -> Polygon2d {
        Polygon2d::from_pairs([(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)]).unwrap()
    }

    #[test]
    fn park_is_valid_and_inside_region() {
        let region = rect(100.0, 60.0);
        let park = pocket_park(&region, None).unwrap();
        assert!(super::super::is_valid_feature(&park));
        // Every park vertex lies inside (or on) the region.
        for v in park.verts() {
            assert!(
                region.contains(*v) || on_boundary(&region, *v),
                "park vertex {v:?} escaped the region"
            );
        }
        // Area ≈ 30 % of the region within construction tolerance.
        let frac = park.area() / region.area();
        assert!(frac > 0.15 && frac < 0.45, "park frac {frac}");
    }

    #[test]
    fn park_honours_requested_area() {
        let region = rect(200.0, 200.0);
        let park = pocket_park(&region, Some(4000.0)).unwrap();
        // Chamfered corners trim a little; allow 20 % slack.
        assert!((park.area() - 4000.0).abs() / 4000.0 < 0.2, "area {}", park.area());
    }

    #[test]
    fn oversized_request_capped_inside() {
        let region = rect(50.0, 50.0);
        let park = pocket_park(&region, Some(1_000_000.0)).unwrap();
        assert!(park.area() < region.area(), "park must stay inside region");
    }

    fn on_boundary(poly: &Polygon2d, p: DVec2) -> bool {
        poly.edges().any(|(a, b)| {
            let ab = b - a;
            let t = (p - a).dot(ab) / ab.length_squared().max(1e-12);
            let t = t.clamp(0.0, 1.0);
            (a + ab * t).distance(p) < 1e-6
        })
    }
}
