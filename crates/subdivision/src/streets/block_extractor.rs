// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Block extraction (plan §7.3 / §5): offset every street centerline by ROW/2
//! into a right-of-way ribbon, boolean-subtract the ROW union from the site,
//! and tag every resulting block edge with its generating street (id + width +
//! length). Original-boundary edges that are not streets stay `is_street = false`
//! — the §5 hard dependency that skeleton/corner/loading/frontage all need.
//!
//! If `loading == AlleyLoaded`, a second tier of narrower rear lanes is inserted
//! bisecting each block along its long axis and tagged `is_alley = true`.
//!
//! Determinism: all inputs (streets + site + settings) are deterministic, and
//! every operation here is pure, so the block set is byte-identical for a fixed
//! seed — the replay invariant.

use crate::blocks::block::Block;
use crate::blocks::block_edge::BlockEdge;
use crate::geometry::clip_bridge;
use crate::geometry::oriented_box::OrientedBox;
use crate::geometry::polygon2d::Polygon2d;
use crate::geometry::split::{split_by_line, Line2d};
use crate::settings::{LoadingType, SubdivisionSettings};
use crate::streets::street_graph::{Street, StreetGraph};
use glam::DVec2;

/// Tolerance (metres) for deciding a block edge is coincident with a street ROW
/// boundary. ROW edges sit exactly width/2 from the centerline; boundary noise
/// from the boolean is well under a mm at [`clip_bridge::CLIP_SCALE`].
const TAG_TOL: f64 = 0.05;

/// Build the closed right-of-way **ribbon** polygon of one street: the centerline
/// offset by `width/2` to each side. For an open polyline this is a strip; for a
/// closed loop (a cul-de-sac bulb) it is an annulus approximated by its outer
/// ring (holes are dropped downstream anyway). Returns `None` for a degenerate
/// centerline.
pub fn street_ribbon(street: &Street) -> Option<Polygon2d> {
    let half = street.width * 0.5;
    let c = &street.centerline;
    if c.len() < 2 || half <= 0.0 {
        return None;
    }
    // Left offsets forward, right offsets on the return pass → a closed ring.
    let mut left: Vec<DVec2> = Vec::with_capacity(c.len());
    let mut right: Vec<DVec2> = Vec::with_capacity(c.len());
    for i in 0..c.len() {
        // Tangent at vertex i (average of adjacent segment directions).
        let prev = if i > 0 { c[i] - c[i - 1] } else { DVec2::ZERO };
        let next = if i + 1 < c.len() {
            c[i + 1] - c[i]
        } else {
            DVec2::ZERO
        };
        let mut t = prev + next;
        if t.length_squared() < 1e-18 {
            t = if next.length_squared() > 1e-18 { next } else { prev };
        }
        if t.length_squared() < 1e-18 {
            continue;
        }
        let t = t.normalize();
        let n = DVec2::new(-t.y, t.x); // left normal
        left.push(c[i] + n * half);
        right.push(c[i] - n * half);
    }
    if left.len() < 2 {
        return None;
    }
    // Ring: left forward, right backward.
    let mut ring = left;
    ring.extend(right.into_iter().rev());
    Polygon2d::new(ring)
}

