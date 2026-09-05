// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Skeleton (street-following) subdivision (plan §7.5, Phase 7) —
//! `method=streetfollowing`.
//!
//! This is the method that produces lot lines *perpendicular to the curve* around
//! cul-de-sac bulbs and curved streets — the visually important case. Steps (§7.5):
//!
//! 1. Straight skeleton of the block → faces, one per contour edge
//!    ([`crate::straight_skeleton`]).
//! 2. Group adjacent faces whose STREET edges have similar curvature (a run of
//!    lots along a curved street reads as one band, not a per-segment fan). Uses
//!    the block's `is_street` edge tags.
//! 3. Assign corner regions by `CornerAlignment` (widest street wins; tie-break
//!    on length).
//! 4. Slice each face group perpendicular to its street edges at `lot_width_min`
//!    spacing.
//! 5. Merge lots below `lot_area_min`.
//! 6. Merge shallow / triangular lots per `shallow_lot_frac`.
//! 7. Apply `simplify` vertex reduction.
//!
//! Determinism: the construction is fully deterministic (no RNG — the skeleton and
//! the perpendicular slicing are geometric), so op-log replay is byte-identical
//! for a fixed seed automatically. The `Lot` type + street tagging match the
//! recursive/offset paths so the bake path is identical.
//!
//! Untagged blocks (a plain `lotsubdivide` on an arbitrary curve, no Phase-5
//! street graph) treat EVERY contour edge as frontage — same convention as
//! `recursive_obb` — so the method is usable on the §8 sample blocks directly.

use crate::blocks::block::Block;
use crate::blocks::block_edge::BlockEdge;
use crate::geometry::polygon2d::Polygon2d;
use crate::geometry::polyline::PolylineTools;
use crate::geometry::split::{split_by_line, Line2d};
use crate::settings::{CornerAlignment, SkeletonImpl, SubdivisionSettings};
use crate::straight_skeleton::{FelkelSkeleton, OffsetApproxSkeleton, SkeletonFace, StraightSkeleton};
use crate::subdivision::lot_rules::sliver;
use crate::subdivision::recursive_obb::Lot;
use glam::DVec2;

/// Public entry: subdivide a *tagged* `block` into lots using the straight
/// skeleton (street-following). Deterministic. When the block carries Phase-5
/// street tags, only street edges seed frontage; an untagged block treats every
/// contour edge as frontage.
pub fn subdivide_block(block: &Block, settings: &SubdivisionSettings) -> Vec<Lot> {
    let poly = &block.polygon;
    if poly.len() < 3 || poly.area() < 1e-9 {
        return Vec::new();
    }

    // 1. Straight skeleton → one face per contour edge. The backend is selected
    // by `settings.skeleton_impl` (§12.2): the robust Phase-7 offset-approximate
    // skeleton (default) or the true Phase-12 Felkel skeleton (convex-exact,
    // non-convex fallback). Both implement the same `StraightSkeleton` trait, so
    // the rest of the subdivider is unchanged.
    let faces = match settings.skeleton_impl {
        SkeletonImpl::Felkel => FelkelSkeleton::new().faces(poly),
        SkeletonImpl::Offset => OffsetApproxSkeleton::new().faces(poly),
    };
    if faces.is_empty() {
        return Vec::new();
    }

    // Which contour edges count as frontage? Street-tagged if any tag exists,
    // else every edge (untagged block).
    let has_tags = block.edges.iter().any(|e| e.is_street);
    let frontage: Vec<bool> = (0..poly.len())
        .map(|i| {
            if has_tags {
                edge_is_street(&block.edges, poly, i)
            } else {
                true
            }
        })
        .collect();

    // 2 + 3. Group adjacent faces along runs of similar-curvature street edges;
    // assign corner regions to the winning street per `corner_align`.
    let groups = group_faces(&faces, &frontage, &block.edges, poly, settings.corner_align);

    // 3 + 4. For each group, slice perpendicular to the street edge(s) at
    // lot_width_min spacing.
    let mut lots: Vec<Lot> = Vec::new();
    let block_edges: Vec<(DVec2, DVec2)> = poly.edges().collect();
    for group in &groups {
        slice_group(group, &faces, settings, &block_edges, &mut lots);
    }

    if lots.is_empty() {
        // Degenerate: emit each face as a lot so area is conserved.
        for f in &faces {
            lots.push(make_lot(f.polygon.clone(), &block_edges));
        }
    }

    // 5. Merge lots below lot_area_min.
    lots = merge_small(lots, settings.lot_area_min);

    // 6. Merge shallow / triangular lots per shallow_lot_frac.
    if settings.shallow_lot_frac > 0.0 {
        lots = merge_shallow(lots, settings.shallow_lot_frac);
    }

    // Street-access guarantee: fold any lot that lost its frontage (a wedge that
    // ended up interior after slicing/merging) into a street-touching neighbour.
    if settings.force_street_access >= 1.0 {
        lots = merge_streetless(lots);
    }

    // 7. simplify vertex reduction.
    if settings.simplify > 0.0 {
        for lot in &mut lots {
            if let Some(simplified) = simplify_polygon(&lot.polygon, settings.simplify) {
                lot.polygon = simplified;
            }
        }
    }

    lots
}

