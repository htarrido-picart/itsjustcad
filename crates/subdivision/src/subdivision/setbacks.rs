// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Setbacks + buildable envelopes (plan §7, §9 Phase 8).
//!
//! Given a lot polygon + its edge street-tags, compute the **buildable
//! envelope** = the lot inset by a per-edge setback:
//!
//! - **front** setback from the STREET-tagged edge(s),
//! - **rear** from the edge opposite the front,
//! - **side** from every remaining edge (euro_latam side = 0 → party-wall
//!   / `medianería`: no side inset by default, envelope spans the full width).
//!
//! The inset is done by a **per-edge half-plane clip** ([`split_by_line`]): for
//! each contour edge we clip the working polygon by a line parallel to that edge,
//! moved inward by that edge's role setback. Successive clips carve the envelope.
//! This is robust on non-convex / notched lots (no naive per-edge offset
//! self-intersection) and collapses cleanly to `None` when the setbacks exceed
//! the lot size — reported, never a panic.
//!
//! If `build_to_line > 0`, the front of the envelope is pinned to the **build-to
//! line** (a continuous street wall) at `build_to_line` from the front edge,
//! rather than the `setback_front` line — so the envelope's front sits exactly on
//! the build-to line even when it differs from the front setback.
//!
//! **Frontage measured at the setback line** (plan §5, Manuel's explicit ask —
//! the DEFAULT, not an option): [`frontage`] measures each lot's frontage length
//! along the **setback line** by default (`FrontageAt::Setback`), which differs
//! from the curb line on cul-de-sac bulbs and curves.
//!
//! Untagged lots (a plain lot with no Phase-5 street tags) treat the single
//! **longest** edge as the front — same "usable without a street graph" spirit as
//! the subdivision methods.

use crate::blocks::block::Block;
use crate::blocks::block_edge::BlockEdge;
use crate::geometry::polygon2d::Polygon2d;
use crate::geometry::split::{split_by_line, Line2d};
use crate::settings::SubdivisionSettings;
use glam::DVec2;

/// The role a lot edge plays for setback purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeRole {
    /// Fronts a street (or, for an untagged lot, the longest edge).
    Front,
    /// Opposite the front.
    Rear,
    /// A side edge (party-wall in euro_latam: side setback 0).
    Side,
}

/// Where to measure frontage: the default `Setback` line (plan §5) or the raw
/// `Curb` (the street-tagged contour edge itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontageAt {
    /// Along the front setback line (DEFAULT — Manuel's explicit ask).
    Setback,
    /// Along the curb (the street-tagged contour edge as drawn).
    Curb,
}

/// The result of computing a buildable envelope for one lot.
#[derive(Debug, Clone)]
pub struct Envelope {
    /// The inset buildable-area polygon, or `None` if it collapsed (setbacks
    /// exceed the lot). A collapsed envelope is reported, never a panic.
    pub polygon: Option<Polygon2d>,
    /// Whether the front edge was pinned to a build-to line.
    pub build_to_used: bool,
}

impl Envelope {
    /// Buildable area (0 when collapsed).
    pub fn area(&self) -> f64 {
        self.polygon.as_ref().map(|p| p.area()).unwrap_or(0.0)
    }

    /// True if the envelope collapsed to nothing.
    pub fn is_collapsed(&self) -> bool {
        self.polygon.is_none()
    }
}

