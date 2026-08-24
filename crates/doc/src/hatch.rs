// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Hatch pattern tessellation: pure geometry, no GPU dependencies.
//! Used by both the render crate (viewport) and the commands crate (PDF/DXF).

use glam::{DVec2, DVec3};

/// Parallel hatch lines clipped to a closed polygon (even-odd rule) in the
/// XY plane; the boundary's first-point z carries through.
pub fn hatch_lines(boundary: &[DVec3], angle_deg: f64, spacing: f64) -> Vec<[DVec3; 2]> {
    if boundary.len() < 3 || spacing <= 0.0 {
        return Vec::new();
    }
    let z = boundary[0].z;
    let angle = angle_deg.to_radians();
    let (sin, cos) = angle.sin_cos();
    let to_pat = |p: DVec3| DVec2::new(p.x * cos + p.y * sin, -p.x * sin + p.y * cos);
    let from_pat = |p: DVec2| DVec3::new(p.x * cos - p.y * sin, p.x * sin + p.y * cos, z);
    let pts: Vec<DVec2> = boundary.iter().map(|&p| to_pat(p)).collect();
    let (min_y, max_y) = pts
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| {
            (lo.min(p.y), hi.max(p.y))
        });
    let mut segments = Vec::new();
    let mut k = (min_y / spacing).ceil() as i64;
    while (k as f64) * spacing <= max_y {
        let y = k as f64 * spacing;
        let mut xs: Vec<f64> = Vec::new();
        for i in 0..pts.len() {
            let a = pts[i];
            let b = pts[(i + 1) % pts.len()];
            if (a.y <= y) != (b.y <= y) {
                xs.push(a.x + (y - a.y) / (b.y - a.y) * (b.x - a.x));
            }
        }
        xs.sort_by(|p, q| p.partial_cmp(q).expect("finite"));
        for chunk in xs.chunks(2) {
            if let [x0, x1] = chunk && x1 - x0 > 1e-9 {
                segments.push([
                    from_pat(DVec2::new(*x0, y)),
                    from_pat(DVec2::new(*x1, y)),
                ]);
            }
        }
        k += 1;
    }
    segments
}

/// Running-bond brick hatch.
pub fn hatch_brick(boundary: &[DVec3], spacing: f64) -> Vec<[DVec3; 2]> {
    if boundary.len() < 3 || spacing <= 0.0 {
        return Vec::new();
    }
    let z = boundary[0].z;
    let pts2: Vec<DVec2> = boundary.iter().map(|p| p.truncate()).collect();
    let (min_x, max_x, min_y, max_y) = pts2.iter().fold(
        (f64::INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY),
        |(lx, hx, ly, hy), p| (lx.min(p.x), hx.max(p.x), ly.min(p.y), hy.max(p.y)),
    );
    let mut segs = Vec::new();
    // Horizontal bed joints.
    let base_ky = (min_y / spacing).ceil() as i64;
    let mut ky = base_ky;
    while (ky as f64) * spacing <= max_y {
        let y = ky as f64 * spacing;
        let xs = scanline_xs(&pts2, y);
        for chunk in xs.chunks(2) {
            if let [x0, x1] = chunk && x1 - x0 > 1e-9 {
                segs.push([DVec3::new(*x0, y, z), DVec3::new(*x1, y, z)]);
            }
        }
        ky += 1;
    }
    // Staggered vertical head joints.
    let brick_w = spacing * 2.0;
    ky = base_ky;
    while (ky as f64) * spacing <= max_y {
        let y0 = ky as f64 * spacing;
        let y1 = y0 + spacing;
        let offset = if ky.rem_euclid(2) == 0 { 0.0 } else { brick_w / 2.0 };
        let bx_start = ((min_x - offset) / brick_w).floor() as i64;
        let bx_end = ((max_x - offset) / brick_w).ceil() as i64;
        for kx in bx_start..=bx_end {
            let x = kx as f64 * brick_w + offset;
            if point_in_poly(&pts2, DVec2::new(x, (y0 + y1) / 2.0)) {
                segs.push([DVec3::new(x, y0.max(min_y), z), DVec3::new(x, y1.min(max_y), z)]);
            }
        }
        ky += 1;
    }
    segs
}

