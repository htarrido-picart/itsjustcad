// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Mesh booleans via BSP clipping (csg.js port, f64 document space).
//!
//! Good enough for watertight massing solids from `make_box`/`extrude_profile`.
//! Not exact CSG: coplanar flush faces can leave slivers (workaround: nudge the
//! tool by a micron), and splits introduce t-junctions — harmless for the
//! flat-shaded render path, but volume, not manifoldness, is the invariant to
//! test against.

use glam::DVec3;

use crate::Mesh;

/// Classification tolerance for point-vs-plane tests, in meters.
const PLANE_EPS: f64 = 1e-9;
/// Vertex weld tolerance for rebuilding an indexed mesh.
const WELD_TOL: f64 = 1e-9;
/// Triangles below this area are dropped as degenerate.
const MIN_AREA: f64 = 1e-12;

#[derive(Clone, Copy)]
struct Plane {
    normal: DVec3,
    w: f64,
}

const COPLANAR: u8 = 0;
const FRONT: u8 = 1;
const BACK: u8 = 2;
const SPANNING: u8 = 3;

impl Plane {
    fn from_points(a: DVec3, b: DVec3, c: DVec3) -> Option<Self> {
        let n = (b - a).cross(c - a);
        let len = n.length();
        if len < MIN_AREA {
            return None;
        }
        let normal = n / len;
        Some(Self {
            normal,
            w: normal.dot(a),
        })
    }

    fn flip(&mut self) {
        self.normal = -self.normal;
        self.w = -self.w;
    }

    /// Split `poly` by this plane into the four output lists.
    fn split_polygon(
        &self,
        poly: &Polygon,
        coplanar_front: &mut Vec<Polygon>,
        coplanar_back: &mut Vec<Polygon>,
        front: &mut Vec<Polygon>,
        back: &mut Vec<Polygon>,
    ) {
        let mut polygon_type = 0u8;
        let mut types = Vec::with_capacity(poly.verts.len());
        for v in &poly.verts {
            let t = self.normal.dot(*v) - self.w;
            let ty = if t < -PLANE_EPS {
                BACK
            } else if t > PLANE_EPS {
                FRONT
            } else {
                COPLANAR
            };
            polygon_type |= ty;
            types.push(ty);
        }
        match polygon_type {
            COPLANAR => {
                if self.normal.dot(poly.plane.normal) > 0.0 {
                    coplanar_front.push(poly.clone());
                } else {
                    coplanar_back.push(poly.clone());
                }
            }
            FRONT => front.push(poly.clone()),
            BACK => back.push(poly.clone()),
            _ => {
                let mut f = Vec::new();
                let mut b = Vec::new();
                for i in 0..poly.verts.len() {
                    let j = (i + 1) % poly.verts.len();
                    let (ti, tj) = (types[i], types[j]);
                    let (vi, vj) = (poly.verts[i], poly.verts[j]);
                    if ti != BACK {
                        f.push(vi);
                    }
                    if ti != FRONT {
                        b.push(vi);
                    }
                    if (ti | tj) == SPANNING {
                        let t = (self.w - self.normal.dot(vi)) / self.normal.dot(vj - vi);
                        let v = vi.lerp(vj, t);
                        f.push(v);
                        b.push(v);
                    }
                }
                if f.len() >= 3 {
                    front.push(Polygon {
                        verts: f,
                        plane: poly.plane,
                    });
                }
                if b.len() >= 3 {
                    back.push(Polygon {
                        verts: b,
                        plane: poly.plane,
                    });
                }
            }
        }
    }
}

#[derive(Clone)]
struct Polygon {
    verts: Vec<DVec3>,
    plane: Plane,
}

impl Polygon {
    fn flip(&mut self) {
        self.verts.reverse();
        self.plane.flip();
    }
}

#[derive(Default)]
struct Node {
    plane: Option<Plane>,
    front: Option<Box<Node>>,
    back: Option<Box<Node>>,
    polygons: Vec<Polygon>,
}

impl Node {
    fn from_polygons(polygons: Vec<Polygon>) -> Self {
        let mut node = Node::default();
        node.build(polygons);
        node
    }

    fn invert(&mut self) {
        for p in &mut self.polygons {
            p.flip();
        }
        if let Some(plane) = &mut self.plane {
            plane.flip();
        }
        if let Some(front) = &mut self.front {
            front.invert();
        }
        if let Some(back) = &mut self.back {
            back.invert();
        }
        std::mem::swap(&mut self.front, &mut self.back);
    }

    /// Remove all polygons in `polygons` that are inside this BSP tree.
    fn clip_polygons(&self, polygons: Vec<Polygon>) -> Vec<Polygon> {
        let Some(plane) = &self.plane else {
            return polygons;
        };
        let mut front = Vec::new();
        let mut back = Vec::new();
        let mut co_front = Vec::new();
        let mut co_back = Vec::new();
        for poly in &polygons {
            plane.split_polygon(poly, &mut co_front, &mut co_back, &mut front, &mut back);
        }
        front.extend(co_front);
        back.extend(co_back);
        let mut front = match &self.front {
            Some(node) => node.clip_polygons(front),
            None => front,
        };
        let back = match &self.back {
            Some(node) => node.clip_polygons(back),
            None => Vec::new(), // no back subtree: back side is inside the solid
        };
        front.extend(back);
        front
    }

