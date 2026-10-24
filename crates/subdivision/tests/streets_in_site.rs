// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Regression for #166: every street pattern keeps its road network inside the
//! site. Each road centerline vertex lies inside the site polygon or on its
//! boundary (within 1e-6), no piece is zero-length, and each road ROW ribbon
//! stays inside the site except for its end cap where the road meets the
//! boundary (a ribbon vertex outside the site is never more than half the road
//! width from the boundary).

use glam::DVec2;
use subdivision::streets::block_extractor::street_ribbon;
use subdivision::{generate_streets, Polygon2d, StreetPattern, SubdivisionSettings};

const TOL: f64 = 1e-6;

const ALL: [StreetPattern; 7] = [
    StreetPattern::Orthogonal,
    StreetPattern::Skewed,
    StreetPattern::Hexagonal,
    StreetPattern::Voronoi,
    StreetPattern::Radial,
    StreetPattern::Organic,
    StreetPattern::CulDeSac,
];

/// The irregular (concave) site from the #166 repro.
fn irregular_site() -> Polygon2d {
    Polygon2d::from_pairs([
        (0.0, 0.0),
        (900.0, 0.0),
        (1100.0, 380.0),
        (820.0, 520.0),
        (980.0, 900.0),
        (300.0, 820.0),
        (-120.0, 420.0),
    ])
    .unwrap()
}

fn rect_site() -> Polygon2d {
    Polygon2d::from_pairs([(0.0, 0.0), (200.0, 0.0), (200.0, 150.0), (0.0, 150.0)]).unwrap()
}

fn dist_to_segment(p: DVec2, a: DVec2, b: DVec2) -> f64 {
    let ab = b - a;
    let t = if ab.length_squared() > 0.0 {
        ((p - a).dot(ab) / ab.length_squared()).clamp(0.0, 1.0)
    } else {
        0.0
    };
    p.distance(a + ab * t)
}

fn dist_to_boundary(site: &Polygon2d, p: DVec2) -> f64 {
    site.edges()
        .map(|(a, b)| dist_to_segment(p, a, b))
        .fold(f64::INFINITY, f64::min)
}

/// Inside the site, or within `tol` of its boundary.
fn in_site(site: &Polygon2d, p: DVec2, tol: f64) -> bool {
    site.contains(p) || dist_to_boundary(site, p) <= tol
}

fn check(site: &Polygon2d, name: &str, settings: &SubdivisionSettings) {
    let mut bad: Vec<String> = Vec::new();
    for pattern in ALL {
        let s = SubdivisionSettings {
            street_pattern: pattern,
            ..settings.clone()
        };
        let g = generate_streets(site, &s);
        assert!(!g.is_empty(), "{name}/{pattern:?}: no roads");
        for (i, st) in g.streets.iter().enumerate() {
            assert_eq!(
                st.id as usize, i,
                "{name}/{pattern:?}: ids must stay sequential"
            );
            assert!(
                st.centerline.len() >= 2,
                "{name}/{pattern:?}: street {i} has < 2 points"
            );
            assert!(
                st.length() > TOL,
                "{name}/{pattern:?}: street {i} is zero-length"
            );
            for p in &st.centerline {
                if !in_site(site, *p, TOL) {
                    bad.push(format!(
                        "{name}/{pattern:?}: street {i} vertex {p:?} is outside the site by {}",
                        dist_to_boundary(site, *p)
                    ));
                }
            }
            let half = st.width * 0.5;
            if let Some(rb) = street_ribbon(st) {
                for p in rb.verts() {
                    if !in_site(site, *p, half + TOL) {
                        bad.push(format!(
                            "{name}/{pattern:?}: street {i} ribbon vertex {p:?} is {} outside the site",
                            dist_to_boundary(site, *p)
                        ));
                    }
                }
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} violation(s):\n{}",
        bad.len(),
        bad.join("\n")
    );
}

#[test]
fn every_pattern_stays_inside_irregular_site() {
    let s = SubdivisionSettings::default();
    check(&irregular_site(), "irregular", &s);
}

#[test]
fn every_pattern_stays_inside_irregular_site_small_blocks() {
    let s = SubdivisionSettings {
        block_depth: 60.0,
        road_width: 12.0,
        seed: 7,
        ..SubdivisionSettings::default()
    };
    check(&irregular_site(), "irregular-60", &s);
}

#[test]
fn every_pattern_stays_inside_rectangle() {
    let s = SubdivisionSettings {
        block_depth: 40.0,
        road_width: 10.0,
        seed: 3,
        ..SubdivisionSettings::default()
    };
    check(&rect_site(), "rect", &s);
    // (The default 100 m block depth leaves Voronoi with < 3 seeds on this
    // small site — no roads at all — so the second run uses 60 m blocks.)
    let s60 = SubdivisionSettings {
        block_depth: 60.0,
        seed: 11,
        ..SubdivisionSettings::default()
    };
    check(&rect_site(), "rect-60", &s60);
}
