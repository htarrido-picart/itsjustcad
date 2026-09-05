// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Stepped building massing (plan §7.6, Phase 10).
//!
//! Extrude a [`Footprint`] into a 3D mass, floor by floor. Height is FIXED:
//! `height = floor_count × floor_height`. Floors at or above
//! `stepback_start_floor` inset by `stepback_depth` (a cumulative stepped
//! massing — each step above the threshold pulls the perimeter in further),
//! giving the classic ziggurat / wedding-cake upper-floor step-backs.
//!
//! The mass is emitted as an indexed triangle mesh ([`kernel_mesh::Mesh`]) so
//! the commands crate can bake it directly onto a 3D layer. Per-floor areas are
//! recorded on [`Massing::floors`] so Phase 11 yield can compute GFA / FAR:
//! **GFA = Σ per-floor (net) areas** (net of step-backs and any courtyard void).
//!
//! Determinism: the mesh depends only on the footprint + settings; no seed.
//! A footprint that steps back to nothing simply stops adding floors above that
//! level (height clamps to the floors that survived) — never a panic.

use crate::buildings::footprint::Footprint;
use crate::geometry::polygon2d::Polygon2d;
use crate::settings::SubdivisionSettings;
use glam::{DVec2, DVec3};
use kernel_mesh::{earcut, Mesh};

/// One floor of a stepped mass: its base elevation, its (possibly stepped-back)
/// outer ring, its net floor area (outer minus void), and whether it carries the
/// courtyard void.
#[derive(Debug, Clone)]
pub struct Floor {
    /// Floor index (0 = ground).
    pub index: usize,
    /// Base elevation (`index × floor_height`, plus the lot z added by the caller).
    pub base_z: f64,
    /// This floor's outer footprint ring (after any step-backs).
    pub outer: Polygon2d,
    /// This floor's interior void (courtyard), inset with the outer.
    pub void: Option<Polygon2d>,
    /// Net floor area (outer − void) — the GFA contribution of this floor.
    pub area: f64,
}

/// The result of massing one footprint: the 3D mesh, the per-floor list (for
/// GFA/FAR), the fixed height, and the achieved floor count (≤ requested when a
/// step-back collapsed the upper floors).
#[derive(Debug, Clone)]
pub struct Massing {
    /// The extruded, stepped 3D mass (triangulated).
    pub mesh: Mesh,
    /// Per-floor records for yield (Phase 11).
    pub floors: Vec<Floor>,
    /// Total mass height = `floors.len() × floor_height`.
    pub height: f64,
    /// The floor height used (metres).
    pub floor_height: f64,
}

impl Massing {
    /// Gross floor area = Σ per-floor net areas. This is the value Phase 11 FAR
    /// (GFA / site area) is built from.
    pub fn gfa(&self) -> f64 {
        self.floors.iter().map(|f| f.area).sum()
    }

    /// Achieved floor count (may be < requested if a step-back collapsed upper
    /// floors).
    pub fn floor_count(&self) -> usize {
        self.floors.len()
    }
}

/// Extrude `footprint` into a stepped mass at lot elevation `base_z` under
/// `settings`. Deterministic. Returns `None` if not even the ground floor is
/// buildable.
pub fn massing(footprint: &Footprint, base_z: f64, settings: &SubdivisionSettings) -> Option<Massing> {
    let floor_h = settings.floor_height.max(0.1);
    let requested = settings.floor_count.max(1);
    let step_start = settings.stepback_start_floor;
    let step_depth = settings.stepback_depth.max(0.0);

    let mut floors: Vec<Floor> = Vec::new();
    for i in 0..requested {
        // Cumulative step-back: floors at/above the threshold inset by
        // step_depth per floor above the threshold.
        let inset = if step_depth > 0.0 && i >= step_start {
            step_depth * (i - step_start + 1) as f64
        } else {
            0.0
        };
        let outer = inset_rect(&footprint.outer, inset);
        let Some(outer) = outer else {
            // This floor stepped back to nothing — stop adding floors above.
            break;
        };
        // The void (courtyard) grows with the same inset so the wall band holds;
        // if the void meets the outer the floor becomes solid (no void).
        let void = footprint
            .void
            .as_ref()
            .and_then(|v| inset_rect(v, -inset))
            .filter(|v| v.area() < outer.area());
        let area = outer.area() - void.as_ref().map(|v| v.area()).unwrap_or(0.0);
        if area <= 1e-6 {
            break;
        }
        floors.push(Floor {
            index: i,
            base_z: base_z + i as f64 * floor_h,
            outer,
            void,
            area,
        });
    }

    if floors.is_empty() {
        return None;
    }

    let mesh = build_mesh(&floors, floor_h);
    let height = floors.len() as f64 * floor_h;
    Some(Massing { mesh, floors, height, floor_height: floor_h })
}

