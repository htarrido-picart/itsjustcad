// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Push/Pull a flat face of a solid along its normal (#370, SketchUp).
//!
//! Three ways, picked from the face's surroundings:
//! - **Stretch**: every side touching the face runs along the normal (a box
//!   top, a box side), so the face's vertices move and the sides stretch. No
//!   new edges, the result keeps its triangle count.
//! - **Extrude**: a pull out (positive distance) that cannot stretch, or one
//!   asked to keep the old face (`keep`, SketchUp's Ctrl): the face is
//!   lifted and walled to its old outline, which stays as an edge ring.
//! - **Cut**: a push in that cannot stretch, or one past the far end of the
//!   sides: the prism the face sweeps is subtracted. Through the far side it
//!   is a hole; through everything it is an error (nothing would be left).
//!
//! Pure and deterministic: the same mesh, face and distance give the same
//! mesh, so a logged push/pull replays identically.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use glam::DVec3;

use crate::{Mesh, csg_difference};

/// How a push/pull changed the solid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushPullHow {
    Stretched,
    Extruded,
    Cut,
}

impl PushPullHow {
    pub fn label(self) -> &'static str {
        match self {
            PushPullHow::Stretched => "stretched",
            PushPullHow::Extruded => "extruded",
            PushPullHow::Cut => "cut",
        }
    }
}

/// Why a push/pull could not run.
#[derive(Clone, Debug, PartialEq)]
pub enum PushPullError {
    /// The face names no (non-degenerate) triangle of the mesh.
    NoFace,
    /// A zero (or non-finite) distance changes nothing.
    Distance,
    /// The push removes the whole solid.
    RemovesSolid,
}

impl std::fmt::Display for PushPullError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PushPullError::NoFace => "no flat face there",
            PushPullError::Distance => "the distance must be a non-zero number",
            PushPullError::RemovesSolid => "the push goes through the whole solid; nothing would be left",
        })
    }
}

/// Push/pull the flat face made of triangles `face` of `mesh` by `distance`
/// along its normal (positive pulls out). `keep` keeps the old face's outline
/// as an edge ring (an extrusion) instead of stretching the sides.
pub fn push_pull(
    mesh: &Mesh,
    face: &[usize],
    distance: f64,
    keep: bool,
) -> Result<(Mesh, PushPullHow), PushPullError> {
    if !distance.is_finite() || distance == 0.0 {
        return Err(PushPullError::Distance);
    }
    let (pos, tris) = weld_indices(mesh);
    let fset: BTreeSet<usize> = face.iter().copied().filter(|&i| i < tris.len()).collect();
    let (p0, n) = fset
        .iter()
        .find_map(|&i| tri_normal(&pos, tris[i]).map(|n| (pos[tris[i][0] as usize], n)))
        .ok_or(PushPullError::NoFace)?;
    let scale = 1.0 + mesh.aabb().max.abs().max(mesh.aabb().min.abs()).max_element();
    let tol = 1e-9 * scale;
    let fverts: BTreeSet<u32> = fset.iter().flat_map(|&i| tris[i]).collect();
    // Directed face edges; a boundary edge has no reverse inside the face.
    let fedges: HashSet<(u32, u32)> = fset.iter().flat_map(|&i| tri_edges(tris[i])).collect();
    let boundary: Vec<(u32, u32)> = fset
        .iter()
        .flat_map(|&i| tri_edges(tris[i]))
        .filter(|&(a, b)| !fedges.contains(&(b, a)))
        .collect();

    // Can the sides stretch? Each triangle touching the face must run along
    // the normal; `lo..hi` is how far the face may move before it meets the
    // far end of a side.
    let (mut stretchy, mut lo, mut hi) = (true, f64::NEG_INFINITY, f64::INFINITY);
    for (i, t) in tris.iter().enumerate() {
        if fset.contains(&i) || !t.iter().any(|v| fverts.contains(v)) {
            continue;
        }
        match tri_normal(&pos, *t) {
            Some(nt) if nt.dot(n).abs() <= 1e-6 => {}
            _ => {
                stretchy = false;
                break;
            }
        }
        for v in t.iter().filter(|v| !fverts.contains(v)) {
            let s = (pos[*v as usize] - p0).dot(n);
            if s.abs() <= tol {
                stretchy = false;
            } else if s < 0.0 {
                lo = lo.max(s);
            } else {
                hi = hi.min(s);
            }
        }
    }
    if !keep && stretchy && distance > lo + tol && distance < hi - tol {
        let mut pos = pos;
        for v in &fverts {
            pos[*v as usize] += n * distance;
        }
        return Ok((Mesh::new(pos, tris), PushPullHow::Stretched));
    }
    if distance > 0.0 {
        let (mut out_pos, mut out) = (pos.clone(), Vec::with_capacity(tris.len() + 2 * boundary.len()));
        let lifted = lift(&mut out_pos, &pos, &fverts, n * distance);
        out.extend(tris.iter().enumerate().filter(|(i, _)| !fset.contains(i)).map(|(_, t)| *t));
        out.extend(fset.iter().map(|&i| tris[i].map(|v| lifted[&v])));
        walls(&mut out, &boundary, |v| v, |v| lifted[&v]);
        return Ok((Mesh::new(out_pos, out), PushPullHow::Extruded));
    }
    // Cut: subtract the swept prism, lifted a hair off the face so it opens
    // through it, and run a hair past a far face it lands on (a hole).
    let eps = 1e-6 * scale;
    let lands = pos.iter().any(|p| ((*p - p0).dot(n) - distance).abs() <= eps);
    let bottom = distance - if lands { eps } else { 0.0 };
    let mut ppos = Vec::new();
    let low = lift(&mut ppos, &pos, &fverts, n * bottom);
    let high = lift(&mut ppos, &pos, &fverts, n * eps);
    let mut prism = Vec::new();
    prism.extend(fset.iter().map(|&i| tris[i].map(|v| high[&v])));
    prism.extend(fset.iter().map(|&i| {
        let [a, b, c] = tris[i].map(|v| low[&v]);
        [a, c, b]
    }));
    walls(&mut prism, &boundary, |v| low[&v], |v| high[&v]);
    let cut = csg_difference(&Mesh::new(pos, tris), &Mesh::new(ppos, prism));
    if cut.faces().is_empty() {
        return Err(PushPullError::RemovesSolid);
    }
    Ok((cut, PushPullHow::Cut))
}

