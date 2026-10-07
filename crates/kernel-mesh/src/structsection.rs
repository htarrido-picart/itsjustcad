// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Named structural cross-section profiles. Each variant yields a closed 2D
//! boundary polyline (centered on its own centroid, in the local section plane)
//! ready to be swept along a member's axis by the mesh solids kernel.
//!
//! Sections model steel/concrete member shapes for interoperability
//! ("model here, analyze elsewhere"). No section properties are computed here —
//! only the geometric boundary — because analysis lives in downstream tools.

use glam::DVec2;

/// A parametric structural section. Dimensions are in meters.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "section", rename_all = "snake_case")]
pub enum Section {
    /// Solid rectangle `w` wide (local x) by `h` tall (local y).
    Rectangular { w: f64, h: f64 },
    /// Solid circle of diameter `d`.
    Circular { d: f64 },
    /// I / wide-flange: overall depth `d`, flange width `bf`, flange
    /// thickness `tf`, web thickness `tw`.
    IWideFlange { d: f64, bf: f64, tf: f64, tw: f64 },
    /// Hollow circular pipe: outer diameter `d`, wall thickness `t`.
    Pipe { d: f64, t: f64 },
    /// Engineered-timber member (glulam / CLT panel edge): a solid rectangle
    /// `w` wide by `h` deep. Modeled geometrically like a rectangle; the variant
    /// records that it is timber for scheduling and material takeoff.
    Timber { w: f64, h: f64 },
    /// Guadua / bamboo culm: a round hollow section, outer diameter `d`, wall
    /// thickness `t`. Geometrically the same outer ring as a pipe; kept distinct
    /// so downstream takeoff/labels read "bamboo" and to reserve room for a
    /// tapered variant later.
    Guadua { d: f64, t: f64 },
    /// Structural tee: overall depth `d`, flange width `bf`, flange thickness
    /// `tf`, web thickness `tw`. Flange across the top, web hanging down.
    Tee { d: f64, bf: f64, tf: f64, tw: f64 },
    /// Channel (C / U): overall depth `d`, flange width `bf`, flange thickness
    /// `tf`, web thickness `tw`. Web on the left, two flanges pointing right.
    Channel { d: f64, bf: f64, tf: f64, tw: f64 },
    /// Angle (L): leg length `a` along local x, leg length `b` along local y,
    /// uniform thickness `t`.
    Angle { a: f64, b: f64, t: f64 },
    /// Hollow structural section (square/rectangular tube): outer width `w`,
    /// outer height `h`, wall thickness `t`. Like `Pipe`, `boundary()` returns
    /// only the OUTER ring; the wall thickness is metadata.
    Hss { w: f64, h: f64, t: f64 },
}

impl Section {
    /// Closed boundary polyline of the section, centered on the origin in the
    /// local (x = width, y = depth) plane. First point is NOT repeated at the
    /// end. Winds counter-clockwise. For the hollow `Pipe` this returns only the
    /// outer ring (the solids kernel sweeps a single loop); the wall thickness is
    /// preserved as metadata for downstream analysis.
    pub fn boundary(&self) -> Vec<DVec2> {
        match *self {
            Section::Rectangular { w, h } => {
                let (x, y) = (w * 0.5, h * 0.5);
                vec![
                    DVec2::new(-x, -y),
                    DVec2::new(x, -y),
                    DVec2::new(x, y),
                    DVec2::new(-x, y),
                ]
            }
            Section::Circular { d } => circle(d * 0.5, 32),
            Section::Pipe { d, .. } => circle(d * 0.5, 32),
            Section::Guadua { d, .. } => circle(d * 0.5, 32),
            Section::IWideFlange { d, bf, tf, tw } => iwf(d, bf, tf, tw),
            Section::Timber { w, h } => {
                let (x, y) = (w * 0.5, h * 0.5);
                vec![
                    DVec2::new(-x, -y),
                    DVec2::new(x, -y),
                    DVec2::new(x, y),
                    DVec2::new(-x, y),
                ]
            }
            Section::Tee { d, bf, tf, tw } => tee(d, bf, tf, tw),
            Section::Channel { d, bf, tf, tw } => channel(d, bf, tf, tw),
            Section::Angle { a, b, t } => angle(a, b, t),
            Section::Hss { w, h, .. } => {
                let (x, y) = (w * 0.5, h * 0.5);
                vec![
                    DVec2::new(-x, -y),
                    DVec2::new(x, -y),
                    DVec2::new(x, y),
                    DVec2::new(-x, y),
                ]
            }
        }
    }

