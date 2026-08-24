// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

use glam::DVec3;

use crate::Mesh;

/// Vertices closer to the plane than this are nudged to one side so every
/// crossing triangle yields exactly one clean segment (no coplanar cases).
const PLANE_EPS: f64 = 1e-9;

/// Intersect `mesh` with the plane through `point` with `normal`, returning
/// the closed intersection loops. Each triangle crossing the plane
/// contributes one segment; segments are chained end-to-end within `tol` and
/// collinear runs are collapsed, so slicing a box mid-height yields its
/// rectangle outline. Open chains (non-watertight meshes) are dropped.
pub fn slice(mesh: &Mesh, point: DVec3, normal: DVec3, tol: f64) -> Vec<Vec<DVec3>> {
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
    chain_loops(segments, tol)
        .into_iter()
        .map(|pts| simplify_loop(pts, tol))
        .filter(|pts| pts.len() >= 3)
        .collect()
}

/// Chain segments into closed loops by walking matching endpoints (within
/// `tol`). Chains that never return to their start are discarded.
fn chain_loops(mut segments: Vec<(DVec3, DVec3)>, tol: f64) -> Vec<Vec<DVec3>> {
    let mut loops = Vec::new();
    while let Some((a, b)) = segments.pop() {
        let mut chain = vec![a, b];
        loop {
            let tail = *chain.last().expect("chain is non-empty");
            if tail.distance(chain[0]) <= tol && chain.len() >= 3 {
                chain.pop(); // closed: drop the duplicate of the start point
                loops.push(chain);
                break;
            }
            let Some(i) = segments
                .iter()
                .position(|(p, q)| p.distance(tail) <= tol || q.distance(tail) <= tol)
            else {
                break; // open chain (hole in the mesh): drop it
            };
            let (p, q) = segments.swap_remove(i);
            chain.push(if p.distance(tail) <= tol { q } else { p });
        }
    }
    loops
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
        let outline = &loops[0];
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
        assert_eq!(loops[0].len(), 4);
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
        let mut areas: Vec<f64> = loops.iter().map(|l| loop_area(l)).collect();
        areas.sort_by(f64::total_cmp);
        assert!((areas[0] - 16.0).abs() < 1e-6, "hole 4x4, got {areas:?}");
        assert!((areas[1] - 80.0).abs() < 1e-6, "outer 10x8, got {areas:?}");
    }
}
