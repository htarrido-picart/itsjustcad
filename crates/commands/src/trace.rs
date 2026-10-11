// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Raster → vector tracing for the underlay image.
//!
//! Given decoded RGBA pixels and a target colour, this extracts the matching
//! strokes as **centerline polylines** (in pixel space, x→right, y→down). The
//! pipeline is: colour mask → Zhang–Suen thinning (1-px skeleton) → skeleton
//! walk into polylines (split at junctions/endpoints) → Douglas–Peucker
//! simplification. The exec layer maps the pixel polylines to world coordinates
//! via the underlay quad and emits them as logged `Polyline` ops.
//!
//! This is deliberately dependency-free (no OpenCV/imageproc): the algorithms
//! are small, well-understood, and keep the shared crate lean. Centerline (not
//! contour/outline) tracing is the right model for line art — a contour follower
//! would double every thin stroke into a loop hugging both of its edges.

/// A polyline in pixel space: successive `(x, y)` points, `x` to the right and
/// `y` downward (image convention). Open unless the first and last points
/// coincide.
pub type PixelPolyline = Vec<(f64, f64)>;

/// Extract centerline polylines for every stroke matching `target` (within
/// `tol`, max per-channel absolute difference) in an RGBA8 image of size
/// `w`×`h`. `min_len_px` drops specks (polylines whose point count is below it).
/// `epsilon_px` is the Douglas–Peucker tolerance (≈ how far a simplified vertex
/// may stray from the raw path, in pixels). Returns polylines in pixel space.
pub fn trace_color(
    rgba: &[u8],
    w: u32,
    h: u32,
    target: [u8; 3],
    tol: u8,
    min_len_px: usize,
    epsilon_px: f64,
) -> Vec<PixelPolyline> {
    let (w, h) = (w as usize, h as usize);
    if w == 0 || h == 0 || rgba.len() < w * h * 4 {
        return Vec::new();
    }
    let mut mask = color_mask(rgba, w, h, target, tol);
    thin_zhang_suen(&mut mask, w, h);
    let paths = walk_skeleton(&mask, w, h);
    paths
        .into_iter()
        .filter(|p| p.len() >= min_len_px.max(2))
        .map(|p| rdp(&p, epsilon_px.max(0.0)))
        .filter(|p| p.len() >= 2)
        .collect()
}

/// Build a boolean foreground mask: `true` where the pixel is within `tol` of
/// `target` on every channel (max-channel distance ≤ `tol`). Alpha is treated
/// as coverage — fully transparent pixels never match.
fn color_mask(rgba: &[u8], w: usize, h: usize, target: [u8; 3], tol: u8) -> Vec<bool> {
    let tol = tol as i32;
    let mut mask = vec![false; w * h];
    for (i, m) in mask.iter_mut().enumerate() {
        let p = i * 4;
        if rgba[p + 3] < 8 {
            continue; // transparent → background
        }
        let dr = (rgba[p] as i32 - target[0] as i32).abs();
        let dg = (rgba[p + 1] as i32 - target[1] as i32).abs();
        let db = (rgba[p + 2] as i32 - target[2] as i32).abs();
        *m = dr.max(dg).max(db) <= tol;
    }
    mask
}

#[inline]
fn at(mask: &[bool], w: usize, h: usize, x: isize, y: isize) -> bool {
    x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h && mask[y as usize * w + x as usize]
}

