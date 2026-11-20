// Screen-space ambient occlusion (#358): normal prepass, occlusion, blur.
// See ao.rs for the passes. Camera and unproject() come from common.wgsl.

// --- Prepass: world normals (and depth) of the opaque faces. ---

struct PreIn {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
};

struct PreOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) normal: vec3<f32>,
};

@vertex
fn vs_prepass(in: PreIn) -> PreOut {
    var out: PreOut;
    out.clip = camera.view_proj * vec4(in.position, 1.0);
    out.normal = in.normal;
    return out;
}

@fragment
fn fs_prepass(in: PreOut) -> @location(0) vec4<f32> {
    return vec4(normalize(in.normal) * 0.5 + 0.5, 1.0);
}

// --- Occlusion and blur: fullscreen over the pane's targets. ---

@group(0) @binding(2) var depth_tex: texture_depth_2d;
@group(0) @binding(3) var normal_tex: texture_2d<f32>;
@group(1) @binding(0) var raw_tex: texture_2d<f32>;

const SAMPLES: u32 = 16u;

@vertex
fn vs_full(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    let x = f32((vi << 1u) & 2u);
    let y = f32(vi & 2u);
    return vec4(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
}

// World point at pixel `px` (xyz) and its depth (w; 1 = background).
fn world_at(px: vec2<i32>) -> vec4<f32> {
    let dims = vec2<f32>(textureDimensions(depth_tex));
    let d = textureLoad(depth_tex, px, 0);
    let uv = (vec2<f32>(px) + 0.5) / dims;
    return vec4(unproject(vec2(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0), d), d);
}

fn normal_at(px: vec2<i32>) -> vec3<f32> {
    return normalize(textureLoad(normal_tex, px, 0).xyz * 2.0 - 1.0);
}

// The view direction (perspective and parallel alike).
fn view_fwd() -> vec3<f32> {
    return normalize(unproject(vec2(0.0), 0.5) - unproject(vec2(0.0), 0.0));
}

// Distance of world point `p` in front of the eye, along the view direction.
fn view_depth(p: vec3<f32>, fwd: vec3<f32>) -> f32 {
    return dot(p - camera.eye.xyz, fwd);
}

@fragment
fn fs_ao(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let px = vec2<i32>(pos.xy);
    let c = world_at(px);
    let radius = camera.ao.w;
    if (c.w >= 1.0 || radius <= 0.0) {
        return vec4(0.0);
    }
    let fwd = view_fwd();
    var n = normal_at(px);
    if (dot(n, fwd) > 0.0) {
        n = -n;
    }
    let helper = select(vec3(0.0, 0.0, 1.0), vec3(1.0, 0.0, 0.0), abs(n.z) > 0.9);
    let t = normalize(cross(helper, n));
    let b = cross(n, t);
    // Fixed per-pixel kernel rotation (4x4 Bayer order): the same every
    // frame, so nothing crawls while orbiting; the 4x4 blur averages it out.
    var bayer = array<f32, 16>(0.0, 8.0, 2.0, 10.0, 12.0, 4.0, 14.0, 6.0, 3.0, 11.0, 1.0, 9.0, 15.0, 7.0, 13.0, 5.0);
    let rot = bayer[(px.x & 3) + (px.y & 3) * 4] * (6.2831853 / 16.0);
    let dims = vec2<f32>(textureDimensions(depth_tex));
    let p_depth = view_depth(c.xyz, fwd);
    let bias = 0.03 * radius;
    var occ = 0.0;
    for (var i = 0u; i < SAMPLES; i++) {
        let fi = f32(i) + 0.5;
        // Cosine-weighted hemisphere (golden-angle spiral), lengths spread
        // so near occluders weigh in.
        let r = sqrt(fi / f32(SAMPLES));
        let phi = fi * 2.3999632 + rot;
        let h = vec3(r * cos(phi), r * sin(phi), sqrt(max(1.0 - r * r, 0.0)));
        let scale = mix(0.15, 1.0, fract(fi * 0.618034));
        let s = c.xyz + (t * h.x + b * h.y + n * h.z) * (radius * scale);
        let clip = camera.view_proj * vec4(s, 1.0);
        if (clip.w <= 0.0) {
            continue;
        }
        let ndc = clip.xy / clip.w;
        let q = vec2<i32>(floor(vec2(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5) * dims));
        if (q.x < 0 || q.y < 0 || q.x >= i32(dims.x) || q.y >= i32(dims.y)) {
            continue;
        }
        let o = world_at(q);
        if (o.w >= 1.0) {
            continue;
        }
        let o_depth = view_depth(o.xyz, fwd);
        if (o_depth < view_depth(s, fwd) - bias) {
            // Occluders far in front (beyond the radius) fade out.
            occ += smoothstep(0.0, 1.0, radius / max(abs(p_depth - o_depth), 1e-6));
        }
    }
    return vec4(clamp(occ / f32(SAMPLES) * 1.5, 0.0, 1.0), 0.0, 0.0, 1.0);
}

@fragment
fn fs_blur(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let px = vec2<i32>(pos.xy);
    let c = world_at(px);
    if (c.w >= 1.0) {
        return vec4(0.0);
    }
    let fwd = view_fwd();
    let d0 = view_depth(c.xyz, fwd);
    let n0 = normal_at(px);
    let tol = max(camera.ao.w * 0.25, 1e-6);
    let hi = vec2<i32>(textureDimensions(raw_tex)) - 1;
    var sum = 0.0;
    var wsum = 0.0;
    // 4x4 window: exactly one period of the rotation pattern.
    for (var y = -2; y < 2; y++) {
        for (var x = -2; x < 2; x++) {
            let q = clamp(px + vec2(x, y), vec2(0), hi);
            let o = world_at(q);
            if (o.w >= 1.0) {
                continue;
            }
            let dz = abs(view_depth(o.xyz, fwd) - d0);
            let w = max(1.0 - dz / tol, 0.0) * step(0.8, dot(normal_at(q), n0));
            sum += textureLoad(raw_tex, q, 0).r * w;
            wsum += w;
        }
    }
    return vec4(sum / max(wsum, 1e-4), 0.0, 0.0, 1.0);
}