/// Concrete hatch: irregular short dashes.
pub fn hatch_concrete(boundary: &[DVec3], spacing: f64) -> Vec<[DVec3; 2]> {
    if boundary.len() < 3 || spacing <= 0.0 {
        return Vec::new();
    }
    let z = boundary[0].z;
    let pts2: Vec<DVec2> = boundary.iter().map(|p| p.truncate()).collect();
    let (min_x, max_x, min_y, max_y) = pts2.iter().fold(
        (f64::INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY),
        |(lx, hx, ly, hy), p| (lx.min(p.x), hx.max(p.x), ly.min(p.y), hy.max(p.y)),
    );
    let dash = spacing * 0.4;
    let mut segs = Vec::new();
    let mut ky = (min_y / spacing).floor() as i64;
    while (ky as f64) * spacing <= max_y + spacing {
        let y = ky as f64 * spacing;
        let mut kx = (min_x / spacing).floor() as i64;
        while (kx as f64) * spacing <= max_x + spacing {
            let x = kx as f64 * spacing;
            let hx_off = ((kx * 7 + ky * 13) % 7) as f64 * spacing / 7.0 - spacing * 0.3;
            let hy_off = ((kx * 11 + ky * 5) % 5) as f64 * spacing / 5.0 - spacing * 0.2;
            let cx = x + hx_off;
            let cy = y + hy_off;
            let angle = if (kx + ky) % 2 == 0 { 45f64 } else { -45f64 };
            let (sin_a, cos_a) = angle.to_radians().sin_cos();
            let a = DVec2::new(cx - cos_a * dash / 2.0, cy - sin_a * dash / 2.0);
            let b = DVec2::new(cx + cos_a * dash / 2.0, cy + sin_a * dash / 2.0);
            if point_in_poly(&pts2, DVec2::new(cx, cy)) {
                segs.push([DVec3::new(a.x, a.y, z), DVec3::new(b.x, b.y, z)]);
            }
            kx += 1;
        }
        ky += 1;
    }
    segs
}

/// Insulation batt hatch: zigzag line.
pub fn hatch_insulation(boundary: &[DVec3], spacing: f64) -> Vec<[DVec3; 2]> {
    if boundary.len() < 3 || spacing <= 0.0 {
        return Vec::new();
    }
    let z = boundary[0].z;
    let pts2: Vec<DVec2> = boundary.iter().map(|p| p.truncate()).collect();
    let (min_x, max_x, min_y, max_y) = pts2.iter().fold(
        (f64::INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY),
        |(lx, hx, ly, hy), p| (lx.min(p.x), hx.max(p.x), ly.min(p.y), hy.max(p.y)),
    );
    let half = spacing / 2.0;
    let period = spacing;
    let mut segs = Vec::new();
    let mut ky = (min_y / spacing).floor() as i64;
    while (ky as f64) * spacing <= max_y {
        let base_y = ky as f64 * spacing + half;
        let steps = ((max_x - min_x) / (period / 2.0)).ceil() as i64 + 2;
        let x_start = (min_x / (period / 2.0)).floor() as i64;
        for i in x_start..x_start + steps {
            let x0 = i as f64 * period / 2.0;
            let x1 = (i + 1) as f64 * period / 2.0;
            let y0 = base_y + if i % 2 == 0 { -half } else { half };
            let y1 = base_y + if i % 2 == 0 { half } else { -half };
            let mid = DVec2::new((x0 + x1) / 2.0, (y0 + y1) / 2.0);
            if mid.x >= min_x - 1e-9
                && mid.x <= max_x + 1e-9
                && mid.y >= min_y - 1e-9
                && mid.y <= max_y + 1e-9
                && point_in_poly(&pts2, mid)
            {
                segs.push([DVec3::new(x0, y0, z), DVec3::new(x1, y1, z)]);
            }
        }
        ky += 1;
    }
    segs
}

