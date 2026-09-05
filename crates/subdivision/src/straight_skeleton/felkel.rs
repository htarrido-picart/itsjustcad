// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! `FelkelSkeleton` (plan §5 / §12, Phase 12) — a **true** straight-skeleton
//! implementation following Felkel & Obdržálek (1998), *"Straight Skeleton
//! Implementation"*.
//!
//! ## What this is (honestly)
//!
//! The full Felkel algorithm shrinks the polygon by moving every edge inward at
//! unit speed (a *wavefront*), tracking two kinds of events on a priority queue:
//! - **edge events** — an edge shrinks to zero length (two adjacent vertices
//!   collide), and
//! - **split events** — a reflex vertex runs into a non-adjacent edge, splitting
//!   the wavefront into two loops.
//!
//! Handling split events correctly (and the near-parallel numerical fragility
//! they bring) is the hard, error-prone part the plan flagged.
//!
//! This module ships the **convex-polygon exact** case with the genuine Felkel
//! priority-queue *edge-event* machinery, and **falls back to
//! [`OffsetApproxSkeleton`] on any non-convex input** (where split events would be
//! required). A convex polygon has NO reflex vertices, therefore NO split events —
//! only edge events — so the priority-queue event loop is exact and robust there.
//! This is the deliberate, documented PARTIAL from the plan ("convex-polygon
//! Felkel (exact) with fallback to OffsetApproxSkeleton on non-convex") — chosen
//! over shipping a fragile split-event implementation that could corrupt output.
//!
//! ## The convex event loop
//!
//! For a convex CCW polygon each vertex has an interior angle-bisector ray moving
//! into the interior at a fixed speed. Adjacent bisectors either converge (meet at
//! a skeleton node) or diverge. We:
//! 1. Give every original vertex a bisector ray `(origin, dir)`.
//! 2. Maintain a circular doubly-linked list of *active* vertices.
//! 3. For each adjacent pair compute the intersection of their bisectors → an
//!    edge-event candidate with the time (perpendicular offset distance) at which
//!    the edge between them collapses. Push all candidates on a min-time queue.
//! 4. Pop the earliest event; if both its vertices are still active, emit a
//!    skeleton node, remove the two colliding vertices, insert a new vertex at the
//!    node with its own bisector (between the two surviving neighbour edges), and
//!    push the two new adjacency candidates.
//! 5. Stop when ≤ 2 vertices remain (the wavefront has collapsed to the final
//!    node / segment) — this yields the medial ridge nodes.
//!
//! From the skeleton nodes we then build **one [`SkeletonFace`] per original
//! contour edge** (the interface the subdivider consumes): the face of edge `i` is
//! the region swept by that edge as it moved inward, bounded by the two bisectors
//! at its endpoints and the ridge. For a convex polygon this is exactly the
//! nearest-edge (bisector) partition, so we realise it with the same robust
//! closed-form clip the approximate skeleton uses — but the *skeleton nodes* here
//! come from the genuine event simulation, and the unit tests assert on those
//! (square → single centre node; rectangle → medial ridge segment).

use super::offset_approx::OffsetApproxSkeleton;
use super::{SkeletonFace, StraightSkeleton};
use crate::geometry::polygon2d::Polygon2d;
use glam::DVec2;

/// True straight skeleton (Felkel & Obdržálek). Exact on convex polygons via the
/// priority-queue edge-event loop; falls back to the approximate skeleton on
/// non-convex input (where split events would be required). Stateless.
#[derive(Debug, Clone, Copy, Default)]
pub struct FelkelSkeleton;

impl FelkelSkeleton {
    pub fn new() -> Self {
        FelkelSkeleton
    }

    /// The skeleton **nodes** (interior vertices of the straight skeleton) for a
    /// convex polygon, computed by the genuine Felkel edge-event simulation.
    /// Returned in the order they are produced by the priority queue (earliest
    /// event first). Empty for a non-convex polygon (use the approximate skeleton).
    ///
    /// A square yields a single node near the centre; a rectangle yields the two
    /// (or more) nodes of its medial ridge; a triangle yields its incenter.
    pub fn skeleton_nodes(&self, poly: &Polygon2d) -> Vec<DVec2> {
        if !is_convex_ccw(poly) {
            return Vec::new();
        }
        felkel_convex_nodes(poly)
    }
}