    /// Remove all polygons in this tree that are inside `bsp`.
    fn clip_to(&mut self, bsp: &Node) {
        self.polygons = bsp.clip_polygons(std::mem::take(&mut self.polygons));
        if let Some(front) = &mut self.front {
            front.clip_to(bsp);
        }
        if let Some(back) = &mut self.back {
            back.clip_to(bsp);
        }
    }

    fn all_polygons(&self) -> Vec<Polygon> {
        let mut out = self.polygons.clone();
        if let Some(front) = &self.front {
            out.extend(front.all_polygons());
        }
        if let Some(back) = &self.back {
            out.extend(back.all_polygons());
        }
        out
    }

    fn build(&mut self, polygons: Vec<Polygon>) {
        if polygons.is_empty() {
            return;
        }
        if self.plane.is_none() {
            self.plane = Some(polygons[0].plane);
        }
        let plane = self.plane.expect("set above");
        let mut front = Vec::new();
        let mut back = Vec::new();
        let mut co_front = Vec::new();
        let mut co_back = Vec::new();
        for poly in &polygons {
            plane.split_polygon(poly, &mut co_front, &mut co_back, &mut front, &mut back);
        }
        self.polygons.extend(co_front);
        self.polygons.extend(co_back);
        if !front.is_empty() {
            self.front
                .get_or_insert_with(Default::default)
                .build(front);
        }
        if !back.is_empty() {
            self.back.get_or_insert_with(Default::default).build(back);
        }
    }
}

fn to_polygons(mesh: &Mesh) -> Vec<Polygon> {
    let pos = mesh.positions();
    mesh.faces()
        .iter()
        .filter_map(|f| {
            let [a, b, c] = f.map(|i| pos[i as usize]);
            let plane = Plane::from_points(a, b, c)?;
            Some(Polygon {
                verts: vec![a, b, c],
                plane,
            })
        })
        .collect()
}

/// BSP split fragments are convex, so each n-gon fan-triangulates safely.
fn to_mesh(polygons: &[Polygon]) -> Mesh {
    let mut positions = Vec::new();
    let mut faces = Vec::new();
    for poly in polygons {
        let base = positions.len() as u32;
        positions.extend_from_slice(&poly.verts);
        for i in 1..poly.verts.len() as u32 - 1 {
            faces.push([base, base + i, base + i + 1]);
        }
    }
    weld(&Mesh::new(positions, faces), WELD_TOL)
}

/// Rebuild the mesh with vertices deduplicated at `tol` and degenerate
/// (near-zero-area) triangles dropped.
pub fn weld(mesh: &Mesh, tol: f64) -> Mesh {
    let inv = 1.0 / tol;
    let mut map: std::collections::HashMap<(i64, i64, i64), u32> =
        std::collections::HashMap::new();
    let mut positions: Vec<DVec3> = Vec::new();
    let mut remap = Vec::with_capacity(mesh.positions().len());
    for p in mesh.positions() {
        let key = (
            (p.x * inv).round() as i64,
            (p.y * inv).round() as i64,
            (p.z * inv).round() as i64,
        );
        let idx = *map.entry(key).or_insert_with(|| {
            positions.push(*p);
            positions.len() as u32 - 1
        });
        remap.push(idx);
    }
    let faces = mesh
        .faces()
        .iter()
        .map(|f| f.map(|i| remap[i as usize]))
        .filter(|&[a, b, c]| {
            if a == b || b == c || a == c {
                return false;
            }
            let (pa, pb, pc) = (
                positions[a as usize],
                positions[b as usize],
                positions[c as usize],
            );
            (pb - pa).cross(pc - pa).length() * 0.5 > MIN_AREA
        })
        .collect();
    Mesh::new(positions, faces)
}

/// Signed volume via the divergence theorem. Positive for CCW-from-outside
/// closed meshes; near the true volume even with t-junctions.
pub fn signed_volume(mesh: &Mesh) -> f64 {
    let pos = mesh.positions();
    mesh.faces()
        .iter()
        .map(|f| {
            let [a, b, c] = f.map(|i| pos[i as usize]);
            a.dot(b.cross(c)) / 6.0
        })
        .sum()
}

/// A ∪ B.
pub fn csg_union(a: &Mesh, b: &Mesh) -> Mesh {
    let mut na = Node::from_polygons(to_polygons(a));
    let mut nb = Node::from_polygons(to_polygons(b));
    na.clip_to(&nb);
    nb.clip_to(&na);
    nb.invert();
    nb.clip_to(&na);
    nb.invert();
    let mut polys = na.all_polygons();
    polys.extend(nb.all_polygons());
    to_mesh(&polys)
}

