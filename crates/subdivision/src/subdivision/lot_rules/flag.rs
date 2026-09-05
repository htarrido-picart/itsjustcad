// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! `FlagLots` (plan §7.4, Phase 6) — panhandle lots reached by a narrow pole.
//! Built ONLY when `allow_flag_lots` (euro_latam false). The pole width must be
//! ≥ `flag_pole_width_min` (euro_latam 3 m). **The pole area is EXCLUDED from the
//! countable lot area** (plan §7.4 open — implemented here as the exclusion; it
//! is an ASSUMPTION to confirm with Manuel, surfaced by the placeholder note).

use crate::geometry::polygon2d::Polygon2d;
use glam::DVec2;

/// A flag / panhandle lot: the full polygon plus the pole (access strip)
/// sub-polygon whose area does NOT count toward the buildable lot area.
#[derive(Debug, Clone)]
pub struct FlagLot {
    /// The complete lot outline (flag + pole), as baked.
    pub outline: Polygon2d,
    /// The access pole (narrow strip to the street).
    pub pole: Polygon2d,
    /// Pole ROW width used (≥ `flag_pole_width_min`).
    pub pole_width: f64,
}

impl FlagLot {
    /// Total (gross) area including the pole.
    pub fn gross_area(&self) -> f64 {
        self.outline.area()
    }

    /// Countable (net) lot area — the pole is EXCLUDED (§7.4 assumption).
    pub fn countable_area(&self) -> f64 {
        (self.outline.area() - self.pole.area()).max(0.0)
    }
}

/// Build a flag lot from a `flag` body (the rear parcel) and a pole reaching to
/// the street. Returns `None` when `allow_flag_lots` is false or the pole width
/// is below `min_pole_width` (the euro_latam 3 m floor). `pole` is expected to be
/// a rectangle of width `pole_width` running from the flag body to the street.
pub fn make_flag_lot(
    body: &Polygon2d,
    pole: &Polygon2d,
    pole_width: f64,
    allow: bool,
    min_pole_width: f64,
) -> Option<FlagLot> {
    if !allow {
        return None;
    }
    if pole_width + 1e-9 < min_pole_width {
        return None;
    }
    // Union the body + pole into a single outline. Fall back to the body if the
    // union is degenerate (disjoint) — still a valid flag lot for accounting.
    let unioned = crate::geometry::clip_bridge::union(body, pole);
    let outline = unioned
        .into_iter()
        .max_by(|a, b| a.area().partial_cmp(&b.area()).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or_else(|| body.clone());
    Some(FlagLot {
        outline,
        pole: pole.clone(),
        pole_width,
    })
}

/// Convenience: a rectangular pole of `width` from `from` to `to` (street).
pub fn rectangular_pole(from: DVec2, to: DVec2, width: f64) -> Option<Polygon2d> {
    let axis = to - from;
    if axis.length_squared() < 1e-12 || width <= 0.0 {
        return None;
    }
    let dir = axis.normalize();
    let normal = DVec2::new(-dir.y, dir.x) * (width * 0.5);
    Polygon2d::new(vec![
        from + normal,
        to + normal,
        to - normal,
        from - normal,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body() -> Polygon2d {
        Polygon2d::from_pairs([(0.0, 20.0), (20.0, 20.0), (20.0, 40.0), (0.0, 40.0)]).unwrap()
    }

    #[test]
    fn disabled_returns_none() {
        let pole = rectangular_pole(DVec2::new(10.0, 20.0), DVec2::new(10.0, 0.0), 3.0).unwrap();
        assert!(make_flag_lot(&body(), &pole, 3.0, false, 3.0).is_none());
    }

    #[test]
    fn pole_below_min_rejected() {
        let pole = rectangular_pole(DVec2::new(10.0, 20.0), DVec2::new(10.0, 0.0), 2.0).unwrap();
        assert!(make_flag_lot(&body(), &pole, 2.0, true, 3.0).is_none());
    }

    #[test]
    fn pole_area_excluded_from_countable() {
        // Pole: 3 m wide × 20 m long = 60 m². Body: 20×20 = 400 m².
        let pole = rectangular_pole(DVec2::new(10.0, 20.0), DVec2::new(10.0, 0.0), 3.0).unwrap();
        let f = make_flag_lot(&body(), &pole, 3.0, true, 3.0).unwrap();
        assert!((f.pole.area() - 60.0).abs() < 1.0, "pole area {}", f.pole.area());
        // Countable excludes the pole → gross − 60.
        assert!(
            (f.gross_area() - f.countable_area() - 60.0).abs() < 1.5,
            "exclusion off: gross {} net {}",
            f.gross_area(),
            f.countable_area()
        );
        assert!(f.countable_area() < f.gross_area());
    }
}
