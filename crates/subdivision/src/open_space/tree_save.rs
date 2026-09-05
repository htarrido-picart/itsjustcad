// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Tree-save (preserved-vegetation) placement (plan §9 Phase 9).
//!
//! Mark a preserved-vegetation polygon the layout works around. Unlike a park
//! or pond (a designed shape), a tree-save marks EXISTING vegetation, so it
//! follows the selected region rather than imposing a geometric form: when no
//! area is asked it preserves the whole region; with an `area`, it preserves an
//! area-scaled copy of the region shape, centred on it (an irregular blob that
//! honours the region's natural outline).
//!
//! **Advisory:** this is a design-intent preservation boundary, not a surveyed
//! canopy or arborist assessment — the commands layer surfaces the note.

use crate::geometry::polygon2d::Polygon2d;
use glam::DVec2;

/// Mark a tree-save polygon inside `region`. If `area` is `None`, the whole
/// region is preserved (returned as-is). If `area` is given (and smaller than
/// the region), the region shape is scaled about its centroid to that area — a
/// preserved stand smaller than the region but with the same natural outline.
pub fn tree_save(region: &Polygon2d, area: Option<f64>) -> Option<Polygon2d> {
    if !super::is_valid_feature(region) {
        return None;
    }
    let region_area = region.area();
    match area {
        None => Some(region.clone()),
        Some(a) if a >= region_area => Some(region.clone()),
        Some(a) if a <= 1e-6 => None,
        Some(a) => {
            // Area scales with the square of a linear scale factor about the
            // centroid: keep the region's shape, shrink to the target area.
            let s = (a / region_area).sqrt();
            let c = region.centroid();
            let verts: Vec<DVec2> = region
                .verts()
                .iter()
                .map(|v| c + (*v - c) * s)
                .collect();
            let poly = Polygon2d::new(verts)?;
            if super::is_valid_feature(&poly) {
                Some(poly)
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ell() -> Polygon2d {
        // An L-shaped region so we can prove the outline is preserved.
        Polygon2d::from_pairs([
            (0.0, 0.0),
            (60.0, 0.0),
            (60.0, 30.0),
            (30.0, 30.0),
            (30.0, 60.0),
            (0.0, 60.0),
        ])
        .unwrap()
    }

    #[test]
    fn no_area_preserves_whole_region() {
        let r = ell();
        let ts = tree_save(&r, None).unwrap();
        assert!(super::super::is_valid_feature(&ts));
        assert!((ts.area() - r.area()).abs() < 1e-9);
        // Same vertex count → outline preserved.
        assert_eq!(ts.len(), r.len());
    }

    #[test]
    fn area_scales_outline() {
        let r = ell();
        let target = r.area() / 4.0;
        let ts = tree_save(&r, Some(target)).unwrap();
        assert!(super::super::is_valid_feature(&ts));
        assert!((ts.area() - target).abs() / target < 1e-6, "area {}", ts.area());
        // Outline shape (vertex count) preserved even when shrunk.
        assert_eq!(ts.len(), r.len());
    }

    #[test]
    fn oversized_area_clamps_to_region() {
        let r = ell();
        let ts = tree_save(&r, Some(r.area() * 10.0)).unwrap();
        assert!((ts.area() - r.area()).abs() < 1e-9);
    }
}
