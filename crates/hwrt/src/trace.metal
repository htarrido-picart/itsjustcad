// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart
//
// Closest-hit and any-hit rays for the `raytrace` renderer (#318) over a
// triangle primitive acceleration structure. Plain f32: the rendered image
// is not logged state (#261 item 3), so these answers are used directly.

#include <metal_stdlib>
#include <metal_raytracing>
using namespace metal;
using namespace raytracing;

struct Ray { packed_float3 o; packed_float3 d; };
struct Hit { uint prim; float t; };

// `kernel_mesh::RAY_TRIANGLE_EPS`: the CPU accepts only `t > 1e-9`.
constant float T_MIN = 1e-9f;
constant uint MISS = 0xffffffffu;

// Closest hit, keeping the CPU tie rule: on exactly equal `t` the lowest
// primitive index wins (`Bvh::ray_closest_hit`). Pass 1 is the opaque
// hardware closest hit. It returns some primitive at the closest `t`, but
// after committing one the hardware culls candidates at an equal `t`, so a
// lower index can be missed. Pass 2 re-walks only `[T_MIN, t]` with every
// triangle forced non-opaque and nothing committed, so every triangle at
// that `t` is reported, and keeps the lowest index among them.
kernel void closest_hit(primitive_acceleration_structure accel [[buffer(0)]],
                        device const Ray* rays [[buffer(1)]],
                        device Hit* out [[buffer(2)]],
                        constant uint& n [[buffer(3)]],
                        uint gid [[thread_position_in_grid]]) {
    if (gid >= n) return;
    float3 o = float3(rays[gid].o), d = float3(rays[gid].d);
    intersector<triangle_data> it;
    it.assume_geometry_type(geometry_type::triangle);
    auto h = it.intersect(ray(o, d, T_MIN, INFINITY), accel);
    if (h.type == intersection_type::none) {
        out[gid] = Hit { MISS, INFINITY };
        return;
    }
    float best = h.distance;
    uint prim = h.primitive_id;
    intersection_params params;
    params.force_opacity(forced_opacity::non_opaque);
    params.assume_geometry_type(geometry_type::triangle);
    intersection_query<triangle_data> q;
    q.reset(ray(o, d, T_MIN, nextafter(best, INFINITY)), accel, params);
    while (q.next()) {
        if (q.get_candidate_intersection_type() == intersection_type::triangle &&
            q.get_candidate_triangle_distance() == best) {
            prim = min(prim, q.get_candidate_primitive_id());
        }
    }
    out[gid] = Hit { prim, best };
}

// Any hit at `t > T_MIN` (shadow rays toward the sun, unbounded).
kernel void any_hit(primitive_acceleration_structure accel [[buffer(0)]],
                    device const Ray* rays [[buffer(1)]],
                    device uchar* out [[buffer(2)]],
                    constant uint& n [[buffer(3)]],
                    uint gid [[thread_position_in_grid]]) {
    if (gid >= n) return;
    ray r(float3(rays[gid].o), float3(rays[gid].d), T_MIN, INFINITY);
    intersector<> it;
    it.accept_any_intersection(true);
    it.assume_geometry_type(geometry_type::triangle);
    out[gid] = it.intersect(r, accel).type != intersection_type::none ? 1 : 0;
}
