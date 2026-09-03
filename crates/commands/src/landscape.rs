// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Landscape-design math (M-landscape): pure, deterministic functions behind
//! the `contours`, `pad`, `cutfill`, `plant*`, `flowarrows`, `ponding` and
//! `sitepath` verbs. No document access here — exec.rs owns the scene; this
//! module owns the geometry so it can be tested against analytic terrains.

use glam::DVec3;

// ─── contours ────────────────────────────────────────────────────────────────

/// One extracted contour polyline at `level` (contour index `index` =
/// `level / interval`, used for the minor/major split).
#[derive(Debug, Clone)]
pub struct ContourLine {
    pub points: Vec<DVec3>,
    pub closed: bool,
    pub level: f64,
    pub index: i64,
}

/// Contour levels strictly inside `(zmin, zmax)`: every multiple of
/// `interval`. Levels that would land exactly on a vertex elevation are
/// handled per-triangle by the crossing test (strict sign change), so a level
/// equal to `zmin`/`zmax` simply yields nothing.
pub fn contour_levels(zmin: f64, zmax: f64, interval: f64) -> Vec<(i64, f64)> {
    if interval <= 0.0 || !interval.is_finite() || !zmin.is_finite() || !zmax.is_finite() || zmax <= zmin
    {
        return Vec::new();
    }
    let first = (zmin / interval).floor() as i64;
    let last = (zmax / interval).ceil() as i64;
    (first..=last)
        .map(|k| (k, k as f64 * interval))
        .filter(|(_, z)| *z > zmin && *z < zmax)
        .collect()
}

/// Marching triangles: intersection segments of the mesh with the horizontal
/// plane `z = level`. Vertices sitting exactly on the level are nudged down by
/// an infinitesimal (treated as below), which keeps the crossing test a strict
/// sign change and the chained loops watertight on gridded synthetic terrain.
fn contour_segments(
    positions: &[DVec3],
    faces: &[[u32; 3]],
    level: f64,
) -> Vec<(DVec3, DVec3)> {
    // "Above" is strict; a vertex exactly at the level counts as below.
    let above = |p: DVec3| p.z > level;
    let mut segs = Vec::new();
    for f in faces {
        let p = [
            positions[f[0] as usize],
            positions[f[1] as usize],
            positions[f[2] as usize],
        ];
        let mut hits: Vec<DVec3> = Vec::with_capacity(2);
        for e in 0..3 {
            let (a, b) = (p[e], p[(e + 1) % 3]);
            if above(a) != above(b) {
                // Guaranteed a.z != b.z when the side test differs, except the
                // degenerate a.z == b.z == level case which `above` maps to
                // (false, false) — never reaches here.
                let t = (level - a.z) / (b.z - a.z);
                hits.push(a + (b - a) * t);
            }
        }
        if hits.len() == 2 && hits[0].distance_squared(hits[1]) > 1e-18 {
            segs.push((hits[0], hits[1]));
        }
    }
    segs
}

/// Quantized XY key for endpoint matching while chaining (1 µm grid — far
/// below any drafting tolerance, far above f64 noise).
fn qkey(p: DVec3) -> (i64, i64) {
    ((p.x * 1e6).round() as i64, (p.y * 1e6).round() as i64)
}

/// Chain unordered segments into polylines. Deterministic: seeds walk in
/// segment order, adjacency uses ordered maps. Returns `(points, closed)`.
pub fn chain_segments(segs: &[(DVec3, DVec3)]) -> Vec<(Vec<DVec3>, bool)> {
    use std::collections::BTreeMap;
    // endpoint key → list of (segment index, which end is at this key)
    let mut at: BTreeMap<(i64, i64), Vec<(usize, bool)>> = BTreeMap::new();
    for (i, (a, b)) in segs.iter().enumerate() {
        at.entry(qkey(*a)).or_default().push((i, false));
        at.entry(qkey(*b)).or_default().push((i, true));
    }
    let mut used = vec![false; segs.len()];
    let mut out = Vec::new();
    for seed in 0..segs.len() {
        if used[seed] {
            continue;
        }
        used[seed] = true;
        let mut pts = vec![segs[seed].0, segs[seed].1];
        // Extend forward from the tail, then backward from the head.
        for forward in [true, false] {
            loop {
                let end = if forward { *pts.last().unwrap() } else { pts[0] };
                let Some(cands) = at.get(&qkey(end)) else { break };
                let next = cands.iter().find(|(i, _)| !used[*i]).copied();
                let Some((i, end_is_b)) = next else { break };
                used[i] = true;
                // The far endpoint of the found segment.
                let far = if end_is_b { segs[i].0 } else { segs[i].1 };
                if forward {
                    pts.push(far);
                } else {
                    pts.insert(0, far);
                }
            }
        }
        let closed = pts.len() > 2 && qkey(pts[0]) == qkey(*pts.last().unwrap());
        if closed {
            pts.pop(); // drop the duplicate closing point; `closed` flag carries it
        }
        out.push((pts, closed));
    }
    out
}