/// Extract tagged blocks: `site` minus every street ROW ribbon, with every edge
/// tagged back to its street. When `loading == AlleyLoaded`, a rear-lane tier is
/// inserted per block.
pub fn extract(
    site: &Polygon2d,
    graph: &StreetGraph,
    settings: &SubdivisionSettings,
) -> Vec<Block> {
    // Subtract every street ribbon from the site independently, accumulating the
    // remaining pieces. Starting from the whole site, each subtraction may split
    // a piece into several. Subtracting ribbons one by one (rather than a
    // pre-unioned blob) avoids the boolean union merging disjoint ribbons and
    // swallowing interior blocks.
    let mut pieces: Vec<Polygon2d> = vec![site.clone()];
    for s in graph.streets.iter() {
        let Some(row) = street_ribbon(s) else { continue };
        let mut next: Vec<Polygon2d> = Vec::new();
        for piece in &pieces {
            let diff = clip_bridge::difference(piece, &row);
            if diff.is_empty() {
                // Either fully consumed by this ROW, or the ribbon does not
                // touch this piece. Distinguish: if the piece is untouched, keep
                // it; if it was inside the ROW, drop it.
                if clip_bridge::intersection(piece, &row).is_empty() {
                    next.push(piece.clone());
                }
                continue;
            }
            next.extend(diff);
        }
        pieces = next;
    }

    // Drop slivers (numerical crumbs from the boolean) below a small area. Two
    // floors: a site-relative epsilon, and — when a block depth is known — a
    // fraction of one cell (block_depth²). Curved generators (radial/hex/Voronoi)
    // subtract many overlapping ribbons whose endcaps leave sub-metre crumbs that
    // can pairwise-overlap; a real block is always a meaningful fraction of a
    // cell, so this floor removes the crumbs without touching genuine blocks
    // (rectilinear blocks are ~block_depth², far above it).
    let depth = crate::streets::generators::effective_block_depth(settings);
    let cell_floor = if depth > 0.0 { depth * depth * 0.03 } else { 0.0 };
    let min_keep = (site.area() * 1e-6).max(1e-6).max(cell_floor);
    let mut blocks: Vec<Block> = pieces
        .into_iter()
        .filter(|p| p.area() > min_keep)
        .map(|p| tag_block(p, graph, site))
        .collect();

    if settings.loading == LoadingType::AlleyLoaded {
        blocks = blocks
            .into_iter()
            .flat_map(|b| insert_alley(b, settings))
            .collect();
    }

    blocks
}

/// Tag every edge of `poly` (a block): street edges get the id/width/length of
/// the street whose ROW they sit on; edges coincident with the original site
/// boundary (and not a street) stay `is_street = false`.
fn tag_block(poly: Polygon2d, graph: &StreetGraph, site: &Polygon2d) -> Block {
    let site_edges: Vec<(DVec2, DVec2)> = site.edges().collect();
    let edges: Vec<BlockEdge> = poly
        .edges()
        .map(|(a, b)| {
            let mid = (a + b) * 0.5;
            // A block edge is a street edge if its midpoint sits on a street ROW
            // boundary (i.e. width/2 from that street's centerline) AND it is not
            // on the untouched original site boundary.
            let on_site_boundary = site_edges
                .iter()
                .any(|&(sa, sb)| point_on_segment(mid, sa, sb, TAG_TOL));
            if !on_site_boundary
                && let Some(s) = matching_street(mid, graph)
            {
                return BlockEdge {
                    a,
                    b,
                    is_street: true,
                    street_id: Some(s.id),
                    street_width: s.width,
                    street_length: s.length(),
                    is_alley: s.is_alley(),
                };
            }
            BlockEdge::boundary(a, b)
        })
        .collect();
    Block { polygon: poly, edges }
}

/// The street whose ROW boundary passes through `mid`: the centerline nearest to
/// `width/2` away from `mid`, within [`TAG_TOL`].
fn matching_street(mid: DVec2, graph: &StreetGraph) -> Option<&Street> {
    let mut best: Option<(&Street, f64)> = None;
    for s in graph.streets.iter() {
        let d = dist_point_to_polyline(mid, &s.centerline);
        let err = (d - s.width * 0.5).abs();
        if err < TAG_TOL {
            match best {
                Some((_, be)) if be <= err => {}
                _ => best = Some((s, err)),
            }
        }
    }
    best.map(|(s, _)| s)
}

/// Distance from `p` to the nearest point on the polyline `pts`.
fn dist_point_to_polyline(p: DVec2, pts: &[DVec2]) -> f64 {
    let mut best = f64::INFINITY;
    for w in pts.windows(2) {
        let d = dist_point_to_segment(p, w[0], w[1]);
        if d < best {
            best = d;
        }
    }
    best
}

fn dist_point_to_segment(p: DVec2, a: DVec2, b: DVec2) -> f64 {
    let ab = b - a;
    let len2 = ab.length_squared();
    if len2 < 1e-18 {
        return p.distance(a);
    }
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    p.distance(a + ab * t)
}

