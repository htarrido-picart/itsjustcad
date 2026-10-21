// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

use glam::DVec3;

use crate::Mesh;

/// Vertices closer to the plane than this are nudged to one side so every
/// crossing triangle yields exactly one clean segment (no coplanar cases).
const PLANE_EPS: f64 = 1e-9;

/// One chained run of plane/mesh intersection segments.
#[derive(Clone, Debug, PartialEq)]
pub struct SliceCurve {
    pub points: Vec<DVec3>,
    /// `true` for a loop (watertight solid); `false` for an open polyline that
    /// ends on a mesh boundary (open surface such as a vault or hypar shell).
    pub closed: bool,
}

/// Intersect `mesh` with the plane through `point` with `normal`. Each
/// triangle crossing the plane contributes one segment; segments are chained
/// end-to-end within `tol` and collinear runs are collapsed, so slicing a box
/// mid-height yields its rectangle outline. A chain that returns to its start
/// is a closed loop; one that stops on a boundary edge of an open surface is
/// kept as an open polyline.
pub fn slice(mesh: &Mesh, point: DVec3, normal: DVec3, tol: f64) -> Vec<SliceCurve> {
    let n = normal.normalize_or_zero();
    if n == DVec3::ZERO {
        return Vec::new();
    }
    let dists: Vec<f64> = mesh
        .positions()
        .iter()
        .map(|p| {
            let d = n.dot(*p - point);
            // Nudge on-plane vertices to the positive side: a triangle then
            // either misses the plane or crosses it on exactly two edges.
            if d.abs() < PLANE_EPS { PLANE_EPS } else { d }
        })
        .collect();
    let mut segments = Vec::new();
    for face in mesh.faces() {
        let idx = face.map(|i| i as usize);
        let mut ends = Vec::with_capacity(2);
        for k in 0..3 {
            let (a, b) = (idx[k], idx[(k + 1) % 3]);
            if dists[a] * dists[b] < 0.0 {
                let t = dists[a] / (dists[a] - dists[b]);
                ends.push(mesh.positions()[a].lerp(mesh.positions()[b], t));
            }
        }
        if let [a, b] = ends[..] {
            segments.push((a, b));
        }
    }
    chain_segments(segments, tol)
        .into_iter()
        .filter_map(|c| {
            let points = if c.closed {
                simplify_loop(c.points, tol)
            } else {
                simplify_open(c.points, tol)
            };
            let keep = if c.closed {
                points.len() >= 3
            } else {
                // Drop slivers where the plane only grazes a boundary corner.
                points.windows(2).map(|w| w[0].distance(w[1])).sum::<f64>() > tol
            };
            keep.then_some(SliceCurve { points, ..c })
        })
        .collect()
}

/// Chain segments by walking matching endpoints (within `tol`). A chain that
/// returns to its start is closed; otherwise it is grown from both ends until
/// neither touches another segment, giving an open polyline.
fn chain_segments(mut segments: Vec<(DVec3, DVec3)>, tol: f64) -> Vec<SliceCurve> {
    let mut curves = Vec::new();
    while let Some((a, b)) = segments.pop() {
        let mut points = vec![a, b];
        if grow_tail(&mut points, &mut segments, tol) {
            points.pop(); // closed: drop the duplicate of the start point
            curves.push(SliceCurve { points, closed: true });
            continue;
        }
        // Open: the tail sits on a boundary; grow the other end too.
        points.reverse();
        grow_tail(&mut points, &mut segments, tol);
        curves.push(SliceCurve { points, closed: false });
    }
    curves
}

/// Extend `chain` from its last point; `true` once it closes on its start.
fn grow_tail(chain: &mut Vec<DVec3>, segments: &mut Vec<(DVec3, DVec3)>, tol: f64) -> bool {
    loop {
        let tail = *chain.last().expect("chain is non-empty");
        if tail.distance(chain[0]) <= tol && chain.len() >= 3 {
            return true;
        }
        let Some(i) = segments
            .iter()
            .position(|(p, q)| p.distance(tail) <= tol || q.distance(tail) <= tol)
        else {
            return false;
        };
        let (p, q) = segments.swap_remove(i);
        chain.push(if p.distance(tail) <= tol { q } else { p });
    }
}

/// [`simplify_loop`] for an open polyline: interior points only, the two
/// boundary endpoints are kept.
fn simplify_open(mut pts: Vec<DVec3>, tol: f64) -> Vec<DVec3> {
    loop {
        let Some(i) = (1..pts.len().saturating_sub(1)).find(|&i| {
            let (prev, p, next) = (pts[i - 1], pts[i], pts[i + 1]);
            p.distance(prev) <= tol || distance_to_segment(p, prev, next) <= tol
        }) else {
            return pts;
        };
        pts.remove(i);
    }
}

/// Drop near-duplicate neighbours and collinear points (triangle diagonals
/// land mid-edge on straight walls), treating the loop as circular.
fn simplify_loop(mut pts: Vec<DVec3>, tol: f64) -> Vec<DVec3> {
    loop {
        let n = pts.len();
        if n < 4 {
            return pts;
        }
        let Some(i) = (0..n).find(|&i| {
            let (prev, p, next) = (pts[(i + n - 1) % n], pts[i], pts[(i + 1) % n]);
            p.distance(prev) <= tol || distance_to_segment(p, prev, next) <= tol
        }) else {
            return pts;
        };
        pts.remove(i);
    }
}