/// Zhang–Suen thinning: iteratively peel boundary pixels until the foreground is
/// a 1-pixel-wide skeleton. In-place on `mask`. Converges in a bounded number of
/// passes (each pass removes at least one pixel until stable).
fn thin_zhang_suen(mask: &mut [bool], w: usize, h: usize) {
    loop {
        let mut changed = false;
        for sub in 0..2 {
            let mut to_clear = Vec::new();
            for y in 0..h as isize {
                for x in 0..w as isize {
                    if !at(mask, w, h, x, y) {
                        continue;
                    }
                    // 8 neighbours clockwise from north: p2..p9.
                    let p = [
                        at(mask, w, h, x, y - 1),     // p2 N
                        at(mask, w, h, x + 1, y - 1), // p3 NE
                        at(mask, w, h, x + 1, y),     // p4 E
                        at(mask, w, h, x + 1, y + 1), // p5 SE
                        at(mask, w, h, x, y + 1),     // p6 S
                        at(mask, w, h, x - 1, y + 1), // p7 SW
                        at(mask, w, h, x - 1, y),     // p8 W
                        at(mask, w, h, x - 1, y - 1), // p9 NW
                    ];
                    let b: u32 = p.iter().map(|&v| v as u32).sum(); // neighbour count
                    if !(2..=6).contains(&b) {
                        continue;
                    }
                    // A = number of 0→1 transitions in the ordered sequence
                    // p2,p3,…,p9,p2.
                    let mut a = 0;
                    for i in 0..8 {
                        if !p[i] && p[(i + 1) % 8] {
                            a += 1;
                        }
                    }
                    if a != 1 {
                        continue;
                    }
                    // Sub-iteration-specific corner conditions.
                    let (c1, c2) = if sub == 0 {
                        (
                            !(p[0] && p[2] && p[4]), // p2*p4*p6
                            !(p[2] && p[4] && p[6]), // p4*p6*p8
                        )
                    } else {
                        (
                            !(p[0] && p[2] && p[6]), // p2*p4*p8
                            !(p[0] && p[4] && p[6]), // p2*p6*p8
                        )
                    };
                    if c1 && c2 {
                        to_clear.push(y as usize * w + x as usize);
                    }
                }
            }
            if !to_clear.is_empty() {
                changed = true;
                for i in to_clear {
                    mask[i] = false;
                }
            }
        }
        if !changed {
            break;
        }
    }
}

/// Count 8-connected skeleton neighbours of `(x,y)`.
fn degree(mask: &[bool], w: usize, h: usize, x: isize, y: isize) -> usize {
    let mut d = 0;
    for dy in -1..=1 {
        for dx in -1..=1 {
            if (dx != 0 || dy != 0) && at(mask, w, h, x + dx, y + dy) {
                d += 1;
            }
        }
    }
    d
}

/// Walk a 1-px skeleton into polylines. Paths run between endpoints (degree 1)
/// and junctions (degree ≥ 3); pure loops (all degree 2) are emitted as closed
/// polylines. Every skeleton pixel is consumed at most once per emitted segment.
fn walk_skeleton(mask: &[bool], w: usize, h: usize) -> Vec<PixelPolyline> {
    // Neighbour offsets, 8-connected. Order favours straight runs first.
    const N: [(isize, isize); 8] =
        [(1, 0), (-1, 0), (0, 1), (0, -1), (1, 1), (-1, -1), (1, -1), (-1, 1)];
    let idx = |x: isize, y: isize| y as usize * w + x as usize;
    let mut used = vec![false; w * h]; // an edge-visited marker per pixel
    let mut out = Vec::new();

    // Helper: trace from a node pixel along one unused neighbour until the next
    // node (endpoint/junction) or until the path can't continue.
    let trace_from = |sx: isize, sy: isize, used: &mut [bool]| -> Option<PixelPolyline> {
        // Pick an unused neighbour to start the edge.
        let mut next = None;
        for (dx, dy) in N {
            let (nx, ny) = (sx + dx, sy + dy);
            if at(mask, w, h, nx, ny) && !used[idx(nx, ny)] {
                next = Some((nx, ny));
                break;
            }
        }
        let (mut cx, mut cy) = next?;
        let mut path = vec![(sx as f64, sy as f64)];
        loop {
            path.push((cx as f64, cy as f64));
            used[idx(cx, cy)] = true;
            // Stop when we reach a node (not a simple degree-2 pass-through).
            if degree(mask, w, h, cx, cy) != 2 {
                break;
            }
            // Continue to the single unused neighbour (the one we didn't come
            // from is implicitly handled by the `used` marks).
            let mut moved = false;
            for (dx, dy) in N {
                let (nx, ny) = (cx + dx, cy + dy);
                if at(mask, w, h, nx, ny) && !used[idx(nx, ny)] {
                    cx = nx;
                    cy = ny;
                    moved = true;
                    break;
                }
            }
            if !moved {
                break;
            }
        }
        Some(path)
    };

    // 1) Trace every edge incident to a node (endpoint or junction).
    for y in 0..h as isize {
        for x in 0..w as isize {
            if !at(mask, w, h, x, y) {
                continue;
            }
            let d = degree(mask, w, h, x, y);
            if d == 1 || d >= 3 {
                while let Some(p) = trace_from(x, y, &mut used) {
                    out.push(p);
                }
            }
        }
    }

    // 2) Pure loops (every pixel degree 2) have no node to seed from — pick any
    //    remaining unused skeleton pixel and walk it around.
    for y in 0..h as isize {
        for x in 0..w as isize {
            if at(mask, w, h, x, y) && !used[idx(x, y)] && degree(mask, w, h, x, y) == 2 {
                used[idx(x, y)] = true;
                if let Some(mut p) = trace_from(x, y, &mut used) {
                    p.push((x as f64, y as f64)); // close the loop
                    out.push(p);
                }
            }
        }
    }

    out
}

