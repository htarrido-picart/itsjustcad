// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! The exactness argument for the hardware any-hit path, and the `f32` data
//! it needs. Pure Rust, built on every platform so the bounds are tested on
//! Linux CI too; `shadow.metal` transcribes [`classify`] line for line.
//!
//! # What must hold
//!
//! The CPU answer for a ray is `TriBvh::ray_occluded(o, d)`: is there a
//! primitive `i` whose BVH leaf passes the `f64` slab test **and** whose
//! `f64` Möller–Trumbore test `ray_triangle(o, d, ..)` accepts? The GPU path
//! must return that boolean bit for bit. It does so in two halves.
//!
//! **`true` is taken from the GPU only when both halves are certain** for
//! one primitive `i`: [`classify`] (below) certifies that the `f64` triangle
//! test accepts `i`, and [`admits`] certifies that the `f64` slab test
//! `ray_aabb(o, 1/d, leaf box of i)` passes, which the traversal admits
//! leaves with (`Bvh::ray_reaches_prim`; the traversal's `f32` inner-node
//! pruning never drops a leaf that test admits, see `kernel_mesh::bvh`, and
//! `ray_hits_prim_characterises_ray_occluded` tests it). If only the triangle test is
//! certain, the CPU runs the leaf test; if neither, `ray_hits_prim` (leaf
//! test plus triangle test), and if that fails, the full CPU query.
//!
//! [`admits`]: the `f64` test is `lo_a = min(t1, t2)`, `hi_a = max(t1, t2)`
//! with `t = (b − o)·(1/d)` per axis, and passes iff `lo_a ≤ hi_b` for all
//! axes `a ≠ b` (`a = b` holds by min ≤ max) and `hi_b ≥ 0`. In `f32`, with
//! `b` and `o` (its high part only) rounded once, the difference once, `d` once and the division
//! within a few ulps, `|t₃₂ − t₆₄| ≤ 2⁻²¹·(|b| + |o| + |b − o|)/|d| +
//! 2⁻²⁰·|t| + 10⁻³⁰` (8× and 4× the first-order bounds, which also absorb the
//! three `f64` roundings). [`admits`] says yes only if the inequalities hold
//! for every value in those intervals, and refuses components with
//! `|d| < 2⁻³⁰` (where `1/d` would be huge or infinite and `0·∞` possible).
//! A flat box (`min = max` on an axis) works: there `t1 = t2` in `f64`, and
//! only the other axes' slabs are compared against it.
//!
//! **`false` is taken from the GPU only when every primitive is provably
//! rejected by the `f64` test.** Two lemmas cover the two kinds of primitive.
//!
//! *Primitives the GPU reports as candidates* get [`classify`], an `f32`
//! running-error evaluation of the same Möller–Trumbore quantities
//! `D = e1·(d×e2)`, `U = tvec·(d×e2)`, `V = d·(tvec×e1)`, `T = e2·(tvec×e1)`.
//! Each is a sum of degree-3 monomials. `d`, `e1`, `e2` are rounded once to
//! `f32` (relative error ≤ 2⁻²⁴·(1+2⁻²⁸), counting the `f64` edge). The
//! origin and `v0` (translated by `c` in `f64`) are each stored as an `f32`
//! pair `hi + lo`, and `tvec = (o − v0) + (o_lo − v0_lo)`: three roundings,
//! each relative to its own result, plus the pairs' own error (≤ 2⁻⁴⁸·|o|)
//! and the translation's (≤ 2⁻⁵³·|o|). So `|t̂vec − tvec| ≤ 3·2⁻²⁴·ta` with
//! `ta = |hi| + |lo| + |t̂vec| + 2⁻²²·(|o| + |v0|)`: the error scales with
//! the distance to the triangle, not with the coordinates. The cross and dot
//! products round at most 2 + 3 times, so with `ta` in place of `|tvec|`
//! `|X̂ − X| ≤ (3 + 7)·2⁻²⁴·(1+…)·X_abs` (Higham, Thm 3.1/3.5), where `X` is
//! the exact real value and `X_abs` the same polynomial over absolute
//! values. The `f64` test's own value obeys `|X₆₄ − X| ≤ γ₇(f64)·X_abs`.
//! [`classify`] uses `B = 2⁻²⁰·X̂_abs + 10⁻³⁴`: 2⁻²⁰ = 16·2⁻²⁴ ≥ 10·2⁻²⁴ +
//! γ₇(f64), plus the rounding of `X̂_abs` and `B` themselves and of the later `X̂ ± B`
//! (each a single rounding relative to its result); the absolute 10⁻³⁴
//! covers subnormal flushing (≤ 2⁻¹²⁶ per operation). So `X₆₄ ∈ [X̂ ± B]`.
//! The `f64` test then forms `u = U₆₄·(1/D₆₄)` (two more roundings) etc.;
//! [`classify`] bounds each quotient over the intervals and widens by 2⁻¹⁹
//! relative (≥ 32 `f32` ulps: any `f32` division Metal emits, plus the
//! `f64` roundings) and 10⁻²⁴ absolute. It answers *Reject* only when a
//! condition of the `f64` test (`|D| ≥ ε`, `u ∈ [−ε, 1+ε]`, `v ≥ −ε`,
//! `u+v ≤ 1+ε`, `t > ε`, ε = 10⁻⁹) is false for **every** value in the
//! intervals, with thresholds moved 0.1 % (or one `f32` ulp of 1) to the
//! safe side so `f32` constants cannot round across ε. Non-finite values,
//! or a `D` interval that reaches 0, are *Uncertain*. The `f64` code checks
//! the conditions in sequence, but its result is their conjunction, so the
//! order does not matter.
//!
//! *Primitives the GPU never reports* are handled by inflating each GPU box.
//! Lemma ([`reach`]): if `ray_triangle` accepts, the exact forward ray
//! `o + s·d`, `s ≥ 0`, passes within `R_i` of the exact triangle, for every
//! valid ray (origin in [`Frame::omega`], `|d|∞ ≤ 1`). Proof sketch: accepting
//! needs `|D₆₄| ≥ ε`, so `|D| ≥ ε − E_D` with `E_D = γ·D_abs`; then
//! `|u₆₄ − u| ≤ ((E_U + 1.01·E_D)/(ε − E_D) + 3.1·2⁻⁵³) / (1 − E_D/(ε − E_D))`
//! (same for `v`), so the ray meets the triangle's plane at barycentrics at
//! most `5ε + 3Δu + 3Δv` outside the triangle, i.e. within that times the
//! longest edge; if that meeting point is behind the origin (`t < 0 < t₆₄`),
//! it is at most `E_T/((ε − E_D)(1 − 4·2⁻⁵³))·|d|` behind it. The `X_abs`
//! here use the largest `|o − v0|` over Ω and `|d_k| ≤ 1`. If `E_D > ε/4`
//! the bound is not used: that primitive's box becomes all of Ω, so every
//! valid ray reports it as a candidate.
//! The GPU ray is `(ô, d̂)`, `f32` roundings of `(o − c, d)`. Along the
//! segment that matters (`|o + s·d − c| ≤ S + R_i`, `|o − c| ≤ S`), it stays
//! within `2⁻²³·(3S + R_i)` of the exact ray. Each box is the triangle's
//! box inflated by `R_i + 2⁻¹⁶·S` and rounded outward, so the `f32` ray
//! passes through it with a clearance of more than `2⁻¹⁷·S`, about 128
//! `f32` ulps of the scene's coordinates.
//!
//! **The one assumption** is the hardware's: Metal's acceleration structure
//! keeps every box we give it conservatively, and its box test reports a box
//! that the `f32` ray passes through with that clearance. Ray tracing hardware
//! has to be conservative to be watertight, and its slab tests are only a few
//! ulps off. The margin is two orders of magnitude larger. The tests in this
//! crate and `crate::metal` check it on adversarial rays.

