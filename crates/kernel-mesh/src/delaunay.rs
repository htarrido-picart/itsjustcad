//! Bowyer-Watson Delaunay triangulation of 2D points (zero deps).
//!
//! Used by the `terrain` command: triangulate the XY projection of scattered
//! survey points, then lift each triangle's vertices back to their z to make a
//! terrain surface mesh. Returns index triples into the input point slice.

use glam::DVec2;

/// Delaunay triangulation of `pts` (XY). Returns CCW index triples into `pts`.
///
/// Points are deduplicated in XY (first occurrence wins); triangles reference
/// the original indices. Fewer than 3 distinct points → no triangles. Collinear
/// point sets also yield no triangles (no area to triangulate).
pub fn triangulate(pts: &[DVec2]) -> Vec<[u32; 3]> {
    let n = pts.len();
    if n < 3 {
        return Vec::new();
    }

    // Super-triangle enclosing every point. Indices n, n+1, n+2 are the
    // super-triangle corners; triangles touching them are dropped at the end.
    let mut min = pts[0];
    let mut max = pts[0];
    for p in pts {
        min = min.min(*p);
        max = max.max(*p);
    }
    let d = (max - min).max_element().max(1.0);
    let mid = (min + max) * 0.5;
    // A generous margin keeps the super-triangle well outside the point cloud
    // so its circumcircles never wrongly reject interior edges.
    let sp = [
        DVec2::new(mid.x - 20.0 * d, mid.y - d),
        DVec2::new(mid.x + 20.0 * d, mid.y - d),
        DVec2::new(mid.x, mid.y + 20.0 * d),
    ];
    let point = |i: u32| -> DVec2 {
        if (i as usize) < n {
            pts[i as usize]
        } else {
            sp[i as usize - n]
        }
    };

    let mut tris: Vec<[u32; 3]> = vec![[n as u32, n as u32 + 1, n as u32 + 2]];

    for (pi, p) in pts.iter().enumerate() {
        // Skip exact XY duplicates: they add no triangles and break the
        // circumcircle test (degenerate). First occurrence keeps the point.
        if pts[..pi].contains(p) {
            continue;
        }
        let pi = pi as u32;

        // Find every triangle whose circumcircle contains p ("bad"), collect
        // their edges, and re-triangulate the resulting cavity to p.
        let mut bad: Vec<[u32; 3]> = Vec::new();
        let mut good: Vec<[u32; 3]> = Vec::new();
        for t in &tris {
            if in_circumcircle(*p, point(t[0]), point(t[1]), point(t[2])) {
                bad.push(*t);
            } else {
                good.push(*t);
            }
        }

        // Boundary of the cavity: edges belonging to exactly one bad triangle.
        let mut boundary: Vec<[u32; 2]> = Vec::new();
        for t in &bad {
            for e in [[t[0], t[1]], [t[1], t[2]], [t[2], t[0]]] {
                let shared = bad.iter().filter(|o| triangle_has_edge(o, e)).count();
                if shared == 1 {
                    boundary.push(e);
                }
            }
        }

        tris = good;
        for e in boundary {
            // Keep CCW winding: e is oriented from its owning triangle.
            tris.push([e[0], e[1], pi]);
        }
    }

    // Drop triangles that touch a super-triangle corner and normalize winding.
    tris.into_iter()
        .filter(|t| t.iter().all(|&i| (i as usize) < n))
        .map(|t| {
            let (a, b, c) = (point(t[0]), point(t[1]), point(t[2]));
            if (b - a).perp_dot(c - a) < 0.0 {
                [t[0], t[2], t[1]]
            } else {
                t
            }
        })
        .collect()
}