/// Douglas–Peucker polyline simplification. Keeps endpoints; drops interior
/// points closer than `epsilon` to the chord. `epsilon <= 0` returns the input.
fn rdp(points: &[(f64, f64)], epsilon: f64) -> PixelPolyline {
    if points.len() < 3 || epsilon <= 0.0 {
        return points.to_vec();
    }
    let (a, b) = (points[0], points[points.len() - 1]);
    let (mut idx, mut max) = (0usize, 0.0f64);
    for (i, &p) in points.iter().enumerate().take(points.len() - 1).skip(1) {
        let d = perp_dist(p, a, b);
        if d > max {
            idx = i;
            max = d;
        }
    }
    if max > epsilon {
        let mut left = rdp(&points[..=idx], epsilon);
        let right = rdp(&points[idx..], epsilon);
        left.pop(); // shared midpoint
        left.extend(right);
        left
    } else {
        vec![a, b]
    }
}

/// Perpendicular distance from `p` to the segment `a`–`b` (treated as a line;
/// degenerate `a==b` falls back to point distance).
fn perp_dist(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    if len2 == 0.0 {
        return ((p.0 - a.0).powi(2) + (p.1 - a.1).powi(2)).sqrt();
    }
    let cross = (dx * (a.1 - p.1) - dy * (a.0 - p.0)).abs();
    cross / len2.sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A solid RGBA image where a predicate picks the foreground colour.
    fn img(w: usize, h: usize, f: impl Fn(usize, usize) -> bool) -> Vec<u8> {
        let mut v = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let p = (y * w + x) * 4;
                let c = if f(x, y) { [255, 0, 0] } else { [255, 255, 255] };
                v[p] = c[0];
                v[p + 1] = c[1];
                v[p + 2] = c[2];
                v[p + 3] = 255;
            }
        }
        v
    }

    #[test]
    fn traces_a_horizontal_line_into_one_polyline() {
        // A 3-px-thick red line across the middle → one centerline polyline
        // spanning (roughly) the full width.
        let (w, h) = (40usize, 11usize);
        let rgba = img(w, h, |_, y| (4..=6).contains(&y));
        let polys = trace_color(&rgba, w as u32, h as u32, [255, 0, 0], 60, 5, 1.0);
        assert_eq!(polys.len(), 1, "expected a single stroke, got {}", polys.len());
        let p = &polys[0];
        let xs: Vec<f64> = p.iter().map(|q| q.0).collect();
        let (min_x, max_x) = (xs.iter().cloned().fold(f64::MAX, f64::min), xs.iter().cloned().fold(0.0, f64::max));
        assert!(max_x - min_x > (w as f64) * 0.7, "span too short: {min_x}..{max_x}");
        // All points sit on the center row (y ≈ 5) after thinning.
        for q in p {
            assert!((q.1 - 5.0).abs() <= 1.5, "point off centerline: {q:?}");
        }
    }

    #[test]
    fn ignores_a_different_color() {
        let (w, h) = (20usize, 20usize);
        let rgba = img(w, h, |_, y| (9..=11).contains(&y)); // red line
        // Ask for blue → nothing.
        let polys = trace_color(&rgba, w as u32, h as u32, [0, 0, 255], 60, 5, 1.0);
        assert!(polys.is_empty());
    }

    #[test]
    fn simplifies_a_straight_run_to_two_points() {
        let pts: Vec<(f64, f64)> = (0..50).map(|i| (i as f64, 0.0)).collect();
        assert_eq!(rdp(&pts, 1.0), vec![(0.0, 0.0), (49.0, 0.0)]);
    }
}
