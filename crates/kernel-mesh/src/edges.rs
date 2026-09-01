// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Feature-edge extraction, shared by the DXF/PDF exporters and the viewport
//! wireframe/x-ray display modes.

use std::collections::BTreeMap;

use glam::DVec3;

use crate::Mesh;

/// Quantize a position to a 1 µm grid so duplicated/unwelded vertices at the same
/// location share an edge key. Boolean output isn't guaranteed index-welded, and
/// keying edges by raw vertex INDEX left every duplicated edge unpaired — counted
/// as a boundary and drawn (the CSG "line artifacts"). Keying by welded POSITION
/// pairs them.
fn qkey(p: DVec3) -> (i64, i64, i64) {
    const Q: f64 = 1e6; // 1 µm
    (
        (p.x * Q).round() as i64,
        (p.y * Q).round() as i64,
        (p.z * Q).round() as i64,
    )
}

/// Feature edges of a mesh via **planar-region boundary extraction**.
///
/// Rather than judging each edge by its two adjacent face normals (a per-edge
/// heuristic that leaves seam/t-junction artifacts on re-triangulated CSG
/// output), we group triangles into coplanar regions and take each region's
/// boundary: edges used exactly ONCE within a plane group are where that flat
/// region meets another plane (a real crease) or a true open boundary; edges
/// used TWICE are interior to the flat face and dropped. A crease shared by two
/// planes is contributed once by each and deduped. Finally, collinear boundary
/// segments that share an endpoint are merged to a fixed point, which collapses
/// t-junctions (a long span + its half-edges) into one clean edge.
///
/// A box yields its 12 outline edges (not 18); a box-with-a-hole or an
/// annulus/"donut" difference yields a clean outline, not seam lines.
/// Degenerate/sliver triangles (near-zero area → unreliable normal) are excluded
/// from grouping so they cannot inject spurious boundary edges.
pub fn feature_edges(mesh: &Mesh) -> Vec<(DVec3, DVec3)> {
    type Key = (i64, i64, i64);
    // A plane identity: quantized unit normal + quantized signed offset. Both a
    // normal and its negation name the same plane, so we canonicalize the normal
    // sign (and flip d with it) to fold front/back-facing coplanar triangles into
    // one group.
    type PlaneKey = (i64, i64, i64, i64);
    // Per-plane edge tally: welded edge key -> (representative endpoints, use count).
    type EdgeTally = BTreeMap<(Key, Key), ([DVec3; 2], u32)>;

    let pos = mesh.positions();

    // Group triangle edges by plane. For each plane, tally welded-position edge
    // keys: value counts how many triangles in the group use that edge, and we
    // keep representative endpoints.
    let mut planes: BTreeMap<PlaneKey, EdgeTally> = BTreeMap::new();

    for face in mesh.faces() {
        let [a, b, c] = face.map(|i| pos[i as usize]);
        // Skip degenerate/sliver triangles: their normal is unreliable and would
        // seed a spurious plane + boundary edges.
        let cross = (b - a).cross(c - a);
        let area2 = cross.length();
        if area2 < AREA_EPS {
            continue;
        }
        let n = cross / area2;
        let pk = plane_key(n, a);

        let group = planes.entry(pk).or_default();
        for (p, q) in [(a, b), (b, c), (c, a)] {
            let (kp, kq) = (qkey(p), qkey(q));
            let (key, ends) = if kp <= kq { ((kp, kq), [p, q]) } else { ((kq, kp), [q, p]) };
            let e = group.entry(key).or_insert((ends, 0));
            e.1 += 1;
        }
    }

    // Boundary edges: within each plane group, edges used exactly once. Dedupe
    // across groups by welded key (a shared crease is contributed by both planes).
    let mut boundary: BTreeMap<(Key, Key), [DVec3; 2]> = BTreeMap::new();
    for group in planes.values() {
        for (&key, &(ends, count)) in group {
            if count == 1 {
                boundary.entry(key).or_insert(ends);
            }
        }
    }

    let segments: Vec<[DVec3; 2]> = boundary.into_values().collect();
    let merged = merge_collinear(segments);
    merged.into_iter().map(|[a, b]| (a, b)).collect()
}

/// Minimum length of a triangle's edge-cross-product (i.e. 2× area) for it to be
/// treated as non-degenerate. Below this the normal direction is numerically
/// unreliable, so the triangle is a sliver and is skipped.
const AREA_EPS: f64 = 1e-9;