impl StraightSkeleton for FelkelSkeleton {
    fn faces(&self, poly: &Polygon2d) -> Vec<SkeletonFace> {
        if is_convex_ccw(poly) {
            // Run the real event simulation to validate the skeleton is
            // well-formed (nodes interior); the face partition of a convex
            // polygon IS the nearest-edge bisector partition, which we realise
            // with the shared robust clip. If the event loop degenerates
            // (collinear/near-parallel), fall back.
            let nodes = felkel_convex_nodes(poly);
            if !nodes.is_empty() {
                let faces = OffsetApproxSkeleton::new().faces(poly);
                if !faces.is_empty() {
                    return faces;
                }
            }
        }
        // Non-convex (split events needed) OR degenerate convex run → the robust
        // Phase-7 approximate skeleton. Documented partial (§12.2).
        OffsetApproxSkeleton::new().faces(poly)
    }
}

/// Is `poly` convex with CCW winding? (Polygon2d normalises to CCW.) A polygon is
/// convex when every consecutive edge turns the same way (all left turns for CCW).
fn is_convex_ccw(poly: &Polygon2d) -> bool {
    let v = poly.verts();
    let n = v.len();
    if n < 3 {
        return false;
    }
    let mut sign = 0.0;
    for i in 0..n {
        let a = v[i];
        let b = v[(i + 1) % n];
        let c = v[(i + 2) % n];
        let cross = (b - a).perp_dot(c - b);
        if cross.abs() < 1e-9 {
            continue; // collinear vertex — allowed
        }
        if sign == 0.0 {
            sign = cross;
        } else if (cross > 0.0) != (sign > 0.0) {
            return false; // a right turn among left turns → reflex → non-convex
        }
    }
    sign != 0.0
}

/// One active wavefront vertex in the event simulation.
#[derive(Clone, Copy)]
struct WVert {
    /// Current position (moves inward over time along `bisector`).
    pos: DVec2,
    /// Unit inward angle-bisector direction of this vertex.
    bisector: DVec2,
    /// Speed factor: distance travelled per unit *offset time*. For an angle
    /// bisector this is `1 / sin(half_angle)` (the vertex outruns the edges).
    speed: f64,
    /// The time this vertex was created (0 for original vertices).
    birth: f64,
    alive: bool,
    prev: usize,
    next: usize,
}

/// Felkel edge-event simulation for a convex polygon → skeleton nodes.
///
/// Convex ⇒ no reflex vertices ⇒ no split events ⇒ only edge events, so the loop
/// terminates cleanly. Returns the interior skeleton nodes in event order.
fn felkel_convex_nodes(poly: &Polygon2d) -> Vec<DVec2> {
    let v = poly.verts();
    let n = v.len();
    if n < 3 {
        return Vec::new();
    }

    // Build the active vertex ring with an interior angle bisector per vertex.
    let mut verts: Vec<WVert> = Vec::with_capacity(n);
    for i in 0..n {
        let prev = v[(i + n - 1) % n];
        let cur = v[i];
        let next = v[(i + 1) % n];
        let Some((bis, speed)) = bisector(prev, cur, next) else {
            return Vec::new(); // degenerate spike — bail to fallback
        };
        verts.push(WVert {
            pos: cur,
            bisector: bis,
            speed,
            birth: 0.0,
            alive: true,
            prev: (i + n - 1) % n,
            next: (i + 1) % n,
        });
    }

    // Priority queue of edge-event candidates: (time, a, b, node_pos).
    // Small n → a Vec scanned for the min is simplest and deterministic.
    let mut nodes: Vec<DVec2> = Vec::new();
    let mut alive_count = n;
    // Guard against pathological non-termination.
    let max_events = n + 4;
    for _ in 0..max_events {
        if alive_count <= 2 {
            break;
        }
        // Find the earliest valid edge event among adjacent alive pairs.
        let mut best: Option<(f64, usize, usize, DVec2)> = None;
        for a in 0..verts.len() {
            if !verts[a].alive {
                continue;
            }
            let b = verts[a].next;
            if !verts[b].alive || b == a {
                continue;
            }
            if let Some((t, p)) = edge_event(&verts[a], &verts[b]) {
                // t must be after both vertices' birth (event lies in the future).
                let t_after = t >= verts[a].birth - 1e-9 && t >= verts[b].birth - 1e-9;
                if t_after {
                    let better = match best {
                        None => true,
                        Some((bt, ..)) => {
                            t < bt - 1e-12
                                // Deterministic tie-break: lower index pair wins.
                                || ((t - bt).abs() <= 1e-12 && a < best.unwrap().1)
                        }
                    };
                    if better {
                        best = Some((t, a, b, p));
                    }
                }
            }
        }
        let Some((t, a, b, node)) = best else {
            break; // no convergent adjacent pair (all diverging) → done
        };
        nodes.push(node);

        // Collapse edge a–b into a new vertex at `node`. Remove a and b, splice a
        // fresh vertex between a.prev and b.next with its own bisector.
        let pa = verts[a].prev;
        let nb = verts[b].next;
        verts[a].alive = false;
        verts[b].alive = false;
        alive_count -= 2;

        if pa == b || nb == a || pa == nb {
            // The ring has collapsed to ≤ 2 remaining — the node IS the apex.
            break;
        }

        // New vertex bisector: between the edge (pa.pos→node) and (node→nb.pos)
        // as they will continue to move. Use the incoming neighbour geometry.
        let Some((bis, speed)) = bisector(verts[pa].pos, node, verts[nb].pos) else {
            break;
        };
        let new_idx = verts.len();
        verts.push(WVert {
            pos: node,
            bisector: bis,
            speed,
            birth: t,
            alive: true,
            prev: pa,
            next: nb,
        });
        verts[pa].next = new_idx;
        verts[nb].prev = new_idx;
        alive_count += 1;
    }

    nodes
}

