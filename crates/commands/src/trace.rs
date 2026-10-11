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
    // Bridge pixel-level gaps (anti-aliasing / JPEG noise leaves thin strokes
    // dotted) by one dilation before thinning, so a stroke solidifies into a
    // connected region and thins to a continuous centerline. Radius 1 keeps
    // adjacent parallel contours from merging.
    dilate(&mut mask, w, h, 1);
    thin_zhang_suen(&mut mask, w, h);
    let paths = walk_skeleton(&mask, w, h);
    // Stitch fragments whose endpoints are close AND directionally continuous —
    // reconnects a contour broken by spurious skeleton junctions without joining
    // a different parallel contour running alongside it.
    let max_gap = (w.max(h) as f64 / 180.0).clamp(4.0, 12.0);
    let bridged = bridge_endpoints(paths, max_gap, 35.0_f64.to_radians());
    // Filter by ARC LENGTH, not point count: after simplification a long contour
    // may be only a handful of points, while noise specks are short regardless.
    // `min_len_px` is a minimum stroke length in pixels.
    let min_len = min_len_px as f64;
    bridged
        .into_iter()
        .filter(|p| polyline_len(p) >= min_len)
        .map(|p| rdp(&p, epsilon_px.max(0.0)))
        .filter(|p| p.len() >= 2)
        .collect()
}

/// Total arc length of a polyline in pixels.
fn polyline_len(p: &[(f64, f64)]) -> f64 {
    p.windows(2)
        .map(|s| ((s[1].0 - s[0].0).powi(2) + (s[1].1 - s[0].1).powi(2)).sqrt())
        .sum()
}

/// Grow the foreground by `radius` (Chebyshev) — a morphological dilation. Used
/// to bridge small gaps in dotted/anti-aliased strokes before thinning.
fn dilate(mask: &mut [bool], w: usize, h: usize, radius: isize) {
    let src = mask.to_vec();
    for y in 0..h as isize {
        for x in 0..w as isize {
            if at(&src, w, h, x, y) {
                continue; // already set
            }
            let mut hit = false;
            'r: for dy in -radius..=radius {
                for dx in -radius..=radius {
                    if at(&src, w, h, x + dx, y + dy) {
                        hit = true;
                        break 'r;
                    }
                }
            }
            if hit {
                mask[y as usize * w + x as usize] = true;
            }
        }
    }
}

/// Tangent direction at the end of `p` (unit vector pointing OUT of the
/// endpoint), estimated from the last/first `k` points. `at_start` picks which
/// end. Returns `(0,0)` for a degenerate path.
fn end_dir(p: &[(f64, f64)], at_start: bool, k: usize) -> (f64, f64) {
    let n = p.len();
    if n < 2 {
        return (0.0, 0.0);
    }
    let (tip, base) = if at_start {
        (p[0], p[k.min(n - 1)])
    } else {
        (p[n - 1], p[n - 1 - k.min(n - 1)])
    };
    let (dx, dy) = (tip.0 - base.0, tip.1 - base.1);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1e-9 { (0.0, 0.0) } else { (dx / len, dy / len) }
}

