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
            Section::IWideFlange { d, bf, tf, tw } => iwf(d, bf, tf, tw),
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
}
