// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! The ray-traceable scene: a triangle soup with per-triangle materials, an
//! acceleration structure reused from `kernel-mesh`, and the sun/sky lighting
//! model.
//!
//! Acceleration reuse: we build a [`kernel_mesh::Bvh`] over per-triangle AABBs
//! (the same median-split tree the picker and solar sun-hours use) and query
//! `ray_closest_hit` / `ray_any_hit` with the exact Möller–Trumbore test from
//! `kernel_mesh::ray_triangle`. We keep our own triangle vertex/normal/material
//! arrays alongside so a hit yields the shading normal and material — the doc
//! `Bvh`/`TriBvh` alone return only a distance.
//!
//! Hardware ray tracing (#318): on an Apple9 GPU the batched queries
//! ([`Scene::submit`] / [`Scene::finish`]) run on
//! [`itsjustcad_hwrt::Tracer`] in f32; everywhere else, and with
//! `ITSJUSTCAD_HWRT=cpu`, they map [`Scene::intersect`] / [`Scene::occluded`].

use std::sync::OnceLock;

use glam::DVec3;
use itsjustcad_hwrt::Tracer;
use kernel_mesh::{ray_triangle, Aabb, Bvh};
use rayon::prelude::*;

use crate::material::Material;

/// A single scene triangle with its material index.
#[derive(Clone, Copy, Debug)]
struct Tri {
    v0: DVec3,
    v1: DVec3,
    v2: DVec3,
    /// Geometric normal (unit), precomputed.
    n: DVec3,
    mat: u32,
}

/// A ray hit: point, front-facing normal, distance, and the material.
#[derive(Clone, Copy, Debug)]
pub struct Hit {
    pub t: f64,
    pub point: DVec3,
    /// Normal oriented against the ray (front-facing).
    pub normal: DVec3,
    pub material: Material,
}

/// A directional sun light with a small angular radius for soft shadows.
#[derive(Clone, Copy, Debug)]
pub struct Sun {
    /// Unit direction *toward* the sun.
    pub dir: DVec3,
    /// Radiance (linear, warm-tinted, may exceed 1).
    pub radiance: DVec3,
    /// Angular radius in radians (~0.0047 = the real solar disc; larger = softer
    /// shadows).
    pub angular_radius: f64,
}

impl Default for Sun {
    fn default() -> Self {
        Self {
            // Late-afternoon default: up and to one side.
            dir: DVec3::new(0.3, 0.4, 0.85).normalize(),
            // Warm white, bright.
            radiance: DVec3::new(1.0, 0.95, 0.85) * 6.0,
            angular_radius: 0.02,
        }
    }
}

/// Hemispheric sky: a gradient from horizon to zenith, used as ambient/IBL for
/// rays that escape the geometry.
#[derive(Clone, Copy, Debug)]
pub struct Sky {
    pub zenith: DVec3,
    pub horizon: DVec3,
    /// Ground colour for rays pointing below the horizon (Z < 0).
    pub ground: DVec3,
}

impl Default for Sky {
    fn default() -> Self {
        Self {
            zenith: DVec3::new(0.30, 0.50, 0.90),
            horizon: DVec3::new(0.75, 0.82, 0.92),
            ground: DVec3::splat(0.28),
        }
    }
}

impl Sky {
    /// Radiance seen along unit direction `d` (Z-up world).
    pub fn radiance(&self, d: DVec3) -> DVec3 {
        if d.z >= 0.0 {
            let t = d.z.clamp(0.0, 1.0);
            self.horizon.lerp(self.zenith, t)
        } else {
            self.ground
        }
    }
}

/// Rays started by [`Scene::submit`].
pub(crate) enum Pending {
    Hw(itsjustcad_hwrt::Batch),
    Cpu,
}

/// The complete ray-traceable scene.
pub struct Scene {
    tris: Vec<Tri>,
    materials: Vec<Material>,
    bvh: Bvh,
    pub sun: Option<Sun>,
    pub sky: Sky,
    /// World-space AABB of all geometry (for camera framing / epsilon scaling).
    pub bounds: Option<Aabb>,
    /// The GPU scene, built on first use by a batched render.
    hw: OnceLock<Option<Tracer>>,
}

/// A coarse lobe classification the integrator uses to decide whether to run
/// next-event estimation (diffuse only) — kept separate from [`crate::Bsdf`] so
/// the integrator never matches on the full material internals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bsdf {
    Diffuse,
    Metal,
    Glass,
}

/// Incremental builder so callers can push meshes with their materials.
#[derive(Clone, Default)]
pub struct SceneBuilder {
    tris: Vec<Tri>,
    materials: Vec<Material>,
}