/// Convenience for plain (untagged) blocks / the §8 samples.
pub fn subdivide(poly: &Polygon2d, settings: &SubdivisionSettings) -> Vec<Lot> {
    subdivide_block(&Block::untagged(poly.clone()), settings)
}

/// Whether contour edge `i` (of `poly`) is a street edge, per the block tags.
/// Matches an edge by its endpoints (tags are stored in the same CCW order as the
/// polygon, but we match by geometry to be robust to any reordering).
fn edge_is_street(edges: &[BlockEdge], poly: &Polygon2d, i: usize) -> bool {
    let verts = poly.verts();
    let n = verts.len();
    let a = verts[i];
    let b = verts[(i + 1) % n];
    edges.iter().any(|e| {
        e.is_street
            && ((e.a.distance(a) < 1e-6 && e.b.distance(b) < 1e-6)
                || (e.a.distance(b) < 1e-6 && e.b.distance(a) < 1e-6))
    })
}

/// A contiguous run of face indices that share a curved-street band.
#[derive(Debug, Clone)]
struct FaceGroup {
    /// Indices into the `faces` slice, in contour order.
    members: Vec<usize>,
    /// True if this group is a street band (its faces front a street).
    is_street: bool,
}

/// The street "weight" of a contour edge used for corner assignment: `(width,
/// length)`. For untagged blocks width is 0 so the tie-break falls to length.
fn edge_street_weight(edges: &[BlockEdge], poly: &Polygon2d, i: usize) -> (f64, f64) {
    let verts = poly.verts();
    let n = verts.len();
    let a = verts[i];
    let b = verts[(i + 1) % n];
    let len = a.distance(b);
    let width = edges
        .iter()
        .find(|e| {
            e.is_street
                && ((e.a.distance(a) < 1e-6 && e.b.distance(b) < 1e-6)
                    || (e.a.distance(b) < 1e-6 && e.b.distance(a) < 1e-6))
        })
        .map(|e| e.street_width)
        .unwrap_or(0.0);
    (width, len)
}

/// Compare two street edges under `CornerAlignment` — the winner claims a shared
/// corner region. `StreetWidth`: widest wins, tie-break on length. `StreetLength`:
/// longest wins, tie-break on width.
fn corner_wins(a: (f64, f64), b: (f64, f64), align: CornerAlignment) -> bool {
    let (aw, al) = a;
    let (bw, bl) = b;
    match align {
        CornerAlignment::StreetWidth => aw > bw || (aw == bw && al >= bl),
        CornerAlignment::StreetLength => al > bl || (al == bl && aw >= bw),
    }
}