/// Classify every contour edge of `lot` (using its street tags) as
/// Front / Rear / Side. Returns one role per polygon edge, in CCW edge order.
///
/// - Street-tagged edges → Front.
/// - If the lot is untagged, the single **longest** edge → Front.
/// - The edge whose outward normal is most anti-parallel to the front's outward
///   normal → Rear (the "opposite" edge).
/// - Everything else → Side.
pub fn classify_edges(lot: &Block) -> Vec<EdgeRole> {
    let poly = &lot.polygon;
    let n = poly.len();
    if n == 0 {
        return Vec::new();
    }
    let verts = poly.verts();
    let has_tags = lot.edges.iter().any(|e| e.is_street);

    // Front edges.
    let mut is_front = vec![false; n];
    if has_tags {
        for (i, front) in is_front.iter_mut().enumerate() {
            *front = edge_is_street(&lot.edges, poly, i);
        }
    } else {
        // Longest edge is the front (deterministic: first on ties).
        let mut best = 0usize;
        let mut best_len = -1.0;
        for i in 0..n {
            let len = verts[i].distance(verts[(i + 1) % n]);
            if len > best_len + 1e-12 {
                best_len = len;
                best = i;
            }
        }
        is_front[best] = true;
    }

    // Representative front outward normal (average of front-edge outward normals).
    let mut front_normal = DVec2::ZERO;
    for (i, &front) in is_front.iter().enumerate() {
        if front {
            front_normal += outward_normal(verts, i); // i indexes edges, not is_front
        }
    }
    let front_normal = if front_normal.length_squared() > 1e-18 {
        front_normal.normalize()
    } else {
        outward_normal(verts, 0)
    };

    // Rear: the non-front edge whose outward normal is most anti-parallel to the
    // front normal (dot most negative). None if no non-front edge exists.
    let mut rear = None;
    let mut rear_dot = f64::INFINITY;
    for (i, &front) in is_front.iter().enumerate() {
        if front {
            continue;
        }
        let d = outward_normal(verts, i).dot(front_normal);
        if d < rear_dot {
            rear_dot = d;
            rear = Some(i);
        }
    }

    (0..n)
        .map(|i| {
            if is_front[i] {
                EdgeRole::Front
            } else if Some(i) == rear {
                EdgeRole::Rear
            } else {
                EdgeRole::Side
            }
        })
        .collect()
}

/// Outward (right-hand) unit normal of edge `i` of a CCW ring. For CCW winding
/// the outward normal is the right-hand perpendicular of the edge direction.
fn outward_normal(verts: &[DVec2], i: usize) -> DVec2 {
    let n = verts.len();
    let a = verts[i];
    let b = verts[(i + 1) % n];
    let dir = b - a;
    if dir.length_squared() < 1e-18 {
        return DVec2::ZERO;
    }
    // CCW ring interior is on the LEFT of a→b, so outward is the RIGHT normal.
    DVec2::new(dir.y, -dir.x).normalize()
}

/// The inward inset distance for an edge given its role + settings.
fn setback_for(role: EdgeRole, settings: &SubdivisionSettings) -> f64 {
    match role {
        EdgeRole::Front => settings.setback_front,
        EdgeRole::Rear => settings.setback_rear,
        EdgeRole::Side => settings.setback_side,
    }
    .max(0.0)
}

/// Compute the buildable envelope for one lot (plan §7 / §9 Phase 8).
///
/// The envelope is the lot successively clipped by a line parallel to each
/// contour edge, moved inward by that edge's role setback. `build_to_line > 0`
/// pins the front line to the build-to distance instead of the front setback.
/// Returns an [`Envelope`]; a collapse (setbacks exceed the lot) yields
/// `polygon: None` — reported, never a panic.
pub fn buildable_envelope(lot: &Block, settings: &SubdivisionSettings) -> Envelope {
    let roles = classify_edges(lot);
    let poly = &lot.polygon;
    let n = poly.len();
    if n < 3 || roles.len() != n {
        return Envelope { polygon: None, build_to_used: false };
    }

    let build_to = settings.build_to_line;
    let use_build_to = build_to > 0.0;

    let mut work = poly.clone();
    for i in 0..n {
        let verts = poly.verts();
        let a = verts[i];
        let b = verts[(i + 1) % n];
        let dir = b - a;
        if dir.length_squared() < 1e-18 {
            continue;
        }
        let out_n = outward_normal(verts, i);
        // Inward inset distance for this edge.
        let inset = if roles[i] == EdgeRole::Front && use_build_to {
            build_to
        } else {
            setback_for(roles[i], settings)
        };
        if inset <= 1e-9 {
            // No inset for this edge (e.g. euro_latam side = 0) — leave it.
            continue;
        }
        // The inset line is the edge line moved inward by `inset` (inward =
        // −outward). Clip the working polygon, keeping the interior side.
        let line_point = a - out_n * inset;
        // Line direction = edge direction; its left-hand normal points to the
        // interior (opposite the outward normal), which is the side we keep.
        let line = Line2d::new(line_point, dir);
        let (pos, _neg) = split_by_line(&work, &line);
        // Keep the INTERIOR side only. `Line2d::normal()` is the left-hand normal
        // of `dir`; for a CCW edge that points inward, so the positive half-plane
        // (`signed >= 0`, returned as `pos`) is the interior. If insetting past the
        // opposite edge leaves no interior piece, the envelope has collapsed —
        // report it, do NOT fall back to the exterior remainder.
        match pos {
            Some(p) if p.area() > 1e-9 => work = p,
            _ => return Envelope { polygon: None, build_to_used: use_build_to },
        }
    }

    // Sanity: a valid envelope is strictly inside the lot and smaller.
    if work.area() < 1e-9 || work.area() > poly.area() + 1e-6 {
        return Envelope { polygon: None, build_to_used: use_build_to };
    }
    Envelope { polygon: Some(work), build_to_used: use_build_to }
}