/// A − B.
pub fn csg_difference(a: &Mesh, b: &Mesh) -> Mesh {
    let mut na = Node::from_polygons(to_polygons(a));
    let mut nb = Node::from_polygons(to_polygons(b));
    na.invert();
    na.clip_to(&nb);
    nb.clip_to(&na);
    nb.invert();
    nb.clip_to(&na);
    nb.invert();
    let polys = nb.all_polygons();
    na.build(polys);
    na.invert();
    to_mesh(&na.all_polygons())
}

/// A ∩ B.
pub fn csg_intersection(a: &Mesh, b: &Mesh) -> Mesh {
    let mut na = Node::from_polygons(to_polygons(a));
    let mut nb = Node::from_polygons(to_polygons(b));
    na.invert();
    nb.clip_to(&na);
    nb.invert();
    na.clip_to(&nb);
    nb.clip_to(&na);
    let polys = nb.all_polygons();
    na.build(polys);
    na.invert();
    to_mesh(&na.all_polygons())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::make_box;

    fn cube(corner: [f64; 3], size: f64) -> Mesh {
        make_box(DVec3::from_array(corner), DVec3::splat(size))
    }

    fn assert_volume(mesh: &Mesh, expected: f64) {
        let v = signed_volume(mesh);
        assert!(
            (v - expected).abs() < 1e-6,
            "volume {v} != expected {expected}"
        );
    }

    #[test]
    fn difference_overlapping_cubes() {
        // 2³ cube minus a 1³ cube overlapping one corner: 8 − 1 = 7.
        let a = cube([0.0, 0.0, 0.0], 2.0);
        let b = cube([1.0, 1.0, 1.0], 2.0); // overlap region is the 1³ corner
        assert_volume(&csg_difference(&a, &b), 7.0);
    }

    #[test]
    fn union_disjoint_cubes() {
        let a = cube([0.0, 0.0, 0.0], 1.0);
        let b = cube([5.0, 0.0, 0.0], 1.0);
        let u = csg_union(&a, &b);
        assert_volume(&u, 2.0);
        assert_eq!(u.faces().len(), a.faces().len() + b.faces().len());
    }

    #[test]
    fn union_overlapping_cubes() {
        // 8 + 8 − 1 (shared corner) = 15.
        let a = cube([0.0, 0.0, 0.0], 2.0);
        let b = cube([1.0, 1.0, 1.0], 2.0);
        assert_volume(&csg_union(&a, &b), 15.0);
    }

    #[test]
    fn intersection_contained_cube() {
        let outer = cube([0.0, 0.0, 0.0], 4.0);
        let inner = cube([1.0, 1.0, 1.0], 1.0);
        assert_volume(&csg_intersection(&outer, &inner), 1.0);
    }

    #[test]
    fn intersection_disjoint_is_empty() {
        let a = cube([0.0, 0.0, 0.0], 1.0);
        let b = cube([5.0, 5.0, 5.0], 1.0);
        assert!(csg_intersection(&a, &b).faces().is_empty());
    }

    #[test]
    fn difference_through_hole() {
        // Courtyard case: tool taller than the slab cuts a through-hole.
        let slab = make_box(DVec3::ZERO, DVec3::new(10.0, 10.0, 3.0));
        let core = make_box(DVec3::new(3.0, 3.0, -1.0), DVec3::new(4.0, 4.0, 5.0));
        assert_volume(&csg_difference(&slab, &core), 300.0 - 48.0);
    }

    #[test]
    fn difference_flush_face() {
        // Tool flush with the top face: 2×2×2 minus 1×1×1 sitting on top half.
        let a = cube([0.0, 0.0, 0.0], 2.0);
        let b = make_box(DVec3::new(0.5, 0.5, 1.0), DVec3::splat(1.0));
        assert_volume(&csg_difference(&a, &b), 7.0);
    }

    #[test]
    fn mirror_transform_keeps_outward_winding() {
        let mut m = cube([1.0, 0.0, 0.0], 2.0);
        let mirror_x = glam::DMat4::from_scale(DVec3::new(-1.0, 1.0, 1.0));
        m.transform(mirror_x);
        // Reflection flips winding; Mesh::transform must flip it back so the
        // signed volume stays positive (outward normals).
        assert_volume(&m, 8.0);
    }

    #[test]
    fn weld_dedupes_and_drops_degenerates() {
        let m = Mesh::new(
            vec![
                DVec3::ZERO,
                DVec3::new(1.0, 0.0, 0.0),
                DVec3::new(0.0, 1.0, 0.0),
                DVec3::new(1e-12, 0.0, 0.0), // welds onto ZERO
            ],
            vec![[0, 1, 2], [3, 1, 2], [0, 1, 3]], // last face degenerates after weld
        );
        let w = weld(&m, 1e-9);
        assert_eq!(w.positions().len(), 3);
        assert_eq!(w.faces().len(), 2);
    }
}