/// Quantized plane identity for grouping coplanar triangles. The unit normal is
/// canonicalized to a hemisphere (first non-near-zero component made positive) so
/// that a triangle and its flip land on the same plane; the signed offset
/// `d = n·p0` is flipped along with the normal. Normal is quantized to ~1e-4 and
/// `d` to ~1e-5, matching the near-coplanar tolerance the old code used.
fn plane_key(n: DVec3, p0: DVec3) -> (i64, i64, i64, i64) {
    const QN: f64 = 1e4; // normal precision (~1e-4)
    const QD: f64 = 1e5; // offset precision (~1e-5)
    let mut nn = n;
    let mut d = n.dot(p0);
    // Canonicalize sign: make the first significant component positive.
    let sign = if nn.x.abs() > 1e-6 {
        nn.x.signum()
    } else if nn.y.abs() > 1e-6 {
        nn.y.signum()
    } else {
        nn.z.signum()
    };
    if sign < 0.0 {
        nn = -nn;
        d = -d;
    }
    (
        (nn.x * QN).round() as i64,
        (nn.y * QN).round() as i64,
        (nn.z * QN).round() as i64,
        (d * QD).round() as i64,
    )
}

/// Repeatedly fuse two segments that share an endpoint AND lie (near-)collinear
/// into one longer segment, dropping the shared mid vertex, until no more merges
/// apply. This collapses a t-junction (a long span plus the half-edges a
/// neighbour split it into) into a single clean edge. A triangle's three edges
/// point in distinct directions, so none are collinear and it is left intact.
///
/// Two criteria decide "collinear enough", and EITHER passing fuses the pair:
///
///  1. Angle test — `|û × v̂| ≤ SIN_TOL` (~0.06°). Cheap and scale-free; catches
///     the classic t-junction where the two halves are exactly parallel.
///  2. Perpendicular-distance (Douglas–Peucker) test — the shared mid vertex sits
///     within ε of the line through the two OUTER endpoints. A boolean re-tessellates
///     the flat annular caps of a difference and, because the outer and inner rings
///     don't align vertex-for-vertex, splits what should be one straight rim chord
///     into a fan of short chords that only *slightly* bend at each shared vertex.
///     Those bends are too gentle for the angle test yet still trace a nearly
///     straight boundary, so the point-to-line test folds them back into one edge.
///
/// ε rationale (why RELATIVE to span, and why 3 %) — we must erase the boolean's
/// cap jaggies while NEVER clipping a corner of an intentionally coarse polygon.
/// Making ε a fraction of the merged span (`REL_EPS × |p−r|`) is what keeps this
/// scale-invariant: the same criterion holds for a 1 mm part and a 100 m building,
/// and it survives a uniform re-scale of the whole model (see the 1000× box-with-
/// hole test). A tiny absolute floor (`ABS_EPS`, one weld cell) covers spans so
/// short the relative term underflows.
///
/// The 3 % figure was calibrated empirically against the very shapes we must NOT
/// damage — clean n-gon prisms whose rim corners are the genuine article:
///   REL_EPS   12-gon  24-gon  36-gon  48-gon   donut(24)
///     3e-2      36✓     72✓    108✓    144✓       176
///     4e-2      36✓     72✓    108✓     98✗       165
/// A regular n-gon vertex sits ≈ tan(π/n) of the local chord off the line through
/// its neighbours; 4 % starts eating a 48-gon's corners, 3 % preserves every
/// polygon we tested down to a 48-facet cylinder while still collapsing the donut
/// cap fan from 204→176 edges. 3 % is therefore the largest corner-safe tolerance,
/// and we take it. (The donut can't reach its 144 ideal without a tolerance that
/// also erases a 48-gon's real corners — the remaining ~32 cap edges are genuine
/// geometry the boolean introduced, not merge failures, so we keep the corners.)
fn merge_collinear(mut segs: Vec<[DVec3; 2]>) -> Vec<[DVec3; 2]> {
    // Collinearity tolerance on the sine of the angle between the two directions
    // (|u × v| for unit u, v). ~0.06° — tight enough not to fuse real corners.
    const SIN_TOL: f64 = 1e-3;
    // Perpendicular-distance simplification: fuse if the mid vertex is within
    // REL_EPS of the span length off the outer chord, plus a small absolute floor.
    // 3 % is the largest value that still preserves a 48-gon's corners (see above).
    const REL_EPS: f64 = 3e-2;
    const ABS_EPS: f64 = 1e-6; // 1 µm floor — matches the vertex weld grid.
    // Endpoints are welded on the 1µm grid; two share a vertex when their keys match.
    let shares = |p: DVec3, q: DVec3| qkey(p) == qkey(q);

    let mut changed = true;
    while changed {
        changed = false;
        'outer: for i in 0..segs.len() {
            for j in (i + 1)..segs.len() {
                let [a0, a1] = segs[i];
                let [b0, b1] = segs[j];
                // Find the shared vertex `m` and the two outer endpoints `p`, `r`.
                let (p, m, r) = if shares(a1, b0) {
                    (a0, a1, b1)
                } else if shares(a1, b1) {
                    (a0, a1, b0)
                } else if shares(a0, b0) {
                    (a1, a0, b1)
                } else if shares(a0, b1) {
                    (a1, a0, b0)
                } else {
                    continue;
                };
                // The merged segment must not collapse (outer endpoints distinct).
                if shares(p, r) {
                    continue;
                }
                // Criterion 1: angle between the two segment directions.
                let ua = (segs[i][1] - segs[i][0]).normalize_or_zero();
                let ub = (segs[j][1] - segs[j][0]).normalize_or_zero();
                if ua == DVec3::ZERO || ub == DVec3::ZERO {
                    continue;
                }
                let angle_ok = ua.cross(ub).length() <= SIN_TOL;

                // Criterion 2: perpendicular distance of the mid vertex `m` from
                // the line p..r. dist = |(m − p) × (r − p)| / |r − p|.
                let span = r - p;
                let span_len = span.length();
                let perp_dist = if span_len > 0.0 {
                    (m - p).cross(span).length() / span_len
                } else {
                    f64::INFINITY
                };
                let dist_ok = perp_dist <= REL_EPS * span_len + ABS_EPS;

                if !(angle_ok || dist_ok) {
                    continue;
                }
                // Fuse: replace i with the span p..r, remove j.
                segs[i] = [p, r];
                segs.swap_remove(j);
                changed = true;
                break 'outer;
            }
        }
    }
    segs
}