/// All contour polylines of a terrain mesh at multiples of `interval`.
pub fn contours(positions: &[DVec3], faces: &[[u32; 3]], interval: f64) -> Vec<ContourLine> {
    let (zmin, zmax) = positions.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| {
        (lo.min(p.z), hi.max(p.z))
    });
    let mut out = Vec::new();
    for (index, level) in contour_levels(zmin, zmax, interval) {
        let segs = contour_segments(positions, faces, level);
        for (points, closed) in chain_segments(&segs) {
            if points.len() >= 2 {
                out.push(ContourLine { points, closed, level, index });
            }
        }
    }
    out
}

// ─── grading & earthwork ─────────────────────────────────────────────────────

/// Grade a flat rectangular building pad into terrain vertex heights, with
/// side slopes to daylight. The pad is axis-aligned, centered at `(cx, cy)`,
/// `w`×`d`, at elevation `elev`. `slope_h` is the side-slope ratio expressed
/// as horizontal-run per unit rise (e.g. 2.0 = a 2:1 slope). Outside the pad
/// the graded surface may rise (cut) or fall (fill) by `dist / slope_h`
/// relative to `elev`; wherever the existing grade already sits inside that
/// envelope it is left untouched — that is the daylight line.
///
/// Returns `(pad_vertices, slope_vertices)` — how many vertices were set to
/// the pad elevation and how many were pulled onto a side slope.
pub fn grade_pad(
    positions: &mut [DVec3],
    cx: f64,
    cy: f64,
    w: f64,
    d: f64,
    elev: f64,
    slope_h: f64,
) -> (usize, usize) {
    let (mut on_pad, mut on_slope) = (0usize, 0usize);
    for p in positions.iter_mut() {
        let dx = ((p.x - cx).abs() - w / 2.0).max(0.0);
        let dy = ((p.y - cy).abs() - d / 2.0).max(0.0);
        let dist = dx.hypot(dy);
        if dist == 0.0 {
            if (p.z - elev).abs() > 1e-12 {
                on_pad += 1;
            }
            p.z = elev;
        } else {
            // Envelope the graded surface may occupy at this distance.
            let rise = dist / slope_h;
            let clamped = p.z.clamp(elev - rise, elev + rise);
            if (clamped - p.z).abs() > 1e-12 {
                p.z = clamped;
                on_slope += 1;
            }
        }
    }
    (on_pad, on_slope)
}