/// Earth fill hatch: 45° short dashes.
pub fn hatch_earth(boundary: &[DVec3], spacing: f64) -> Vec<[DVec3; 2]> {
    if boundary.len() < 3 || spacing <= 0.0 {
        return Vec::new();
    }
    let z = boundary[0].z;
    let dash = spacing * 0.5;
    let pts2: Vec<DVec2> = boundary.iter().map(|p| p.truncate()).collect();
    hatch_lines(boundary, 45.0, spacing)
        .into_iter()
        .flat_map(|[a, b]| {
            let span = (b - a).length();
            if span < 1e-9 {
                return Vec::new();
            }
            let dir = (b - a) / span;
            let step = dash * 2.0;
            let count = (span / step).floor() as usize + 1;
            (0..count)
                .filter_map(|i| {
                    let t0 = i as f64 * step;
                    let t1 = (t0 + dash).min(span);
                    if t0 >= span || (t1 - t0) < 1e-9 {
                        return None;
                    }
                    let pa = a + dir * t0;
                    let pb = a + dir * t1;
                    if point_in_poly(&pts2, pa.truncate()) {
                        Some([DVec3::new(pa.x, pa.y, z), DVec3::new(pb.x, pb.y, z)])
                    } else {
                        None
                    }
                })
                .collect()
        })
        .collect()
}

/// Even-odd point-in-polygon test (XY plane).
pub fn point_in_poly(pts: &[DVec2], p: DVec2) -> bool {
    let mut inside = false;
    let n = pts.len();
    let mut j = n - 1;
    for i in 0..n {
        let a = pts[i];
        let b = pts[j];
        if ((a.y > p.y) != (b.y > p.y))
            && (p.x < (b.x - a.x) * (p.y - a.y) / (b.y - a.y) + a.x)
        {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Sorted x-intercepts of a scanline at `y` with the polygon.
pub fn scanline_xs(pts: &[DVec2], y: f64) -> Vec<f64> {
    let mut xs = Vec::new();
    let n = pts.len();
    for i in 0..n {
        let a = pts[i];
        let b = pts[(i + 1) % n];
        if (a.y <= y) != (b.y <= y) {
            xs.push(a.x + (y - a.y) / (b.y - a.y) * (b.x - a.x));
        }
    }
    xs.sort_by(|p, q| p.partial_cmp(q).expect("finite"));
    xs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_square() -> Vec<DVec3> {
        vec![
            DVec3::ZERO,
            DVec3::new(1.0, 0.0, 0.0),
            DVec3::new(1.0, 1.0, 0.0),
            DVec3::new(0.0, 1.0, 0.0),
        ]
    }

    #[test]
    fn hatch_lines_clips_to_boundary() {
        let sq = unit_square();
        let segs = hatch_lines(&sq, 0.0, 0.25);
        assert!(segs.len() >= 3, "got {}", segs.len());
        for [a, b] in &segs {
            assert!((a.y - b.y).abs() < 1e-9, "must be horizontal");
        }
        // degenerate
        assert!(hatch_lines(&sq[..2], 0.0, 0.25).is_empty());
        assert!(hatch_lines(&sq, 0.0, 0.0).is_empty());
    }

    #[test]
    fn hatch_brick_produces_segments() {
        let sq = unit_square();
        assert!(!hatch_brick(&sq, 0.25).is_empty());
        assert!(hatch_brick(&sq, 0.0).is_empty());
    }

    #[test]
    fn hatch_concrete_produces_segments() {
        let sq = unit_square();
        assert!(!hatch_concrete(&sq, 0.3).is_empty());
        assert!(hatch_concrete(&sq, 0.0).is_empty());
    }

    #[test]
    fn hatch_insulation_produces_segments() {
        let sq = unit_square();
        assert!(!hatch_insulation(&sq, 0.3).is_empty());
        assert!(hatch_insulation(&sq, 0.0).is_empty());
    }

    #[test]
    fn hatch_earth_inside_boundary() {
        let sq = unit_square();
        let segs = hatch_earth(&sq, 0.25);
        assert!(!segs.is_empty());
        for [a, b] in &segs {
            for p in [a, b] {
                assert!(p.x > -1e-6 && p.x < 1.0 + 1e-6, "x={}", p.x);
                assert!(p.y > -1e-6 && p.y < 1.0 + 1e-6, "y={}", p.y);
            }
        }
        assert!(hatch_earth(&sq, 0.0).is_empty());
    }
}
