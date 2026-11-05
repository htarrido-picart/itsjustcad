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

/// One item's [`Tally`], summed without atomics (one shared counter per
/// ray made the score phase contend on a cache line).
#[derive(Default)]
struct Counts {
    cpu_only: u64,
    certain: u64,
    confirmed: u64,
    uncertain_confirmed: u64,
    rechecked: u64,
    recheck_ns: u64,
}

impl Tally {
    fn add(&self, c: &Counts) {
        for (a, v) in [
            (&self.cpu_only, c.cpu_only),
            (&self.certain, c.certain),
            (&self.confirmed, c.confirmed),
            (&self.uncertain_confirmed, c.uncertain_confirmed),
            (&self.rechecked, c.rechecked),
            (&self.recheck_ns, c.recheck_ns),
        ] {
            if v != 0 {
                a.fetch_add(v, Relaxed);
            }
        }
    }
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
    /// [`ShadowCaster::map_fans`] runs `f` once when the queries are known
    /// up front.
    pub fn map<I: Sync, T: Send>(
        &self,
        bvh: &TriBvh,
        items: &[I],
        f: impl Fn(&I, &mut Occluded<'_>) -> T + Sync,
    ) -> Vec<T> {
        assert_eq!(bvh.triangle_count(), self.tri_count, "ShadowCaster used with another TriBvh");
        self.pipeline(items, |chunk| self.record(chunk, &f), |b, tally, clock| self.finish(bvh, b, &f, tally, clock))
    }

    /// [`ShadowCaster::map`] for items whose queries are a fan: one origin
    /// and directions from the set given to [`ShadowCaster::new`].
    /// `plan(item, fan)` returns the origin and pushes, in query order, the
    /// index into that set of each direction `f` will query. The plan goes
    /// straight to the GPU, and `f` runs once, with the answers (#323).
    ///
    /// The same exactness guard as `map`: a query of `f` that differs
    /// (bitwise) from the planned one, and every later query of that item,
    /// is answered by the CPU, so a wrong plan costs time, never bits.
    /// Without a direction set this is `map`.
    pub fn map_fans<I: Sync, T: Send>(
        &self,
        bvh: &TriBvh,
        items: &[I],
        plan: impl Fn(&I, &mut Vec<u32>) -> DVec3 + Sync,
        f: impl Fn(&I, &mut Occluded<'_>) -> T + Sync,
    ) -> Vec<T> {
        let Some(ds) = &self.dirs else {
            return self.map(bvh, items, f);
        };
        assert_eq!(bvh.triangle_count(), self.tri_count, "ShadowCaster used with another TriBvh");
        self.pipeline(
            items,
            |chunk| self.record_fans(chunk, ds, &plan),
            |b, tally, clock| self.finish_fans(bvh, b, ds, &f, tally, clock),
        )
    }

    /// Record batch `k + 1`, then finish batch `k` while the GPU traces
    /// `k + 1`; print the tallies when asked.
    fn pipeline<'a, I, B, T>(
        &self,
        items: &'a [I],
        record: impl Fn(&'a [I]) -> B,
        finish: impl Fn(B, &Tally, &mut Clock) -> Vec<T>,
    ) -> Vec<T> {
        let tally = Tally::default();
        let mut clock = Clock::default();
        let t0 = Instant::now();
        let mut out = Vec::with_capacity(items.len());
        let mut pending: Option<B> = None;
        let finish = |b: B, clock: &mut Clock, out: &mut Vec<T>| {
            let t = Instant::now();
            out.extend(finish(b, &tally, clock));
            clock.score += t.elapsed().as_secs_f64();
        };
        for chunk in items.chunks(ITEMS_PER_BATCH) {
            let t = Instant::now();
            let next = record(chunk);
            clock.record += t.elapsed().as_secs_f64();
            if let Some(p) = pending.take() {
                finish(p, &mut clock, &mut out);
            }
            pending = Some(next);
        }
        if let Some(p) = pending.take() {
            finish(p, &mut clock, &mut out);
        }
        if crate::stats_enabled() {
            let g = |a: &AtomicU64| a.load(Relaxed);
            let rays = g(&tally.rays).max(1) as f64;
            let rerun = g(&tally.rechecked) + g(&tally.cpu_only);
            eprintln!(
                "hwrt: {} rays in {:.1} ms (record {:.1} ms, score {:.1} ms); \
                 GPU {:.1} ms (CPU waited {:.1} ms); \
                 {} certain on GPU, {} leaf-confirmed, {} uncertain-confirmed; \
                 CPU re-run {} ({:.4}%: {} uncertain, {} not on GPU), \
                 re-run CPU time {:.1} ms",
                g(&tally.rays),
                t0.elapsed().as_secs_f64() * 1e3,
                clock.record * 1e3,
                (clock.score - clock.wait) * 1e3,
                clock.gpu * 1e3,
                clock.wait * 1e3,
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
        let offs = prefix(counts.iter().copied());
        let n = *offs.last().unwrap();
        let mut rays = vec![[DVec3::ZERO; 2]; n];
        let gpu = match self.alloc(n) {
            Ok((ray_buf, out_buf)) => {
                // SAFETY: a fresh shared buffer of `n` records, not yet
                // visible to the GPU.
                let dst = unsafe { metal::contents::<RayF>(&ray_buf, n) };
                self.fill(chunk, f, &counts, &mut rays, Some(dst));
                self.launch(ray_buf, out_buf, n)
            }
            Err(e) => {
                eprintln!("hwrt: allocation failed, batch on CPU: {e}");
                self.fill(chunk, f, &counts, &mut rays, None);
                None
            }
        };
        Batch { items: chunk, rays, offs, gpu }
    }

    /// Plan `chunk`'s fans and write their GPU rays straight into the GPU
    /// buffer; then dispatch.
    fn record_fans<'a, I: Sync>(
        &self,
        chunk: &'a [I],
        ds: &DirSet,
        plan: &(impl Fn(&I, &mut Vec<u32>) -> DVec3 + Sync),
    ) -> FanBatch<'a, I> {
        let groups: Vec<FanGroup> = chunk
            .par_chunks(FAN_GROUP)
            .map(|items| {
                let mut g = FanGroup {
                    origins: Vec::with_capacity(items.len()),
                    ends: Vec::with_capacity(items.len()),
                    dirs: Vec::new(),
                };
                let mut fan = Vec::new();
                for it in items {
                    fan.clear();
                    g.origins.push(plan(it, &mut fan));
                    g.dirs.extend_from_slice(&fan);
                    g.ends.push(g.dirs.len());
                }
                g
            })
            .collect();
        let offs = prefix(groups.iter().map(|g| g.dirs.len()));
        let n = *offs.last().unwrap();
        let gpu = match self.alloc(n) {
            Ok((ray_buf, out_buf)) => {
                // SAFETY: a fresh shared buffer of `n` records, not yet
                // visible to the GPU.
                let mut rest = unsafe { metal::contents::<RayF>(&ray_buf, n) };
                let mut parts = Vec::with_capacity(groups.len());
                for g in &groups {
                    let (a, r) = std::mem::take(&mut rest).split_at_mut(g.dirs.len());
                    rest = r;
                    parts.push(a);
                }
                groups.par_iter().zip(parts).for_each(|(g, gs)| {
                    let mut start = 0;
                    for (o, &end) in g.origins.iter().zip(&g.ends) {
                        // `Frame::ray` in halves: the same bits as `gpu_ray`.
                        let og = self.frame.origin(*o);
                        for (r, &k) in gs[start..end].iter_mut().zip(&g.dirs[start..end]) {
                            *r = match (og, ds.gpu.get(k as usize)) {
                                (Some([o, o_lo]), Some(&Some(d))) => RayF { o, o_lo, d },
                                _ => NOT_GPU,
                            };
                        }
                        start = end;
                    }
                });
                self.launch(ray_buf, out_buf, n)
            }
            Err(e) => {
                eprintln!("hwrt: allocation failed, batch on CPU: {e}");
                None
            }
        };
        FanBatch { items: chunk, groups, offs, gpu }
    }

    fn alloc(&self, n: usize) -> Result<(Buffer, Buffer), String> {
        let rays = self.gpu.buffer_len(n * std::mem::size_of::<RayF>()).ok_or("ray buffer")?;
        let out = self.gpu.buffer_len(n * 4).ok_or("result buffer")?;
        Ok((rays, out))
    }

    /// Dispatch `n` recorded rays, or `None` (batch on the CPU).
    fn launch(&self, ray_buf: Buffer, out_buf: Buffer, n: usize) -> Option<GpuBatch> {
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

    /// Wait for the batch, then replay `f` with each query resolved by
    /// [`resolve`]. A query that differs (bitwise) from the recorded one
    /// goes to the CPU.
    fn finish<I: Sync, T: Send>(
        &self,
        bvh: &TriBvh,
        batch: Batch<'_, I>,
        f: &(impl Fn(&I, &mut Occluded<'_>) -> T + Sync),
        tally: &Tally,
        clock: &mut Clock,
    ) -> Vec<T> {
        let n = batch.rays.len();
        let codes = codes(&batch.gpu, n, clock);
        tally.rays.fetch_add(n as u64, Relaxed);
        let (rays, offs) = (&batch.rays, &batch.offs);
        batch
            .items
            .par_iter()
            .enumerate()
            .map(|(k, it)| {
                let mut c = Counts::default();
                let Some(codes) = codes else {
                    let r = f(it, &mut |o, d| cpu(bvh, &mut c, o, d));
                    tally.add(&c);
                    return r;
                };
                let (rs, cs) = (&rays[offs[k]..offs[k + 1]], &codes[offs[k]..offs[k + 1]]);
                let mut j = 0;
                let r = f(it, &mut |o, d| match rs.get(j) {
                    Some(r) if same(r[0], o) && same(r[1], d) => {
                        // Same f64 ray ⇒ same GPU ray (`gpu_ray` is a
                        // function of the ray), so the code is for it.
                        j += 1;
                        resolve(bvh, &mut c, o, d, cs[j - 1])
                    }
                    _ => {
                        // Diverged from the recording: CPU from here on.
                        j = usize::MAX;
                        cpu(bvh, &mut c, o, d)
                    }
                });
                tally.add(&c);
                r
            })
            .collect()
    }

    /// Wait for the batch, then run `f` once per item with each query
    /// resolved by [`resolve`]. A query that is not the planned one goes to
    /// the CPU, as do all later queries of that item.
    fn finish_fans<I: Sync, T: Send>(
        &self,
        bvh: &TriBvh,
        batch: FanBatch<'_, I>,
        ds: &DirSet,
        f: &(impl Fn(&I, &mut Occluded<'_>) -> T + Sync),
        tally: &Tally,
        clock: &mut Clock,
    ) -> Vec<T> {
        let n = *batch.offs.last().unwrap();
        let codes = codes(&batch.gpu, n, clock);
        tally.rays.fetch_add(n as u64, Relaxed);
        let groups = batch.items.par_chunks(FAN_GROUP).zip(&batch.groups).enumerate();
        let out: Vec<Vec<T>> = groups
            .map(|(k, (items, g))| {
                let mut c = Counts::default();
                let mut start = 0;
                let out = items
                    .iter()
                    .enumerate()
                    .map(|(i, it)| {
                        let Some(codes) = codes else {
                            return f(it, &mut |o, d| cpu(bvh, &mut c, o, d));
                        };
                        let (origin, end) = (g.origins[i], g.ends[i]);
                        let (fan, cs) = (&g.dirs[start..end], &codes[batch.offs[k] + start..batch.offs[k] + end]);
                        start = end;
                        let mut j = 0;
                        f(it, &mut |o, d| match fan.get(j).and_then(|&x| ds.list.get(x as usize)) {
                            Some(&pd) if same(origin, o) && same(pd, d) => {
                                // The planned ray, so the code is for it.
                                j += 1;
                                resolve(bvh, &mut c, o, d, cs[j - 1])
                            }
                            _ => {
                                j = usize::MAX;
                                cpu(bvh, &mut c, o, d)
                            }
                        })
                    })
                    .collect();
                tally.add(&c);
                out
            })
            .collect();
        out.into_iter().flatten().collect()
    }
}

/// Wait for `gpu` and return its `n` result codes, or `None` (no GPU batch,
/// or it failed): the CPU answers every query.
fn codes<'b>(gpu: &'b Option<GpuBatch>, n: usize, clock: &mut Clock) -> Option<&'b [i32]> {
    let g = gpu.as_ref()?;
    let t = Instant::now();
    let r = g.job.wait();
    clock.wait += t.elapsed().as_secs_f64();
    match r {
        Ok(s) => {
            clock.gpu += s;
            // SAFETY: the GPU finished writing `n` results.
            Some(&*unsafe { metal::contents::<i32>(&g.out.0, n) })
        }
        Err(e) => {
            eprintln!("hwrt: GPU batch failed, batch on CPU: {e}");
            None
        }
    }
}

/// A query the GPU did not answer.
fn cpu(bvh: &TriBvh, c: &mut Counts, o: DVec3, d: DVec3) -> bool {
    c.cpu_only += 1;
    bvh.ray_occluded(o, d)
}

/// The exact answer from a GPU code: miss → `false`; certain → `true`;
/// candidate → one exact CPU test, else the full CPU query.
fn resolve(bvh: &TriBvh, c: &mut Counts, o: DVec3, d: DVec3, code: i32) -> bool {
    match code {
        -1 => false,
        CODE_TRUE => {
            c.certain += 1;
            true
        }
        CODE_CPU => cpu(bvh, c, o, d),
        // `classify` certified the f64 triangle test; the leaf test
        // decides whether the traversal reaches it.
        k if k >= 0 && bvh.ray_reaches_prim(o, d, k as u32) => {
            c.confirmed += 1;
            true
        }
        k if k <= -2 && bvh.ray_hits_prim(o, d, (-2 - k) as u32) => {
            c.uncertain_confirmed += 1;
            true
        }
        _ => {
            c.rechecked += 1;
            let t = Instant::now();
            let r = bvh.ray_occluded(o, d);
            c.recheck_ns += t.elapsed().as_nanos() as u64;
            r
        }
    }
}

/// Offsets `[0, c₀, c₀ + c₁, …]`.
fn prefix(counts: impl ExactSizeIterator<Item = usize>) -> Vec<usize> {
    let mut offs = Vec::with_capacity(counts.len() + 1);
    offs.push(0);
    for c in counts {
        offs.push(offs.last().unwrap() + c);
    }
    offs
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
    /// The directions in the caller's order, and their `f32` halves
    /// ([`Frame::dir`]) for [`ShadowCaster::map_fans`].
    list: Vec<DVec3>,
    gpu: Vec<Option<[f32; 3]>>,
}

impl DirSet {
    fn new(dirs: &[DVec3]) -> Self {
        let size = (dirs.len() * 4).next_power_of_two().max(8);
        let mut s = Self {
            slots: vec![None; size],
            shift: 64 - size.trailing_zeros(),
            list: dirs.to_vec(),
            gpu: dirs.iter().map(|&d| Frame::dir(d)).collect(),
        };
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

/// Items planned per task in [`ShadowCaster::map_fans`]: one flat plan
/// per group, not one allocation per item.
const FAN_GROUP: usize = 256;

struct FanBatch<'a, I> {
    items: &'a [I],
    groups: Vec<FanGroup>,
    /// Each group's first ray in the batch.
    offs: Vec<usize>,
    gpu: Option<GpuBatch>,
}

/// The fans of [`FAN_GROUP`] consecutive items: item `i` has origin
/// `origins[i]` and directions `dirs[ends[i - 1]..ends[i]]`.
struct FanGroup {
    origins: Vec<DVec3>,
    ends: Vec<usize>,
    dirs: Vec<u32>,
}

/// Main-thread seconds per [`ShadowCaster::map`] phase.
#[derive(Default)]
struct Clock {
    record: f64,
    score: f64,
    gpu: f64,
    wait: f64,
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

    /// `map_fans` equals the CPU path bit for bit: right plans, plans that
    /// miss, reorder or add queries, answer-dependent queries, directions
    /// outside the GPU's range, and no direction set.
    #[test]
    fn map_fans_matches_cpu() {
        let mut rng = Rng(0x323_0001);
        let tris = scene(&mut rng);
        let bvh = TriBvh::build(tris.clone());
        let mut dirs: Vec<DVec3> = (0..40).map(|_| rng.dir()).collect();
        dirs.extend([DVec3::Z, DVec3::new(0.3, 1.0, 0.0), DVec3::new(1e-4, 0.0, 1e-4), DVec3::splat(2.0)]);
        let Some(hw) = ShadowCaster::new(&bvh, Some(&dirs)) else {
            assert!(Gpu::get().is_none() || crate::forced_cpu(), "GPU present but caster failed");
            eprintln!("no hardware ray tracing here: skipped");
            return;
        };
        // Item: origin, the planned fan, and how `f` departs from it.
        let items: Vec<(DVec3, Vec<u32>, u32)> = (0..30_000)
            .map(|k| {
                let o = match k % 50 {
                    0 => DVec3::splat(1e6), // outside the frame
                    _ => rng.v(0.0, 20.0) + DVec3::Z * rng.f(-0.5, 5.0),
                };
                let fan = (0..dirs.len() as u32).filter(|_| !rng.next().is_multiple_of(3)).collect();
                (o, fan, k % 7)
            })
            .collect();
        let plan = |it: &(DVec3, Vec<u32>, u32), fan: &mut Vec<u32>| {
            fan.extend(&it.1);
            match it.2 {
                1 => fan.truncate(fan.len() / 2),
                2 => fan.reverse(),
                3 => fan.push(u32::MAX),
                _ => {}
            }
            it.0
        };
        let f = |it: &(DVec3, Vec<u32>, u32), occ: &mut Occluded<'_>| -> Vec<bool> {
            let mut out = Vec::new();
            for &k in &it.1 {
                let hit = occ(it.0, dirs[k as usize]);
                out.push(hit);
                if hit && it.2 == 4 {
                    out.push(occ(it.0 + DVec3::Z, -dirs[k as usize]));
                }
            }
            if it.2 == 5 {
                out.push(occ(it.0, DVec3::X));
            }
            out
        };
        let cpu: Vec<Vec<bool>> = items.iter().map(|it| f(it, &mut |o, d| bvh.ray_occluded(o, d))).collect();
        assert!(cpu.iter().flatten().filter(|&&b| b).count() > 1000);
        assert_eq!(hw.map_fans(&bvh, &items, plan, f), cpu);
        let hw = ShadowCaster::new(&bvh, None).expect("caster");
        assert_eq!(hw.map_fans(&bvh, &items, plan, f), cpu);
    }
}
