// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! [`ShadowCaster`]: batched any-hit queries on the GPU, resolved to the
//! exact CPU answer.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

use glam::DVec3;
use kernel_mesh::TriBvh;
use rayon::prelude::*;

use crate::margin::{self, Frame, RayF};
use crate::metal::{self, Accel, Buffer, Gpu, Job, Pipeline, Shared};
use crate::Occluded;

const SHADER: &str = include_str!("shadow.metal");

/// Items recorded per GPU batch. Big enough to amortise a dispatch, small
/// enough that one batch's rays (`f64` + `f32` + result, 88 bytes each) stay
/// around 50 MB for `radiation`.
const ITEMS_PER_BATCH: usize = 8192;

/// The shadow pipeline, compiled once per process.
fn pipeline(gpu: &Gpu) -> Option<&'static Shared<Pipeline>> {
    static PIPE: OnceLock<Option<Shared<Pipeline>>> = OnceLock::new();
    PIPE.get_or_init(|| {
        let t = Instant::now();
        match gpu.pipeline(SHADER, "shadow_any_hit") {
            Ok(p) => {
                if crate::stats_enabled() {
                    eprintln!("hwrt: shader compile {:.1} ms", t.elapsed().as_secs_f64() * 1e3);
                }
                Some(Shared(p))
            }
            Err(e) => {
                eprintln!("hwrt: shader compile failed, using CPU: {e}");
                None
            }
        }
    })
    .as_ref()
}

/// Per-[`ShadowCaster::map`] tallies, printed with `ITSJUSTCAD_HWRT_STATS`.
#[derive(Default)]
struct Tally {
    rays: AtomicU64,
    /// Not for the GPU (outside the frame or direction set, or a replay
    /// that diverged from its recording): CPU only.
    cpu_only: AtomicU64,
    /// Certain on the GPU (triangle and leaf test): no CPU work.
    certain: AtomicU64,
    /// GPU-certain triangle, leaf test run on the CPU.
    confirmed: AtomicU64,
    /// GPU uncertain, but its candidate confirmed by one exact CPU test.
    uncertain_confirmed: AtomicU64,
    /// Full CPU re-runs (uncertain, or an accept the CPU did not confirm).
    rechecked: AtomicU64,
    recheck_ns: AtomicU64,
}

/// Any-hit shadow rays on the GPU for one `TriBvh`.
pub struct ShadowCaster {
    gpu: &'static Gpu,
    pipe: &'static Shared<Pipeline>,
    accel: Shared<Accel>,
    tris: Shared<Buffer>,
    leaves: Shared<Buffer>,
    frame: Frame,
    tri_count: usize,
    /// The allowed ray directions, when restricted.
    dirs: Option<DirSet>,
}

