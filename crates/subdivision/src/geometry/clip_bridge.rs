// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! `clip_bridge` — the *single* place `i_overlay` is touched. Polygon boolean
//! ops + polygon OFFSET (buffer), with the float→int scale defined ONCE here so
//! no raw f64 tolerance choice leaks elsewhere.
//!
//! **i_overlay evaluation (Phase-1 gate):** `i_overlay` 8.1 is pure Rust,
//! MIT OR Apache-2.0 (AGPLv3-clean), on crates.io. It ships BOTH:
//!
//! - boolean overlay (`SingleFloatOverlay::overlay` — union/intersection/
//!   difference/xor), and
//! - polygon OFFSET via the `OutlineOffset` trait (`OutlineStyle` with
//!   outer/inner offset + join style).
//!
//! So the Phase-1 offset blocker is CLEARED — offset works; no fallback to `geo`
//! or a vendored Clipper2 is needed. Phase 3 (recursive OBB) does not use any of
//! this — it needs only the half-plane clip in `split.rs`. Boolean/offset here
//! are wired and smoke-tested now so Phase 4+ (offset/perimeter, block
//! extraction) can build on them without revisiting the dependency question.

use crate::geometry::polygon2d::Polygon2d;
use glam::DVec2;
use i_overlay::core::fill_rule::FillRule;
use i_overlay::core::overlay_rule::OverlayRule;
use i_overlay::float::single::SingleFloatOverlay;
use i_overlay::mesh::outline::offset::OutlineOffset;
use i_overlay::mesh::style::OutlineStyle;

/// Fixed float→integer scale for every clipper call. 1000 = 1 mm resolution at
/// metre units — far below drafting tolerance, far above f64 noise. Defined once
/// here; never let a caller pick a different scale.
pub const CLIP_SCALE: f64 = 1000.0;

type IPoint = [f64; 2];

fn to_contour(poly: &Polygon2d) -> Vec<IPoint> {
    poly.verts().iter().map(|v| [v.x, v.y]).collect()
}

fn contour_to_polygon(c: &[IPoint]) -> Option<Polygon2d> {
    Polygon2d::new(c.iter().map(|p| DVec2::new(p[0], p[1])).collect())
}

/// Collect the OUTER contours of a `Shapes` result into `Polygon2d`s (holes are
/// dropped — Phase 3/4 blocks are treated as their outer boundary; hole support
/// arrives with the retention-pond case in a later phase).
fn shapes_to_polygons(shapes: &[Vec<Vec<IPoint>>]) -> Vec<Polygon2d> {
    shapes
        .iter()
        .filter_map(|shape| shape.first().and_then(|outer| contour_to_polygon(outer)))
        .collect()
}

/// Boolean of two polygons under `rule`. Returns every resulting outer ring.
fn boolean(a: &Polygon2d, b: &Polygon2d, rule: OverlayRule) -> Vec<Polygon2d> {
    let subj = vec![to_contour(a)];
    let clip = vec![to_contour(b)];
    let shapes = subj.overlay(&clip, rule, FillRule::NonZero);
    shapes_to_polygons(&shapes)
}

/// Union of two polygons.
pub fn union(a: &Polygon2d, b: &Polygon2d) -> Vec<Polygon2d> {
    boolean(a, b, OverlayRule::Union)
}

/// Intersection of two polygons.
pub fn intersection(a: &Polygon2d, b: &Polygon2d) -> Vec<Polygon2d> {
    boolean(a, b, OverlayRule::Intersect)
}

/// `a` minus `b`.
pub fn difference(a: &Polygon2d, b: &Polygon2d) -> Vec<Polygon2d> {
    boolean(a, b, OverlayRule::Difference)
}

/// Offset (buffer) a polygon by `delta`: positive grows outward, negative insets
/// inward. Returns the resulting outer rings (an inward offset that collapses
/// yields an empty vec). Uses a bevel join and the fixed [`CLIP_SCALE`].
pub fn offset(poly: &Polygon2d, delta: f64) -> Vec<Polygon2d> {
    let contour: Vec<IPoint> = to_contour(poly);
    let style = OutlineStyle::new(delta);
    // `outline_fixed_scale` pins the float→int scale we control here.
    let shapes = match contour.outline_fixed_scale(&style, CLIP_SCALE) {
        Ok(s) => s,
        Err(_) => contour.outline(&style),
    };
    shapes_to_polygons(&shapes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(x0: f64, y0: f64, s: f64) -> Polygon2d {
        Polygon2d::from_pairs([
            (x0, y0),
            (x0 + s, y0),
            (x0 + s, y0 + s),
            (x0, y0 + s),
        ])
        .unwrap()
    }

    #[test]
    fn roundtrip_no_coordinate_drift() {
        // Offset by 0 should return (essentially) the same polygon area.
        let sq = square(0.0, 0.0, 10.0);
        let out = offset(&sq, 0.0);
        assert_eq!(out.len(), 1);
        assert!((out[0].area() - 100.0).abs() < 1e-3, "area {}", out[0].area());
    }

    #[test]
    fn inward_offset_shrinks_area() {
        let sq = square(0.0, 0.0, 10.0);
        let out = offset(&sq, -1.0);
        assert_eq!(out.len(), 1);
        // Inset by 1 on all sides → 8×8 = 64.
        assert!((out[0].area() - 64.0).abs() < 0.5, "area {}", out[0].area());
    }

    #[test]
    fn outward_offset_grows_area() {
        let sq = square(0.0, 0.0, 10.0);
        let out = offset(&sq, 2.0);
        assert_eq!(out.len(), 1);
        // Outward offset grows the polygon (bevel-cut corners keep it below the
        // naive 14×14=196, but well above the original 100).
        assert!(out[0].area() > 170.0, "area {}", out[0].area());
    }

    #[test]
    fn deep_inset_collapses_to_empty() {
        let sq = square(0.0, 0.0, 4.0);
        let out = offset(&sq, -5.0);
        assert!(out.is_empty(), "expected collapse, got {} rings", out.len());
    }

    #[test]
    fn union_of_overlapping_squares() {
        let a = square(0.0, 0.0, 10.0);
        let b = square(5.0, 0.0, 10.0);
        let u = union(&a, &b);
        assert_eq!(u.len(), 1);
        // 10×10 + 10×10 − 5×10 overlap = 150.
        assert!((u[0].area() - 150.0).abs() < 1e-2, "area {}", u[0].area());
    }

    #[test]
    fn difference_cuts_a_bite() {
        let a = square(0.0, 0.0, 10.0);
        let b = square(5.0, 0.0, 10.0);
        let d = difference(&a, &b);
        assert_eq!(d.len(), 1);
        assert!((d[0].area() - 50.0).abs() < 1e-2, "area {}", d[0].area());
    }

    #[test]
    fn intersection_of_overlap() {
        let a = square(0.0, 0.0, 10.0);
        let b = square(5.0, 0.0, 10.0);
        let i = intersection(&a, &b);
        assert_eq!(i.len(), 1);
        assert!((i[0].area() - 50.0).abs() < 1e-2, "area {}", i[0].area());
    }
}