/// Inset an axis-aligned-ish rectangle by `margin` (shrinking its bbox toward
/// its centroid). Negative margin grows it. `None` if it collapses. Kept simple
/// (bbox-based) because Phase-10 footprints are rectangles / rings.
fn inset_rect(poly: &Polygon2d, margin: f64) -> Option<Polygon2d> {
    if margin.abs() < 1e-9 {
        return Some(poly.clone());
    }
    let (lo, hi) = poly.aabb();
    let nlo = DVec2::new(lo.x + margin, lo.y + margin);
    let nhi = DVec2::new(hi.x - margin, hi.y - margin);
    if nhi.x - nlo.x <= 1e-3 || nhi.y - nlo.y <= 1e-3 {
        return None;
    }
    Polygon2d::from_pairs([
        (nlo.x, nlo.y),
        (nhi.x, nlo.y),
        (nhi.x, nhi.y),
        (nlo.x, nhi.y),
    ])
}

/// Build the closed triangle mesh for a stepped stack of floors. Each floor
/// contributes its vertical side walls; the ground floor gets a floor slab (cap
/// down) and the top floor a roof-less cap up (roofs are added separately by
/// `roof.rs`, but we cap the top here so the raw mass is watertight when no roof
/// is requested). Courtyard voids get inner side walls too.
fn build_mesh(floors: &[Floor], floor_h: f64) -> Mesh {
    let mut positions: Vec<DVec3> = Vec::new();
    let mut faces: Vec<[u32; 3]> = Vec::new();

    for floor in floors {
        let z0 = floor.base_z;
        let z1 = floor.base_z + floor_h;
        // Outer side walls.
        add_walls(&mut positions, &mut faces, floor.outer.verts(), z0, z1, true);
        // Inner (courtyard) side walls, wound the other way.
        if let Some(v) = &floor.void {
            add_walls(&mut positions, &mut faces, v.verts(), z0, z1, false);
        }
    }

    // Bottom cap (ground floor) — the floor slab, facing down.
    if let Some(ground) = floors.first() {
        add_cap(&mut positions, &mut faces, ground, ground.base_z, false);
    }
    // Top cap (highest floor) — facing up. A roof mesh may be baked on top of
    // this by roof.rs; keeping the cap makes the bare mass watertight.
    if let Some(top) = floors.last() {
        add_cap(&mut positions, &mut faces, top, top.base_z + floor_h, true);
    }

    Mesh::new(positions, faces)
}

/// Add a vertical wall band around `ring` from `z0` to `z1`. `outward` selects
/// the winding so outer walls face out and void walls face in.
fn add_walls(
    positions: &mut Vec<DVec3>,
    faces: &mut Vec<[u32; 3]>,
    ring: &[DVec2],
    z0: f64,
    z1: f64,
    outward: bool,
) {
    let n = ring.len();
    for i in 0..n {
        let a = ring[i];
        let b = ring[(i + 1) % n];
        let base = positions.len() as u32;
        positions.push(DVec3::new(a.x, a.y, z0));
        positions.push(DVec3::new(b.x, b.y, z0));
        positions.push(DVec3::new(b.x, b.y, z1));
        positions.push(DVec3::new(a.x, a.y, z1));
        if outward {
            faces.push([base, base + 1, base + 2]);
            faces.push([base, base + 2, base + 3]);
        } else {
            faces.push([base, base + 2, base + 1]);
            faces.push([base, base + 3, base + 2]);
        }
    }
}

