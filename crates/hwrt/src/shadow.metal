// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart
//
// Any-hit shadow rays over a bounding-box primitive acceleration structure.
// `classify` transcribes `margin::classify` (Rust) operation for operation;
// the exactness argument is in `margin.rs`. Compiled at runtime with fast
// math off.

#include <metal_stdlib>
#include <metal_raytracing>
using namespace metal;
using namespace raytracing;

struct Tri { packed_float3 v0; packed_float3 v0_lo; packed_float3 e1; packed_float3 e2; };
struct Ray { packed_float3 o; packed_float3 o_lo; packed_float3 d; };

constant float K32 = 1.0f / 1048576.0f;   // 2^-20
constant float ABS32 = 1e-34f;
constant float TA_FAR = 1.0f / 4194304.0f; // 2^-22
constant float QREL = 1.0f / 524288.0f;   // 2^-19
constant float QABS = 1e-24f;
constant float SREL = 1.0f / 4194304.0f;  // 2^-22
constant float EPS_LO = 0.999e-9f;
constant float EPS_HI = 1.001e-9f;
constant float ONE_UP = 1.00000012f;      // next float above 1

constant int REJECT = 0;
constant int ACCEPT = 1;
constant int UNCERTAIN = 2;

static float3 cross_abs(float3 a, float3 b) {
    return float3(a.y * b.z + a.z * b.y, a.z * b.x + a.x * b.z, a.x * b.y + a.y * b.x);
}

static float3 cross3(float3 a, float3 b) {
    return float3(a.y * b.z - a.z * b.y, a.z * b.x - a.x * b.z, a.x * b.y - a.y * b.x);
}

static float dot3(float3 a, float3 b) {
    return a.x * b.x + a.y * b.y + a.z * b.z;
}

static float2 quot(float x, float b, float lo, float hi) {
    float nlo = x - b, nhi = x + b;
    float qlo = nlo >= 0.0f ? nlo / hi : nlo / lo;
    float qhi = nhi >= 0.0f ? nhi / lo : nhi / hi;
    return float2(qlo - (fabs(qlo) * QREL + QABS), qhi + (fabs(qhi) * QREL + QABS));
}

static int classify(float3 o, float3 o_lo, float3 d, Tri t) {
    float3 v0 = float3(t.v0), e1 = float3(t.e1), e2 = float3(t.e2);
    float3 pv = cross3(d, e2);
    float det = dot3(e1, pv);
    float3 th = o - v0;
    float3 tl = o_lo - float3(t.v0_lo);
    float3 tv = th + tl;
    float u = dot3(tv, pv);
    float3 qv = cross3(tv, e1);
    float v = dot3(d, qv);
    float tt = dot3(e2, qv);

    float3 ad = fabs(d), a1 = fabs(e1), a2 = fabs(e2);
    float3 far = (fabs(o) + fabs(v0)) * TA_FAR;
    float3 ta = fabs(th) + fabs(tl) + fabs(tv) + far;
    float3 pa = cross_abs(ad, a2);
    float3 qa = cross_abs(ta, a1);
    float bd = K32 * dot3(a1, pa) + ABS32;
    float bu = K32 * dot3(ta, pa) + ABS32;
    float bv = K32 * dot3(ad, qa) + ABS32;
    float bt = K32 * dot3(a2, qa) + ABS32;
    if (!(isfinite(det) && isfinite(u) && isfinite(v) && isfinite(tt) &&
          isfinite(bd) && isfinite(bu) && isfinite(bv) && isfinite(bt))) {
        return UNCERTAIN;
    }
    float mag = fabs(det);
    float dlo = mag - bd, dhi = mag + bd;
    if (dhi < EPS_LO) return REJECT;
    if (dlo <= 0.0f) return UNCERTAIN;
    float s = det > 0.0f ? 1.0f : -1.0f;
    float lo = max(dlo, EPS_LO);
    float2 uq = quot(s * u, bu, lo, dhi);
    float2 vq = quot(s * v, bv, lo, dhi);
    float2 tq = quot(s * tt, bt, lo, dhi);
    float slo = uq.x + vq.x, shi = uq.y + vq.y;
    slo = slo - (fabs(slo) * SREL + QABS);
    shi = shi + (fabs(shi) * SREL + QABS);
    if (uq.y < -EPS_HI || uq.x > ONE_UP || vq.y < -EPS_HI || slo > ONE_UP || tq.y < EPS_LO) {
        return REJECT;
    }
    if (dlo >= EPS_HI && uq.x >= -EPS_LO && uq.y <= 1.0f && vq.x >= -EPS_LO && shi <= 1.0f &&
        tq.x > EPS_HI) {
        return ACCEPT;
    }
    return UNCERTAIN;
}

struct Box { packed_float3 lo; packed_float3 hi; };

constant float ADMIT_MIN_D = 1.0f / 1073741824.0f; // 2^-30
constant float ADMIT_K1 = 1.0f / 2097152.0f;        // 2^-21
constant float ADMIT_K2 = 1.0f / 1048576.0f;        // 2^-20
constant float ADMIT_ABS = 1e-30f;