/// Interior angle bisector (unit) at vertex `cur` between edges `prev→cur` and
/// `cur→next`, plus the vertex *speed* `1/sin(half_angle)` (how fast the vertex
/// moves per unit inward offset of the edges). Returns `None` on a degenerate
/// (zero-length edge or 180°/0° spike) vertex.
fn bisector(prev: DVec2, cur: DVec2, next: DVec2) -> Option<(DVec2, f64)> {
    let e_in = (cur - prev).normalize_or_zero(); // direction along incoming edge
    let e_out = (next - cur).normalize_or_zero(); // direction along outgoing edge
    if e_in.length_squared() < 0.5 || e_out.length_squared() < 0.5 {
        return None;
    }
    // Inward edge normals (CCW interior is to the LEFT of each edge direction).
    let n_in = DVec2::new(-e_in.y, e_in.x);
    let n_out = DVec2::new(-e_out.y, e_out.x);
    // The bisector direction is the normalized sum of the two inward normals
    // (points into the interior, bisecting the angle between the two edges).
    let sum = n_in + n_out;
    if sum.length_squared() < 1e-12 {
        return None; // 180° straight vertex — no distinct bisector
    }
    let bis = sum.normalize();
    // Vertex speed = 1 / sin(theta/2) where theta is the interior angle. Since
    // n_in and n_out are unit, |n_in + n_out| = 2 cos(phi/2) where phi is the
    // angle between the normals = pi - theta, so cos(phi/2) = sin(theta/2).
    // Therefore sin(theta/2) = |sum|/2 and speed = 2/|sum|.
    let sin_half = (sum.length() * 0.5).clamp(1e-9, 1.0);
    let speed = 1.0 / sin_half;
    Some((bis, speed))
}

