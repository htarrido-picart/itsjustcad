// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! One closed solid per building.
//!
//! The stepped tiers and the roof are built directly as ONE watertight,
//! consistently wound triangle mesh — the boolean union of the floor prisms
//! and the roof, constructed rather than computed:
//!
//! - consecutive floors with the same rings merge into one tier (no floor
//!   slabs between floors, one wall quad per edge per tier);
//! - a step-back contributes only the exposed ledge (lower ring minus upper
//!   ring, an annulus) — never the full slab under the upper tier;
//! - the roof's faces close the top tier directly on its eave ring (no cap
//!   under a pitched roof, no coincident duplicate slab under a flat one).
//!
//! Vertices are welded, so every edge is shared by exactly two faces.

use super::massing::Floor;
use super::roof;
use crate::geometry::polygon2d::Polygon2d;
use crate::settings::RoofType;
use glam::{DVec2, DVec3};
use kernel_mesh::{earcut, Mesh};
use std::collections::HashMap;

/// A run of identical floors: one prism from `z0` to `z1`.
struct Tier<'a> {
    z0: f64,
    z1: f64,
    outer: &'a Polygon2d,
    void: Option<&'a Polygon2d>,
}

/// Build the single closed solid for a stepped stack of `floors` capped by
/// `roof_type`. Each floor ring must lie strictly inside the one below it (and
/// each courtyard void strictly inside its outer ring and strictly containing
/// the void below) — what [`super::massing::massing`] produces.
///
/// Returns the mesh and the roof actually built: a pitched roof needs an
/// axis-aligned rectangular top ring (roofs are built on the ring's bounding
/// rectangle), so any other top ring falls back to [`RoofType::Flat`].
pub fn building_solid(floors: &[Floor], floor_h: f64, roof_type: RoofType, pitch_deg: f64) -> (Mesh, RoofType) {
    let mut tiers: Vec<Tier> = Vec::new();
    for f in floors {
        let z1 = f.base_z + floor_h;
        if let Some(t) = tiers.last_mut()
            && t.outer.verts() == f.outer.verts()
            && t.void.map(Polygon2d::verts) == f.void.as_ref().map(Polygon2d::verts)
        {
            t.z1 = z1;
            continue;
        }
        tiers.push(Tier { z0: f.base_z, z1, outer: &f.outer, void: f.void.as_ref() });
    }
    let mut s = Solid::default();
    let (Some(first), Some(top)) = (tiers.first(), tiers.last()) else {
        return (Mesh::new(Vec::new(), Vec::new()), RoofType::Flat);
    };
    s.region(first.outer, first.void, first.z0, false);
    for t in &tiers {
        s.walls(t.outer, t.z0, t.z1, true);
        if let Some(v) = t.void {
            s.walls(v, t.z0, t.z1, false);
        }
    }
    // Step-back ledges: the part of the lower tier's top the upper one leaves
    // exposed — between the outer rings, and between the (growing) voids.
    for w in tiers.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        s.annulus(a.outer, b.outer, a.z1, true);
        if let (Some(av), Some(bv)) = (a.void, b.void) {
            s.annulus(bv, av, a.z1, true);
        }
    }
    let pitched = !matches!(roof_type, RoofType::Flat | RoofType::PerTypology) && is_axis_rect(top.outer);
    if pitched {
        // The roof is open at the eave ring = the top tier's outer ring, so its
        // faces close the solid there. A courtyard under a pitched roof is
        // closed by a ceiling over the void at the eave.
        s.mesh(&roof::roof(top.outer, top.z1, roof_type, pitch_deg));
        if let Some(v) = top.void {
            s.region(v, None, top.z1, false);
        }
        (s.finish(), roof_type)
    } else {
        s.region(top.outer, top.void, top.z1, true);
        (s.finish(), RoofType::Flat)
    }
}