/// Add a horizontal cap (slab) for a floor's ring at elevation `z`. `up` selects
/// the facing. A courtyard void punches a hole via triangulation of the ring
/// with the hole removed (simple: triangulate the outer, drop triangles whose
/// centroid falls inside the void).
fn add_cap(positions: &mut Vec<DVec3>, faces: &mut Vec<[u32; 3]>, floor: &Floor, z: f64, up: bool) {
    let ring = floor.outer.verts();
    let tris = earcut(ring);
    let base = positions.len() as u32;
    for &v in ring {
        positions.push(DVec3::new(v.x, v.y, z));
    }
    for t in tris {
        // Skip triangles whose centroid falls in the courtyard void.
        if let Some(void) = &floor.void {
            let c = (ring[t[0] as usize] + ring[t[1] as usize] + ring[t[2] as usize]) / 3.0;
            if void.contains(c) {
                continue;
            }
        }
        if up {
            faces.push([base + t[0], base + t[1], base + t[2]]);
        } else {
            faces.push([base + t[0], base + t[2], base + t[1]]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buildings::footprint::footprint;
    use crate::settings::{FootprintMode, Typology};

    fn env(w: f64, d: f64) -> Polygon2d {
        Polygon2d::from_pairs([(0.0, 0.0), (w, 0.0), (w, d), (0.0, d)]).unwrap()
    }

    fn settings(floors: usize, step_start: usize, step_depth: f64) -> SubdivisionSettings {
        SubdivisionSettings {
            typology: Typology::Slab,
            footprint_mode: FootprintMode::FullEnvelope,
            floor_height: 3.0,
            floor_count: floors,
            stepback_start_floor: step_start,
            stepback_depth: step_depth,
            ..SubdivisionSettings::default()
        }
    }

    #[test]
    fn height_is_floors_times_floor_height() {
        let e = env(30.0, 40.0);
        let s = settings(5, 99, 0.0); // no step-back
        let fp = footprint(&e, &e, &s).unwrap();
        let m = massing(&fp, 0.0, &s).unwrap();
        assert_eq!(m.floor_count(), 5);
        assert!((m.height - 15.0).abs() < 1e-9, "height {} vs 15", m.height);
    }

    #[test]
    fn stepback_reduces_upper_floor_footprint() {
        let e = env(40.0, 40.0);
        // Step back starting at floor 2 (0-based), 2 m per floor above.
        let s = settings(4, 2, 2.0);
        let fp = footprint(&e, &e, &s).unwrap();
        let m = massing(&fp, 0.0, &s).unwrap();
        // Floors 0,1 = full; floor 2 inset by 2, floor 3 by 4.
        assert!(m.floors[0].area > m.floors[2].area, "floor 2 must be smaller");
        assert!(m.floors[2].area > m.floors[3].area, "floor 3 must be smaller still");
        // Ground floor area == full envelope.
        assert!((m.floors[0].area - e.area()).abs() < 1e-6);
    }

    #[test]
    fn gfa_is_sum_of_floor_areas() {
        let e = env(20.0, 30.0); // 600 m²
        let s = settings(3, 99, 0.0);
        let fp = footprint(&e, &e, &s).unwrap();
        let m = massing(&fp, 0.0, &s).unwrap();
        let want: f64 = m.floors.iter().map(|f| f.area).sum();
        assert!((m.gfa() - want).abs() < 1e-9);
        // No step-back → GFA = floors × ground area.
        assert!((m.gfa() - 3.0 * 600.0).abs() < 1e-6, "gfa {}", m.gfa());
    }

    #[test]
    fn gfa_net_of_stepbacks_is_less_than_naive() {
        let e = env(40.0, 40.0);
        let s = settings(4, 1, 3.0);
        let fp = footprint(&e, &e, &s).unwrap();
        let m = massing(&fp, 0.0, &s).unwrap();
        let naive = 4.0 * e.area();
        assert!(m.gfa() < naive, "stepped GFA {} must be < naive {}", m.gfa(), naive);
    }

    #[test]
    fn mesh_is_nonempty_and_valid() {
        let e = env(20.0, 20.0);
        let s = settings(2, 99, 0.0);
        let fp = footprint(&e, &e, &s).unwrap();
        let m = massing(&fp, 0.0, &s).unwrap();
        assert!(!m.mesh.positions().is_empty());
        assert!(!m.mesh.faces().is_empty());
        // Mesh spans the full height in z.
        let bb = m.mesh.aabb();
        assert!((bb.max.z - 6.0).abs() < 1e-6, "top at {}", bb.max.z);
        assert!(bb.min.z.abs() < 1e-6, "base at {}", bb.min.z);
    }

    #[test]
    fn base_z_offsets_the_mass() {
        let e = env(10.0, 10.0);
        let s = settings(1, 99, 0.0);
        let fp = footprint(&e, &e, &s).unwrap();
        let m = massing(&fp, 5.0, &s).unwrap();
        assert!((m.floors[0].base_z - 5.0).abs() < 1e-9);
        let bb = m.mesh.aabb();
        assert!((bb.min.z - 5.0).abs() < 1e-6, "base z {}", bb.min.z);
    }
}