    /// Cross-sectional area of the (solid or hollow) section, m². Used for the
    /// member volume readout; not a structural property.
    pub fn area(&self) -> f64 {
        match *self {
            Section::Rectangular { w, h } => w * h,
            Section::Circular { d } => std::f64::consts::PI * (d * 0.5).powi(2),
            Section::Pipe { d, t } => {
                let ro = d * 0.5;
                let ri = (ro - t).max(0.0);
                std::f64::consts::PI * (ro * ro - ri * ri)
            }
            Section::IWideFlange { d, bf, tf, tw } => {
                // Two flanges + web between them.
                2.0 * bf * tf + (d - 2.0 * tf).max(0.0) * tw
            }
            Section::Timber { w, h } => w * h,
            Section::Guadua { d, t } => {
                let ro = d * 0.5;
                let ri = (ro - t).max(0.0);
                std::f64::consts::PI * (ro * ro - ri * ri)
            }
            Section::Tee { d, bf, tf, tw } => bf * tf + (d - tf).max(0.0) * tw,
            Section::Channel { d, bf, tf, tw } => 2.0 * bf * tf + (d - 2.0 * tf).max(0.0) * tw,
            Section::Angle { a, b, t } => t * (a + b - t),
            Section::Hss { w, h, t } => {
                let iw = (w - 2.0 * t).max(0.0);
                let ih = (h - 2.0 * t).max(0.0);
                w * h - iw * ih
            }
        }
    }
}

fn circle(r: f64, n: usize) -> Vec<DVec2> {
    (0..n)
        .map(|i| {
            let a = std::f64::consts::TAU * i as f64 / n as f64;
            DVec2::new(r * a.cos(), r * a.sin())
        })
        .collect()
}

/// I-shape outline, 12 vertices, walked CCW from the bottom-left of the bottom
/// flange. Centered on the origin.
fn iwf(d: f64, bf: f64, tf: f64, tw: f64) -> Vec<DVec2> {
    let (hb, hd) = (bf * 0.5, d * 0.5);
    let hw = tw * 0.5;
    let yb = hd - tf; // top of bottom flange / bottom of top flange (abs)
    vec![
        DVec2::new(-hb, -hd),  // 0 bottom-left of bottom flange
        DVec2::new(hb, -hd),   // 1 bottom-right
        DVec2::new(hb, -yb),   // 2 top-right of bottom flange
        DVec2::new(hw, -yb),   // 3 web bottom-right
        DVec2::new(hw, yb),    // 4 web top-right
        DVec2::new(hb, yb),    // 5 bottom-right of top flange
        DVec2::new(hb, hd),    // 6 top-right
        DVec2::new(-hb, hd),   // 7 top-left
        DVec2::new(-hb, yb),   // 8 bottom-left of top flange
        DVec2::new(-hw, yb),   // 9 web top-left
        DVec2::new(-hw, -yb),  // 10 web bottom-left
        DVec2::new(-hb, -yb),  // 11 top-left of bottom flange
    ]
}

/// T-shape outline, 8 vertices, CCW. Flange across the top, web hanging down.
/// Built with the flange top at y = d and the web bottom at y = 0, then shifted
/// down by the true centroid so the returned outline is centered on its centroid.
fn tee(d: f64, bf: f64, tf: f64, tw: f64) -> Vec<DVec2> {
    let hb = bf * 0.5;
    let hw = tw * 0.5;
    let flange_top = d; // flange top edge
    let flange_bot = d - tf; // flange bottom / web top
    // Centroid (y) with the web bottom at y = 0.
    let a_flange = bf * tf;
    let a_web = (d - tf).max(0.0) * tw;
    let yc_flange = d - tf * 0.5;
    let yc_web = (d - tf) * 0.5;
    let cy = if a_flange + a_web > 0.0 {
        (a_flange * yc_flange + a_web * yc_web) / (a_flange + a_web)
    } else {
        0.0
    };
    let shift = |y: f64| y - cy;
    vec![
        DVec2::new(-hw, shift(0.0)),         // 0 web bottom-left
        DVec2::new(hw, shift(0.0)),          // 1 web bottom-right
        DVec2::new(hw, shift(flange_bot)),   // 2 web top-right
        DVec2::new(hb, shift(flange_bot)),   // 3 flange bottom-right
        DVec2::new(hb, shift(flange_top)),   // 4 flange top-right
        DVec2::new(-hb, shift(flange_top)),  // 5 flange top-left
        DVec2::new(-hb, shift(flange_bot)),  // 6 flange bottom-left
        DVec2::new(-hw, shift(flange_bot)),  // 7 web top-left
    ]
}