/// Copies of `verts` moved by `by`, appended to `out`; old index -> new.
fn lift(out: &mut Vec<DVec3>, pos: &[DVec3], verts: &BTreeSet<u32>, by: DVec3) -> BTreeMap<u32, u32> {
    verts
        .iter()
        .map(|&v| {
            out.push(pos[v as usize] + by);
            (v, out.len() as u32 - 1)
        })
        .collect()
}

/// Side quads along the face outline, from `low` to `high` copies, facing
/// out (the outline runs counter-clockwise about the face normal).
fn walls(out: &mut Vec<[u32; 3]>, boundary: &[(u32, u32)], low: impl Fn(u32) -> u32, high: impl Fn(u32) -> u32) {
    for &(a, b) in boundary {
        let (a0, b0, a1, b1) = (low(a), low(b), high(a), high(b));
        out.push([a0, b0, b1]);
        out.push([a0, b1, a1]);
    }
}

fn tri_edges(t: [u32; 3]) -> [(u32, u32); 3] {
    [(t[0], t[1]), (t[1], t[2]), (t[2], t[0])]
}

fn tri_normal(pos: &[DVec3], t: [u32; 3]) -> Option<DVec3> {
    let [a, b, c] = t.map(|i| pos[i as usize]);
    (b - a).cross(c - a).try_normalize()
}

