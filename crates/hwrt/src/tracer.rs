// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! [`Tracer`]: batched f32 closest-hit and any-hit rays for the `raytrace`
//! renderer (#318). Unlike [`crate::ShadowCaster`] there is no CPU re-check:
//! the image is not logged state (#261 item 3), so the GPU answer is used
//! as is. The scene is re-centred on its bounding-box centre before the cast
//! to `f32`, so precision follows the scene size, not its coordinates.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use glam::DVec3;
use rayon::prelude::*;

use crate::metal::{self, Accel, Buffer, Gpu, Job, Pipeline, Shared};

const SHADER: &str = include_str!("trace.metal");

const MISS: u32 = u32::MAX;

#[repr(C)]
#[derive(Clone, Copy)]
struct RayF {
    o: [f32; 3],
    d: [f32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct HitF {
    prim: u32,
    t: f32,
}

struct Pipes {
    closest: Pipeline,
    any: Pipeline,
}

/// Both pipelines, compiled once per process.
fn pipes(gpu: &Gpu) -> Option<&'static Shared<Pipes>> {
    static PIPES: OnceLock<Option<Shared<Pipes>>> = OnceLock::new();
    PIPES
        .get_or_init(|| {
            let t = Instant::now();
            let built = gpu
                .pipeline(SHADER, "closest_hit")
                .and_then(|closest| Ok(Pipes { closest, any: gpu.pipeline(SHADER, "any_hit")? }));
            match built {
                Ok(p) => {
                    if crate::stats_enabled() {
                        eprintln!("hwrt: trace shader compile {:.1} ms", t.elapsed().as_secs_f64() * 1e3);
                    }
                    Some(Shared(p))
                }
                Err(e) => {
                    eprintln!("hwrt: trace shader compile failed, using CPU: {e}");
                    None
                }
            }
        })
        .as_ref()
}

/// Input/output buffers for `cap` rays, reused across batches.
struct Slot {
    rays: Buffer,
    out: Buffer,
    cap: usize,
}

/// Work committed by [`Tracer::submit`], answered by [`Tracer::finish`].
/// Callers keep several in flight to overlap CPU work with the GPU.
pub struct Batch {
    job: Job,
    closest: (Shared<Slot>, usize),
    any: (Shared<Slot>, usize),
}

/// Closest-hit and any-hit rays on the GPU over one triangle list.
pub struct Tracer {
    gpu: &'static Gpu,
    pipes: &'static Shared<Pipes>,
    accel: Shared<Accel>,
    /// Kept alive for the acceleration structure.
    _verts: Shared<Buffer>,
    center: DVec3,
    /// Idle buffers; a batch takes two and [`Tracer::finish`] returns them.
    slots: Mutex<Vec<Shared<Slot>>>,
    rays: AtomicU64,
    gpu_ns: AtomicU64,
    wait_ns: AtomicU64,
    wall_ns: AtomicU64,
}

impl Tracer {
    /// Build the GPU scene for `count` triangles (`tri(i)` gives triangle
    /// `i`'s vertices), or `None` when hardware ray tracing is unavailable,
    /// forced off, or fails to initialise. Primitive ids match `i`.
    pub fn new(count: usize, tri: impl Fn(usize) -> [DVec3; 3] + Sync) -> Option<Self> {
        if crate::forced_cpu() || count == 0 || u32::try_from(count).is_err() {
            return None;
        }
        let gpu = Gpu::get()?;
        let pipes = pipes(gpu)?;
        let t0 = Instant::now();
        let (lo, hi) = (0..count)
            .into_par_iter()
            .map(|i| {
                let [a, b, c] = tri(i);
                (a.min(b).min(c), a.max(b).max(c))
            })
            .reduce(|| (DVec3::INFINITY, DVec3::NEG_INFINITY), |x, y| (x.0.min(y.0), x.1.max(y.1)));
        let center = (lo + hi) * 0.5;
        let verts = gpu.buffer_len(count * 36)?;
        // SAFETY: `count * 9` f32s fit, and the GPU has not seen the buffer.
        let vs = unsafe { metal::contents::<[f32; 9]>(&verts, count) };
        vs.par_iter_mut().enumerate().for_each(|(i, v)| {
            let [a, b, c] = tri(i).map(|p| (p - center).as_vec3().to_array());
            *v = [a[0], a[1], a[2], b[0], b[1], b[2], c[0], c[1], c[2]];
        });
        let t1 = Instant::now();
        let accel = match gpu.build_triangle_accel(&verts, count) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("hwrt: acceleration structure build failed, using CPU: {e}");
                return None;
            }
        };
        if crate::stats_enabled() {
            eprintln!(
                "hwrt: {count} tris (triangle AS); pack {:.1} ms, AS build {:.1} ms",
                (t1 - t0).as_secs_f64() * 1e3,
                t1.elapsed().as_secs_f64() * 1e3
            );
        }
        Some(Self {
            gpu,
            pipes,
            accel: Shared(accel),
            _verts: Shared(verts),
            center,
            slots: Mutex::new(Vec::new()),
            rays: AtomicU64::new(0),
            gpu_ns: AtomicU64::new(0),
            wait_ns: AtomicU64::new(0),
            wall_ns: AtomicU64::new(0),
        })
    }

    /// Closest hit `(primitive, t)` with `t > 1e-9` for each `[origin, dir]`,
    /// in f32. On exactly equal `t` the lowest primitive wins, as on the CPU.
    /// `t` is in units of `dir`'s length. `out` has one slot per ray.
    pub fn closest(&self, rays: &[[DVec3; 2]], out: &mut [Option<(u32, f32)>]) {
        self.finish(self.submit(&[rays], &[]), out, &mut []);
    }

    /// Is anything hit at `t > 1e-9` along each `[origin, dir]`? `out` has
    /// one slot per ray.
    pub fn occluded(&self, rays: &[[DVec3; 2]], out: &mut [bool]) {
        self.finish(self.submit(&[], &[rays]), &mut [], out);
    }

    /// Commit [`Tracer::closest`] for `closest` and [`Tracer::occluded`] for
    /// `any` (each a list of pieces, answered flattened in order) as one GPU
    /// round trip, without waiting. Panics if Metal fails
    /// (out of memory or a lost device), which a renderer has no exact
    /// fallback for once some answers are in.
    pub fn submit(&self, closest: &[&[[DVec3; 2]]], any: &[&[[DVec3; 2]]]) -> Batch {
        let t0 = Instant::now();
        let (c, nc) = self.pack(closest);
        let (a, na) = self.pack(any);
        let p = &self.pipes.0;
        let job = self
            .gpu
            .dispatch_all(
                &self.accel.0,
                &[
                    (&p.closest, &[&c.0.rays, &c.0.out], nc as u32),
                    (&p.any, &[&a.0.rays, &a.0.out], na as u32),
                ],
            )
            .expect("hwrt: dispatch");
        self.wall_ns.fetch_add(t0.elapsed().as_nanos() as u64, Relaxed);
        self.rays.fetch_add((nc + na) as u64, Relaxed);
        Batch { job, closest: (c, nc), any: (a, na) }
    }

    /// Wait for `b` and write its answers: `hits` and `occ` have one slot per
    /// ray of the `closest` and `any` lists it was submitted with.
    pub fn finish(&self, b: Batch, hits: &mut [Option<(u32, f32)>], occ: &mut [bool]) {
        assert!(hits.len() == b.closest.1 && occ.len() == b.any.1, "one output slot per ray");
        let w = Instant::now();
        let gpu_s = b.job.wait().expect("hwrt: trace");
        let t1 = Instant::now();
        self.gpu_ns.fetch_add((gpu_s * 1e9) as u64, Relaxed);
        self.wait_ns.fetch_add((t1 - w).as_nanos() as u64, Relaxed);
        // SAFETY: the job that wrote these results has completed, and each
        // slot belongs to this batch alone.
        let res = unsafe { metal::contents::<HitF>(&b.closest.0.0.out, hits.len()) };
        hits.par_iter_mut().zip(res.par_iter()).for_each(|(o, h)| *o = (h.prim != MISS).then_some((h.prim, h.t)));
        // SAFETY: as above.
        let res = unsafe { metal::contents::<u8>(&b.any.0.0.out, occ.len()) };
        occ.par_iter_mut().zip(res.par_iter()).for_each(|(o, &r)| *o = r != 0);
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        slots.push(b.closest.0);
        slots.push(b.any.0);
        self.wall_ns.fetch_add(t1.elapsed().as_nanos() as u64, Relaxed);
    }

    /// An idle slot holding the `pieces`' rays, re-centred and cast to f32,
    /// and their count.
    fn pack(&self, pieces: &[&[[DVec3; 2]]]) -> (Shared<Slot>, usize) {
        let n: usize = pieces.iter().map(|p| p.len()).sum();
        let slot = {
            let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
            slots.iter().position(|s| s.0.cap >= n).map(|i| slots.swap_remove(i))
        };
        let slot = slot.unwrap_or_else(|| {
            let cap = n.next_power_of_two().max(1024);
            let rays_buf = self.gpu.buffer_len(cap * std::mem::size_of::<RayF>()).expect("hwrt: ray buffer");
            let out_buf = self.gpu.buffer_len(cap * std::mem::size_of::<HitF>()).expect("hwrt: hit buffer");
            Shared(Slot { rays: rays_buf, out: out_buf, cap })
        });
        let center = self.center;
        // SAFETY: the slot holds `cap >= n` rays and is idle: no batch owns
        // it, so no GPU work uses it.
        let mut buf = unsafe { metal::contents::<RayF>(&slot.0.rays, n) };
        let mut work = Vec::with_capacity(pieces.len());
        for p in pieces {
            let (head, tail) = std::mem::take(&mut buf).split_at_mut(p.len());
            work.push((head, *p));
            buf = tail;
        }
        work.into_par_iter().for_each(|(g, p)| {
            for (g, [o, d]) in g.iter_mut().zip(p) {
                *g = RayF { o: (*o - center).as_vec3().to_array(), d: d.as_vec3().to_array() };
            }
        });
        (slot, n)
    }

    /// One stderr line of totals since construction (`ITSJUSTCAD_HWRT_STATS`).
    pub fn report(&self) {
        if crate::stats_enabled() {
            let ms = |a: &AtomicU64| a.load(Relaxed) as f64 / 1e6;
            eprintln!(
                "hwrt: {} rays; submit/finish {:.1} ms on the calling thread (waiting {:.1} ms); GPU busy {:.1} ms",
                self.rays.load(Relaxed),
                ms(&self.wall_ns),
                ms(&self.wait_ns),
                ms(&self.gpu_ns),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernel_mesh::ray_triangle;

    fn tracer(tris: &[[DVec3; 3]]) -> Option<Tracer> {
        let t = Tracer::new(tris.len(), |i| tris[i]);
        if t.is_none() {
            assert!(Gpu::get().is_none() || crate::forced_cpu(), "GPU present but tracer failed");
        }
        t
    }

    /// Exactly coincident triangles: the lowest index wins, wherever the
    /// copies sit in the list and whichever the hardware reports first.
    #[test]
    fn closest_hit_ties_go_to_lowest_index() {
        let tri = |z: f64, s: f64| {
            [DVec3::new(-s, -s, z), DVec3::new(s, -s, z), DVec3::new(0.0, s, z)]
        };
        // Background triangles below, of varied sizes so the builder mixes
        // them with the copies.
        let mut tris: Vec<[DVec3; 3]> = (0..512).map(|k| tri(-1.0 - (k % 7) as f64, 0.5 + (k % 5) as f64)).collect();
        let copies = [500, 3, 257, 41, 129, 7, 400];
        for &k in &copies {
            tris[k] = tri(0.0, 1.0);
        }
        let Some(t) = tracer(&tris) else { return };
        let rays: Vec<[DVec3; 2]> = (0..400)
            .map(|k| [DVec3::new(-0.4 + 0.002 * k as f64, -0.3 + 0.001 * k as f64, 5.0), DVec3::NEG_Z])
            .collect();
        let mut hits = vec![None; rays.len()];
        t.closest(&rays, &mut hits);
        for h in hits {
            let (p, d) = h.expect("hit");
            assert_eq!(p, 3);
            assert!((d - 5.0).abs() < 1e-5, "t={d}");
        }
    }

    /// Random soup at UTM-like coordinates: the GPU's closest primitive has
    /// the CPU's closest `t` to f32 precision, and occlusion agrees except
    /// within f32 rounding of an edge.
    #[test]
    fn matches_cpu_on_random_soup() {
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        let mut f = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 11) as f64 / (1u64 << 53) as f64
        };
        let base = DVec3::new(500_000.0, 4_200_000.0, 30.0);
        let tris: Vec<[DVec3; 3]> = (0..2000)
            .map(|_| {
                let c = base + DVec3::new(f(), f(), f()) * 100.0;
                [c, c + DVec3::new(f(), f(), f()) * 8.0, c + DVec3::new(f(), f(), f()) * 8.0]
            })
            .collect();
        let Some(t) = tracer(&tris) else { return };
        let rays: Vec<[DVec3; 2]> = (0..20_000)
            .map(|_| {
                let o = base + DVec3::new(f(), f(), f()) * 100.0;
                [o, (DVec3::new(f(), f(), f()) - 0.5).normalize()]
            })
            .collect();
        let mut gpu = vec![None; rays.len()];
        t.closest(&rays, &mut gpu);
        let mut occ = vec![false; rays.len()];
        t.occluded(&rays, &mut occ);
        let mut bad = 0;
        for (k, [o, d]) in rays.iter().enumerate() {
            let cpu = tris
                .iter()
                .enumerate()
                .filter_map(|(i, v)| ray_triangle(*o, *d, v[0], v[1], v[2]).map(|t| (i as u32, t)))
                .min_by(|a, b| a.1.total_cmp(&b.1));
            match (cpu, gpu[k]) {
                (Some((_, ct)), Some((_, gt))) if (ct - gt as f64).abs() < 1e-3 => {}
                (None, None) => {}
                _ => bad += 1,
            }
            if occ[k] != cpu.is_some() {
                bad += 1;
            }
        }
        // Only rays within f32 rounding of an edge may disagree.
        assert!(bad <= rays.len() / 1000, "{bad} disagreements");
        t.report();
    }
}