/// C-shape (channel) outline, 8 vertices, CCW. Web on the left, two flanges to
/// the right. Depth `d` is vertical; the outline is centered on its centroid.
fn channel(d: f64, bf: f64, tf: f64, tw: f64) -> Vec<DVec2> {
    let hd = d * 0.5;
    let x_left = 0.0; // outer face of the web
    let x_web = tw; // inner face of the web
    let x_right = bf; // flange tips
    let y_low_in = -hd + tf; // top of bottom flange
    let y_high_in = hd - tf; // bottom of top flange
    // Centroid (x): two flanges (full bf wide) + web strip between them.
    let a_flanges = 2.0 * bf * tf;
    let a_web = (d - 2.0 * tf).max(0.0) * tw;
    let xc_flanges = bf * 0.5;
    let xc_web = tw * 0.5;
    let cx = if a_flanges + a_web > 0.0 {
        (a_flanges * xc_flanges + a_web * xc_web) / (a_flanges + a_web)
    } else {
        0.0
    };
    let sx = |x: f64| x - cx;
    vec![
        DVec2::new(sx(x_left), -hd),      // 0 bottom-left
        DVec2::new(sx(x_right), -hd),     // 1 bottom-right (flange tip)
        DVec2::new(sx(x_right), y_low_in),// 2 top of bottom flange, tip
        DVec2::new(sx(x_web), y_low_in),  // 3 inner corner
        DVec2::new(sx(x_web), y_high_in), // 4 inner corner
        DVec2::new(sx(x_right), y_high_in),// 5 bottom of top flange, tip
        DVec2::new(sx(x_right), hd),      // 6 top-right (flange tip)
        DVec2::new(sx(x_left), hd),       // 7 top-left
    ]
}