impl ShadowCaster {
    /// Build the GPU scene for `bvh`'s triangles, or `None` when hardware
    /// ray tracing is unavailable, forced off, or fails to initialise.
    /// `dirs`, when given, lists every ray direction the batches will use
    /// (others go to the CPU); it lets [`margin::reach`] bound each box by
    /// the triangle's real conditioning, which keeps boxes tight.
    pub fn new(bvh: &TriBvh, dirs: Option<&[DVec3]>) -> Option<Self> {
        if crate::forced_cpu() || bvh.is_empty() {
            return None;
        }
        let gpu = Gpu::get()?;
        let pipe = pipeline(gpu)?;
        let t0 = Instant::now();
        let frame = Frame::new(bvh.triangles())?;
        let (tris, boxes) = margin::pack(bvh.triangles(), &frame, dirs);
        let leaves: Vec<kernel_mesh::Aabb> =
            (0..tris.len() as u32).into_par_iter().map(|i| bvh.prim_leaf_bounds(i)).collect();
        let leaves = margin::pack_leaves(&leaves, &frame);
        let t1 = Instant::now();
        let leaf_buf = gpu.buffer(&leaves)?;
        let tri_buf = gpu.buffer(&tris)?;
        let box_buf = gpu.buffer(&boxes)?;
        let accel = match gpu.build_box_accel(&box_buf, boxes.len()) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("hwrt: acceleration structure build failed, using CPU: {e}");
                return None;
            }
        };
        if crate::stats_enabled() {
            eprintln!(
                "hwrt: {} tris; pack {:.1} ms, AS build {:.1} ms",
                tris.len(),
                (t1 - t0).as_secs_f64() * 1e3,
                t1.elapsed().as_secs_f64() * 1e3
            );
        }
        let dirs = dirs.map(DirSet::new);
        Some(Self {
            gpu,
            pipe,
            accel: Shared(accel),
            tris: Shared(tri_buf),
            leaves: Shared(leaf_buf),
            frame,
            tri_count: tris.len(),
            dirs,
        })
    }

    /// `f(item, occluded)` for every item, in parallel, where `occluded(o, d)`
    /// is `bvh.ray_occluded(o, d)` bit for bit. `bvh` must be the tree this
    /// caster was built from.
    ///
    /// Each batch runs `f` three times: counting its queries, recording them
    /// (both answering `false`), then — after one GPU dispatch for the whole
    /// batch — replaying the answers. A replayed query that differs from the recorded
    /// one (bitwise) is answered by the CPU, so `f` need not be oblivious to
    /// the answers for the output to be exact. The GPU runs batch `k` while
    /// the CPU records batch `k + 1` and replays batch `k − 1`.
    pub fn map<I: Sync, T: Send>(
        &self,
        bvh: &TriBvh,
        items: &[I],
        f: impl Fn(&I, &mut Occluded<'_>) -> T + Sync,
    ) -> Vec<T> {
        assert_eq!(bvh.triangle_count(), self.tri_count, "ShadowCaster used with another TriBvh");
        let tally = Tally::default();
        let t0 = Instant::now();
        let (mut gpu_s, mut wait_s) = (0.0, 0.0);
        let mut out = Vec::with_capacity(items.len());
        let mut pending: Option<Batch<'_, I>> = None;
        for chunk in items.chunks(ITEMS_PER_BATCH) {
            let next = self.record(chunk, &f);
            if let Some(p) = pending.take() {
                out.extend(self.finish(bvh, p, &f, &tally, &mut gpu_s, &mut wait_s));
            }
            pending = Some(next);
        }
        if let Some(p) = pending.take() {
            out.extend(self.finish(bvh, p, &f, &tally, &mut gpu_s, &mut wait_s));
        }
        if crate::stats_enabled() {
            let g = |a: &AtomicU64| a.load(Relaxed);
            let rays = g(&tally.rays).max(1) as f64;
            let rerun = g(&tally.rechecked) + g(&tally.cpu_only);
            eprintln!(
                "hwrt: {} rays in {:.1} ms; GPU {:.1} ms (CPU waited {:.1} ms); \
                 {} certain on GPU, {} leaf-confirmed, {} uncertain-confirmed; \
                 CPU re-run {} ({:.4}%: {} uncertain, {} not on GPU), \
                 re-run CPU time {:.1} ms",
                g(&tally.rays),
                t0.elapsed().as_secs_f64() * 1e3,
                gpu_s * 1e3,
                wait_s * 1e3,
                g(&tally.certain),
                g(&tally.confirmed),
                g(&tally.uncertain_confirmed),
                rerun,
                rerun as f64 / rays * 100.0,
                g(&tally.rechecked),
                g(&tally.cpu_only),
                g(&tally.recheck_ns) as f64 / 1e6,
            );
        }
        out
    }

    /// The ray for the GPU, or `None` when the margins do not cover it.
    fn gpu_ray(&self, o: DVec3, d: DVec3) -> Option<RayF> {
        if let Some(ds) = &self.dirs
            && !ds.contains(d)
        {
            return None;
        }
        self.frame.ray(o, d)
    }

    /// Run `f` over `chunk` twice: counting its queries, then recording
    /// them (the `f64` rays, and their `f32` GPU rays straight into the GPU
    /// buffer, [`NOT_GPU`] for rays the GPU must not answer); then dispatch.
    fn record<'a, I: Sync, T: Send>(
        &self,
        chunk: &'a [I],
        f: &(impl Fn(&I, &mut Occluded<'_>) -> T + Sync),
    ) -> Batch<'a, I> {
        let counts: Vec<usize> = chunk
            .par_iter()
            .map(|it| {
                let mut c = 0;
                f(it, &mut |_, _| {
                    c += 1;
                    false
                });
                c
            })
            .collect();
        let mut offs = Vec::with_capacity(chunk.len() + 1);
        offs.push(0);
        for c in &counts {
            offs.push(offs.last().unwrap() + c);
        }
        let n = *offs.last().unwrap();
        let mut rays = vec![[DVec3::ZERO; 2]; n];
        let gpu = match self.alloc(n) {
            Ok((ray_buf, out_buf)) => {
                // SAFETY: a fresh shared buffer of `n` records, not yet
                // visible to the GPU.
                let dst = unsafe { metal::contents::<RayF>(&ray_buf, n) };
                self.fill(chunk, f, &counts, &mut rays, Some(dst));
                let n32 = u32::try_from(n).unwrap_or(u32::MAX);
                match self.gpu.dispatch(&self.pipe.0, &self.accel.0, &[&self.tris.0, &ray_buf, &out_buf, &self.leaves.0], n32) {
                    Ok(job) if n32 as usize == n => Some(GpuBatch { job, _rays: Shared(ray_buf), out: Shared(out_buf) }),
                    Ok(_) => None,
                    Err(e) => {
                        eprintln!("hwrt: dispatch failed, batch on CPU: {e}");
                        None
                    }
                }
            }
            Err(e) => {
                eprintln!("hwrt: allocation failed, batch on CPU: {e}");
                self.fill(chunk, f, &counts, &mut rays, None);
                None
            }
        };
        Batch { items: chunk, rays, offs, gpu }
    }

    fn alloc(&self, n: usize) -> Result<(Buffer, Buffer), String> {
        let rays = self.gpu.buffer_len(n * std::mem::size_of::<RayF>()).ok_or("ray buffer")?;
        let out = self.gpu.buffer_len(n * 4).ok_or("result buffer")?;
        Ok((rays, out))
    }

    /// The recording pass: item `k`'s queries go to its slice of `rays` (and
    /// of `gpu`).
    fn fill<I: Sync, T: Send>(
        &self,
        chunk: &[I],
        f: &(impl Fn(&I, &mut Occluded<'_>) -> T + Sync),
        counts: &[usize],
        rays: &mut [[DVec3; 2]],
        gpu: Option<&mut [RayF]>,
    ) {
        let mut parts = Vec::with_capacity(chunk.len());
        let (mut r, mut g) = (rays, gpu);
        for &c in counts {
            let (a, rest) = std::mem::take(&mut r).split_at_mut(c);
            r = rest;
            let b = match g.take() {
                Some(gg) => {
                    let (b, rest) = gg.split_at_mut(c);
                    g = Some(rest);
                    Some(b)
                }
                None => None,
            };
            parts.push((a, b));
        }
        chunk.par_iter().zip(parts).for_each(|(it, (rs, mut gs))| {
            let mut j = 0;
            f(it, &mut |o, d| {
                // A query count that changed between passes leaves the
                // rest of this item to the CPU (its slots keep zero rays,
                // which the replay will not match).
                if j < rs.len() {
                    rs[j] = [o, d];
                    if let Some(gs) = gs.as_deref_mut() {
                        gs[j] = self.gpu_ray(o, d).unwrap_or(NOT_GPU);
                    }
                    j += 1;
                }
                false
            });
        });
    }

    /// Wait for the batch, then replay `f` with each query resolved:
    /// GPU miss → `false`; GPU candidate → one exact CPU test, else the full
    /// CPU query. A query that differs (bitwise) from the recorded one, or
    /// that the GPU must not answer, goes to the CPU.
    fn finish<I: Sync, T: Send>(
        &self,
        bvh: &TriBvh,
        batch: Batch<'_, I>,
        f: &(impl Fn(&I, &mut Occluded<'_>) -> T + Sync),
        tally: &Tally,
        gpu_s: &mut f64,
        wait_s: &mut f64,
    ) -> Vec<T> {
        let n = batch.rays.len();
        let codes: Option<&[i32]> = batch.gpu.as_ref().and_then(|g| {
            let t = Instant::now();
            let r = g.job.wait();
            *wait_s += t.elapsed().as_secs_f64();
            match r {
                Ok(s) => {
                    *gpu_s += s;
                    // SAFETY: the GPU finished writing `n` results.
                    Some(&*unsafe { metal::contents::<i32>(&g.out.0, n) })
                }
                Err(e) => {
                    eprintln!("hwrt: GPU batch failed, batch on CPU: {e}");
                    None
                }
            }
        });
        tally.rays.fetch_add(n as u64, Relaxed);
        let cpu = |o: DVec3, d: DVec3| {
            tally.cpu_only.fetch_add(1, Relaxed);
            bvh.ray_occluded(o, d)
        };
        let rerun = |o: DVec3, d: DVec3| {
            tally.rechecked.fetch_add(1, Relaxed);
            let t = Instant::now();
            let r = bvh.ray_occluded(o, d);
            tally.recheck_ns.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            r
        };
        let resolve = |o: DVec3, d: DVec3, code: i32| match code {
            -1 => false,
            CODE_TRUE => {
                tally.certain.fetch_add(1, Relaxed);
                true
            }
            CODE_CPU => cpu(o, d),
            // `classify` certified the f64 triangle test; the leaf test
            // decides whether the traversal reaches it.
            c if c >= 0 && bvh.ray_reaches_prim(o, d, c as u32) => {
                tally.confirmed.fetch_add(1, Relaxed);
                true
            }
            c if c <= -2 && bvh.ray_hits_prim(o, d, (-2 - c) as u32) => {
                tally.uncertain_confirmed.fetch_add(1, Relaxed);
                true
            }
            _ => rerun(o, d),
        };
        let (rays, offs) = (&batch.rays, &batch.offs);
        let out = batch
            .items
            .par_iter()
            .enumerate()
            .map(|(k, it)| {
                let Some(codes) = codes else {
                    return f(it, &mut |o, d| cpu(o, d));
                };
                let (rs, cs) = (&rays[offs[k]..offs[k + 1]], &codes[offs[k]..offs[k + 1]]);
                let mut j = 0;
                f(it, &mut |o, d| match rs.get(j) {
                    Some(r) if same(r[0], o) && same(r[1], d) => {
                        // Same f64 ray ⇒ same GPU ray (`gpu_ray` is a
                        // function of the ray), so the code is for it.
                        j += 1;
                        resolve(o, d, cs[j - 1])
                    }
                    _ => {
                        // Diverged from the recording: CPU from here on.
                        j = usize::MAX;
                        cpu(o, d)
                    }
                })
            })
            .collect();
        drop(batch.gpu);
        out
    }
}

/// Marker GPU ray (non-finite direction) for queries the CPU answers; the
/// kernel returns [`CODE_CPU`] for it.
const NOT_GPU: RayF = RayF { o: [0.0; 3], o_lo: [0.0; 3], d: [f32::NAN; 3] };
/// Kernel result codes (see `shadow.metal`).
const CODE_CPU: i32 = i32::MIN;
const CODE_TRUE: i32 = i32::MIN + 1;

fn bits(v: DVec3) -> [u64; 3] {
    v.to_array().map(f64::to_bits)
}

/// Bitwise equality (distinguishes ±0, which `ray_occluded` can tell apart).
fn same(a: DVec3, b: DVec3) -> bool {
    a.x.to_bits() == b.x.to_bits() && a.y.to_bits() == b.y.to_bits() && a.z.to_bits() == b.z.to_bits()
}

/// A small open-addressing set of directions, keyed by their bits.
struct DirSet {
    slots: Vec<Option<[u64; 3]>>,
    shift: u32,
}

impl DirSet {
    fn new(dirs: &[DVec3]) -> Self {
        let size = (dirs.len() * 4).next_power_of_two().max(8);
        let mut s = Self { slots: vec![None; size], shift: 64 - size.trailing_zeros() };
        for d in dirs {
            let b = bits(*d);
            let mut i = s.slot(b);
            while let Some(x) = s.slots[i] {
                if x == b {
                    break;
                }
                i = (i + 1) & (size - 1);
            }
            s.slots[i] = Some(b);
        }
        s
    }

    fn slot(&self, b: [u64; 3]) -> usize {
        let h = b[0] ^ b[1].rotate_left(21) ^ b[2].rotate_left(42);
        (h.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> self.shift) as usize
    }

    fn contains(&self, d: DVec3) -> bool {
        let b = bits(d);
        let mut i = self.slot(b);
        while let Some(x) = self.slots[i] {
            if x == b {
                return true;
            }
            i = (i + 1) & (self.slots.len() - 1);
        }
        false
    }
}

struct Batch<'a, I> {
    items: &'a [I],
    rays: Vec<[DVec3; 2]>,
    offs: Vec<usize>,
    gpu: Option<GpuBatch>,
}

struct GpuBatch {
    job: Job,
    _rays: Shared<Buffer>,
    out: Shared<Buffer>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::margin::tests::Rng;
    use kernel_mesh::ray_triangle;

    impl ShadowCaster {
        fn raw_codes(&self, rays: &[[DVec3; 2]]) -> Vec<i32> {
            let n = rays.len();
            let (rb, ob) = self.alloc(n).expect("alloc");
            // SAFETY: fresh buffer of `n` records.
            let dst = unsafe { metal::contents::<RayF>(&rb, n) };
            for (g, r) in dst.iter_mut().zip(rays) {
                *g = self.gpu_ray(r[0], r[1]).unwrap_or(NOT_GPU);
            }
            let job = self.gpu.dispatch(&self.pipe.0, &self.accel.0, &[&self.tris.0, &rb, &ob, &self.leaves.0], n as u32).expect("dispatch");
            let g = GpuBatch { job, _rays: Shared(rb), out: Shared(ob) };
            g.job.wait().expect("gpu");
            // SAFETY: the GPU finished writing `rays.len()` results.
            unsafe { metal::contents::<i32>(&g.out.0, rays.len()) }.to_vec()
        }
    }

    /// A scene with shared edges (a grid of quads), a coplanar overlap,
    /// walls and scattered triangles.
    fn scene(rng: &mut Rng) -> Vec<[DVec3; 3]> {
        let mut tris = Vec::new();
        for i in 0..20 {
            for j in 0..20 {
                let p = |a: usize, b: usize| DVec3::new(a as f64, b as f64, 0.0);
                tris.push([p(i, j), p(i + 1, j), p(i + 1, j + 1)]);
                tris.push([p(i, j), p(i + 1, j + 1), p(i, j + 1)]);
            }
        }
        tris.push([DVec3::new(2.0, 2.0, 0.0), DVec3::new(6.0, 2.0, 0.0), DVec3::new(2.0, 6.0, 0.0)]);
        for k in 0..10 {
            let x = 2.0 * k as f64;
            tris.push([DVec3::new(x, 5.0, 0.0), DVec3::new(x + 1.5, 5.0, 0.0), DVec3::new(x, 5.0, 3.0)]);
        }
        for _ in 0..400 {
            let b = rng.v(0.0, 20.0) + DVec3::Z * rng.f(0.0, 5.0);
            tris.push([b, b + rng.v(-1.0, 1.0), b + rng.v(-1.0, 1.0)]);
        }
        tris
    }

    fn rays(rng: &mut Rng, tris: &[[DVec3; 3]], n: usize) -> Vec<[DVec3; 2]> {
        (0..n)
            .map(|k| {
                let t = tris[rng.next() as usize % tris.len()];
                let w = rng.f(0.0, 1.0);
                let target = match k % 4 {
                    0 => t[0] + (t[1] - t[0]) * w,
                    1 => t[(k / 4) % 3],
                    _ => t[0] + (t[1] - t[0]) * w * 0.5 + (t[2] - t[0]) * rng.f(0.0, 0.5),
                } + rng.v(-1.0, 1.0) * [0.0, 1e-12, 1e-8, 1e-5][(k / 5) % 4];
                let d = match k % 6 {
                    0 => DVec3::new(rng.f(-1.0, 1.0), 1.0, rng.f(-1.0, 1.0) * 1e-9),
                    1 => DVec3::new(rng.f(-1.0, 1.0), 1.0, 0.0),
                    2 => DVec3::Z,
                    _ => rng.dir(),
                };
                let o = match k % 3 {
                    0 => target - d * rng.f(0.0, 4.0),
                    1 => target + DVec3::Z * [0.0, 1e-3, 1e-9][(k / 3) % 3],
                    _ => target,
                };
                [o, d]
            })
            .collect()
    }

    /// Raw GPU codes never contradict the exact `f64` test: an accepted
    /// candidate is accepted by `ray_triangle`, and a miss is a CPU miss.
    #[test]
    fn gpu_codes_are_sound() {
        let mut rng = Rng(0x317_0001);
        let tris = scene(&mut rng);
        let bvh = TriBvh::build(tris.clone());
        let Some(hw) = ShadowCaster::new(&bvh, None) else {
            assert!(Gpu::get().is_none() || crate::forced_cpu(), "GPU present but caster failed");
            eprintln!("no hardware ray tracing here: skipped");
            return;
        };
        let rs = rays(&mut rng, &tris, 200_000);
        let codes = hw.raw_codes(&rs);
        let (mut miss, mut acc, mut unc) = (0, 0, 0);
        for (&[o, d], &c) in rs.iter().zip(&codes) {
            if hw.gpu_ray(o, d).is_none() {
                continue;
            }
            match c {
                -1 => {
                    miss += 1;
                    assert!(!bvh.ray_occluded(o, d), "GPU miss but CPU hit o={o} d={d}");
                }
                CODE_TRUE => {
                    acc += 1;
                    assert!(bvh.ray_occluded(o, d), "GPU certain but CPU miss o={o} d={d}");
                }
                c if c >= 0 => {
                    acc += 1;
                    let [a, b, cc] = tris[c as usize];
                    assert!(ray_triangle(o, d, a, b, cc).is_some(), "GPU accept but f64 rejects o={o} d={d}");
                }
                _ => unc += 1,
            }
        }
        eprintln!("gpu codes: {miss} miss, {acc} accept, {unc} uncertain");
        assert!(miss > 1000 && acc > 1000);
    }

    /// `map` output equals the CPU path bit for bit, including items whose
    /// queries depend on earlier answers (replay divergence).
    #[test]
    fn map_matches_cpu() {
        let mut rng = Rng(0x317_0002);
        let tris = scene(&mut rng);
        let bvh = TriBvh::build(tris.clone());
        let Some(hw) = ShadowCaster::new(&bvh, None) else {
            assert!(Gpu::get().is_none() || crate::forced_cpu(), "GPU present but caster failed");
            eprintln!("no hardware ray tracing here: skipped");
            return;
        };
        let rs = rays(&mut rng, &tris, 60_000);
        let items: Vec<&[[DVec3; 2]]> = rs.chunks(7).collect();
        let f = |it: &&[[DVec3; 2]], occ: &mut Occluded<'_>| -> Vec<bool> {
            let mut out = Vec::new();
            for &[o, d] in it.iter() {
                let hit = occ(o, d);
                out.push(hit);
                if hit {
                    // Answer-dependent follow-up query.
                    out.push(occ(o + DVec3::Z, -d));
                }
            }
            out
        };
        let cpu: Vec<Vec<bool>> = items.iter().map(|it| f(it, &mut |o, d| bvh.ray_occluded(o, d))).collect();
        assert_eq!(hw.map(&bvh, &items, f), cpu);
        // Restricted directions: every other ray's direction is outside the
        // set and must come from the CPU.
        let dirs: Vec<DVec3> = rs.iter().step_by(2).map(|r| r[1]).collect();
        let hw = ShadowCaster::new(&bvh, Some(&dirs)).expect("caster");
        assert_eq!(hw.map(&bvh, &items, f), cpu);
    }
}