/// True if `p` is an axis-aligned rectangle (4 vertices on its bbox corners).
pub(crate) fn is_axis_rect(p: &Polygon2d) -> bool {
    let (lo, hi) = p.aabb();
    p.len() == 4
        && p.verts().iter().all(|v| {
            ((v.x - lo.x).abs() < 1e-9 || (v.x - hi.x).abs() < 1e-9)
                && ((v.y - lo.y).abs() < 1e-9 || (v.y - hi.y).abs() < 1e-9)
        })
}

/// Welding mesh builder (1 µm grid) — shared vertices make the solid closed.
#[derive(Default)]
struct Solid {
    pos: Vec<DVec3>,
    faces: Vec<[u32; 3]>,
    map: HashMap<[i64; 3], u32>,
}

impl Solid {
    fn v(&mut self, p: DVec3) -> u32 {
        let k = [p.x, p.y, p.z].map(|c| (c * 1e6).round() as i64);
        let pos = &mut self.pos;
        *self.map.entry(k).or_insert_with(|| {
            pos.push(p);
            pos.len() as u32 - 1
        })
    }

    fn tri(&mut self, a: DVec3, b: DVec3, c: DVec3) {
        let t = [self.v(a), self.v(b), self.v(c)];
        if t[0] != t[1] && t[1] != t[2] && t[0] != t[2] {
            self.faces.push(t);
        }
    }

    /// Vertical wall band around CCW `ring`; `outward` faces out (outer
    /// walls), otherwise in (courtyard walls).
    fn walls(&mut self, ring: &Polygon2d, z0: f64, z1: f64, outward: bool) {
        let r = ring.verts();
        for i in 0..r.len() {
            let (a, b) = (r[i], r[(i + 1) % r.len()]);
            let (a0, b0) = (a.extend(z0), b.extend(z0));
            let (a1, b1) = (a.extend(z1), b.extend(z1));
            if outward {
                self.tri(a0, b0, b1);
                self.tri(a0, b1, a1);
            } else {
                self.tri(a0, b1, b0);
                self.tri(a0, a1, b1);
            }
        }
    }

    /// Horizontal face over `outer` (minus `hole`) at `z`, facing up or down.
    fn region(&mut self, outer: &Polygon2d, hole: Option<&Polygon2d>, z: f64, up: bool) {
        match hole {
            Some(h) => self.annulus(outer, h, z, up),
            None => {
                let r = outer.verts();
                let tris: Vec<[usize; 3]> = earcut(r).iter().map(|t| t.map(|i| i as usize)).collect();
                self.flat_tris(r, &tris, z, up);
            }
        }
    }

    /// Horizontal face over `outer` minus the strictly-inside `hole` at `z`.
    fn annulus(&mut self, outer: &Polygon2d, hole: &Polygon2d, z: f64, up: bool) {
        let pts = keyhole(outer.verts(), hole.verts());
        let tris = ear_clip(&pts);
        self.flat_tris(&pts, &tris, z, up);
    }

    fn flat_tris(&mut self, pts: &[DVec2], tris: &[[usize; 3]], z: f64, up: bool) {
        for t in tris {
            let [a, b, c] = t.map(|i| pts[i].extend(z));
            if up {
                self.tri(a, b, c);
            } else {
                self.tri(a, c, b);
            }
        }
    }

    fn mesh(&mut self, m: &Mesh) {
        let p = m.positions();
        for f in m.faces() {
            self.tri(p[f[0] as usize], p[f[1] as usize], p[f[2] as usize]);
        }
    }

    fn finish(self) -> Mesh {
        Mesh::new(self.pos, self.faces)
    }
}

fn cross(u: DVec2, v: DVec2) -> f64 {
    u.x * v.y - u.y * v.x
}

/// True if segments `p–q` and `a–b` cross at a point interior to both.
fn crosses(p: DVec2, q: DVec2, a: DVec2, b: DVec2) -> bool {
    let d1 = cross(q - p, a - p);
    let d2 = cross(q - p, b - p);
    let d3 = cross(b - a, p - a);
    let d4 = cross(b - a, q - a);
    d1 * d2 < 0.0 && d3 * d4 < 0.0
}