use glam::DVec3;
use kernel_mesh::Aabb;
use rayon::prelude::*;

/// [`kernel_mesh::ray_triangle`]'s tolerance.
const EPS: f64 = kernel_mesh::RAY_TRIANGLE_EPS;
/// Relative error bound used for the `f32` quantities: 2⁻²⁰ (see module docs).
const K32: f32 = 1.0 / (1u32 << 20) as f32;
/// Absolute floor for `f32` quantities (subnormal flushing).
const ABS32: f32 = 1e-34;
/// Weight of `|o| + |v0|` in the `tvec` shadow (2⁻²²).
const TA_FAR: f32 = 1.0 / (1u32 << 22) as f32;
/// Quotient widening, relative and absolute.
const QREL: f32 = 1.0 / (1u32 << 19) as f32;
const QABS: f32 = 1e-24;
/// `u + v` widening (two more roundings).
const SREL: f32 = 1.0 / (1u32 << 22) as f32;
/// ε moved to the safe side for `f32` comparisons.
const EPS_LO: f32 = 0.999e-9;
const EPS_HI: f32 = 1.001e-9;
/// The smallest `f32` above 1 (> 1 + ε).
const ONE_UP: f32 = 1.000_000_1;

/// One triangle for the GPU: `v0 − c` as an unevaluated `f32` pair
/// `v0 + v0_lo`, and the `f64` edges rounded to nearest `f32`. Layout
/// matches `Tri` in `shadow.metal` (packed_float3 ×4).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TriF {
    pub v0: [f32; 3],
    pub v0_lo: [f32; 3],
    pub e1: [f32; 3],
    pub e2: [f32; 3],
}

