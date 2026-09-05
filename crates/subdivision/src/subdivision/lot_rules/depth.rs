// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! `DepthController` (plan §7.4, Phase 6) — an INDEPENDENT depth target, separate
//! from area. euro_latam default 25 m ±5 (placeholder). A lot's "depth" is its
//! extent perpendicular to its street frontage; where no frontage edge is known
//! we use the OBB long extent. The controller does not itself cut geometry (that
//! is the width-mix frontage slicer's job) — it reports whether each lot's depth
//! sits inside the target band, and clamps the depth used when the frontage
//! slicer builds a lot strip.

use crate::geometry::oriented_box::OrientedBox;
use crate::geometry::polygon2d::Polygon2d;

/// An independent depth target band (metres).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DepthController {
    pub target: f64,
    pub tolerance: f64,
}

impl DepthController {
    pub fn new(target: f64, tolerance: f64) -> DepthController {
        DepthController { target, tolerance }
    }

    /// The lower / upper bounds of the acceptable depth band.
    pub fn band(&self) -> (f64, f64) {
        ((self.target - self.tolerance).max(0.0), self.target + self.tolerance)
    }

    /// Clamp a requested depth into the band. When the target is 0 (disabled),
    /// pass the value through unchanged.
    pub fn clamp(&self, depth: f64) -> f64 {
        if self.target <= 0.0 {
            return depth;
        }
        let (lo, hi) = self.band();
        depth.clamp(lo, hi)
    }

    /// True if `depth` is within the band (or the target is disabled).
    pub fn in_band(&self, depth: f64) -> bool {
        if self.target <= 0.0 {
            return true;
        }
        let (lo, hi) = self.band();
        depth >= lo - 1e-6 && depth <= hi + 1e-6
    }

    /// Estimate a lot polygon's depth as its OBB long extent (a good proxy for
    /// front-to-rear depth on a rectangular residential lot).
    pub fn lot_depth(poly: &Polygon2d) -> f64 {
        OrientedBox::of_polygon(poly).map(|ob| ob.long_len()).unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_is_target_plus_minus_tol() {
        let d = DepthController::new(25.0, 5.0);
        assert_eq!(d.band(), (20.0, 30.0));
    }

    #[test]
    fn clamp_into_band() {
        let d = DepthController::new(25.0, 5.0);
        assert_eq!(d.clamp(40.0), 30.0);
        assert_eq!(d.clamp(10.0), 20.0);
        assert_eq!(d.clamp(26.0), 26.0);
    }

    #[test]
    fn disabled_target_passes_through() {
        let d = DepthController::new(0.0, 0.0);
        assert_eq!(d.clamp(999.0), 999.0);
        assert!(d.in_band(999.0));
    }

    #[test]
    fn in_band_check() {
        let d = DepthController::new(25.0, 5.0);
        assert!(d.in_band(22.0));
        assert!(!d.in_band(35.0));
    }
}