/// Merge CCW `outer` and the CCW `hole` inside it into one weakly simple CCW
/// ring by a bridge between the closest mutually visible vertex pair: outer
/// from the bridge vertex all the way round, across, the hole clockwise, and
/// back. Deterministic (ties broken by index).
fn keyhole(o: &[DVec2], h: &[DVec2]) -> Vec<DVec2> {
    let (n, m) = (o.len(), h.len());
    let mut pairs: Vec<(f64, usize, usize)> =
        (0..n).flat_map(|i| (0..m).map(move |j| (o[i].distance_squared(h[j]), i, j))).collect();
    pairs.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    let all: Vec<(DVec2, DVec2)> = [o, h]
        .into_iter()
        .flat_map(|r| (0..r.len()).map(move |k| (r[k], r[(k + 1) % r.len()])))
        .collect();
    let (i, j) = pairs
        .iter()
        .find(|&&(_, i, j)| all.iter().all(|&(a, b)| !crosses(o[i], h[j], a, b)))
        .map_or((0, 0), |&(_, i, j)| (i, j));
    let mut out = Vec::with_capacity(n + m + 2);
    out.extend((0..=n).map(|k| o[(i + k) % n]));
    out.extend((0..=m).map(|k| h[(j + m - k) % m]));
    out
}

/// Ear clipping over a weakly simple CCW ring (a keyhole repeats its bridge
/// vertices): like [`earcut`], but a vertex coincident with an ear's corner
/// never blocks that ear. Returns CCW index triples into `pts`.
fn ear_clip(pts: &[DVec2]) -> Vec<[usize; 3]> {
    let n = pts.len();
    let mut idx: Vec<usize> = (0..n).collect();
    let mut tris = Vec::with_capacity(n.saturating_sub(2));
    let mut guard = 0usize;
    while idx.len() > 3 && guard < n * n {
        guard += 1;
        let m = idx.len();
        let ear = (0..m).find(|&i| {
            let (ia, ib, ic) = (idx[(i + m - 1) % m], idx[i], idx[(i + 1) % m]);
            let (a, b, c) = (pts[ia], pts[ib], pts[ic]);
            if cross(b - a, c - b) <= 1e-12 {
                return false; // reflex or degenerate
            }
            idx.iter().all(|&j| {
                let p = pts[j];
                p == a || p == b || p == c || !in_triangle(p, a, b, c)
            })
        });
        let Some(i) = ear else { break };
        tris.push([idx[(i + m - 1) % m], idx[i], idx[(i + 1) % m]]);
        idx.remove(i);
    }
    if idx.len() == 3 {
        tris.push([idx[0], idx[1], idx[2]]);
    }
    tris
}