fn point_on_segment(p: DVec2, a: DVec2, b: DVec2, tol: f64) -> bool {
    dist_point_to_segment(p, a, b) < tol
}

/// Insert a rear-lane (alley) bisecting `block` along its long axis, splitting it
/// into two sub-blocks whose shared edges are tagged `is_alley`. Falls back to
/// the single block unchanged if the bisector does not cleanly divide it.
fn insert_alley(block: Block, settings: &SubdivisionSettings) -> Vec<Block> {
    let Some(ob) = OrientedBox::of_polygon(&block.polygon) else {
        return vec![block];
    };
    // The alley runs ALONG the long axis, through the center → its cut line has
    // the long axis as its direction (bisecting the short dimension).
    let line = Line2d::new(ob.center, ob.long_axis);
    let (a, b) = split_by_line(&block.polygon, &line);
    let (Some(a_poly), Some(b_poly)) = (a, b) else {
        return vec![block];
    };
    if a_poly.area() < 1e-6 || b_poly.area() < 1e-6 {
        return vec![block];
    }
    let alley_w = settings.alley_width;
    // Re-tag each sub-block: inherit the parent's street tags where an edge
    // coincides with a parent edge; the newly created bisector edge is an alley.
    vec![
        retag_with_alley(a_poly, &block, line, alley_w),
        retag_with_alley(b_poly, &block, line, alley_w),
    ]
}

/// Rebuild a sub-block's edge tags: an edge lying on the bisector `line` is an
/// alley edge; otherwise inherit the matching parent edge's tag (street or
/// boundary).
fn retag_with_alley(poly: Polygon2d, parent: &Block, line: Line2d, alley_w: f64) -> Block {
    let edges: Vec<BlockEdge> = poly
        .edges()
        .map(|(a, b)| {
            let mid = (a + b) * 0.5;
            // On the bisector line (within tolerance) → alley edge.
            if line.signed(mid).abs() < TAG_TOL {
                return BlockEdge {
                    a,
                    b,
                    is_street: false,
                    street_id: None,
                    street_width: alley_w,
                    street_length: 0.0,
                    is_alley: true,
                };
            }
            // Inherit the parent edge whose segment contains this midpoint.
            for pe in &parent.edges {
                if point_on_segment(mid, pe.a, pe.b, TAG_TOL) {
                    return BlockEdge { a, b, ..*pe };
                }
            }
            BlockEdge::boundary(a, b)
        })
        .collect();
    Block { polygon: poly, edges }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streets::street_graph::StreetTier;

    fn rect(w: f64, h: f64) -> Polygon2d {
        Polygon2d::from_pairs([(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)]).unwrap()
    }

    #[test]
    fn ribbon_has_expected_area() {
        let s = Street {
            id: 0,
            centerline: vec![DVec2::new(0.0, 50.0), DVec2::new(100.0, 50.0)],
            width: 10.0,
            tier: StreetTier::Spine,
        };
        let rb = street_ribbon(&s).unwrap();
        // 100 long × 10 wide = ~1000.
        assert!((rb.area() - 1000.0).abs() < 1.0, "area {}", rb.area());
    }

    #[test]
    fn single_road_splits_site_in_two() {
        // A 100×100 site with one horizontal road at y=50, ROW 10, no streets
        // graph beyond it → two blocks (top + bottom), total area = site - ROW.
        let site = rect(100.0, 100.0);
        let mut g = StreetGraph::new();
        g.add(vec![DVec2::new(0.0, 50.0), DVec2::new(100.0, 50.0)], 10.0, StreetTier::Spine);
        let s = SubdivisionSettings::default();
        let blocks = extract(&site, &g, &s);
        assert_eq!(blocks.len(), 2, "expected 2 blocks");
        let total: f64 = blocks.iter().map(|b| b.area()).sum();
        // 100×100 − 100×10 ROW = 9000.
        assert!((total - 9000.0).abs() < 50.0, "total {total}");
        // Each block should have exactly one street-tagged edge (the road side).
        for b in &blocks {
            assert!(b.has_street(), "block should front the road");
        }
    }
}