/// `MTLAxisAlignedBoundingBox`: `min`, `max` as packed floats (24 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BoxF {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

/// One ray for the GPU, in the translated frame: origin `o + o_lo` (the
/// hardware traverses from `o`), direction `d`. Layout matches `Ray` in
/// `shadow.metal`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RayF {
    pub o: [f32; 3],
    pub o_lo: [f32; 3],
    pub d: [f32; 3],
}

/// The translated `f32` frame shared by the scene and its rays.
#[derive(Clone, Copy, Debug)]
pub struct Frame {
    /// Translation `c`: the centre of [`Frame::omega`].
    pub center: DVec3,
    /// Valid ray origins: the scene box padded by 1 + 10⁻³·extent. Rays
    /// starting elsewhere go to the CPU.
    pub omega: Aabb,
    /// `S`: the largest `|x − c|` over Ω.
    pub scale: f64,
}

impl Frame {
    /// `None` for an empty or non-finite scene.
    pub fn new(tris: &[[DVec3; 3]]) -> Option<Self> {
        let scene = Aabb::from_points(tris.iter().flatten().copied());
        if tris.is_empty() || !scene.min.is_finite() || !scene.max.is_finite() {
            return None;
        }
        let pad = 1.0 + 1e-3 * (scene.max - scene.min).max_element();
        let omega = Aabb { min: scene.min - pad, max: scene.max + pad };
        let center = (omega.min + omega.max) * 0.5;
        let scale = (omega.max - center).max(center - omega.min).max_element();
        // Keep every `f32` product finite: S³ (and `X_abs`) well below f32::MAX.
        (scale.is_finite() && scale < 1e9).then_some(Self { center, omega, scale })
    }

    /// The ray in `f32`, or `None` when it is outside what the bounds cover
    /// (origin outside Ω, `|d|∞ > 1`, `|d|∞ < 2⁻¹⁰`, non-finite): the CPU
    /// answers those.
    pub fn ray(&self, o: DVec3, d: DVec3) -> Option<RayF> {
        let inside = o.cmpge(self.omega.min).all() && o.cmple(self.omega.max).all();
        let m = d.abs().max_element();
        if !inside || !(1.0 / 1024.0..=1.0).contains(&m) {
            return None;
        }
        let (o, o_lo) = split(o - self.center);
        Some(RayF { o, o_lo, d: f32s(d) })
    }
}

fn f32s(v: DVec3) -> [f32; 3] {
    [v.x as f32, v.y as f32, v.z as f32]
}

/// `v` as `hi + lo`, both `f32` rounded to nearest.
fn split(v: DVec3) -> ([f32; 3], [f32; 3]) {
    let hi = f32s(v);
    let lo = v - DVec3::new(hi[0].into(), hi[1].into(), hi[2].into());
    (hi, f32s(lo))
}

fn f32_down(x: f64) -> f32 {
    let f = x as f32;
    if f64::from(f) > x { f.next_down() } else { f }
}

fn f32_up(x: f64) -> f32 {
    let f = x as f32;
    if f64::from(f) < x { f.next_up() } else { f }
}

/// `(a.y·b.z + a.z·b.y, …)`: the absolute-value shadow of `a × b`.
fn cross_abs(a: DVec3, b: DVec3) -> DVec3 {
    DVec3::new(a.y * b.z + a.z * b.y, a.z * b.x + a.x * b.z, a.x * b.y + a.y * b.x)
}