/// Join open polylines whose endpoints are close AND directionally continuous,
/// reconnecting a contour broken into fragments without merging a different
/// parallel contour (a hop to a neighbour fails the tangent test — it runs
/// roughly perpendicular to the strokes). Single pass: score every candidate
/// join once, apply greedily (closest + straightest first) with each endpoint
/// used at most once and union-find preventing a polyline joining itself, then
/// reconstruct the merged chains in one walk. O(E²) to score, O(E) to rebuild —
/// no per-join rescans or clones.
fn bridge_endpoints(polys: Vec<PixelPolyline>, max_gap: f64, max_turn: f64) -> Vec<PixelPolyline> {
    let min_cos = max_turn.cos();
    let k = 4; // points back used to estimate the endpoint tangent

    // Closed loops and degenerate paths pass through untouched; the rest get two
    // endpoints each (indices 2m, 2m+1 are siblings — same polyline, opposite
    // ends), so `sibling = e ^ 1`.
    struct Ep {
        pt: (f64, f64),
        dir: (f64, f64),
        poly: usize,
        start: bool,
    }
    let mut out: Vec<PixelPolyline> = Vec::new();
    let mut open: Vec<&PixelPolyline> = Vec::new();
    let mut eps: Vec<Ep> = Vec::new();
    for p in &polys {
        if p.len() < 2 || is_closed(p) {
            out.push(p.clone());
            continue;
        }
        let poly = open.len();
        open.push(p);
        eps.push(Ep { pt: p[0], dir: end_dir(p, true, k), poly, start: true });
        eps.push(Ep { pt: *p.last().unwrap(), dir: end_dir(p, false, k), poly, start: false });
    }

    // Score candidate joins once.
    let mut cands: Vec<(f64, usize, usize)> = Vec::new();
    for a in 0..eps.len() {
        for b in (a + 1)..eps.len() {
            if eps[a].poly == eps[b].poly {
                continue;
            }
            let (pa, pb) = (eps[a].pt, eps[b].pt);
            let gap = ((pa.0 - pb.0).powi(2) + (pa.1 - pb.1).powi(2)).sqrt();
            if gap > max_gap || gap < 1e-6 {
                continue;
            }
            let cl = gap;
            let (cx, cy) = ((pb.0 - pa.0) / cl, (pb.1 - pa.1) / cl);
            let cont_a = eps[a].dir.0 * cx + eps[a].dir.1 * cy;
            let cont_b = eps[b].dir.0 * (-cx) + eps[b].dir.1 * (-cy);
            if cont_a >= min_cos && cont_b >= min_cos {
                cands.push((gap - (cont_a + cont_b), a, b)); // prefer close + straight
            }
        }
    }
    cands.sort_by(|x, y| x.0.partial_cmp(&y.0).unwrap_or(std::cmp::Ordering::Equal));

    // Greedy apply: each endpoint used once; union-find stops a poly joining
    // itself (which would otherwise orphan a loop).
    let mut parent: Vec<usize> = (0..open.len()).collect();
    fn find(parent: &mut [usize], x: usize) -> usize {
        let mut r = x;
        while parent[r] != r {
            r = parent[r];
        }
        let mut c = x;
        while parent[c] != r {
            let n = parent[c];
            parent[c] = r;
            c = n;
        }
        r
    }
    let mut partner: Vec<Option<usize>> = vec![None; eps.len()];
    for (_, a, b) in cands {
        if partner[a].is_some() || partner[b].is_some() {
            continue;
        }
        let (ra, rb) = (find(&mut parent, eps[a].poly), find(&mut parent, eps[b].poly));
        if ra == rb {
            continue;
        }
        partner[a] = Some(b);
        partner[b] = Some(a);
        parent[ra] = rb;
    }

    // Reconstruct. Each endpoint has one internal edge (to its sibling, via the
    // polyline body) and ≤1 join edge → everything is a path or a cycle.
    let mut visited = vec![false; eps.len()];
    let emit = |e_start: usize, visited: &mut [bool], out: &mut Vec<PixelPolyline>| {
        let mut path: PixelPolyline = Vec::new();
        let mut e = e_start;
        loop {
            if visited[e] {
                break;
            }
            visited[e] = true;
            let sib = e ^ 1;
            visited[sib] = true;
            let poly = open[eps[e].poly];
            if eps[e].start {
                path.extend_from_slice(poly); // entered at start → forward
            } else {
                path.extend(poly.iter().rev().copied()); // entered at end → reversed
            }
            match partner[sib] {
                Some(next) => e = next, // hop across the gap to the next fragment
                None => break,
            }
        }
        if path.len() >= 2 {
            out.push(path);
        }
    };
    // Open chains first (start only at a true free end: no join there).
    for s in 0..eps.len() {
        if !visited[s] && partner[s].is_none() {
            emit(s, &mut visited, &mut out);
        }
    }
    // Any remainder is a pure cycle of fragments — emit and close it.
    for s in 0..eps.len() {
        if !visited[s] {
            let before = out.len();
            emit(s, &mut visited, &mut out);
            if let Some(last) = out.get_mut(before)
                && last.len() >= 4
                && last[0] != last[last.len() - 1]
            {
                last.push(last[0]);
            }
        }
    }
    out
}

fn is_closed(p: &[(f64, f64)]) -> bool {
    p.len() >= 4 && {
        let (a, b) = (p[0], p[p.len() - 1]);
        (a.0 - b.0).abs() < 1.5 && (a.1 - b.1).abs() < 1.5
    }
}

/// HSV-ish breakdown: hue in degrees (0..360, −1 if achromatic), chroma 0..1,
/// value 0..1. Line-art inks are better separated by hue than by RGB distance —
/// a faint pink and a saturated red share a hue but are far apart in RGB.
fn hcv(r: u8, g: u8, b: u8) -> (f64, f64, f64) {
    let (r, g, b) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let c = max - min;
    let h = if c < 1e-6 {
        -1.0
    } else if max == r {
        60.0 * (((g - b) / c).rem_euclid(6.0))
    } else if max == g {
        60.0 * ((b - r) / c + 2.0)
    } else {
        60.0 * ((r - g) / c + 4.0)
    };
    (h, c, max)
}

/// Smallest angular distance between two hues, in degrees (0..180).
fn hue_dist(a: f64, b: f64) -> f64 {
    let d = (a - b).abs().rem_euclid(360.0);
    d.min(360.0 - d)
}

/// Build a boolean foreground mask for the pixels matching `target`.
///
/// For a CHROMATIC target (coloured ink like the red/blue contours) a pixel
/// matches when it carries enough chroma to be ink (not white/near-grey) and its
/// HUE is within tolerance of the target's — so a faint, JPEG-muddied pink still
/// registers as "red" while the blue boundary and black linework do not. For an
/// ACHROMATIC target (black/white) it matches low-chroma pixels within a value
/// band. `tol` widens both the hue window and the value band.
fn color_mask(rgba: &[u8], w: usize, h: usize, target: [u8; 3], tol: u8) -> Vec<bool> {
    let (th, tc, tv) = hcv(target[0], target[1], target[2]);
    let chromatic = tc >= 0.10;
    let min_chroma = 0.09; // below this a pixel is treated as white/grey/black
    let hue_tol = 20.0 + (tol as f64 / 255.0) * 70.0; // tol 72→40°, 110→50°
    let val_tol = 0.12 + (tol as f64 / 255.0) * 0.6; // achromatic value band

    let mut mask = vec![false; w * h];
    for (i, m) in mask.iter_mut().enumerate() {
        let p = i * 4;
        if rgba[p + 3] < 8 {
            continue; // transparent → background
        }
        let (h, c, v) = hcv(rgba[p], rgba[p + 1], rgba[p + 2]);
        *m = if chromatic {
            c >= min_chroma && h >= 0.0 && hue_dist(h, th) <= hue_tol
        } else {
            // Black/white ink: low chroma, value near the target's.
            c < min_chroma && (v - tv).abs() <= val_tol
        };
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