/// L-shape (angle) outline, 6 vertices, CCW. Leg `a` runs along +x, leg `b`
/// along +y, thickness `t`. Built in the corner at the origin then shifted by
/// the true centroid so the outline is centered on its centroid.
fn angle(a: f64, b: f64, t: f64) -> Vec<DVec2> {
    // Centroid of an L made of a horizontal leg (a×t) and a vertical leg
    // (t×(b-t)) so the two legs don't double-count the corner square.
    let a_h = a * t;
    let a_v = t * (b - t).max(0.0);
    let xc_h = a * 0.5;
    let yc_h = t * 0.5;
    let xc_v = t * 0.5;
    let yc_v = t + (b - t).max(0.0) * 0.5;
    let total = a_h + a_v;
    let (cx, cy) = if total > 0.0 {
        ((a_h * xc_h + a_v * xc_v) / total, (a_h * yc_h + a_v * yc_v) / total)
    } else {
        (0.0, 0.0)
    };
    let p = |x: f64, y: f64| DVec2::new(x - cx, y - cy);
    vec![
        p(0.0, 0.0), // 0 corner
        p(a, 0.0),   // 1 bottom-right of horizontal leg
        p(a, t),     // 2 top-right of horizontal leg
        p(t, t),     // 3 inner corner
        p(t, b),     // 4 top of vertical leg
        p(0.0, b),   // 5 top-left of vertical leg
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rectangular_boundary_is_four_corners() {
        let b = Section::Rectangular { w: 0.4, h: 0.6 }.boundary();
        assert_eq!(b.len(), 4);
        // centered: extents ±0.2, ±0.3
        assert!((b.iter().map(|p| p.x).fold(f64::MIN, f64::max) - 0.2).abs() < 1e-12);
        assert!((b.iter().map(|p| p.y).fold(f64::MIN, f64::max) - 0.3).abs() < 1e-12);
    }

    #[test]
    fn iwf_boundary_has_twelve_vertices() {
        let b = Section::IWideFlange { d: 0.3, bf: 0.15, tf: 0.01, tw: 0.008 }.boundary();
        assert_eq!(b.len(), 12);
        // overall depth spans ±0.15
        let ymax = b.iter().map(|p| p.y).fold(f64::MIN, f64::max);
        let ymin = b.iter().map(|p| p.y).fold(f64::MAX, f64::min);
        assert!((ymax - 0.15).abs() < 1e-12);
        assert!((ymin + 0.15).abs() < 1e-12);
    }

    #[test]
    fn areas_match_formulas() {
        assert!((Section::Rectangular { w: 2.0, h: 3.0 }.area() - 6.0).abs() < 1e-12);
        assert!(
            (Section::Circular { d: 2.0 }.area() - std::f64::consts::PI).abs() < 1e-12
        );
        // pipe: outer r=1, inner r=0.9 -> pi(1 - 0.81)
        assert!(
            (Section::Pipe { d: 2.0, t: 0.1 }.area() - std::f64::consts::PI * 0.19).abs() < 1e-12
        );
    }

    #[test]
    fn timber_and_guadua_boundary_and_area() {
        // Timber is a rectangle: 4 corners, area w*h.
        let t = Section::Timber { w: 0.2, h: 0.4 };
        assert_eq!(t.boundary().len(), 4);
        assert!((t.area() - 0.08).abs() < 1e-12);
        // Guadua is a round hollow culm: 32-gon outer ring, hollow-tube area.
        let g = Section::Guadua { d: 0.1, t: 0.01 };
        assert_eq!(g.boundary().len(), 32);
        let ro = 0.05;
        let ri = 0.04;
        let expected = std::f64::consts::PI * (ro * ro - ri * ri);
        assert!((g.area() - expected).abs() < 1e-12);
    }

    fn extents(b: &[DVec2]) -> (f64, f64, f64, f64) {
        let xmin = b.iter().map(|p| p.x).fold(f64::MAX, f64::min);
        let xmax = b.iter().map(|p| p.x).fold(f64::MIN, f64::max);
        let ymin = b.iter().map(|p| p.y).fold(f64::MAX, f64::min);
        let ymax = b.iter().map(|p| p.y).fold(f64::MIN, f64::max);
        (xmin, xmax, ymin, ymax)
    }

    #[test]
    fn tee_boundary_centered_eight_vertices() {
        let s = Section::Tee { d: 0.2, bf: 0.15, tf: 0.012, tw: 0.008 };
        let b = s.boundary();
        assert_eq!(b.len(), 8);
        let (xmin, xmax, ymin, ymax) = extents(&b);
        // width spans the flange (bf), depth spans d.
        assert!((xmax - xmin - 0.15).abs() < 1e-12);
        assert!((ymax - ymin - 0.2).abs() < 1e-12);
        // symmetric in x
        assert!((xmax + xmin).abs() < 1e-12);
        // area formula
        let expected = 0.15 * 0.012 + (0.2 - 0.012) * 0.008;
        assert!((s.area() - expected).abs() < 1e-12);
    }

    #[test]
    fn channel_boundary_centered_eight_vertices() {
        let s = Section::Channel { d: 0.3, bf: 0.1, tf: 0.012, tw: 0.008 };
        let b = s.boundary();
        assert_eq!(b.len(), 8);
        let (xmin, xmax, ymin, ymax) = extents(&b);
        assert!((xmax - xmin - 0.1).abs() < 1e-12);
        assert!((ymax - ymin - 0.3).abs() < 1e-12);
        // symmetric in y (web + flanges symmetric about mid-depth)
        assert!((ymax + ymin).abs() < 1e-12);
        let expected = 2.0 * 0.1 * 0.012 + (0.3 - 2.0 * 0.012) * 0.008;
        assert!((s.area() - expected).abs() < 1e-12);
    }

    #[test]
    fn angle_boundary_centered_six_vertices() {
        let s = Section::Angle { a: 0.1, b: 0.15, t: 0.012 };
        let b = s.boundary();
        assert_eq!(b.len(), 6);
        let (xmin, xmax, ymin, ymax) = extents(&b);
        assert!((xmax - xmin - 0.1).abs() < 1e-12);
        assert!((ymax - ymin - 0.15).abs() < 1e-12);
        let expected = 0.012 * (0.1 + 0.15 - 0.012);
        assert!((s.area() - expected).abs() < 1e-12);
    }

    #[test]
    fn hss_boundary_outer_ring_and_area() {
        let s = Section::Hss { w: 0.2, h: 0.1, t: 0.01 };
        let b = s.boundary();
        assert_eq!(b.len(), 4);
        let (xmin, xmax, ymin, ymax) = extents(&b);
        assert!((xmax - 0.1).abs() < 1e-12 && (xmin + 0.1).abs() < 1e-12);
        assert!((ymax - 0.05).abs() < 1e-12 && (ymin + 0.05).abs() < 1e-12);
        let expected = 0.2 * 0.1 - (0.2 - 0.02) * (0.1 - 0.02);
        assert!((s.area() - expected).abs() < 1e-12);
    }
}
