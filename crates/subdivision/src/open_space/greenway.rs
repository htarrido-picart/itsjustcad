// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Greenway / trail-corridor placement (plan §9 Phase 9).
//!
//! Route a linear buffered strip along a path (a polyline) or an edge. The
//! corridor is the path offset by `width/2` on each side into a closed ribbon
//! polygon — the same "offset a centerline into a ROW ribbon" idea the street
//! extractor uses, but baked as open space rather than a road.

use crate::geometry::polygon2d::Polygon2d;
use glam::DVec2;

/// Default corridor width (metres) when the caller gives none. A comfortable
/// shared-use trail-plus-verge.
const DEFAULT_GREENWAY_WIDTH: f64 = 8.0;

/// Route a greenway corridor of total width `width` (default
/// [`DEFAULT_GREENWAY_WIDTH`]) along `path`. `area`, when given, sets the width
/// so the ribbon covers ~that area over the path length (width = area / length),
/// letting the same verb take an `area=` like the other features.
///
/// Returns a closed ribbon polygon (down one side, back the other), valid and
/// non-self-intersecting for any simple, non-self-crossing path.
pub fn greenway(path: &[DVec2], width: Option<f64>, area: Option<f64>) -> Option<Polygon2d> {
    // Drop consecutive duplicates so segment normals are well defined.
    let mut pts: Vec<DVec2> = Vec::with_capacity(path.len());
    for p in path {
        if pts.last().map(|q: &DVec2| q.distance_squared(*p) > 1e-12).unwrap_or(true) {
            pts.push(*p);
        }
    }
    if pts.len() < 2 {
        return None;
    }

    let length = polyline_length(&pts);
    if length <= 1e-6 {
        return None;
    }
    let w = match (width, area) {
        (Some(w), _) => w,
        (None, Some(a)) => a / length,
        (None, None) => DEFAULT_GREENWAY_WIDTH,
    }
    .max(1e-3);
    let half = w / 2.0;

    // Per-vertex normal = average of adjacent segment normals (miter-lite). For
    // gentle paths this stays well inside the segment offsets; sharp turns just
    // widen the corner slightly, still simple.
    let n = pts.len();
    let seg_normal = |i: usize| -> DVec2 {
        let d = (pts[i + 1] - pts[i]).normalize_or_zero();
        DVec2::new(-d.y, d.x)
    };
    let vert_normal = |i: usize| -> DVec2 {
        let a = if i == 0 { seg_normal(0) } else { seg_normal(i - 1) };
        let b = if i + 1 < n { seg_normal(i) } else { seg_normal(n - 2) };
        (a + b).normalize_or_zero()
    };

    let mut left = Vec::with_capacity(n);
    let mut right = Vec::with_capacity(n);
    for (i, &p) in pts.iter().enumerate() {
        let nm = vert_normal(i);
        left.push(p + nm * half);
        right.push(p - nm * half);
    }
    // Close the ring: left forward, right backward.
    right.reverse();
    left.extend(right);
    let poly = Polygon2d::new(left)?;
    if super::is_valid_feature(&poly) {
        Some(poly)
    } else {
        None
    }
}

fn polyline_length(pts: &[DVec2]) -> f64 {
    pts.windows(2).map(|w| w[0].distance(w[1])).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn straight_greenway_is_a_valid_rectangle() {
        let path = vec![DVec2::new(0.0, 0.0), DVec2::new(100.0, 0.0)];
        let g = greenway(&path, Some(10.0), None).unwrap();
        assert!(super::super::is_valid_feature(&g));
        // Strip area ≈ length × width = 100 × 10.
        assert!((g.area() - 1000.0).abs() < 1.0, "area {}", g.area());
    }

    #[test]
    fn bent_greenway_stays_simple() {
        let path = vec![
            DVec2::new(0.0, 0.0),
            DVec2::new(50.0, 0.0),
            DVec2::new(50.0, 50.0),
        ];
        let g = greenway(&path, Some(6.0), None).unwrap();
        assert!(super::super::is_valid_feature(&g));
        assert!(g.area() > 0.0);
    }

    #[test]
    fn area_arg_sets_width() {
        let path = vec![DVec2::new(0.0, 0.0), DVec2::new(100.0, 0.0)];
        // area 800 over length 100 → width 8 → area back ≈ 800.
        let g = greenway(&path, None, Some(800.0)).unwrap();
        assert!((g.area() - 800.0).abs() < 1.0, "area {}", g.area());
    }

    #[test]
    fn degenerate_path_rejected() {
        assert!(greenway(&[DVec2::ZERO], Some(5.0), None).is_none());
        assert!(greenway(&[DVec2::ZERO, DVec2::ZERO], Some(5.0), None).is_none());
    }
}