/// Share vertices that sit at the same place (to a relative nanometre), so
/// a flat-shaded mesh's sides and face join; triangles keep their order.
fn weld_indices(mesh: &Mesh) -> (Vec<DVec3>, Vec<[u32; 3]>) {
    let q = 1e-9 * (1.0 + mesh.aabb().max.abs().max(mesh.aabb().min.abs()).max_element());
    let mut at: HashMap<(i64, i64, i64), u32> = HashMap::new();
    let mut pos = Vec::new();
    let remap: Vec<u32> = mesh
        .positions()
        .iter()
        .map(|p| {
            let k = (*p / q).round();
            *at.entry((k.x as i64, k.y as i64, k.z as i64)).or_insert_with(|| {
                pos.push(*p);
                pos.len() as u32 - 1
            })
        })
        .collect();
    let tris = mesh.faces().iter().map(|f| f.map(|i| remap[i as usize])).collect();
    (pos, tris)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{make_box, signed_volume};

    /// Triangles of `m` whose normal is `n` (a box's face).
    fn face(m: &Mesh, n: DVec3) -> Vec<usize> {
        (0..m.faces().len())
            .filter(|&i| {
                let [a, b, c] = m.faces()[i].map(|v| m.positions()[v as usize]);
                (b - a).cross(c - a).normalize().dot(n) > 0.999
            })
            .collect()
    }

    fn bounds(m: &Mesh) -> (DVec3, DVec3) {
        let bb = m.aabb();
        (bb.min, bb.max)
    }

    #[test]
    fn pulling_a_box_top_stretches_its_sides() {
        let b = make_box(DVec3::ZERO, DVec3::new(2.0, 2.0, 3.0));
        let (m, how) = push_pull(&b, &face(&b, DVec3::Z), 2.0, false).unwrap();
        assert_eq!(how, PushPullHow::Stretched);
        assert_eq!(bounds(&m), (DVec3::ZERO, DVec3::new(2.0, 2.0, 5.0)));
        assert!((signed_volume(&m) - 20.0).abs() < 1e-9);
        assert_eq!(m.faces().len(), 12, "no new edges");
    }

    #[test]
    fn pushing_a_side_in_shrinks_the_box() {
        let b = make_box(DVec3::ZERO, DVec3::new(2.0, 2.0, 3.0));
        let (m, how) = push_pull(&b, &face(&b, DVec3::X), -0.5, false).unwrap();
        assert_eq!(how, PushPullHow::Stretched);
        assert_eq!(bounds(&m).1, DVec3::new(1.5, 2.0, 3.0));
        assert!((signed_volume(&m) - 9.0).abs() < 1e-9);
    }

    #[test]
    fn keep_extrudes_a_new_face_over_the_old_outline() {
        let b = make_box(DVec3::ZERO, DVec3::new(2.0, 2.0, 3.0));
        let (m, how) = push_pull(&b, &face(&b, DVec3::Z), 1.0, true).unwrap();
        assert_eq!(how, PushPullHow::Extruded);
        assert_eq!(bounds(&m).1, DVec3::new(2.0, 2.0, 4.0));
        assert!((signed_volume(&m) - 16.0).abs() < 1e-9);
        assert_eq!(m.faces().len(), 12 + 8, "four walls over the old outline");
    }

    #[test]
    fn pushing_through_everything_is_refused() {
        let b = make_box(DVec3::ZERO, DVec3::new(2.0, 2.0, 3.0));
        assert_eq!(push_pull(&b, &face(&b, DVec3::Z), -3.0, false), Err(PushPullError::RemovesSolid));
        assert_eq!(push_pull(&b, &face(&b, DVec3::Z), 0.0, false), Err(PushPullError::Distance));
        assert_eq!(push_pull(&b, &[], 1.0, false), Err(PushPullError::NoFace));
    }

    #[test]
    fn a_face_on_slanted_sides_is_cut_in_and_extruded_out() {
        // A frustum: the top's sides lean, so the top cannot stretch.
        let bottom = [(-2.0, -2.0), (2.0, -2.0), (2.0, 2.0), (-2.0, 2.0)];
        let mut pos: Vec<DVec3> = bottom.iter().map(|&(x, y)| DVec3::new(x, y, 0.0)).collect();
        pos.extend(bottom.iter().map(|&(x, y)| DVec3::new(x / 2.0, y / 2.0, 2.0)));
        let mut tris = vec![[0, 2, 1], [0, 3, 2], [4, 5, 6], [4, 6, 7]];
        for i in 0..4u32 {
            let j = (i + 1) % 4;
            tris.push([i, j, j + 4]);
            tris.push([i, j + 4, i + 4]);
        }
        let f = Mesh::new(pos, tris);
        let v0 = signed_volume(&f);
        let (up, how) = push_pull(&f, &[2, 3], 1.0, false).unwrap();
        assert_eq!(how, PushPullHow::Extruded);
        assert!((signed_volume(&up) - (v0 + 4.0)).abs() < 1e-9, "a 2x2x1 block on top");
        let (down, how) = push_pull(&f, &[2, 3], -0.5, false).unwrap();
        assert_eq!(how, PushPullHow::Cut);
        assert!((signed_volume(&down) - (v0 - 2.0)).abs() < 1e-6, "a 2x2x0.5 pocket");
    }
}