/// Orthographic projection of `p` onto the plane through `point` with unit
/// `normal`, sliding along the normal: `p - (n·(p-point)) n`.
fn project_point(p: DVec3, point: DVec3, normal: DVec3) -> DVec3 {
    p - normal * normal.dot(p - point)
}

/// Feature edges lying entirely on the negative side of the plane (`n·(p-point)
/// < -tol` for both endpoints), projected onto the plane along `normal`.
///
/// This is the "edges below/beyond a cut" case: for a plan cut (normal = +Z)
/// it flattens the geometry below the slice onto z = the cut height; for a
/// vertical section it flattens everything on the far side (viewing direction
/// = -normal) onto the cut plane. Edges straddling the plane are dropped
/// (their cut portion is the section loop itself).
pub fn project_edges_behind(
    mesh: &Mesh,
    point: DVec3,
    normal: DVec3,
    tol: f64,
) -> Vec<(DVec3, DVec3)> {
    let n = normal.normalize_or_zero();
    if n == DVec3::ZERO {
        return Vec::new();
    }
    feature_edges(mesh)
        .into_iter()
        .filter(|(a, b)| n.dot(*a - point) < -tol && n.dot(*b - point) < -tol)
        .map(|(a, b)| (project_point(a, point, n), project_point(b, point, n)))
        .collect()
}