/// Group adjacent faces whose street edges have similar curvature into bands
/// (§7.5 step 2). Non-street faces become singleton groups, EXCEPT a non-street
/// "corner" face wedged between two street bands is annexed by the winning band
/// per `corner_align` (§7.5 step 3: widest street wins, tie-break on length).
/// Two adjacent street faces join when the turn angle between their base edges is
/// below a threshold (a curved street reads as one band, not a per-segment fan).
fn group_faces(
    faces: &[SkeletonFace],
    frontage: &[bool],
    block_edges: &[BlockEdge],
    poly: &Polygon2d,
    corner_align: CornerAlignment,
) -> Vec<FaceGroup> {
    let n = faces.len();
    if n == 0 {
        return Vec::new();
    }
    // Curvature threshold: faces of a smoothly-curved street turn only a little
    // between successive segments. 40° keeps a bulb/curve together but breaks at
    // true corners (block corners turn ~90°).
    let max_turn = 40.0_f64.to_radians();

    // A face fronts a street when its contour edge is frontage.
    let face_street: Vec<bool> = faces
        .iter()
        .map(|f| frontage.get(f.edge_index).copied().unwrap_or(false))
        .collect();

    let mut groups: Vec<FaceGroup> = Vec::new();
    let mut used = vec![false; n];

    for start in 0..n {
        if used[start] {
            continue;
        }
        if !face_street[start] {
            used[start] = true;
            groups.push(FaceGroup {
                members: vec![start],
                is_street: false,
            });
            continue;
        }
        // Grow a street band forward while the next face is also a street face
        // and the base-edge turn is small (similar curvature).
        let mut members = vec![start];
        used[start] = true;
        let mut cur = start;
        loop {
            let nxt = (cur + 1) % n;
            if nxt == start || used[nxt] || !face_street[nxt] {
                break;
            }
            let turn = base_turn(&faces[cur], &faces[nxt], poly);
            if turn > max_turn {
                break;
            }
            members.push(nxt);
            used[nxt] = true;
            cur = nxt;
        }
        groups.push(FaceGroup {
            members,
            is_street: true,
        });
    }

    // §7.5 step 3 — annex non-street "corner" faces to the winning street band.
    // A corner face is a non-street singleton whose contour neighbours (edge
    // index ±1) are both street bands; it joins whichever wins under corner_align.
    let group_of = |face_idx: usize, groups: &[FaceGroup]| -> Option<usize> {
        groups.iter().position(|g| g.members.contains(&face_idx))
    };
    // Snapshot the pre-annex groups so neighbour lookups are stable.
    let mut annex: Vec<(usize, usize)> = Vec::new(); // (corner face, winner group)
    for (gi, g) in groups.iter().enumerate() {
        if g.is_street || g.members.len() != 1 {
            continue;
        }
        let fi = g.members[0];
        let prev_face = (fi + n - 1) % n;
        let next_face = (fi + 1) % n;
        let (Some(pg), Some(ng)) = (group_of(prev_face, &groups), group_of(next_face, &groups))
        else {
            continue;
        };
        if pg == gi || ng == gi || !groups[pg].is_street || !groups[ng].is_street {
            continue;
        }
        let wp = edge_street_weight(block_edges, poly, faces[prev_face].edge_index);
        let wn = edge_street_weight(block_edges, poly, faces[next_face].edge_index);
        let winner = if corner_wins(wp, wn, corner_align) { pg } else { ng };
        annex.push((fi, winner));
    }
    if !annex.is_empty() {
        for (fi, winner) in &annex {
            groups[*winner].members.push(*fi);
        }
        // Drop the now-annexed singleton corner groups.
        let annexed: std::collections::HashSet<usize> = annex.iter().map(|(fi, _)| *fi).collect();
        groups.retain(|g| !(g.members.len() == 1 && annexed.contains(&g.members[0]) && !g.is_street));
    }

    groups
}

/// Turn angle (radians) between two faces' base-edge directions.
fn base_turn(a: &SkeletonFace, b: &SkeletonFace, _poly: &Polygon2d) -> f64 {
    let da = a.base_dir();
    let db = b.base_dir();
    da.dot(db).clamp(-1.0, 1.0).acos()
}