/// The lemma's `R_i`: if `ray_triangle` accepts a valid ray (with its
/// direction among `dirs`, when given), that ray passes within this
/// distance of the triangle. `∞` when the bound does not apply.
#[allow(clippy::neg_cmp_op_on_partial_ord)] // NaN must take the `∞` branch
pub fn reach(tri: &[DVec3; 3], frame: &Frame, dirs: Option<&[DVec3]>) -> f64 {
    const U64: f64 = 1.0 / (1u64 << 53) as f64;
    // γ₆ for the f64 test (e, cross ×2, dot ×3), with slack for computing
    // this bound in f64.
    const G: f64 = 8.0 * U64;
    let [v0, v1, v2] = *tri;
    let (e1, e2) = (v1 - v0, v2 - v0);
    let (a1, a2) = (e1.abs(), e2.abs());
    let t = (frame.omega.max - v0).abs().max((frame.omega.min - v0).abs());
    let pa = cross_abs(DVec3::ONE, a2);
    let qa = cross_abs(t, a1);
    let ed = G * a1.dot(pa);
    let (eu, ev, et) = (G * t.dot(pa), G * qa.element_sum(), G * a2.dot(qa));
    // Smallest exact |D| = |d·(E1×E2)| over the directions, bounded below:
    // the f64 triple product errs by at most 2·ed. Accepting needs
    // |D₆₄| ≥ ε, and |D₆₄| ≥ |D| − ed, so L bounds |D₆₄| from below.
    let n = e1.cross(e2);
    let l = match dirs {
        Some(ds) => EPS.max(ds.iter().map(|d| d.dot(n).abs()).fold(f64::INFINITY, f64::min) - 3.0 * ed),
        None => EPS,
    };
    if !(ed <= l / 3.0) || !(eu + ev + et).is_finite() {
        return f64::INFINITY;
    }
    let k = 1.0 / (1.0 - ed / l);
    let du = ((eu + 1.01 * ed) / l + 3.1 * U64) * k;
    let dv = ((ev + 1.01 * ed) / l + 3.1 * U64) * k;
    let back = et / (l * (1.0 - 3.0 * U64) - ed);
    let edge = a1.length().max(a2.length());
    ((5.0 * EPS + 3.0 * du + 3.0 * dv) * edge + back * 3f64.sqrt()) * 1.001
}

/// GPU triangles and their inflated boxes (see the module docs), in
/// triangle order.
pub fn pack(tris: &[[DVec3; 3]], frame: &Frame, dirs: Option<&[DVec3]>) -> (Vec<TriF>, Vec<BoxF>) {
    let c = frame.center;
    let margin = frame.scale / f64::from(1u32 << 16);
    let all = BoxF {
        min: (frame.omega.min - c - margin).to_array().map(f32_down),
        max: (frame.omega.max - c + margin).to_array().map(f32_up),
    };
    tris.par_iter()
        .map(|t| {
            let [v0, v1, v2] = *t;
            let (h, l) = split(v0 - c);
            let tri = TriF { v0: h, v0_lo: l, e1: f32s(v1 - v0), e2: f32s(v2 - v0) };
            let r = reach(t, frame, dirs);
            let b = Aabb::from_points(t.iter().copied());
            let grow = r + margin;
            let bx = if grow.is_finite() && grow < frame.scale {
                BoxF {
                    min: (b.min - c - grow).to_array().map(f32_down),
                    max: (b.max - c + grow).to_array().map(f32_up),
                }
            } else {
                all
            };
            (tri, bx)
        })
        .unzip()
}

/// Every primitive's leaf box (`Bvh::prim_leaf_bounds`) minus `c`, rounded
/// to nearest `f32`, in triangle order: the boxes [`admits`] tests.
pub fn pack_leaves(leaves: &[Aabb], frame: &Frame) -> Vec<BoxF> {
    let c = frame.center;
    leaves
        .par_iter()
        .map(|b| BoxF { min: f32s(b.min - c), max: f32s(b.max - c) })
        .collect()
}

/// Smallest `|d|` component [`admits`] handles (2⁻³⁰).
const ADMIT_MIN_D: f32 = 1.0 / (1u32 << 30) as f32;
const ADMIT_K1: f32 = 1.0 / (1u32 << 21) as f32;
const ADMIT_K2: f32 = 1.0 / (1u32 << 20) as f32;
const ADMIT_ABS: f32 = 1e-30;

/// Is the `f64` slab test of the ray against the leaf box `b` certainly
/// passed? `false` means "cannot tell". `shadow.metal`'s `admits` is a
/// transcription.
#[allow(clippy::neg_cmp_op_on_partial_ord)] // NaN must answer "cannot tell"
pub fn admits(r: &RayF, b: &BoxF) -> bool {
    let (mut lo, mut hi) = ([0f32; 3], [0f32; 3]);
    for a in 0..3 {
        let (o, d) = (r.o[a], r.d[a]);
        if !(d.abs() >= ADMIT_MIN_D) {
            return false;
        }
        let t = |x: f32| {
            let diff = x - o;
            let t = diff / d;
            let e = ADMIT_K1 * (x.abs() + o.abs() + diff.abs()) / d.abs() + ADMIT_K2 * t.abs() + ADMIT_ABS;
            (t, e)
        };
        let ((t1, e1), (t2, e2)) = (t(b.min[a]), t(b.max[a]));
        // Upper bound on the f64 min, lower bound on the f64 max.
        lo[a] = (t1 + e1).min(t2 + e2);
        hi[a] = (t1 - e1).max(t2 - e2);
    }
    let ok = |x: f32| x.is_finite();
    if !lo.iter().chain(&hi).all(|&x| ok(x)) {
        return false;
    }
    (0..3).all(|b| {
        hi[b] >= 0.0 && (0..3).all(|a| a == b || lo[a] <= hi[b])
    })
}