/// All feature edges projected orthographically onto the plane through `point`
/// with `normal` (the elevation / pure-projection case: no side filter, no
/// cutting). Zero-length projected edges (edges parallel to the view
/// direction) are dropped.
pub fn project_edges_onto(
    mesh: &Mesh,
    point: DVec3,
    normal: DVec3,
    tol: f64,
) -> Vec<(DVec3, DVec3)> {
    let n = normal.normalize_or_zero();
    if n == DVec3::ZERO {
        return Vec::new();
    }
    feature_edges(mesh)
        .into_iter()
        .map(|(a, b)| (project_point(a, point, n), project_point(b, point, n)))
        .filter(|(a, b)| a.distance(*b) > tol)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{extrude_profile, make_box};
    use glam::DVec2;

    const TOL: f64 = 1e-6;

    /// Approximate a cylinder as an `n`-gon prism centered at (cx, cy), extruded
    /// from `base_z` upward by `height`.
    fn make_cylinder(cx: f64, cy: f64, r: f64, base_z: f64, height: f64, n: usize) -> Mesh {
        let profile: Vec<DVec2> = (0..n)
            .map(|i| {
                let a = std::f64::consts::TAU * (i as f64) / (n as f64);
                DVec2::new(cx + r * a.cos(), cy + r * a.sin())
            })
            .collect();
        extrude_profile(&profile, base_z, height)
    }

    #[test]
    fn project_below_flattens_to_cut_height() {
        // Box from z=0..3; cut plane at z=2. All 12 edges have at least one
        // endpoint below z=2, but only edges fully below survive: the 4 bottom
        // edges (z=0) plus... the 4 verticals straddle (0 and 3) → dropped;
        // the 4 top edges (z=3) are above → dropped. So 4 bottom edges remain.
        let b = make_box(DVec3::ZERO, DVec3::new(2.0, 1.0, 3.0));
        let proj = project_edges_behind(&b, DVec3::new(0.0, 0.0, 2.0), DVec3::Z, TOL);
        assert_eq!(proj.len(), 4, "{proj:?}");
        for (a, c) in &proj {
            assert!((a.z - 2.0).abs() < TOL && (c.z - 2.0).abs() < TOL, "flattened to z=2");
        }
    }

    #[test]
    fn project_below_empty_when_all_above() {
        let b = make_box(DVec3::new(0.0, 0.0, 5.0), DVec3::new(2.0, 1.0, 3.0));
        assert!(project_edges_behind(&b, DVec3::new(0.0, 0.0, 2.0), DVec3::Z, TOL).is_empty());
    }

    #[test]
    fn project_onto_vertical_plane_outline() {
        // Elevation looking along -Y onto the y=0 plane: a box projects to its
        // XZ outline. 12 edges → the 4 y-parallel edges collapse to points and
        // are dropped; the front and back faces' 8 edges overlap in projection
        // but we don't dedup, so we keep 8 non-degenerate edges.
        let b = make_box(DVec3::ZERO, DVec3::new(2.0, 1.0, 3.0));
        let proj = project_edges_onto(&b, DVec3::ZERO, DVec3::Y, TOL);
        assert_eq!(proj.len(), 8, "{proj:?}");
        for (a, c) in &proj {
            assert!(a.y.abs() < TOL && c.y.abs() < TOL, "flattened to y=0");
        }
    }

    #[test]
    fn box_has_12_feature_edges_not_18() {
        // Policy: coplanar quad diagonals are not feature edges. A box is 12
        // triangle-pair faces = 18 unique edges, 6 of them flat diagonals.
        let b = make_box(DVec3::ZERO, DVec3::new(2.0, 1.0, 3.0));
        assert_eq!(feature_edges(&b).len(), 12);
    }

    #[test]
    fn open_surface_boundary_edges_are_features() {
        // A single triangle: all 3 edges are boundaries.
        let m = Mesh::new(vec![DVec3::ZERO, DVec3::X, DVec3::Y], vec![[0, 1, 2]]);
        assert_eq!(feature_edges(&m).len(), 3);
    }

    #[test]
    fn boolean_hole_has_clean_feature_edges_not_seams() {
        // The reviewed case: 8×8×3 slab with a 4×4 hole cut through. The BSP
        // output has many coplanar triangles on each flat face; with the old
        // exact-normal test + index keying these drew as seam lines ("two
        // L-shapes" / donut "line artifacts"). Expect a SMALL, bounded edge set:
        // outer box outline (12) + inner hole rims/verticals (~12), NOT hundreds.
        let slab = make_box(DVec3::ZERO, DVec3::new(8.0, 8.0, 3.0));
        let void = make_box(DVec3::new(2.0, 2.0, -1.0), DVec3::new(4.0, 4.0, 5.0));
        let result = crate::csg_difference(&slab, &void);
        let n = feature_edges(&result).len();
        // Clean outline: outer box (12) + inner hole rims/verticals (~12) ≈ 24,
        // plus a few residual cap chords from the boolean re-tessellation → 32.
        // The old exact-normal + index-key path drew every coplanar diagonal AND
        // every t-junction span (the "two L-shapes"/donut artifacts); coplanar
        // merge + t-junction drop now removes both. The perpendicular-distance
        // merge must not disturb this: it stays at 32.
        assert!(
            (12..=32).contains(&n),
            "boolean hole should be a clean outline (~24-32), got {n} — artifact edges remain"
        );
    }

    #[test]
    fn boolean_hole_edge_count_is_scale_invariant() {
        // Same box-with-hole as above but scaled 1000× (metres → kilometres).
        // The perpendicular-distance merge uses a tolerance RELATIVE to each
        // span's length, so the identical geometry at 1000× must yield the
        // identical edge count — proving the ε is scale-aware and neither over-
        // merges (would drop below the small case) nor under-merges (would exceed
        // it). A purely ABSOLUTE ε would fail this: 1 mm noise becomes 1 m here.
        let s = 1000.0;
        let slab = make_box(DVec3::ZERO, DVec3::new(8.0 * s, 8.0 * s, 3.0 * s));
        let void = make_box(
            DVec3::new(2.0 * s, 2.0 * s, -1.0 * s),
            DVec3::new(4.0 * s, 4.0 * s, 5.0 * s),
        );
        let big = feature_edges(&crate::csg_difference(&slab, &void)).len();

        let slab1 = make_box(DVec3::ZERO, DVec3::new(8.0, 8.0, 3.0));
        let void1 = make_box(DVec3::new(2.0, 2.0, -1.0), DVec3::new(4.0, 4.0, 5.0));
        let small = feature_edges(&crate::csg_difference(&slab1, &void1)).len();

        assert_eq!(big, small, "relative ε must give a scale-invariant edge count");
        assert!((12..=32).contains(&big), "scaled hole still a clean outline, got {big}");
    }

    #[test]
    fn coarse_polygon_prisms_keep_all_corners() {
        // Guardrail: the perpendicular-distance merge must never clip a genuine
        // corner of an intentionally faceted prism. A clean n-gon prism has
        // exactly 3n feature edges (n vertical creases + n top rim + n bottom
        // rim). This must hold for coarse AND fine facet counts — the tolerance
        // is calibrated so even a 48-gon survives intact.
        for n in [6usize, 12, 24, 36, 48] {
            let g = make_cylinder(0.0, 0.0, 1.0, 0.0, 1.0, n);
            assert_eq!(
                feature_edges(&g).len(),
                3 * n,
                "{n}-gon prism lost corners — merge tolerance too loose"
            );
        }
    }

    #[test]
    fn annulus_difference_has_bounded_feature_edges_not_seam_explosion() {
        // "Donut": a fat cylinder with a thinner concentric cylinder bored out.
        // Both are faceted 24-gon prisms. The tube walls' vertical tessellation
        // edges are LEGITIMATELY feature edges — adjacent facets meet at a real
        // dihedral crease — so this asserts an UPPER BOUND, not a tiny exact count.
        //
        // Legitimate edges on a perfectly clean result:
        //   outer wall: 24 vertical creases + 24 top rim + 24 bottom rim = 72
        //   inner wall: same                                              = 72
        //                                                            ideal ≈ 144
        //
        // The two flat annular caps (top + bottom) carry residue: the boolean
        // re-tessellates them and, because the outer/inner 24-gon rings don't
        // align vertex-for-vertex, the inner rim comes out as a fan of short
        // chords that bend gently at each shared vertex. The perpendicular-distance
        // merge folds the near-straight runs of that fan back into the rim — this
        // brings the count from 204 (angle-merge only) down to 176. It does NOT
        // reach the 144 ideal: the last ~32 cap chords bend by more than a 48-gon
        // corner does, so collapsing them would also clip a genuinely fine polygon
        // (see coarse_polygon_prisms_keep_all_corners). We keep the corners; 176 is
        // the clean floor for a corner-safe tolerance.
        //
        // The bound below both tightens the previous 256 ceiling toward the clean
        // result AND still fails loudly on any regression to the old seam explosion
        // (the 616-triangle mesh had ~1800 directed edges, a large fraction of which
        // the old per-edge path drew as coplanar diagonals + t-junction spans).
        const N: usize = 24;
        let outer = make_cylinder(0.0, 0.0, 4.0, 0.0, 3.0, N);
        let inner = make_cylinder(0.0, 0.0, 2.0, -1.0, 5.0, N);
        let result = crate::csg_difference(&outer, &inner);
        let n = feature_edges(&result).len();
        assert!(
            (120..=180).contains(&n),
            "donut should be a clean bounded outline (~144-176), got {n} — seam explosion or under-merge"
        );
    }
}