/// Slice one face group perpendicular to its street edge(s) at `lot_width_min`
/// spacing (§7.5 step 4). For a street band the frontage is the concatenated base
/// edges; we slice along it. For a non-street singleton we still slice its face
/// along its longer base direction so it does not stay one giant lot.
fn slice_group(
    group: &FaceGroup,
    faces: &[SkeletonFace],
    settings: &SubdivisionSettings,
    block_edges: &[(DVec2, DVec2)],
    out: &mut Vec<Lot>,
) {
    let width = settings.lot_width_min.max(1e-3);

    // Build the frontage polyline as the sequence of base vertices of the band.
    let mut frontage_pts: Vec<DVec2> = Vec::new();
    for (k, &fi) in group.members.iter().enumerate() {
        let f = &faces[fi];
        if k == 0 {
            frontage_pts.push(f.base_a);
        }
        frontage_pts.push(f.base_b);
    }
    if frontage_pts.len() < 2 {
        for &fi in &group.members {
            out.push(make_lot(faces[fi].polygon.clone(), block_edges));
        }
        return;
    }

    // Closed-loop band (a cul-de-sac bulb / ring: its faces span the whole
    // contour and the frontage returns to its start). An infinite-line cut would
    // pass through the centre and split the whole ring twice, so pie-slice from
    // the centroid to perimeter division points instead — producing wedge lots
    // whose sides are perpendicular-ish to the curved street (the visual claim).
    let n_faces = faces.len();
    let is_loop = group.members.len() == n_faces
        && frontage_pts.first().unwrap().distance(*frontage_pts.last().unwrap()) < 1e-6;
    if is_loop {
        pie_slice_loop(&frontage_pts, width, block_edges, out);
        return;
    }

    // Open band (a curved street run). Slice each CONVEX skeleton face
    // independently by the global set of perpendicular cut lines, at
    // `lot_width_min` spacing along the shared frontage polyline. Because each
    // face is convex the half-plane split is exact (no area loss / bridging), and
    // a global cut position keeps the lot lines aligned across face boundaries so
    // a curved street reads as continuous perpendicular lot lines. A cut line that
    // does not cross a given face leaves it whole — so faces still tile the band
    // with no gaps and no overlaps.
    let total = PolylineTools::arc_length(&frontage_pts);
    if total < width {
        for &fi in &group.members {
            out.push(make_lot(faces[fi].polygon.clone(), block_edges));
        }
        return;
    }

    // Number of lots along the frontage. Round to nearest so widths sit near
    // lot_width_min (never much below).
    let count = (total / width).round().max(1.0) as usize;
    if count <= 1 {
        for &fi in &group.members {
            out.push(make_lot(faces[fi].polygon.clone(), block_edges));
        }
        return;
    }
    let step = total / count as f64;

    // Cut positions along the frontage (interior cuts only: 1..count).
    let mut cut_lines: Vec<Line2d> = Vec::with_capacity(count - 1);
    for c in 1..count {
        let s = step * c as f64;
        let t = (s / total).clamp(0.0, 1.0);
        let cut_point = PolylineTools::point_at(&frontage_pts, t);
        // Perpendicular to the local frontage tangent → the lot line is
        // perpendicular to the street (the visual claim). The cut LINE runs along
        // the perpendicular direction (so it separates lots along the frontage).
        let perp = PolylineTools::perpendicular_at(&frontage_pts, t);
        cut_lines.push(Line2d::new(cut_point, perp));
    }

    // Slice each face by every cut line. Skeleton faces are convex-ish, so the
    // half-plane split is normally exact; if a split does not conserve the piece's
    // area (a rare non-convex face where Sutherland–Hodgman would bridge), keep
    // the piece whole rather than lose area — the sliver merge tidies up later.
    let mut pieces: Vec<Polygon2d> = group.members.iter().map(|&fi| faces[fi].polygon.clone()).collect();
    for line in &cut_lines {
        let mut next: Vec<Polygon2d> = Vec::with_capacity(pieces.len() + 1);
        for piece in pieces.drain(..) {
            let before = piece.area();
            let (a, b) = split_by_line(&piece, line);
            match (a, b) {
                (Some(a), Some(b))
                    if a.area() > 1e-7
                        && b.area() > 1e-7
                        && (a.area() + b.area() - before).abs() < before * 1e-3 =>
                {
                    next.push(a);
                    next.push(b);
                }
                _ => next.push(piece),
            }
        }
        pieces = next;
    }

    for p in pieces {
        if p.area() > 1e-7 {
            out.push(make_lot(p, block_edges));
        }
    }
}