static float2 slab_t(float x, float o, float d) {
    float diff = x - o;
    float t = diff / d;
    float e = ADMIT_K1 * (fabs(x) + fabs(o) + fabs(diff)) / fabs(d) + ADMIT_K2 * fabs(t) + ADMIT_ABS;
    return float2(t, e);
}

// Transcribes `margin::admits`: is the f64 leaf slab test certainly passed?
static bool admits(float3 o, float3 d, Box b) {
    float3 bmin = float3(b.lo), bmax = float3(b.hi);
    float lo[3], hi[3];
    for (int a = 0; a < 3; a++) {
        if (!(fabs(d[a]) >= ADMIT_MIN_D)) return false;
        float2 p = slab_t(bmin[a], o[a], d[a]);
        float2 q = slab_t(bmax[a], o[a], d[a]);
        lo[a] = min(p.x + p.y, q.x + q.y);
        hi[a] = max(p.x - p.y, q.x - q.y);
        if (!(isfinite(lo[a]) && isfinite(hi[a]))) return false;
    }
    for (int b2 = 0; b2 < 3; b2++) {
        if (!(hi[b2] >= 0.0f)) return false;
        for (int a = 0; a < 3; a++) {
            if (a != b2 && !(lo[a] <= hi[b2])) return false;
        }
    }
    return true;
}

constant int CODE_CPU = -2147483647 - 1;   // i32::MIN: ray not for the GPU
constant int CODE_TRUE = -2147483647;      // i32::MIN + 1: certainly occluded

// out[i]: CODE_TRUE  a triangle certainly accepted in a certainly reached leaf;
//         >= 0       triangle `out` certainly accepted, leaf test for the CPU;
//         -1         every candidate rejected: the ray is not occluded;
//         CODE_CPU   the ray was not for the GPU (non-finite marker);
//         else (<= -2) no accept, candidate -(out+2) was uncertain.
static int any_hit(primitive_acceleration_structure accel, device const Tri* tris,
                   device const Box* leaves, float3 o, float3 o_lo, float3 d) {
    if (!all(isfinite(d))) return CODE_CPU;
    ray r(o, d, 0.0f, INFINITY);
    intersection_params params;
    intersection_query<> q;
    q.reset(r, accel, params);
    int hit = -1, maybe = -1;  // hit: CODE_TRUE or a prim; -1 none
    while (q.next()) {
        if (q.get_candidate_intersection_type() != intersection_type::bounding_box) continue;
        uint i = q.get_candidate_primitive_id();
        int c = classify(o, o_lo, d, tris[i]);
        if (c == ACCEPT) {
            hit = admits(o, d, leaves[i]) ? CODE_TRUE : int(i);
            q.abort();
            break;
        }
        if (c == UNCERTAIN && maybe < 0) maybe = int(i);
    }
    return hit != -1 ? hit : (maybe >= 0 ? -2 - maybe : -1);
}

kernel void shadow_any_hit(primitive_acceleration_structure accel [[buffer(0)]],
                           device const Tri* tris [[buffer(1)]],
                           device const Ray* rays [[buffer(2)]],
                           device int* out [[buffer(3)]],
                           device const Box* leaves [[buffer(4)]],
                           constant uint& n [[buffer(5)]],
                           uint gid [[thread_position_in_grid]]) {
    if (gid >= n) return;
    out[gid] = any_hit(accel, tris, leaves, float3(rays[gid].o), float3(rays[gid].o_lo), float3(rays[gid].d));
}

// Fans (#337): `rays[gid]` packs the ray's item in the batch (high bits) and
// its direction's index (low FAN_DIR_BITS). The item's origin is `origins[i]`
// and the direction `dirs[k]`, both written by the CPU with the same `Frame`
// halves as a `Ray` record, so the kernel sees the same bits. A NaN origin or
// direction marks a ray the CPU answers.
constant uint FAN_DIR_BITS = 19;  // `FAN_DIR_BITS` in caster.rs

struct Origin { packed_float3 o; packed_float3 o_lo; };

kernel void shadow_fans(primitive_acceleration_structure accel [[buffer(0)]],
                        device const Tri* tris [[buffer(1)]],
                        device const Origin* origins [[buffer(2)]],
                        device const uint* rays [[buffer(3)]],
                        device const packed_float3* dirs [[buffer(4)]],
                        device int* out [[buffer(5)]],
                        device const Box* leaves [[buffer(6)]],
                        constant uint& n [[buffer(7)]],
                        uint gid [[thread_position_in_grid]]) {
    if (gid >= n) return;
    uint p = rays[gid];
    uint i = p >> FAN_DIR_BITS;
    float3 o = float3(origins[i].o);
    float3 d = all(isfinite(o)) ? float3(dirs[p & ((1u << FAN_DIR_BITS) - 1)]) : float3(NAN);
    out[gid] = any_hit(accel, tris, leaves, o, float3(origins[i].o_lo), d);
}