impl SceneBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a material, returning its index for [`SceneBuilder::add_triangle`].
    pub fn add_material(&mut self, m: Material) -> u32 {
        let idx = self.materials.len() as u32;
        self.materials.push(m);
        idx
    }

    /// Add one triangle referencing a material index. Degenerate triangles
    /// (zero-area) are skipped so they never produce NaN normals.
    pub fn add_triangle(&mut self, v0: DVec3, v1: DVec3, v2: DVec3, mat: u32) {
        let n = (v1 - v0).cross(v2 - v0);
        let len = n.length();
        if len < 1e-12 {
            return;
        }
        self.tris.push(Tri { v0, v1, v2, n: n / len, mat });
    }

    /// Add an indexed mesh (doc f64 positions + triangle faces) with one
    /// material for the whole mesh.
    pub fn add_mesh(&mut self, positions: &[DVec3], faces: &[[u32; 3]], mat: u32) {
        for f in faces {
            self.add_triangle(
                positions[f[0] as usize],
                positions[f[1] as usize],
                positions[f[2] as usize],
                mat,
            );
        }
    }

    /// Finalise: build the BVH and attach lighting.
    pub fn build(self, sun: Option<Sun>, sky: Sky) -> Scene {
        let boxes: Vec<Aabb> = self
            .tris
            .iter()
            .map(|t| Aabb::from_points([t.v0, t.v1, t.v2]))
            .collect();
        let bvh = Bvh::build(&boxes);
        let bounds = if self.tris.is_empty() {
            None
        } else {
            Some(
                boxes
                    .iter()
                    .copied()
                    .reduce(Aabb::union)
                    .expect("non-empty"),
            )
        };
        Scene { tris: self.tris, materials: self.materials, bvh, sun, sky, bounds, hw: OnceLock::new() }
    }
}

impl Scene {
    pub fn triangle_count(&self) -> usize {
        self.tris.len()
    }

    /// Nearest hit along `origin + t*dir`, `t > eps`. `dir` should be unit for
    /// `t` to be a true distance. Closest-hit BVH traversal with the exact
    /// per-triangle test.
    pub fn intersect(&self, origin: DVec3, dir: DVec3) -> Option<Hit> {
        let best = self.bvh.ray_closest_hit(origin, dir, f64::INFINITY, |i| {
            let tri = &self.tris[i as usize];
            ray_triangle(origin, dir, tri.v0, tri.v1, tri.v2)
        });
        best.map(|(i, t)| self.hit_on(origin, dir, i, t))
    }

    /// The hit on triangle `i` at `t`, front-facing.
    fn hit_on(&self, origin: DVec3, dir: DVec3, i: u32, t: f64) -> Hit {
        let tri = &self.tris[i as usize];
        // Orient the geometric normal against the ray so shading is
        // front-facing (double-sided surfaces).
        let mut n = tri.n;
        if n.dot(dir) > 0.0 {
            n = -n;
        }
        Hit {
            t,
            point: origin + dir * t,
            normal: n,
            material: self.materials[tri.mat as usize],
        }
    }

    /// The hardware ray tracer for this scene, or `None` when unavailable
    /// or forced off (`ITSJUSTCAD_HWRT=cpu`). Builds it on first call.
    pub(crate) fn hw(&self) -> Option<&Tracer> {
        self.hw
            .get_or_init(|| {
                Tracer::new(self.tris.len(), |i| {
                    let t = &self.tris[i];
                    [t.v0, t.v1, t.v2]
                })
            })
            .as_ref()
    }

    /// Start the closest hit for each ray in `rays` and the unbounded
    /// occlusion test for each in `shadows` (both lists of pieces): one GPU
    /// round trip on `hw`, else nothing until [`Scene::finish`] runs them on
    /// the CPU.
    pub(crate) fn submit(&self, hw: Option<&Tracer>, rays: &[&[[DVec3; 2]]], shadows: &[&[[DVec3; 2]]]) -> Pending {
        match hw {
            Some(hw) => Pending::Hw(hw.submit(rays, shadows)),
            None => Pending::Cpu,
        }
    }

    /// Finish a [`Scene::submit`] of the same `rays` and `shadows`: the
    /// closest `(triangle, t)` per ray into `hits` (f32 `t` on the GPU, the
    /// exact one on the CPU; turn one into a [`Hit`] with [`Scene::hit_at`])
    /// and `occluded(o, d, ∞)` per shadow ray into `occ`, pieces flattened.
    pub(crate) fn finish(
        &self,
        hw: Option<&Tracer>,
        p: Pending,
        rays: &[&[[DVec3; 2]]],
        shadows: &[&[[DVec3; 2]]],
        hits: &mut [Option<(u32, f32)>],
        occ: &mut [bool],
    ) {
        if let (Pending::Hw(b), Some(hw)) = (p, hw) {
            return hw.finish(b, hits, occ);
        }
        let mut hs = hits;
        let mut os = occ;
        let mut work = Vec::with_capacity(rays.len());
        for (r, sh) in rays.iter().zip(shadows) {
            let (h, ht) = std::mem::take(&mut hs).split_at_mut(r.len());
            let (o, ot) = std::mem::take(&mut os).split_at_mut(sh.len());
            (hs, os) = (ht, ot);
            work.push((*r, *sh, h, o));
        }
        work.into_par_iter().for_each(|(r, sh, h, o)| {
            for (h, &[o, d]) in h.iter_mut().zip(r) {
                *h = self
                    .bvh
                    .ray_closest_hit(o, d, f64::INFINITY, |i| {
                        let tri = &self.tris[i as usize];
                        ray_triangle(o, d, tri.v0, tri.v1, tri.v2)
                    })
                    .map(|(i, t)| (i, t as f32));
            }
            for (b, &[o, d]) in o.iter_mut().zip(sh) {
                *b = self.occluded(o, d, f64::INFINITY);
            }
        });
    }