/// Pie-slice a closed-loop frontage (cul-de-sac bulb / ring) into wedge lots.
/// The frontage polyline is the full perimeter (first == last); we place lots
/// around it at `width` spacing and build each as `[perimeter arc… , centroid]`.
/// Exactly tiles the enclosed disc (Σ wedge area == polygon area) with no gaps or
/// overlaps, and each wedge fronts the street (its arc is on the boundary).
fn pie_slice_loop(
    frontage_pts: &[DVec2],
    width: f64,
    block_edges: &[(DVec2, DVec2)],
    out: &mut Vec<Lot>,
) {
    // Perimeter (drop the duplicated closing point for the polygon).
    let mut ring: Vec<DVec2> = frontage_pts.to_vec();
    if ring.len() >= 2 && ring[0].distance(*ring.last().unwrap()) < 1e-6 {
        ring.pop();
    }
    let Some(poly) = Polygon2d::new(ring.clone()) else {
        return;
    };
    let centroid = poly.centroid();
    let perimeter = poly.perimeter();
    let count = (perimeter / width).round().max(1.0) as usize;
    if count <= 1 {
        out.push(make_lot(poly, block_edges));
        return;
    }

    // Cumulative arc length around the ring (closed).
    let closed: Vec<DVec2> = {
        let mut v = ring.clone();
        v.push(ring[0]);
        v
    };
    let step = perimeter / count as f64;
    // For each wedge, collect the perimeter points between arc positions
    // [c*step, (c+1)*step], then close to the centroid.
    for c in 0..count {
        let s0 = step * c as f64;
        let s1 = step * (c + 1) as f64;
        let arc = arc_points(&closed, s0, s1);
        if arc.len() < 2 {
            continue;
        }
        let mut verts = arc;
        verts.push(centroid);
        if let Some(wedge) = Polygon2d::new(verts)
            && wedge.area() > 1e-7
        {
            out.push(make_lot(wedge, block_edges));
        }
    }
}

/// The polyline points on `closed` (a closed ring, last == first) between arc
/// lengths `s0` and `s1`, including interpolated endpoints and any ring vertices
/// in between (so the wedge follows the perimeter exactly).
fn arc_points(closed: &[DVec2], s0: f64, s1: f64) -> Vec<DVec2> {
    let cum = PolylineTools::cumulative(closed);
    let total = *cum.last().unwrap_or(&0.0);
    if total < 1e-9 {
        return Vec::new();
    }
    let at = |s: f64| -> DVec2 {
        let t = (s / total).clamp(0.0, 1.0);
        PolylineTools::point_at(closed, t)
    };
    let mut out = vec![at(s0)];
    for (i, &c) in cum.iter().enumerate() {
        if c > s0 + 1e-9 && c < s1 - 1e-9 {
            let p = closed[i];
            if out.last().map(|l| l.distance(p) > 1e-9).unwrap_or(true) {
                out.push(p);
            }
        }
    }
    let end = at(s1);
    if out.last().map(|l| l.distance(end) > 1e-9).unwrap_or(true) {
        out.push(end);
    }
    out
}

/// Build a `Lot`, computing `has_street` against the block boundary.
fn make_lot(poly: Polygon2d, block_edges: &[(DVec2, DVec2)]) -> Lot {
    let has_street = touches_boundary(&poly, block_edges);
    Lot {
        polygon: poly,
        has_street,
    }
}

fn touches_boundary(lot: &Polygon2d, block_edges: &[(DVec2, DVec2)]) -> bool {
    for (a, b) in lot.edges() {
        let mid = (a + b) * 0.5;
        for &(ea, eb) in block_edges {
            if point_on_segment(mid, ea, eb, 1e-4) {
                return true;
            }
        }
    }
    false
}

fn point_on_segment(p: DVec2, a: DVec2, b: DVec2, tol: f64) -> bool {
    let ab = b - a;
    let len2 = ab.length_squared();
    if len2 < 1e-18 {
        return p.distance(a) < tol;
    }
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    p.distance(a + ab * t) < tol
}

/// Fold any streetless lot into its largest-shared-edge (street-touching)
/// neighbour, so `force_street_access` holds. Deterministic.
fn merge_streetless(mut lots: Vec<Lot>) -> Vec<Lot> {
    while let Some(idx) = lots.iter().position(|l| !l.has_street) {
        if lots.len() <= 1 {
            break;
        }
        let victim = lots.remove(idx);
        match pick_merge_target(&victim.polygon, &lots) {
            Some(j) => {
                let u = crate::geometry::clip_bridge::union(&lots[j].polygon, &victim.polygon);
                let target = lots[j].polygon.area() + victim.polygon.area();
                match u.into_iter().find(|p| (p.area() - target).abs() < target * 5e-3) {
                    // Only accept a single conserving union (never drop area).
                    Some(m) => {
                        lots[j].polygon = m;
                        lots[j].has_street = lots[j].has_street || victim.has_street;
                    }
                    None => {
                        // No conserving merge — keep the lot (rare; avoids area
                        // loss). Re-insert at the end so the loop makes progress.
                        lots.push(victim);
                        break;
                    }
                }
            }
            None => {
                lots.push(victim);
                break;
            }
        }
    }
    lots
}