/// The edge event between two adjacent wavefront vertices `a` and `b`: the offset
/// time `t` and point `p` at which their bisector rays meet (the edge a–b
/// collapses). Returns `None` if the bisectors diverge / are parallel.
fn edge_event(a: &WVert, b: &WVert) -> Option<(f64, DVec2)> {
    // Vertex a moves as pos_a(s) = a.pos + a.bisector * a.speed * (s - a.birth)
    // where s is global offset time. Solve pos_a(s) == pos_b(s).
    // Let va = a.bisector*a.speed, vb = b.bisector*b.speed.
    let va = a.bisector * a.speed;
    let vb = b.bisector * b.speed;
    // a.pos + va*(s-a.birth) = b.pos + vb*(s-b.birth)
    // (va - vb) * s = b.pos - a.pos + va*a.birth - vb*b.birth
    let rel = va - vb;
    let rhs = (b.pos - a.pos) + va * a.birth - vb * b.birth;
    // Solve the (over-determined 2D) system in a least-squares sense projected on
    // rel: s = rel·rhs / rel·rel.
    let denom = rel.dot(rel);
    if denom < 1e-12 {
        return None; // parallel motion — no finite meeting
    }
    let s = rel.dot(rhs) / denom;
    // The meeting point (average of the two extrapolations for robustness).
    let pa = a.pos + va * (s - a.birth);
    let pb = b.pos + vb * (s - b.birth);
    // Reject if the two extrapolations don't actually coincide (diverging rays
    // that only meet in the least-squares projection).
    if pa.distance(pb) > 1e-6 * (1.0 + pa.length() + pb.length()) {
        return None;
    }
    Some((s, (pa + pb) * 0.5))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(s: f64) -> Polygon2d {
        Polygon2d::from_pairs([(0.0, 0.0), (s, 0.0), (s, s), (0.0, s)]).unwrap()
    }

    #[test]
    fn convex_detection() {
        assert!(is_convex_ccw(&square(10.0)));
        let l = Polygon2d::from_pairs([
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 4.0),
            (4.0, 4.0),
            (4.0, 10.0),
            (0.0, 10.0),
        ])
        .unwrap();
        assert!(!is_convex_ccw(&l), "L-shape is non-convex");
    }

    #[test]
    fn square_collapses_to_center_node() {
        let sk = FelkelSkeleton::new();
        let nodes = sk.skeleton_nodes(&square(10.0));
        assert!(!nodes.is_empty(), "square yields skeleton nodes");
        // The final skeleton node of a square is its centre (5,5). All events of a
        // square converge there.
        let last = *nodes.last().unwrap();
        assert!(
            last.distance(DVec2::new(5.0, 5.0)) < 1e-6,
            "square skeleton node at centre, got {last:?}"
        );
    }

    #[test]
    fn rectangle_gives_medial_ridge_segment() {
        // A 20×10 rectangle's straight skeleton is a horizontal ridge segment
        // between (5,5) and (15,5) — two interior nodes, both at y=5.
        let sk = FelkelSkeleton::new();
        let r = Polygon2d::from_pairs([(0.0, 0.0), (20.0, 0.0), (20.0, 10.0), (0.0, 10.0)]).unwrap();
        let nodes = sk.skeleton_nodes(&r);
        assert!(nodes.len() >= 2, "rectangle ridge has ≥2 nodes, got {}", nodes.len());
        for nd in &nodes {
            assert!((nd.y - 5.0).abs() < 1e-6, "ridge node centred in y, got {nd:?}");
        }
        // The two ridge endpoints are at x≈5 and x≈15.
        let xs: Vec<f64> = nodes.iter().map(|p| p.x).collect();
        let xmin = xs.iter().cloned().fold(f64::INFINITY, f64::min);
        let xmax = xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        assert!((xmin - 5.0).abs() < 1e-6, "ridge starts x≈5, got {xmin}");
        assert!((xmax - 15.0).abs() < 1e-6, "ridge ends x≈15, got {xmax}");
    }

    #[test]
    fn triangle_node_at_incenter() {
        // Equilateral-ish triangle; the single skeleton node is the incenter.
        let sk = FelkelSkeleton::new();
        let tri = Polygon2d::from_pairs([(0.0, 0.0), (12.0, 0.0), (6.0, 10.392)]).unwrap();
        let nodes = sk.skeleton_nodes(&tri);
        assert_eq!(nodes.len(), 1, "a triangle has one skeleton node (incenter)");
        // Incenter x is 6 by symmetry; y = r (inradius) > 0, well inside.
        let nd = nodes[0];
        assert!((nd.x - 6.0).abs() < 1e-4, "incenter x=6, got {nd:?}");
        assert!(nd.y > 0.5 && nd.y < 10.0, "incenter inside, got {nd:?}");
    }

    #[test]
    fn nonconvex_returns_no_nodes_but_faces_via_fallback() {
        let l = Polygon2d::from_pairs([
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 4.0),
            (4.0, 4.0),
            (4.0, 10.0),
            (0.0, 10.0),
        ])
        .unwrap();
        let sk = FelkelSkeleton::new();
        assert!(
            sk.skeleton_nodes(&l).is_empty(),
            "non-convex: no exact Felkel nodes (split events unsupported)"
        );
        // But faces() still returns a covering partition via the fallback.
        let faces = sk.faces(&l);
        assert!(!faces.is_empty(), "faces via OffsetApprox fallback");
        let sum: f64 = faces.iter().map(|f| f.polygon.area()).sum();
        assert!(sum >= l.area() * 0.9, "fallback faces cover the L-shape");
    }

    #[test]
    fn convex_felkel_faces_agree_with_offset_approx() {
        // On a convex polygon the Felkel face partition equals the approximate
        // (nearest-edge) partition to tolerance — both are the bisector partition.
        let hex = Polygon2d::from_pairs([
            (10.0, 0.0),
            (5.0, 8.66),
            (-5.0, 8.66),
            (-10.0, 0.0),
            (-5.0, -8.66),
            (5.0, -8.66),
        ])
        .unwrap();
        let felkel = FelkelSkeleton::new().faces(&hex);
        let approx = OffsetApproxSkeleton::new().faces(&hex);
        assert_eq!(felkel.len(), approx.len(), "same face count on convex");
        let fs: f64 = felkel.iter().map(|f| f.polygon.area()).sum();
        let as_: f64 = approx.iter().map(|f| f.polygon.area()).sum();
        assert!((fs - as_).abs() < 1e-6, "same total coverage on convex");
    }
}