/// Cut and fill volumes (m³) between the current terrain and its pre-grading
/// vertex heights, over the same triangulation (grading only moves z).
///
/// Per triangle the signed prism volume is `area_xy · mean(Δz)` — exact for a
/// linearly interpolated surface when Δz does not change sign inside the
/// triangle; mixed-sign triangles are attributed by the sign of their mean
/// (a TIN prism estimate, second-order small on a reasonably dense mesh).
/// Returns `(cut, fill)`, both ≥ 0.
pub fn cut_fill(
    positions: &[DVec3],
    faces: &[[u32; 3]],
    original_z: &[f64],
) -> (f64, f64) {
    let (mut cut, mut fill) = (0.0f64, 0.0f64);
    for f in faces {
        let [a, b, c] = [f[0] as usize, f[1] as usize, f[2] as usize];
        let (pa, pb, pc) = (positions[a], positions[b], positions[c]);
        let area_xy = 0.5
            * ((pb.x - pa.x) * (pc.y - pa.y) - (pc.x - pa.x) * (pb.y - pa.y)).abs();
        let dz = (pa.z - original_z[a]) + (pb.z - original_z[b]) + (pc.z - original_z[c]);
        let vol = area_xy * dz / 3.0;
        if vol > 0.0 {
            fill += vol;
        } else {
            cut -= vol;
        }
    }
    (cut, fill)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regular grid over [0,size]² with `n`×`n` cells, z = f(x,y).
    fn grid_terrain(
        n: usize,
        size: f64,
        f: impl Fn(f64, f64) -> f64,
    ) -> (Vec<DVec3>, Vec<[u32; 3]>) {
        let mut pos = Vec::new();
        for j in 0..=n {
            for i in 0..=n {
                let x = size * i as f64 / n as f64;
                let y = size * j as f64 / n as f64;
                pos.push(DVec3::new(x, y, f(x, y)));
            }
        }
        let mut faces = Vec::new();
        let idx = |i: usize, j: usize| (j * (n + 1) + i) as u32;
        for j in 0..n {
            for i in 0..n {
                faces.push([idx(i, j), idx(i + 1, j), idx(i + 1, j + 1)]);
                faces.push([idx(i, j), idx(i + 1, j + 1), idx(i, j + 1)]);
            }
        }
        (pos, faces)
    }

    #[test]
    fn levels_inside_range_only() {
        let lv = contour_levels(0.3, 3.2, 1.0);
        assert_eq!(lv, vec![(1, 1.0), (2, 2.0), (3, 3.0)]);
        assert!(contour_levels(0.0, 1.0, 0.0).is_empty());
        assert!(contour_levels(2.0, 1.0, 0.5).is_empty());
    }

    #[test]
    fn plane_contours_are_straight_open_lines() {
        // z = x on [0,10]²: contours are vertical lines x = level.
        let (pos, faces) = grid_terrain(10, 10.0, |x, _| x);
        let lines = contours(&pos, &faces, 2.5);
        // Levels 2.5, 5.0, 7.5 (0 and 10 are the exact min/max → excluded).
        let mut levels: Vec<f64> = lines.iter().map(|c| c.level).collect();
        levels.sort_by(f64::total_cmp);
        levels.dedup();
        assert_eq!(levels, vec![2.5, 5.0, 7.5]);
        for c in &lines {
            assert!(!c.closed, "plane contours must be open");
            // Every point sits on x = level and z = level.
            for p in &c.points {
                assert!((p.x - c.level).abs() < 1e-9, "x={} level={}", p.x, c.level);
                assert!((p.z - c.level).abs() < 1e-9);
            }
            // Chained into ONE polyline spanning the full y range.
            let (ymin, ymax) = c
                .points
                .iter()
                .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| {
                    (lo.min(p.y), hi.max(p.y))
                });
            assert!((ymin - 0.0).abs() < 1e-9 && (ymax - 10.0).abs() < 1e-9);
        }
        // One polyline per level — chaining must not fragment a straight cut.
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn cone_contours_are_closed_rings_at_predicted_radii() {
        // Cone peak z=5 at center (5,5), slope 1 → contour at level z is a
        // circle of radius 5−z. Grid is fine enough that rings stay closed.
        let (pos, faces) = grid_terrain(40, 10.0, |x, y| {
            (5.0 - ((x - 5.0).powi(2) + (y - 5.0).powi(2)).sqrt()).max(0.0)
        });
        let lines = contours(&pos, &faces, 1.0);
        for want in [1.0, 2.0, 3.0, 4.0] {
            let rings: Vec<_> = lines.iter().filter(|c| c.level == want).collect();
            assert_eq!(rings.len(), 1, "level {want} must chain to one ring");
            let ring = rings[0];
            assert!(ring.closed, "cone contour at {want} must be a closed loop");
            let r_want = 5.0 - want;
            for p in &ring.points {
                let r = ((p.x - 5.0).powi(2) + (p.y - 5.0).powi(2)).sqrt();
                // Linear interpolation on a curved surface: tolerance scales
                // with cell size (0.25 m cells).
                assert!(
                    (r - r_want).abs() < 0.05,
                    "level {want}: radius {r} vs {r_want}"
                );
            }
        }
    }

    #[test]
    fn vertex_exactly_on_level_does_not_break_chaining() {
        // z = x on integer grid: level 2.0 passes exactly through a whole
        // column of vertices. Strict `above` still yields one clean open line.
        let (pos, faces) = grid_terrain(4, 4.0, |x, _| x);
        let lines = contours(&pos, &faces, 2.0);
        assert_eq!(lines.len(), 1);
        assert!((lines[0].level - 2.0).abs() < 1e-12);
        assert!(!lines[0].closed);
        for p in &lines[0].points {
            assert!((p.x - 2.0).abs() < 1e-9);
        }
    }

    #[test]
    fn pad_at_grade_on_flat_plane_changes_nothing() {
        // Flat plane at z=2, pad at elev 2 → no vertex moves, zero cut/fill.
        let (mut pos, faces) = grid_terrain(20, 20.0, |_, _| 2.0);
        let orig: Vec<f64> = pos.iter().map(|p| p.z).collect();
        let (on_pad, on_slope) = grade_pad(&mut pos, 10.0, 10.0, 4.0, 4.0, 2.0, 2.0);
        assert_eq!((on_pad, on_slope), (0, 0));
        let (cut, fill) = cut_fill(&pos, &faces, &orig);
        assert_eq!((cut, fill), (0.0, 0.0));
    }

    #[test]
    fn sunk_pad_in_flat_plane_matches_analytic_cut() {
        // Flat plane z=0, 4×4 pad sunk to −1 with 2:1 slopes. Analytic cut:
        //   pad:      4·4·1                       = 16
        //   edges:    perimeter 16 · ∫₀²(1−t/2)dt = 16·1 = 16
        //   corners:  4·¼-annulus = 2π·∫₀²(r−r²/2)dr = 2π·(2−4/3) = 4π/3
        // total ≈ 36.19 m³, fill = 0. 0.25 m grid cells → small TIN error.
        let (mut pos, faces) = grid_terrain(80, 20.0, |_, _| 0.0);
        let orig: Vec<f64> = pos.iter().map(|p| p.z).collect();
        let (on_pad, on_slope) = grade_pad(&mut pos, 10.0, 10.0, 4.0, 4.0, -1.0, 2.0);
        assert!(on_pad > 0 && on_slope > 0);
        let (cut, fill) = cut_fill(&pos, &faces, &orig);
        let want = 16.0 + 16.0 + 4.0 * std::f64::consts::PI / 3.0;
        assert!((cut - want).abs() < 0.5, "cut {cut} vs analytic {want}");
        assert_eq!(fill, 0.0);
    }

    #[test]
    fn raised_pad_is_pure_fill_and_symmetric() {
        // Same pad raised +1 on the same flat plane → the mirrored volume as
        // fill, zero cut.
        let (mut pos, faces) = grid_terrain(80, 20.0, |_, _| 0.0);
        let orig: Vec<f64> = pos.iter().map(|p| p.z).collect();
        grade_pad(&mut pos, 10.0, 10.0, 4.0, 4.0, 1.0, 2.0);
        let (cut, fill) = cut_fill(&pos, &faces, &orig);
        let want = 32.0 + 4.0 * std::f64::consts::PI / 3.0;
        assert!((fill - want).abs() < 0.5, "fill {fill} vs analytic {want}");
        assert_eq!(cut, 0.0);
    }

    #[test]
    fn pad_in_sloped_plane_daylights_and_balances() {
        // Plane z = x/2 on [0,20]², pad 4×4 at (10,10) elev 5 — exactly the
        // existing grade at pad center x=10. Pad cut (x>10 side) mirrors pad
        // fill (x<10 side): ∫ over 4×4 of (x/2−5) splits into ±2·... cut in
        // pad = fill in pad = 4·∫₁₀¹²(x/2−5)dx = 4·1 = 4 m³ each, plus equal
        // slope-band volumes by symmetry → cut ≈ fill overall.
        let (mut pos, faces) = grid_terrain(80, 20.0, |x, _| x / 2.0);
        let orig: Vec<f64> = pos.iter().map(|p| p.z).collect();
        grade_pad(&mut pos, 10.0, 10.0, 4.0, 4.0, 5.0, 2.0);
        // Far corner untouched: envelope daylights before reaching it.
        assert!((pos[0].z - 0.0).abs() < 1e-12, "corner (0,0) must be untouched");
        let (cut, fill) = cut_fill(&pos, &faces, &orig);
        assert!(cut > 4.0 && fill > 4.0, "pad body alone is 4 m³ each way");
        assert!(
            (cut - fill).abs() < 0.2,
            "symmetric grading must balance: cut {cut} vs fill {fill}"
        );
    }

    #[test]
    fn chaining_is_deterministic() {
        let (pos, faces) = grid_terrain(20, 10.0, |x, y| {
            (5.0 - ((x - 5.0).powi(2) + (y - 5.0).powi(2)).sqrt()).max(0.0)
        });
        let a = contours(&pos, &faces, 1.0);
        let b = contours(&pos, &faces, 1.0);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(&b) {
            assert_eq!(x.points, y.points);
            assert_eq!(x.closed, y.closed);
        }
    }
}