/// §7.5 step 5 — merge any lot below `lot_area_min` into a neighbour.
///
/// Uses the Phase-6 sliver merger, but ONLY on a below-threshold lot that has a
/// genuinely edge-adjacent neighbour (verified by a conserving union). The
/// Phase-6 merger keeps the largest ring of a union and would drop area if it
/// merged two non-adjacent lots; the skeleton produces more such candidates
/// (curved wedges whose shared edges do not register), so we pre-check adjacency
/// per lot and keep any lot that has no conserving merge rather than lose it.
fn merge_small(mut lots: Vec<Lot>, area_min: f64) -> Vec<Lot> {
    if area_min <= 0.0 || lots.len() < 2 {
        return lots;
    }
    let max_iters = lots.len() * 2 + 4;
    for _ in 0..max_iters {
        // Smallest below-threshold lot (deterministic: min area, lowest index).
        let mut si: Option<usize> = None;
        let mut smin = f64::INFINITY;
        for (i, l) in lots.iter().enumerate() {
            let a = l.polygon.area();
            if a < area_min && a < smin - 1e-12 {
                smin = a;
                si = Some(i);
            }
        }
        let Some(si) = si else { break };
        // Best adjacent neighbour whose union is a single, area-conserving ring.
        let victim = lots[si].polygon.clone();
        let mut best: Option<(usize, Polygon2d)> = None;
        let mut best_share = 0.0;
        for (j, l) in lots.iter().enumerate() {
            if j == si {
                continue;
            }
            let share = sliver::shared_edge_length(&victim, &l.polygon);
            if share <= best_share + 1e-9 {
                continue;
            }
            let u = crate::geometry::clip_bridge::union(&victim, &l.polygon);
            if u.len() == 1 {
                let target = victim.area() + l.polygon.area();
                if (u[0].area() - target).abs() < target * 5e-3 {
                    best_share = share;
                    best = Some((j, u.into_iter().next().unwrap()));
                }
            }
        }
        let Some((bj, merged)) = best else {
            // No conserving merge for this sliver → leave it (breaks the loop only
            // for this lot by nudging its effective area above threshold). To keep
            // the loop finite and deterministic, remove-and-reinsert so the next
            // iteration finds the next-smallest instead of looping on this one.
            let keep = lots.remove(si);
            lots.push(keep);
            // If the smallest lot cannot be merged, the rest (larger) also may
            // not; stop to avoid churn.
            break;
        };
        let has_street = lots[si].has_street || lots[bj].has_street;
        let (lo, hi) = if si < bj { (si, bj) } else { (bj, si) };
        lots.remove(hi);
        lots.remove(lo);
        lots.push(Lot { polygon: merged, has_street });
    }
    lots
}

/// §7.5 step 6 — merge shallow/triangular lots. A lot is "shallow" when its OBB
/// short extent is below `shallow_lot_frac × long extent` (a thin sliver / sharp
/// triangle). Merge those into their largest-shared-edge neighbour.
fn merge_shallow(lots: Vec<Lot>, frac: f64) -> Vec<Lot> {
    use crate::geometry::oriented_box::OrientedBox;
    // Reuse the sliver merger but with a "shallow" predicate: tag shallow lots by
    // giving them an artificially small area so the merger folds them. Simpler:
    // repeatedly find a shallow lot and merge it into its best neighbour.
    let mut lots = lots;
    let is_shallow = |p: &Polygon2d| -> bool {
        match OrientedBox::of_polygon(p) {
            Some(ob) => ob.long_len() > 1e-9 && ob.short_len() / ob.long_len() < frac,
            None => true,
        }
    };
    while let Some(idx) = lots.iter().position(|l| is_shallow(&l.polygon)) {
        if lots.len() <= 1 {
            break;
        }
        // Merge into the neighbour sharing the longest boundary.
        let victim = lots.remove(idx);
        let best = pick_merge_target(&victim.polygon, &lots);
        match best {
            Some(j) => {
                let merged = crate::geometry::clip_bridge::union(&lots[j].polygon, &victim.polygon);
                if let Some(m) = merged.into_iter().max_by(|a, b| {
                    a.area().partial_cmp(&b.area()).unwrap_or(std::cmp::Ordering::Equal)
                }) {
                    lots[j].polygon = m;
                    lots[j].has_street = lots[j].has_street || victim.has_street;
                } else {
                    lots.push(victim); // union failed → keep it, avoid infinite loop
                    break;
                }
            }
            None => {
                lots.push(victim);
                break;
            }
        }
    }
    lots
}