fn in_triangle(p: DVec2, a: DVec2, b: DVec2, c: DVec2) -> bool {
    let (d1, d2, d3) = (cross(b - a, p - a), cross(c - b, p - b), cross(a - c, p - c));
    let neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(neg && pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buildings::{build_on_envelope, footprint, massing};
    use crate::settings::{FootprintMode, SubdivisionSettings, Typology};
    use std::collections::BTreeMap;

    fn env(w: f64, d: f64) -> Polygon2d {
        Polygon2d::from_pairs([(0.0, 0.0), (w, 0.0), (w, d), (0.0, d)]).unwrap()
    }

    /// Closed, consistently oriented 2-manifold: every directed edge appears
    /// exactly once and its reverse exactly once (so every undirected edge is
    /// shared by exactly two faces, with no internal or duplicate faces).
    fn assert_closed(m: &Mesh, what: &str) {
        assert!(!m.faces().is_empty(), "{what}: empty");
        let mut directed: BTreeMap<(u32, u32), usize> = BTreeMap::new();
        for f in m.faces() {
            for k in 0..3 {
                *directed.entry((f[k], f[(k + 1) % 3])).or_default() += 1;
            }
        }
        for (&(a, b), &c) in &directed {
            assert_eq!(c, 1, "{what}: directed edge {a}->{b} used {c}×");
            assert_eq!(directed.get(&(b, a)), Some(&1), "{what}: edge {a}-{b} not shared by 2 faces");
        }
    }

    fn volume(m: &Mesh) -> f64 {
        kernel_mesh::signed_volume(m)
    }

    fn settings(typology: Typology, mode: FootprintMode, roof: RoofType) -> SubdivisionSettings {
        SubdivisionSettings {
            typology,
            footprint_mode: mode,
            floor_height: 3.0,
            floor_count: 5,
            stepback_start_floor: 2,
            stepback_depth: 2.0,
            roof_type: roof,
            roof_pitch: 30.0,
            ..SubdivisionSettings::default()
        }
    }

    #[test]
    fn stepped_flat_solid_is_closed_with_no_internal_faces() {
        let e = env(40.0, 30.0);
        let s = settings(Typology::Slab, FootprintMode::FullEnvelope, RoofType::Flat);
        let fp = footprint(&e, &e, &s).unwrap();
        let m = massing(&fp, 0.0, &s).unwrap();
        assert_eq!(m.floor_count(), 5);
        assert_closed(&m.mesh, "stepped flat");
        // Volume == Σ floor area × floor height: no gaps, no double cover.
        let want: f64 = m.floors.iter().map(|f| f.area * 3.0).sum();
        assert!((volume(&m.mesh) - want).abs() < 1e-6, "vol {} vs {want}", volume(&m.mesh));
        // No internal faces: no horizontal face inside the merged ground tier
        // (floors 0-1 share a ring → nothing at z = 3).
        let p = m.mesh.positions();
        assert!(
            !m.mesh.faces().iter().any(|f| f.iter().all(|&i| (p[i as usize].z - 3.0).abs() < 1e-9)),
            "slab between identical floors"
        );
    }

    #[test]
    fn every_roof_and_typology_unions_into_one_closed_solid() {
        let typs = [Typology::Detached, Typology::Row, Typology::Slab, Typology::Courtyard];
        let roofs = [RoofType::Flat, RoofType::Gable, RoofType::Hip, RoofType::Shed];
        for (w, d) in [(40.0, 30.0), (30.0, 44.0)] {
            for typ in typs {
                for rt in roofs {
                    let e = env(w, d);
                    let s = settings(typ, FootprintMode::TypologyDriven, rt);
                    let b = build_on_envelope(&e, &e, 2.0, &s).unwrap();
                    let what = format!("{typ:?}/{rt:?} {w}×{d}");
                    assert_closed(&b.mass, &what);
                    let floors: f64 = b.floors.iter().map(|f| f.area * 3.0).sum();
                    let v = volume(&b.mass);
                    if rt == RoofType::Flat {
                        assert!((v - floors).abs() < 1e-6, "{what}: vol {v} vs {floors}");
                    } else {
                        assert!(v > floors + 1e-6, "{what}: roof adds volume ({v} vs {floors})");
                        let top = b.mass.positions().iter().map(|p| p.z).fold(f64::MIN, f64::max);
                        assert!(top > 2.0 + b.height, "{what}: roof above eave");
                    }
                }
            }
        }
    }

    #[test]
    fn non_rect_envelope_steps_inside_and_falls_back_to_flat() {
        // An L-shaped envelope: step-backs follow the outline (not its bbox),
        // and a pitched roof can't sit on it → flat.
        let e = Polygon2d::from_pairs([(0.0, 0.0), (40.0, 0.0), (40.0, 15.0), (15.0, 15.0), (15.0, 40.0), (0.0, 40.0)])
            .unwrap();
        let s = settings(Typology::Slab, FootprintMode::FullEnvelope, RoofType::Gable);
        let b = build_on_envelope(&e, &e, 0.0, &s).unwrap();
        assert_eq!(b.roof_type, RoofType::Flat);
        assert_closed(&b.mass, "L-shape");
        assert_eq!(b.floor_count(), 5);
        for w in b.floors[1..].windows(2) {
            assert!(w[1].area < w[0].area, "stepped floor smaller");
            assert!(w[1].outer.verts().iter().all(|v| w[0].outer.contains(*v)), "stepped floor nested");
        }
    }
}