fn distance_to_segment(p: DVec3, a: DVec3, b: DVec3) -> f64 {
    let ab = b - a;
    let len2 = ab.length_squared();
    if len2 < PLANE_EPS * PLANE_EPS {
        return p.distance(a);
    }
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    p.distance(a + ab * t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{csg_difference, make_box};

    const TOL: f64 = 1e-6;

    /// Shoelace area of a loop projected on XY.
    fn loop_area(pts: &[DVec3]) -> f64 {
        crate::signed_area(&pts.iter().map(|p| p.truncate()).collect::<Vec<_>>()).abs()
    }

    #[test]
    fn mid_sliced_box_is_rect_outline() {
        let mesh = make_box(DVec3::ZERO, DVec3::new(2.0, 3.0, 4.0));
        let loops = slice(&mesh, DVec3::new(0.0, 0.0, 2.0), DVec3::Z, TOL);
        assert_eq!(loops.len(), 1);
        assert!(loops[0].closed);
        let outline = &loops[0].points;
        // Triangle diagonals collapse away: exactly the 4 rectangle corners.
        assert_eq!(outline.len(), 4, "{outline:?}");
        for p in outline {
            assert!((p.z - 2.0).abs() < TOL);
            assert!([0.0, 2.0].iter().any(|x| (p.x - x).abs() < TOL));
            assert!([0.0, 3.0].iter().any(|y| (p.y - y).abs() < TOL));
        }
        assert!((loop_area(outline) - 6.0).abs() < 1e-9);
    }

    #[test]
    fn vertical_slice_and_miss() {
        let mesh = make_box(DVec3::ZERO, DVec3::new(2.0, 3.0, 4.0));
        // Vertical cut through the middle: 3 x 4 rectangle in the YZ plane.
        let loops = slice(&mesh, DVec3::new(1.0, 0.0, 0.0), DVec3::X, TOL);
        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0].points.len(), 4);
        assert!(loops[0].closed);
        // Plane above the box: nothing.
        assert!(slice(&mesh, DVec3::new(0.0, 0.0, 5.0), DVec3::Z, TOL).is_empty());
        // Plane through the top face (coplanar vertices) must not panic.
        let top = slice(&mesh, DVec3::new(0.0, 0.0, 4.0), DVec3::Z, TOL);
        assert!(top.len() <= 1);
    }

    #[test]
    fn courtyard_slice_yields_outer_and_inner_loops() {
        let outer = make_box(DVec3::ZERO, DVec3::new(10.0, 8.0, 3.0));
        let hole = make_box(DVec3::new(3.0, 2.0, -0.5), DVec3::new(4.0, 4.0, 4.0));
        let courtyard = csg_difference(&outer, &hole);
        let loops = slice(&courtyard, DVec3::new(0.0, 0.0, 1.5), DVec3::Z, TOL);
        assert_eq!(loops.len(), 2, "outer outline + courtyard hole");
        let mut areas: Vec<f64> = loops.iter().map(|l| loop_area(&l.points)).collect();
        areas.sort_by(f64::total_cmp);
        assert!((areas[0] - 16.0).abs() < 1e-6, "hole 4x4, got {areas:?}");
        assert!((areas[1] - 80.0).abs() < 1e-6, "outer 10x8, got {areas:?}");
    }

    #[test]
    fn open_surface_slice_is_open_polyline() {
        // Single-sided 4x6 grid sheet at z=1: not watertight, so the cut stops
        // on the boundary edges instead of closing.
        let (nu, nv) = (4u32, 6u32);
        let positions = (0..=nv)
            .flat_map(|j| (0..=nu).map(move |i| DVec3::new(f64::from(i), f64::from(j), 1.0)))
            .collect();
        let idx = |i: u32, j: u32| j * (nu + 1) + i;
        let faces = (0..nv)
            .flat_map(|j| {
                (0..nu).flat_map(move |i| {
                    let (a0, a2) = (idx(i, j), idx(i + 1, j + 1));
                    [[a0, idx(i + 1, j), a2], [a0, a2, idx(i, j + 1)]]
                })
            })
            .collect();
        let sheet = Mesh::new(positions, faces);
        // Plane through a grid row (vertices on the plane) and between rows.
        for y in [3.0, 2.5] {
            let cuts = slice(&sheet, DVec3::new(0.0, y, 0.0), DVec3::Y, TOL);
            assert_eq!(cuts.len(), 1, "y={y}: {cuts:?}");
            let cut = &cuts[0];
            assert!(!cut.closed);
            assert_eq!(cut.points.len(), 2, "collinear run collapses: {cut:?}");
            let mut xs: Vec<f64> = cut.points.iter().map(|p| p.x).collect();
            xs.sort_by(f64::total_cmp);
            assert!(xs[0].abs() < 1e-6 && (xs[1] - 4.0).abs() < 1e-6, "{cut:?}");
            assert!(cut.points.iter().all(|p| (p.y - y).abs() < 1e-6 && (p.z - 1.0).abs() < 1e-9));
        }
        assert!(slice(&sheet, DVec3::new(0.0, 7.0, 0.0), DVec3::Y, TOL).is_empty());
    }
}