/// Measure the frontage length of one lot (plan §5). By default
/// (`FrontageAt::Setback`) frontage is measured along the **front setback
/// line** — the length of the envelope's front edge, i.e. the portion of the
/// front setback line spanned by the buildable envelope. `FrontageAt::Curb`
/// measures the raw street-tagged contour edge(s) instead.
///
/// These differ on cul-de-sac bulbs and curves: the curb (outer) arc is longer
/// than the setback (inner) line, so setback frontage < curb frontage there.
pub fn frontage(lot: &Block, at: FrontageAt, settings: &SubdivisionSettings) -> f64 {
    let roles = classify_edges(lot);
    let poly = &lot.polygon;
    let n = poly.len();
    if roles.len() != n {
        return 0.0;
    }
    let verts = poly.verts();

    match at {
        FrontageAt::Curb => {
            // Sum the length of every Front (curb / street) edge.
            let mut total = 0.0;
            for (i, &role) in roles.iter().enumerate() {
                if role == EdgeRole::Front {
                    total += verts[i].distance(verts[(i + 1) % n]);
                }
            }
            total
        }
        FrontageAt::Setback => {
            // The front edge(s) of the buildable envelope, i.e. the extent of the
            // front setback line inside the lot. Insetting the curved curb inward
            // shortens the frontage (the visual claim on bulbs/curves).
            let env = buildable_envelope(lot, settings);
            match &env.polygon {
                Some(ep) => front_setback_extent(poly, &roles, ep, settings),
                None => 0.0,
            }
        }
    }
}

/// The length of the envelope boundary that lies on the front setback line(s).
/// The setback line for a front edge is the edge line moved inward by
/// `setback_front` (or `build_to_line`). We sum every envelope edge whose
/// midpoint lies on any front setback line.
fn front_setback_extent(
    lot: &Polygon2d,
    roles: &[EdgeRole],
    env: &Polygon2d,
    settings: &SubdivisionSettings,
) -> f64 {
    let verts = lot.verts();
    let n = verts.len();
    let inset = if settings.build_to_line > 0.0 {
        settings.build_to_line
    } else {
        settings.setback_front.max(0.0)
    };

    // The set of front setback lines (one per front contour edge).
    let mut front_lines: Vec<Line2d> = Vec::new();
    for (i, &role) in roles.iter().enumerate() {
        if role != EdgeRole::Front {
            continue;
        }
        let a = verts[i];
        let b = verts[(i + 1) % n];
        let dir = b - a;
        if dir.length_squared() < 1e-18 {
            continue;
        }
        let out_n = outward_normal(verts, i);
        let pt = a - out_n * inset;
        front_lines.push(Line2d::new(pt, dir));
    }
    if front_lines.is_empty() {
        return 0.0;
    }

    // Sum the envelope edges whose midpoint sits on a front setback line.
    let mut total = 0.0;
    for (ea, eb) in env.edges() {
        let mid = (ea + eb) * 0.5;
        let on_front = front_lines.iter().any(|l| l.signed(mid).abs() < 1e-4);
        if on_front {
            total += ea.distance(eb);
        }
    }
    total
}

