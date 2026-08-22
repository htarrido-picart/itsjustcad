use glam::DVec2;

/// Ear-clipping triangulation of a simple polygon (no holes, no
/// self-intersection). Returns index triples into `pts`, CCW winding.
pub fn earcut(pts: &[DVec2]) -> Vec<[u32; 3]> {
    let n = pts.len();
    if n < 3 {
        return Vec::new();
    }

    // Ensure CCW working order
    let ccw = signed_area(pts) >= 0.0;
    let mut idx: Vec<u32> = if ccw {
        (0..n as u32).collect()
    } else {
        (0..n as u32).rev().collect()
    };

    let mut tris = Vec::with_capacity(n - 2);
    let mut guard = 0usize;
    while idx.len() > 3 && guard < n * n {
        guard += 1;
        let m = idx.len();
        let mut clipped = false;
        for i in 0..m {
            let (ia, ib, ic) = (idx[(i + m - 1) % m], idx[i], idx[(i + 1) % m]);
            let (a, b, c) = (pts[ia as usize], pts[ib as usize], pts[ic as usize]);
            if cross(b - a, c - b) <= 1e-12 {
                continue; // reflex or degenerate
            }
            let is_ear = idx.iter().all(|&j| {
                if j == ia || j == ib || j == ic {
                    true
                } else {
                    !point_in_triangle(pts[j as usize], a, b, c)
                }
            });
            if is_ear {
                tris.push([ia, ib, ic]);
                idx.remove(i);
                clipped = true;
                break;
            }
        }
        if !clipped {
            break; // degenerate input; emit what we have
        }
    }
    if idx.len() == 3 {
        tris.push([idx[0], idx[1], idx[2]]);
    }
    tris
}

pub fn signed_area(pts: &[DVec2]) -> f64 {
    let n = pts.len();
    let mut a = 0.0;
    for i in 0..n {
        let p = pts[i];
        let q = pts[(i + 1) % n];
        a += p.x * q.y - q.x * p.y;
    }
    a * 0.5
}

fn cross(u: DVec2, v: DVec2) -> f64 {
    u.x * v.y - u.y * v.x
}

fn point_in_triangle(p: DVec2, a: DVec2, b: DVec2, c: DVec2) -> bool {
    let d1 = cross(b - a, p - a);
    let d2 = cross(c - b, p - b);
    let d3 = cross(a - c, p - c);
    let has_neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let has_pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(has_neg && has_pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn square_two_triangles() {
        let pts = vec![
            DVec2::new(0.0, 0.0),
            DVec2::new(1.0, 0.0),
            DVec2::new(1.0, 1.0),
            DVec2::new(0.0, 1.0),
        ];
        let tris = earcut(&pts);
        assert_eq!(tris.len(), 2);
    }

    #[test]
    fn concave_l_shape() {
        let pts = vec![
            DVec2::new(0.0, 0.0),
            DVec2::new(2.0, 0.0),
            DVec2::new(2.0, 1.0),
            DVec2::new(1.0, 1.0),
            DVec2::new(1.0, 2.0),
            DVec2::new(0.0, 2.0),
        ];
        let tris = earcut(&pts);
        assert_eq!(tris.len(), 4);
        // Total triangulated area must equal polygon area (3.0)
        let area: f64 = tris
            .iter()
            .map(|t| {
                let [a, b, c] = t.map(|i| pts[i as usize]);
                cross(b - a, c - a).abs() * 0.5
            })
            .sum();
        assert!((area - 3.0).abs() < 1e-9);
    }
}
