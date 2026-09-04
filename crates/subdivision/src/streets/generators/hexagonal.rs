// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Hexagonal generator (plan §7.3 / §1 owner scope, Phase 5b): a pointy-top hex
//! lattice sized so each cell is roughly one block deep, tiled over the site
//! bounding box and clipped to the site. Every distinct hex-cell **edge** is
//! emitted once as a street centerline; block extraction then carves the hex
//! cells out of the site as blocks (partial cells at the boundary are clipped by
//! the boolean, exactly like the rectilinear path).
//!
//! Cell size: with `block_depth = d` we set the hexagon's center-to-vertex
//! circumradius so the cell's short (flat-to-flat) dimension ≈ d, i.e. the cell
//! is about one block deep. Determinism: the lattice is a pure function of the
//! site bbox + block depth — no randomness — so it is byte-identical for a fixed
//! seed.

use crate::geometry::polygon2d::Polygon2d;
use crate::settings::SubdivisionSettings;
use crate::streets::generators::{effective_block_depth, effective_road_width};
use crate::streets::street_graph::{StreetGraph, StreetTier};
use glam::DVec2;

/// Quantize a point to a mm grid so that hex-edge endpoints shared between
/// adjacent cells hash to the same key → each interior edge is emitted once.
fn key(p: DVec2) -> (i64, i64) {
    ((p.x * 1000.0).round() as i64, (p.y * 1000.0).round() as i64)
}

/// An undirected edge key (endpoint order-independent) for dedup.
fn edge_key(a: DVec2, b: DVec2) -> ((i64, i64), (i64, i64)) {
    let (ka, kb) = (key(a), key(b));
    if ka <= kb {
        (ka, kb)
    } else {
        (kb, ka)
    }
}

/// The six pointy-top hexagon corners around `center` with circumradius `r`.
fn hex_corners(center: DVec2, r: f64) -> [DVec2; 6] {
    let mut c = [DVec2::ZERO; 6];
    for (i, slot) in c.iter_mut().enumerate() {
        // Pointy-top: first vertex straight up (90°), then every 60°.
        let a = std::f64::consts::FRAC_PI_2 + i as f64 * std::f64::consts::FRAC_PI_3;
        *slot = center + DVec2::new(a.cos(), a.sin()) * r;
    }
    c
}

/// Generate a hexagonal street lattice over `site`.
pub fn generate(site: &Polygon2d, settings: &SubdivisionSettings) -> StreetGraph {
    let mut graph = StreetGraph::new();
    let depth = effective_block_depth(settings);
    let width = effective_road_width(settings);
    if depth <= 0.0 {
        return graph;
    }

    // Pointy-top hex: flat-to-flat width w = √3 · r, top-to-bottom height = 2r.
    // Size so flat-to-flat ≈ block depth → r = depth / √3.
    let r = depth / 3.0_f64.sqrt();
    let w = 3.0_f64.sqrt() * r; // horizontal spacing between column centers
    let vert = 1.5 * r; // vertical spacing between rows

    let (lo, hi) = site.aabb();
    // Pad by one cell so boundary cells are fully generated then clipped.
    let pad = 2.0 * r;
    let x0 = lo.x - pad;
    let y0 = lo.y - pad;
    let x1 = hi.x + pad;
    let y1 = hi.y + pad;

    // Cell centers on a pointy-top axial layout: odd rows offset by w/2.
    let rows = (((y1 - y0) / vert).ceil() as i64).max(1);
    let cols = (((x1 - x0) / w).ceil() as i64).max(1);

    // Collect unique edges (dedup shared edges between adjacent cells) while
    // preserving deterministic emission order: iterate cells row-major and push
    // the first time each undirected edge is seen.
    let mut seen: std::collections::HashSet<((i64, i64), (i64, i64))> =
        std::collections::HashSet::new();

    for row in 0..=rows {
        let cy = y0 + row as f64 * vert;
        let x_off = if row % 2 != 0 { w * 0.5 } else { 0.0 };
        for col in 0..=cols {
            let cx = x0 + x_off + col as f64 * w;
            let center = DVec2::new(cx, cy);
            let corners = hex_corners(center, r);
            for i in 0..6 {
                let a = corners[i];
                let b = corners[(i + 1) % 6];
                // Only bother with edges that could touch the site (either
                // endpoint within one cell of the bbox).
                if !edge_near_site(a, b, lo, hi, pad) {
                    continue;
                }
                let ek = edge_key(a, b);
                if seen.insert(ek) {
                    graph.add(vec![a, b], width, StreetTier::Connector);
                }
            }
        }
    }

    graph
}

/// Cheap bbox test: keep an edge whose midpoint is within `pad` of the site bbox
/// (edges far outside the site never carve a block, and dropping them keeps the
/// graph small).
fn edge_near_site(a: DVec2, b: DVec2, lo: DVec2, hi: DVec2, pad: f64) -> bool {
    let m = (a + b) * 0.5;
    m.x >= lo.x - pad && m.x <= hi.x + pad && m.y >= lo.y - pad && m.y <= hi.y + pad
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(w: f64, h: f64) -> Polygon2d {
        Polygon2d::from_pairs([(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)]).unwrap()
    }

    #[test]
    fn emits_hex_edges() {
        let site = rect(400.0, 300.0);
        let s = SubdivisionSettings {
            block_depth: 60.0,
            road_width: 8.0,
            ..SubdivisionSettings::default()
        };
        let g = generate(&site, &s);
        assert!(!g.is_empty(), "hex produced no edges");
        // Every emitted street is a single hex edge (2 points).
        for st in &g.streets {
            assert_eq!(st.centerline.len(), 2, "hex edge must be a 2-pt segment");
        }
    }

    #[test]
    fn edge_length_matches_hex_side() {
        // A hex of circumradius r has side length r. Every emitted edge should be
        // ~r long (= depth/√3).
        let depth = 60.0;
        let r = depth / 3.0_f64.sqrt();
        let site = rect(300.0, 300.0);
        let s = SubdivisionSettings {
            block_depth: depth,
            ..SubdivisionSettings::default()
        };
        let g = generate(&site, &s);
        for st in &g.streets {
            let len = st.centerline[0].distance(st.centerline[1]);
            assert!((len - r).abs() < 1e-6, "edge len {len} != side {r}");
        }
    }

    #[test]
    fn deterministic_edge_set() {
        let site = rect(400.0, 300.0);
        let s = SubdivisionSettings {
            block_depth: 60.0,
            ..SubdivisionSettings::default()
        };
        let a = generate(&site, &s);
        let b = generate(&site, &s);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.streets.iter().zip(&b.streets) {
            for (px, py) in x.centerline.iter().zip(&y.centerline) {
                assert_eq!(px.x.to_bits(), py.x.to_bits());
                assert_eq!(px.y.to_bits(), py.y.to_bits());
            }
        }
    }
}