/// What the `f64` test does with a (ray, triangle) pair, as far as `f32`
/// can tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// `ray_triangle` certainly rejects.
    Reject,
    /// `ray_triangle` certainly accepts.
    Accept,
    /// Cannot tell: the CPU decides.
    Uncertain,
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
fn abs3(a: [f32; 3]) -> [f32; 3] {
    a.map(f32::abs)
}
fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}
fn cabs(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[1] * b[2] + a[2] * b[1], a[2] * b[0] + a[0] * b[2], a[0] * b[1] + a[1] * b[0]]
}

/// Interval of `n / |D|` (`n` already sign-corrected) with `n ∈ [x ± b]`
/// and `|D| ∈ [lo, hi]`, `0 < lo`, widened for the roundings.
fn quot(x: f32, b: f32, lo: f32, hi: f32) -> (f32, f32) {
    let (nlo, nhi) = (x - b, x + b);
    let qlo = if nlo >= 0.0 { nlo / hi } else { nlo / lo };
    let qhi = if nhi >= 0.0 { nhi / lo } else { nhi / hi };
    (qlo - (qlo.abs() * QREL + QABS), qhi + (qhi.abs() * QREL + QABS))
}

/// The `f32` classifier. `shadow.metal`'s `classify` is a transcription.
pub fn classify(r: &RayF, t: &TriF) -> Class {
    let (o, d) = (r.o, r.d);
    let pv = cross(d, t.e2);
    let det = dot(t.e1, pv);
    // tvec = (o − v0) + (o_lo − v0_lo): the high parts nearly cancel for
    // nearby points, so its error is relative to |tvec|, not to |o|.
    let th = sub(o, t.v0);
    let tl = sub(r.o_lo, t.v0_lo);
    let tv = add(th, tl);
    let u = dot(tv, pv);
    let qv = cross(tv, t.e1);
    let v = dot(d, qv);
    let tt = dot(t.e2, qv);

    let (ad, a1, a2) = (abs3(d), abs3(t.e1), abs3(t.e2));
    // Shadow of tvec: |hi| + |lo| + |tv| bounds its three roundings; the
    // last term covers the lo parts' own rounding (≤ 2⁻⁴⁸·|o|) and the
    // `f64` translation (≤ 2⁻⁵³·|o|), scaled to the 3·2⁻²⁴ budget.
    let far = add(abs3(o), abs3(t.v0)).map(|x| x * TA_FAR);
    let ta = add(add(add(abs3(th), abs3(tl)), abs3(tv)), far);
    let pa = cabs(ad, a2);
    let qa = cabs(ta, a1);
    let bd = K32 * dot(a1, pa) + ABS32;
    let bu = K32 * dot(ta, pa) + ABS32;
    let bv = K32 * dot(ad, qa) + ABS32;
    let bt = K32 * dot(a2, qa) + ABS32;
    if ![det, u, v, tt, bd, bu, bv, bt].iter().all(|x| x.is_finite()) {
        return Class::Uncertain;
    }
    let mag = det.abs();
    let (dlo, dhi) = (mag - bd, mag + bd);
    if dhi < EPS_LO {
        return Class::Reject;
    }
    if dlo <= 0.0 {
        return Class::Uncertain;
    }
    let s = if det > 0.0 { 1.0 } else { -1.0 };
    let lo = dlo.max(EPS_LO);
    let (ulo, uhi) = quot(s * u, bu, lo, dhi);
    let (vlo, vhi) = quot(s * v, bv, lo, dhi);
    let (tlo, thi) = quot(s * tt, bt, lo, dhi);
    let (slo, shi) = (ulo + vlo, uhi + vhi);
    let (slo, shi) = (slo - (slo.abs() * SREL + QABS), shi + (shi.abs() * SREL + QABS));
    if uhi < -EPS_HI || ulo > ONE_UP || vhi < -EPS_HI || slo > ONE_UP || thi < EPS_LO {
        return Class::Reject;
    }
    if dlo >= EPS_HI
        && ulo >= -EPS_LO
        && uhi <= 1.0
        && vlo >= -EPS_LO
        && shi <= 1.0
        && tlo > EPS_HI
    {
        return Class::Accept;
    }
    Class::Uncertain
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use kernel_mesh::ray_triangle;

    pub(crate) struct Rng(pub u64);
    impl Rng {
        pub fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        pub fn f(&mut self, lo: f64, hi: f64) -> f64 {
            lo + (self.next() >> 11) as f64 / (1u64 << 53) as f64 * (hi - lo)
        }
        pub fn v(&mut self, lo: f64, hi: f64) -> DVec3 {
            DVec3::new(self.f(lo, hi), self.f(lo, hi), self.f(lo, hi))
        }
        pub fn dir(&mut self) -> DVec3 {
            let d = self.v(-1.0, 1.0);
            d / d.abs().max_element().max(1e-3)
        }
    }

    /// Slab test of the exact ray against an `f32` box in the translated
    /// frame, with the box shrunk by the clearance the lemma promises: the
    /// stand-in for "the hardware reports this box".
    fn box_admits(r: &RayF, b: &BoxF, clearance: f64) -> bool {
        let (mut lo, mut hi) = (0.0f64, f64::INFINITY);
        for a in 0..3 {
            let (o, d) = (f64::from(r.o[a]), f64::from(r.d[a]));
            let (bmin, bmax) = (f64::from(b.min[a]) + clearance, f64::from(b.max[a]) - clearance);
            if d == 0.0 {
                if o < bmin || o > bmax {
                    return false;
                }
                continue;
            }
            let (t1, t2) = ((bmin - o) / d, (bmax - o) / d);
            lo = lo.max(t1.min(t2));
            hi = hi.min(t1.max(t2));
        }
        lo <= hi
    }

    /// Checks the two lemmas for one (scene, ray): `classify` never
    /// contradicts `ray_triangle`, and every triangle `ray_triangle` accepts
    /// has a box the `f32` ray passes through with clearance. Returns counts
    /// of (accepts, uncertain).
    pub(crate) fn check(tris: &[[DVec3; 3]], frame: &Frame, packed: &(Vec<TriF>, Vec<BoxF>), o: DVec3, d: DVec3) -> (u32, u32) {
        let Some(r) = frame.ray(o, d) else { return (0, 0) };
        let clearance = frame.scale / f64::from(1u32 << 17);
        let (mut acc, mut unc) = (0, 0);
        for (i, t) in tris.iter().enumerate() {
            let exact = ray_triangle(o, d, t[0], t[1], t[2]).is_some();
            match classify(&r, &packed.0[i]) {
                Class::Accept => assert!(exact, "false accept tri {i} o={o} d={d}"),
                Class::Reject => assert!(!exact, "false reject tri {i} o={o} d={d}"),
                Class::Uncertain => unc += 1,
            }
            if exact {
                acc += 1;
                assert!(box_admits(&r, &packed.1[i], clearance), "box misses accepted tri {i} o={o} d={d}");
            }
        }
        (acc, unc)
    }

    fn scene(rng: &mut Rng, n: usize, at: DVec3, span: f64, size: f64) -> Vec<[DVec3; 3]> {
        (0..n)
            .map(|_| {
                let b = at + rng.v(-span, span);
                [b, b + rng.v(-size, size), b + rng.v(-size, size)]
            })
            .collect()
    }

    #[test]
    fn random_rays_never_misclassified() {
        let mut rng = Rng(0x317);
        let tris = scene(&mut rng, 300, DVec3::ZERO, 20.0, 4.0);
        let frame = Frame::new(&tris).unwrap();
        let packed = pack(&tris, &frame, None);
        let (mut acc, mut unc) = (0, 0);
        for _ in 0..3000 {
            let (a, u) = check(&tris, &frame, &packed, rng.v(-20.0, 20.0), rng.dir());
            acc += a;
            unc += u;
        }
        assert!(acc > 500, "too few hits to mean anything: {acc}");
        assert!(unc < acc / 10, "uncertain {unc} vs accepts {acc}");
    }

    /// Rays aimed at shared edges and vertices of a fan, at grazing angles,
    /// and along the plane of coplanar triangles.
    #[test]
    fn shared_edges_grazing_and_coplanar() {
        let mut rng = Rng(0x261);
        let mut tris = Vec::new();
        let c = DVec3::new(3.0, -2.0, 1.0);
        let ring: Vec<DVec3> = (0..8)
            .map(|k| c + 5.0 * DVec3::new((k as f64 * 0.785).cos(), (k as f64 * 0.785).sin(), 0.0))
            .collect();
        for k in 0..8 {
            tris.push([c, ring[k], ring[(k + 1) % 8]]);
        }
        // A coplanar overlapping pair, and a vertical wall.
        tris.push([c, c + DVec3::X * 3.0, c + DVec3::Y * 3.0]);
        tris.push([c + DVec3::new(0.0, 0.0, 0.0), c + DVec3::new(0.0, 6.0, 0.0), c + DVec3::new(0.0, 0.0, 6.0)]);
        let frame = Frame::new(&tris).unwrap();
        let packed = pack(&tris, &frame, None);
        for k in 0..20000 {
            // Target: a point on an edge or vertex, nudged by a tiny amount.
            let tri = tris[k % tris.len()];
            let w = rng.f(0.0, 1.0);
            let p = match k % 3 {
                0 => tri[0] + (tri[1] - tri[0]) * w,
                1 => tri[1] + (tri[2] - tri[1]) * w,
                _ => tri[(k / 3) % 3],
            } + rng.v(-1.0, 1.0) * [0.0, 1e-12, 1e-9, 1e-6][(k / 7) % 4];
            let d = match k % 5 {
                // Grazing: almost in the plane z = c.z.
                0 => DVec3::new(rng.f(-1.0, 1.0), 1.0, rng.f(-1.0, 1.0) * 1e-9),
                // Exactly in the plane.
                1 => DVec3::new(rng.f(-1.0, 1.0), 1.0, 0.0),
                _ => rng.dir(),
            };
            let s = rng.f(0.0, 3.0);
            check(&tris, &frame, &packed, p - d * s, d);
            // Origin on or a hair off the surface, rays leaving it.
            check(&tris, &frame, &packed, p + DVec3::Z * [0.0, 1e-12, 1e-9][k % 3], d);
        }
    }

    /// Far from the origin (UTM-like coordinates) and very small or very
    /// large triangles.
    #[test]
    fn huge_and_tiny_coordinates() {
        let mut rng = Rng(0x5eed);
        for &(at, span, size) in &[
            (DVec3::new(5.0e5, 4.2e6, 30.0), 50.0, 2.0),
            (DVec3::new(5.0e5, 4.2e6, 30.0), 2000.0, 300.0),
            (DVec3::ZERO, 1e-3, 1e-4),
            (DVec3::new(-1e7, 1e7, 0.0), 1.0, 1e-2),
            (DVec3::ZERO, 1e4, 1e4),
        ] {
            let tris = scene(&mut rng, 150, at, span, size);
            let frame = Frame::new(&tris).unwrap();
            let packed = pack(&tris, &frame, None);
            for k in 0..1500 {
                let target = tris[k % tris.len()][0] + rng.v(-size, size);
                let d = rng.dir();
                check(&tris, &frame, &packed, target - d * rng.f(0.0, span), d);
            }
        }
    }

    /// Hits right at the `t > ε` boundary: origins on, just in front of and
    /// just behind a triangle.
    #[test]
    fn t_boundary() {
        let tris = vec![[DVec3::ZERO, DVec3::X, DVec3::Y], [DVec3::new(0.0, 0.0, 2.0), DVec3::new(1.0, 0.0, 2.0), DVec3::new(0.0, 1.0, 2.0)]];
        let frame = Frame::new(&tris).unwrap();
        let packed = pack(&tris, &frame, None);
        for &z in &[0.0, 1e-9, -1e-9, 1.0001e-9, 0.9999e-9, 2e-9, -2e-9, 1e-7, -1e-7, 1e-15, -1e-15, 2.0 - 1e-9] {
            for &d in &[DVec3::Z, DVec3::NEG_Z, DVec3::new(0.1, 0.1, 1.0), DVec3::new(0.0, 0.0, 0.5)] {
                check(&tris, &frame, &packed, DVec3::new(0.25, 0.25, -z), d);
                check(&tris, &frame, &packed, DVec3::new(0.25, 0.25, z), d);
            }
        }
    }

    /// The degenerate-triangle, zero-direction-component and slivers cases.
    #[test]
    fn degenerate_inputs() {
        let tris = vec![
            [DVec3::ZERO, DVec3::X, DVec3::X * 2.0],
            [DVec3::ZERO, DVec3::new(100.0, 0.0, 0.0), DVec3::new(100.0, 1e-7, 0.0)],
            [DVec3::ONE, DVec3::ONE, DVec3::ONE],
        ];
        let frame = Frame::new(&tris).unwrap();
        let packed = pack(&tris, &frame, None);
        let mut rng = Rng(9);
        for _ in 0..5000 {
            let o = rng.v(-2.0, 102.0) * DVec3::new(1.0, 0.01, 0.01);
            let d = [DVec3::Z, DVec3::NEG_Z, DVec3::Y, rng.dir()][rng.next() as usize % 4];
            check(&tris, &frame, &packed, o - d, d);
        }
    }

    /// The direction-set bound: big triangles, rays drawn from a fixed pool
    /// that includes directions almost in the triangles' planes.
    #[test]
    fn direction_pool_bound() {
        let mut rng = Rng(0x288);
        let mut tris = scene(&mut rng, 200, DVec3::new(300.0, -200.0, 10.0), 500.0, 80.0);
        tris.push([DVec3::ZERO, DVec3::new(1000.0, 0.0, 0.0), DVec3::new(0.0, 1000.0, 0.0)]);
        let mut pool: Vec<DVec3> = (0..40).map(|_| rng.dir()).collect();
        for t in tris.iter().take(30) {
            // In-plane directions (and nearly so) for some triangles.
            let n = (t[1] - t[0]).cross(t[2] - t[0]).normalize();
            let e = (t[1] - t[0]).normalize();
            for &tilt in &[0.0, 1e-12, 1e-9, 1e-6] {
                let d = e + n * tilt;
                pool.push(d / d.abs().max_element());
            }
        }
        let frame = Frame::new(&tris).unwrap();
        let packed = pack(&tris, &frame, Some(&pool));
        for k in 0..6000 {
            let d = pool[rng.next() as usize % pool.len()];
            let t = tris[k % tris.len()];
            let target = t[0] + (t[1] - t[0]) * rng.f(0.0, 1.0) * 0.5 + (t[2] - t[0]) * rng.f(0.0, 0.5);
            check(&tris, &frame, &packed, target - d * rng.f(0.0, 300.0) + rng.v(-1.0, 1.0) * 1e-6, d);
        }
    }

    /// The exact `f64` leaf test of `kernel_mesh` (`ray_aabb` with
    /// `dir.recip()`), replicated.
    fn ray_aabb(o: DVec3, d: DVec3, b: Aabb) -> bool {
        let inv = d.recip();
        let t1 = (b.min - o) * inv;
        let t2 = (b.max - o) * inv;
        let t_min = t1.min(t2).max_element();
        let t_max = t1.max(t2).min_element();
        t_max >= t_min.max(0.0)
    }

    /// `admits` never says yes when the `f64` slab test fails: random,
    /// flat and grazing boxes, rays near their faces and edges.
    #[test]
    fn admits_is_sound() {
        let mut rng = Rng(0xad);
        let tris = vec![[DVec3::splat(-60.0), DVec3::splat(60.0), DVec3::new(60.0, -60.0, 0.0)]];
        let frame = Frame::new(&tris).unwrap();
        let (mut yes, mut pass) = (0, 0);
        for k in 0..200_000 {
            let c = rng.v(-50.0, 50.0);
            let mut h = rng.v(0.0, 5.0);
            if k % 3 == 0 {
                h[k % 3] = 0.0; // flat box
            }
            let b = Aabb { min: c - h, max: c + h };
            let target = c + rng.v(-1.0, 1.0) * h * [1.0, 1.0 + 1e-9, 1.0 - 1e-9, 1.2][k % 4];
            let d = match k % 5 {
                0 => DVec3::new(rng.f(-1.0, 1.0), 1.0, [0.0, 1e-12, 1e-6][k % 3]),
                _ => rng.dir(),
            };
            let o = target - d * rng.f(-2.0, 20.0);
            let Some(r) = frame.ray(o, d) else { continue };
            let bf = pack_leaves(&[b], &frame)[0];
            let exact = ray_aabb(o, d, b);
            pass += u32::from(exact);
            if admits(&r, &bf) {
                yes += 1;
                assert!(exact, "admits but f64 rejects: o={o} d={d} b={b:?}");
            }
        }
        assert!(yes > pass / 2, "admits too rarely: {yes} of {pass}");
    }

    #[test]
    fn out_of_frame_rays_go_to_cpu() {
        let tris = vec![[DVec3::ZERO, DVec3::X, DVec3::Y]];
        let frame = Frame::new(&tris).unwrap();
        assert!(frame.ray(DVec3::splat(100.0), DVec3::Z).is_none());
        assert!(frame.ray(DVec3::ZERO, DVec3::Z * 2.0).is_none());
        assert!(frame.ray(DVec3::ZERO, DVec3::ZERO).is_none());
        assert!(frame.ray(DVec3::ZERO, DVec3::splat(f64::NAN)).is_none());
        assert!(frame.ray(DVec3::ZERO, DVec3::Z).is_some());
    }
}