/// True if `p` lies strictly inside the circumcircle of triangle a,b,c.
/// Uses the standard determinant test; a,b,c are ordered CCW for the sign.
fn in_circumcircle(p: DVec2, a: DVec2, b: DVec2, c: DVec2) -> bool {
    // Orient a,b,c CCW so the determinant sign is consistent.
    let (a, b, c) = if (b - a).perp_dot(c - a) < 0.0 { (a, c, b) } else { (a, b, c) };
    let ax = a.x - p.x;
    let ay = a.y - p.y;
    let bx = b.x - p.x;
    let by = b.y - p.y;
    let cx = c.x - p.x;
    let cy = c.y - p.y;
    let det = (ax * ax + ay * ay) * (bx * cy - cx * by)
        - (bx * bx + by * by) * (ax * cy - cx * ay)
        + (cx * cx + cy * cy) * (ax * by - bx * ay);
    det > 1e-12
}

/// True if triangle `t` contains the undirected edge `e`.
fn triangle_has_edge(t: &[u32; 3], e: [u32; 2]) -> bool {
    let edges = [[t[0], t[1]], [t[1], t[2]], [t[2], t[0]]];
    edges
        .iter()
        .any(|te| (te[0] == e[0] && te[1] == e[1]) || (te[0] == e[1] && te[1] == e[0]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn square_plus_center_is_four_triangles() {
        // Unit square corners + center: the Delaunay triangulation fans the
        // center to all four edges → exactly 4 triangles.
        let pts = vec![
            DVec2::new(0.0, 0.0),
            DVec2::new(1.0, 0.0),
            DVec2::new(1.0, 1.0),
            DVec2::new(0.0, 1.0),
            DVec2::new(0.5, 0.5),
        ];
        let tris = triangulate(&pts);
        assert_eq!(tris.len(), 4, "square + center = 4 triangles");
        // Every triangle must include the center (index 4).
        for t in &tris {
            assert!(t.contains(&4), "each triangle fans from the center: {t:?}");
        }
    }

    #[test]
    fn single_triangle_from_three_points() {
        let pts = vec![
            DVec2::new(0.0, 0.0),
            DVec2::new(2.0, 0.0),
            DVec2::new(0.0, 2.0),
        ];
        let tris = triangulate(&pts);
        assert_eq!(tris.len(), 1);
    }

    #[test]
    fn square_is_two_triangles() {
        let pts = vec![
            DVec2::new(0.0, 0.0),
            DVec2::new(1.0, 0.0),
            DVec2::new(1.0, 1.0),
            DVec2::new(0.0, 1.0),
        ];
        let tris = triangulate(&pts);
        assert_eq!(tris.len(), 2, "square = 2 triangles");
    }

    #[test]
    fn all_triangles_are_ccw() {
        let pts = vec![
            DVec2::new(0.0, 0.0),
            DVec2::new(1.0, 0.0),
            DVec2::new(1.0, 1.0),
            DVec2::new(0.0, 1.0),
            DVec2::new(0.5, 0.5),
        ];
        for t in triangulate(&pts) {
            let (a, b, c) = (
                pts[t[0] as usize],
                pts[t[1] as usize],
                pts[t[2] as usize],
            );
            assert!((b - a).perp_dot(c - a) > 0.0, "CCW winding: {t:?}");
        }
    }

    #[test]
    fn fewer_than_three_points_empty() {
        assert!(triangulate(&[]).is_empty());
        assert!(triangulate(&[DVec2::ZERO]).is_empty());
        assert!(triangulate(&[DVec2::ZERO, DVec2::X]).is_empty());
    }

    #[test]
    fn duplicate_points_are_ignored() {
        let pts = vec![
            DVec2::new(0.0, 0.0),
            DVec2::new(2.0, 0.0),
            DVec2::new(0.0, 2.0),
            DVec2::new(0.0, 0.0), // duplicate of index 0
        ];
        let tris = triangulate(&pts);
        assert_eq!(tris.len(), 1, "duplicate adds no triangle");
        assert!(tris.iter().all(|t| !t.contains(&3)), "dup index unused");
    }

    #[test]
    fn grid_covers_full_area() {
        // 3x3 grid = 9 points → a fully-triangulated 2x2 quad region = 8 tris.
        let mut pts = Vec::new();
        for y in 0..3 {
            for x in 0..3 {
                pts.push(DVec2::new(x as f64, y as f64));
            }
        }
        let tris = triangulate(&pts);
        assert_eq!(tris.len(), 8, "3x3 grid triangulates to 8 triangles");
    }
}