/// Whether contour edge `i` of `poly` is street-tagged in `edges` (matched by
/// endpoint geometry, robust to reordering). Mirrors `skeleton_sub`.
fn edge_is_street(edges: &[BlockEdge], poly: &Polygon2d, i: usize) -> bool {
    let verts = poly.verts();
    let n = verts.len();
    let a = verts[i];
    let b = verts[(i + 1) % n];
    edges.iter().any(|e| {
        e.is_street
            && ((e.a.distance(a) < 1e-6 && e.b.distance(b) < 1e-6)
                || (e.a.distance(b) < 1e-6 && e.b.distance(a) < 1e-6))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::block_edge::BlockEdge;

    fn rect(w: f64, d: f64) -> Polygon2d {
        Polygon2d::from_pairs([(0.0, 0.0), (w, 0.0), (w, d), (0.0, d)]).unwrap()
    }

    /// A rect lot whose bottom edge (0,0)->(w,0) is street-tagged (front).
    fn rect_front_street(w: f64, d: f64) -> Block {
        let poly = rect(w, d);
        let verts = poly.verts().to_vec();
        let n = verts.len();
        let edges = (0..n)
            .map(|i| {
                let a = verts[i];
                let b = verts[(i + 1) % n];
                // Bottom edge is y≈0 for both endpoints.
                if a.y.abs() < 1e-9 && b.y.abs() < 1e-9 {
                    BlockEdge {
                        a,
                        b,
                        is_street: true,
                        street_id: Some(1),
                        street_width: 12.0,
                        street_length: w,
                        is_alley: false,
                    }
                } else {
                    BlockEdge::boundary(a, b)
                }
            })
            .collect();
        Block { polygon: poly, edges }
    }

    fn settings(front: f64, side: f64, rear: f64) -> SubdivisionSettings {
        SubdivisionSettings {
            setback_front: front,
            setback_side: side,
            setback_rear: rear,
            build_to_line: 0.0,
            ..SubdivisionSettings::default()
        }
    }

    #[test]
    fn rect_envelope_area_matches_analytic() {
        // w=30, d=40, front=5, side=3, rear=7 → (30-2*3)*(40-5-7) = 24*28 = 672.
        let lot = rect_front_street(30.0, 40.0);
        let s = settings(5.0, 3.0, 7.0);
        let env = buildable_envelope(&lot, &s);
        let a = env.area();
        assert!((a - 672.0).abs() < 1.0, "envelope area {a} vs 672");
    }

    #[test]
    fn side_zero_spans_full_width() {
        // euro_latam side = 0 (party wall): envelope spans full width.
        // w=20, d=30, front=3, side=0, rear=3 → 20*(30-6) = 480.
        let lot = rect_front_street(20.0, 30.0);
        let s = settings(3.0, 0.0, 3.0);
        let env = buildable_envelope(&lot, &s);
        let a = env.area();
        assert!((a - 480.0).abs() < 1.0, "envelope area {a} vs 480");
        // Full width: the envelope's x-extent equals the lot's.
        let (lo, hi) = env.polygon.unwrap().aabb();
        assert!(lo.x.abs() < 1e-3 && (hi.x - 20.0).abs() < 1e-3, "spans full width");
    }

    #[test]
    fn envelope_collapses_when_setbacks_exceed_lot() {
        // Tiny lot, huge setbacks → collapse cleanly to None (no panic).
        let lot = rect_front_street(10.0, 10.0);
        let s = settings(8.0, 8.0, 8.0);
        let env = buildable_envelope(&lot, &s);
        assert!(env.is_collapsed(), "expected collapse, area {}", env.area());
    }

    #[test]
    fn envelope_inside_lot_no_self_intersection_on_l() {
        // L-shaped lot: envelope must stay inside and be a valid simple polygon.
        let poly = Polygon2d::from_pairs([
            (0.0, 0.0),
            (40.0, 0.0),
            (40.0, 20.0),
            (20.0, 20.0),
            (20.0, 40.0),
            (0.0, 40.0),
        ])
        .unwrap();
        let lot = Block::untagged(poly.clone());
        let s = settings(3.0, 3.0, 3.0);
        let env = buildable_envelope(&lot, &s);
        let ep = env.polygon.expect("L envelope should not collapse");
        assert!(ep.area() > 0.0 && ep.area() < poly.area());
        // Every envelope vertex is inside (or on) the lot.
        for &v in ep.verts() {
            assert!(
                poly.contains(v) || on_boundary(&poly, v),
                "envelope vertex {v:?} outside lot"
            );
        }
    }

    fn on_boundary(poly: &Polygon2d, p: DVec2) -> bool {
        poly.edges().any(|(a, b)| {
            let ab = b - a;
            let len2 = ab.length_squared();
            if len2 < 1e-18 {
                return p.distance(a) < 1e-6;
            }
            let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
            p.distance(a + ab * t) < 1e-4
        })
    }

    #[test]
    fn build_to_line_pins_front() {
        // build_to_line = 2 pins the front to 2 from the curb, ignoring
        // setback_front = 10. Envelope front edge sits at y = 2.
        let lot = rect_front_street(30.0, 40.0);
        let s = SubdivisionSettings {
            setback_front: 10.0,
            setback_side: 0.0,
            setback_rear: 5.0,
            build_to_line: 2.0,
            ..SubdivisionSettings::default()
        };
        let env = buildable_envelope(&lot, &s);
        assert!(env.build_to_used);
        let ep = env.polygon.expect("build-to envelope");
        let (lo, _hi) = ep.aabb();
        // Front (min y) at the build-to line (2), NOT the front setback (10).
        assert!((lo.y - 2.0).abs() < 1e-2, "front pinned at y={} (want 2)", lo.y);
    }

    #[test]
    fn frontage_setback_vs_curb_differ_on_bulb() {
        // A trapezoid whose street (front) edge is the LONG bottom, tapering
        // inward at the top — insetting the front line inward keeps it long, but
        // a fan/bulb-like lot where the curb is the outer (wider) edge makes the
        // setback frontage SHORTER than the curb. Model a wedge: wide curb at the
        // bottom, narrower as we inset because the sides converge inward.
        let poly = Polygon2d::from_pairs([
            (0.0, 0.0),
            (40.0, 0.0),   // wide curb (front)
            (30.0, 30.0),  // sides converge inward
            (10.0, 30.0),
        ])
        .unwrap();
        let verts = poly.verts().to_vec();
        let n = verts.len();
        let edges = (0..n)
            .map(|i| {
                let a = verts[i];
                let b = verts[(i + 1) % n];
                if a.y.abs() < 1e-9 && b.y.abs() < 1e-9 {
                    BlockEdge {
                        a,
                        b,
                        is_street: true,
                        street_id: Some(1),
                        street_width: 12.0,
                        street_length: 40.0,
                        is_alley: false,
                    }
                } else {
                    BlockEdge::boundary(a, b)
                }
            })
            .collect();
        let lot = Block { polygon: poly, edges };
        let s = settings(6.0, 2.0, 2.0);

        let curb = frontage(&lot, FrontageAt::Curb, &s);
        let setback = frontage(&lot, FrontageAt::Setback, &s);
        assert!(curb > 0.0 && setback > 0.0, "curb {curb} setback {setback}");
        // The converging sides mean the front setback line (inset 6 up, clipped
        // by the side setbacks) is measurably SHORTER than the 40 m curb.
        assert!(
            (curb - setback).abs() > 1.0,
            "curb {curb} and setback {setback} should differ on a tapering lot"
        );
        assert!(curb > setback, "curb {curb} should exceed setback {setback}");
    }

    #[test]
    fn deterministic_same_lot() {
        let lot = rect_front_street(30.0, 40.0);
        let s = settings(5.0, 3.0, 7.0);
        let a = buildable_envelope(&lot, &s);
        let b = buildable_envelope(&lot, &s);
        assert_eq!(
            a.polygon.map(|p| p.verts().to_vec()),
            b.polygon.map(|p| p.verts().to_vec())
        );
    }
}