/// Pick the lot in `pool` sharing the longest boundary with `poly`.
fn pick_merge_target(poly: &Polygon2d, pool: &[Lot]) -> Option<usize> {
    let mut best: Option<(usize, f64)> = None;
    for (j, other) in pool.iter().enumerate() {
        let shared = shared_boundary_len(poly, &other.polygon);
        if shared > best.map(|(_, s)| s).unwrap_or(0.0) {
            best = Some((j, shared));
        }
    }
    best.map(|(j, _)| j)
}

/// Approximate shared-boundary length between two polygons (overlapping edge
/// spans). Cheap: sum midpoint-on-edge coincidences weighted by edge length.
fn shared_boundary_len(a: &Polygon2d, b: &Polygon2d) -> f64 {
    let mut total = 0.0;
    for (pa, pb) in a.edges() {
        // Sample the edge; if a sample lies on any edge of `b`, count that span.
        let samples = 8;
        let mut hits = 0;
        for k in 0..=samples {
            let t = k as f64 / samples as f64;
            let p = pa.lerp(pb, t);
            if b.edges().any(|(qa, qb)| point_on_segment(p, qa, qb, 1e-4)) {
                hits += 1;
            }
        }
        if hits > 1 {
            total += pa.distance(pb) * (hits as f64 / (samples + 1) as f64);
        }
    }
    total
}

/// §7.5 step 7 — Douglas–Peucker vertex reduction on a lot polygon.
fn simplify_polygon(poly: &Polygon2d, eps: f64) -> Option<Polygon2d> {
    let mut pts = poly.verts().to_vec();
    // Close the ring for DP, simplify, then reopen.
    pts.push(pts[0]);
    let simplified = PolylineTools::simplify(&pts, eps);
    if simplified.len() < 4 {
        return None; // would collapse
    }
    // Drop the duplicated closing point.
    let mut v = simplified;
    v.pop();
    Polygon2d::new(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::SubdivisionSettings;

    fn base_settings() -> SubdivisionSettings {
        SubdivisionSettings {
            method: crate::settings::SubdivisionMethod::Skeleton,
            lot_area_min: 50.0,
            lot_width_min: 10.0,
            force_street_access: 1.0,
            merge_slivers: true,
            seed: 7,
            ..SubdivisionSettings::default()
        }
    }

    #[test]
    fn square_subdivides_and_conserves_area() {
        let sq = Polygon2d::from_pairs([(0.0, 0.0), (40.0, 0.0), (40.0, 40.0), (0.0, 40.0)]).unwrap();
        let lots = subdivide(&sq, &base_settings());
        assert!(!lots.is_empty());
        let sum: f64 = lots.iter().map(|l| l.polygon.area()).sum();
        assert!(
            (sum - sq.area()).abs() / sq.area() < 0.02,
            "area conserved: {sum} vs {}",
            sq.area()
        );
    }

    #[test]
    fn deterministic_same_seed() {
        let sq = Polygon2d::from_pairs([(0.0, 0.0), (60.0, 0.0), (60.0, 30.0), (0.0, 30.0)]).unwrap();
        let a = subdivide(&sq, &base_settings());
        let b = subdivide(&sq, &base_settings());
        assert_eq!(a.len(), b.len());
        for (la, lb) in a.iter().zip(&b) {
            assert_eq!(la.polygon.verts(), lb.polygon.verts());
        }
    }
}
