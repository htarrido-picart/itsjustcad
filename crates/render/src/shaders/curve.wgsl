// Curves: tessellated line strips, uniform color.
// In pencil mode (camera.misc.x < 0) unselected lines render black (ink on
// paper); high-saturation colours (selection highlights) are preserved.

struct Camera {
    view_proj: mat4x4<f32>,
    inv_view_proj: mat4x4<f32>,
    eye: vec4<f32>,
    misc: vec4<f32>,
    light: vec4<f32>,
    // x,y = target size (device px), z = stroke width (device px); see vs_fat.
    screen: vec4<f32>,
};

@group(0) @binding(0) var<uniform> camera: Camera;

struct ObjectParams {
    color: vec4<f32>,
};

@group(1) @binding(0) var<uniform> object: ObjectParams;

@vertex
fn vs_main(@location(0) position: vec3<f32>) -> @builtin(position) vec4<f32> {
    return camera.view_proj * vec4(position, 1.0);
}

// World-axis variant: nudge the clip-space depth a few Depth24Plus ULPs toward
// the camera. The axes are coplanar with the grid plane (z=0), but the grid
// writes an ANALYTIC frag_depth while lines rasterize interpolated depth — the
// two quantize differently, and without the nudge the depth test flips
// per-pixel and the (opaque, gradient-mode) grid stipples over the axes.
// A fixed-function depth bias can't be used: wgpu rejects it on line topology.
@vertex
fn vs_axis(@location(0) position: vec3<f32>) -> @builtin(position) vec4<f32> {
    var clip = camera.view_proj * vec4(position, 1.0);
    clip.z -= 5e-7 * clip.w; // ~8 ULPs of Depth24Plus in NDC
    return clip;
}

@fragment
fn fs_main() -> @location(0) vec4<f32> {
    return ink();
}

fn ink() -> vec4<f32> {
    // Pencil mode: black ink on paper white, but keep selection highlights
    // (amber/orange) visible by detecting high-saturation colours.
    if (camera.misc.x < 0.0) {
        let c = object.color.rgb;
        let hi = max(c.r, max(c.g, c.b));
        let lo = min(c.r, min(c.g, c.b));
        let saturation = hi - lo;
        // High saturation → selection highlight; keep the colour but darken
        // it slightly so it reads on white.
        if (saturation > 0.25) {
            return vec4(c * 0.75, 1.0);
        }
        return vec4(0.04, 0.04, 0.05, 1.0);
    }
    return object.color;
}

// Fat strokes (itsjustcad-ios#174): wgpu draws lines as 1-device-px hairlines,
// so the hidden-line and sketchy modes expand each segment into a screen-space
// quad (one instance per segment, 6 vertices) of width camera.screen.z, with
// round caps (so polyline joins close up) and a 1 px anti-aliased edge.
struct FatOut {
    @builtin(position) pos: vec4<f32>,
    // Device px across the centre line and along the segment (0 at the start),
    // and the segment's screen length, for the round caps and AA ramp.
    @location(0) @interpolate(linear) local: vec2<f32>,
    @location(1) @interpolate(flat) len_px: f32,
};

// Fraction of the eye distance a stroke is pulled toward the camera so it wins
// over the face it lies on. Relative, so it hides lines as well at 300 m as at
// 3 m; a fixed clip-space offset grows with distance squared under
// perspective and let far lines show through whole buildings (#174).
const STROKE_PULL: f32 = 2e-3;

// Pull a world point toward the eye along its view ray: same pixel, nearer
// depth. Orthographic views (clip w = 1 at the eye, not 0) get a small fixed
// depth offset instead, since their depth is linear.
fn pulled_clip(p: vec3<f32>) -> vec4<f32> {
    let eye_clip = camera.view_proj * vec4(camera.eye.xyz, 1.0);
    if (abs(eye_clip.w) < 0.5) {
        return camera.view_proj * vec4(mix(p, camera.eye.xyz, STROKE_PULL), 1.0);
    }
    var c = camera.view_proj * vec4(p, 1.0);
    c.z -= 5e-7 * c.w;
    return c;
}

@vertex
fn vs_fat(
    @builtin(vertex_index) vi: u32,
    @location(0) a: vec3<f32>,
    @location(1) b: vec3<f32>,
) -> FatOut {
    var out: FatOut;
    var ca = pulled_clip(a);
    var cb = pulled_clip(b);
    // Clip to just in front of the eye so a segment crossing it does not flip.
    let near = 1e-4;
    if (ca.w < near && cb.w < near) {
        out.pos = vec4(2.0, 2.0, 2.0, 1.0); // fully clipped
        out.local = vec2(0.0);
        out.len_px = 0.0;
        return out;
    }
    if (ca.w < near) {
        ca = mix(ca, cb, (near - ca.w) / (cb.w - ca.w));
    } else if (cb.w < near) {
        cb = mix(cb, ca, (near - cb.w) / (ca.w - cb.w));
    }
    let half_size = camera.screen.xy * 0.5;
    let sa = ca.xy / ca.w * half_size;
    let sb = cb.xy / cb.w * half_size;
    let d = sb - sa;
    let len = length(d);
    var dir = vec2(1.0, 0.0);
    if (len > 1e-6) {
        dir = d / len;
    }
    let nrm = vec2(-dir.y, dir.x);
    var corners = array<vec2<f32>, 6>(
        vec2(0.0, -1.0), vec2(1.0, -1.0), vec2(0.0, 1.0),
        vec2(0.0, 1.0), vec2(1.0, -1.0), vec2(1.0, 1.0),
    );
    let c = corners[vi];
    let half_w = camera.screen.z * 0.5 + 1.0; // +1 px for the AA ramp
    var clip = ca;
    var along = -1.0;
    if (c.x > 0.5) {
        clip = cb;
        along = 1.0;
    }
    let off = nrm * c.y * half_w + dir * along * half_w;
    clip = vec4(clip.xy + off / half_size * clip.w, clip.z, clip.w);
    out.pos = clip;
    out.local = vec2(c.y * half_w, c.x * len + along * half_w);
    out.len_px = len;
    return out;
}

@fragment
fn fs_fat(in: FatOut) -> @location(0) vec4<f32> {
    let c = ink();
    // Distance to the segment (not just across it): round caps.
    let past = max(-in.local.y, in.local.y - in.len_px);
    let dist = length(vec2(in.local.x, max(past, 0.0)));
    let cover = clamp(camera.screen.z * 0.5 + 0.5 - dist, 0.0, 1.0);
    if (cover <= 0.0) {
        discard; // keep the cap corners out of the depth buffer
    }
    return vec4(c.rgb, c.a * cover);
}