    /// The hit on triangle `i` found at `t32` by [`Scene::closest_batch`].
    /// `t` is recomputed with the exact f64 test on that one triangle, so a
    /// hit on the primitive [`Scene::intersect`] picks is its hit bit for bit;
    /// `t32` stands in only when f64 rejects what f32 accepted.
    pub(crate) fn hit_at(&self, origin: DVec3, dir: DVec3, i: u32, t32: f32) -> Hit {
        let v = &self.tris[i as usize];
        let t = ray_triangle(origin, dir, v.v0, v.v1, v.v2).unwrap_or(t32 as f64);
        self.hit_on(origin, dir, i, t)
    }

    /// Is the segment from `origin` toward `dir` blocked before `max_t`? Used for
    /// shadow rays (next-event estimation to the sun). `dir` need not be unit;
    /// `max_t` is in units of `dir`.
    pub fn occluded(&self, origin: DVec3, dir: DVec3, max_t: f64) -> bool {
        self.bvh.ray_any_hit(origin, dir, max_t, |i| {
            let tri = &self.tris[i as usize];
            ray_triangle(origin, dir, tri.v0, tri.v1, tri.v2).is_some_and(|t| t < max_t)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_tri_scene() -> Scene {
        let mut b = SceneBuilder::new();
        let m = b.add_material(Material::diffuse(DVec3::splat(0.8)));
        // Triangle in the z=0 plane, spanning the unit square corner.
        b.add_triangle(
            DVec3::new(-1.0, -1.0, 0.0),
            DVec3::new(1.0, -1.0, 0.0),
            DVec3::new(0.0, 1.0, 0.0),
            m,
        );
        b.build(None, Sky::default())
    }

    #[test]
    fn ray_hits_known_triangle_at_expected_point_and_normal() {
        let scene = one_tri_scene();
        // Shoot straight down at the origin from above.
        let hit = scene
            .intersect(DVec3::new(0.0, 0.0, 5.0), DVec3::new(0.0, 0.0, -1.0))
            .expect("should hit the triangle");
        assert!((hit.t - 5.0).abs() < 1e-9, "t={}", hit.t);
        assert!((hit.point - DVec3::ZERO).length() < 1e-9, "point={}", hit.point);
        // Normal faces back up toward the ray origin (+Z).
        assert!((hit.normal - DVec3::Z).length() < 1e-9, "n={}", hit.normal);
    }

    #[test]
    fn ray_missing_triangle_returns_none() {
        let scene = one_tri_scene();
        let miss = scene.intersect(DVec3::new(10.0, 10.0, 5.0), DVec3::new(0.0, 0.0, -1.0));
        assert!(miss.is_none());
    }

    #[test]
    fn normal_flips_to_face_ray_from_below() {
        let scene = one_tri_scene();
        // Shoot up from below: normal should point down (-Z) to face the ray.
        let hit = scene
            .intersect(DVec3::new(0.0, 0.0, -5.0), DVec3::new(0.0, 0.0, 1.0))
            .expect("hit from below");
        assert!((hit.normal - DVec3::NEG_Z).length() < 1e-9, "n={}", hit.normal);
    }

    #[test]
    fn occlusion_detects_blocker() {
        let scene = one_tri_scene();
        // From above the plane, toward a point below it, the triangle blocks.
        let o = DVec3::new(0.0, 0.0, 5.0);
        let d = DVec3::new(0.0, 0.0, -1.0);
        assert!(scene.occluded(o, d, 100.0));
        // Sideways ray misses entirely.
        assert!(!scene.occluded(o, DVec3::new(1.0, 0.0, 0.0), 100.0));
    }

    #[test]
    fn sky_gradient_zenith_brighter_blue_than_ground() {
        let sky = Sky::default();
        let up = sky.radiance(DVec3::Z);
        let down = sky.radiance(DVec3::NEG_Z);
        assert!(up.z > down.z, "sky zenith bluer than ground");
        assert_eq!(down, sky.ground);
    }

    #[test]
    fn bounds_cover_geometry() {
        let scene = one_tri_scene();
        let bb = scene.bounds.expect("has bounds");
        assert_eq!(bb.min, DVec3::new(-1.0, -1.0, 0.0));
        assert_eq!(bb.max, DVec3::new(1.0, 1.0, 0.0));
    }
}
