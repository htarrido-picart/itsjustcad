// Curves: tessellated line strips, uniform color.
// In pencil mode (camera.misc.x < 0) unselected lines render black (ink on
// paper); high-saturation colours (selection highlights) are preserved.

// Camera and fogged() come from common.wgsl. Every stroke carries its
// world position to the fragment for the fog (#363).

struct ObjectParams {
    color: vec4<f32>,
    // w = 1: never cut by the section plane (the world axes, #359).
    material: vec4<f32>,
    // Dash pattern in logical px: on, off, on, off. All zero = solid (#364).
    dash: vec4<f32>,
};

@group(1) @binding(0) var<uniform> object: ObjectParams;

struct LineOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) world: vec3<f32>,
};

@vertex
fn vs_main(@location(0) position: vec3<f32>) -> LineOut {
    return LineOut(camera.view_proj * vec4(position, 1.0), position);
}

// World-axis variant: nudge the clip-space depth a few Depth24Plus ULPs toward
// the camera. The axes are coplanar with the grid plane (z=0), but the grid
// writes an ANALYTIC frag_depth while lines rasterize interpolated depth — the
// two quantize differently, and without the nudge the depth test flips
// per-pixel and the (opaque, gradient-mode) grid stipples over the axes.
// A fixed-function depth bias can't be used: wgpu rejects it on line topology.
@vertex
fn vs_axis(@location(0) position: vec3<f32>) -> LineOut {
    var clip = camera.view_proj * vec4(position, 1.0);
    clip.z -= 5e-7 * clip.w; // ~8 ULPs of Depth24Plus in NDC
    return LineOut(clip, position);
}

@fragment
fn fs_main(in: LineOut) -> @location(0) vec4<f32> {
    if (stroke_cut(in.world)) {
        discard;
    }
    return fogged(ink(), in.world);
}

// Whether the live section cut (#359) removes this stroke at world point `p`.
fn stroke_cut(p: vec3<f32>) -> bool {
    return object.material.w < 0.5 && cut_away(p);
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
    // Stroke width in device px (the depth cue varies it per segment).
    @location(2) @interpolate(flat) width: f32,
    @location(3) world: vec3<f32>,
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
    return fat_quad(vi, a, b, stroke_width(a, b));
}

// Stroke width of a segment in device px: the mode's width, or with the depth
// cue on (#361) a ramp from the cue width at the nearest scene point down to
// the mode's width at the farthest, taken at the segment's midpoint.
fn stroke_width(a: vec3<f32>, b: vec3<f32>) -> f32 {
    let base = camera.screen.z;
    let near = camera.edges.x;
    if (near <= base) {
        return base;
    }
    let t = clamp(dot((a + b) * 0.5, camera.cue.xyz) + camera.cue.w, 0.0, 1.0);
    return mix(near, base, t);
}

fn fat_quad(vi: u32, a: vec3<f32>, b: vec3<f32>, width: f32) -> FatOut {
    var out: FatOut;
    out.width = width;
    var ca = pulled_clip(a);
    var cb = pulled_clip(b);
    // Clip to just in front of the eye so a segment crossing it does not flip.
    let near = 1e-4;
    if (ca.w < near && cb.w < near) {
        out.pos = vec4(2.0, 2.0, 2.0, 1.0); // fully clipped
        out.local = vec2(0.0);
        out.len_px = 0.0;
        out.world = a;
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
    let half_w = width * 0.5 + 1.0; // +1 px for the AA ramp
    var clip = ca;
    var along = -1.0;
    out.world = a;
    if (c.x > 0.5) {
        clip = cb;
        along = 1.0;
        out.world = b;
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
    if (stroke_cut(in.world)) {
        discard;
    }
    return fogged(fat_ink(in.local, in.len_px, in.width), in.world);
}

fn fat_ink(local: vec2<f32>, len_px: f32, width: f32) -> vec4<f32> {
    let c = ink();
    // Distance to the segment (not just across it): round caps.
    let past = max(-local.y, local.y - len_px);
    let dist = length(vec2(local.x, max(past, 0.0)));
    let cover = clamp(width * 0.5 + 0.5 - dist, 0.0, 1.0);
    if (cover <= 0.0) {
        discard; // keep the cap corners out of the depth buffer
    }
    return vec4(c.rgb, c.a * cover);
}

// Dashed strokes (#364). Every vertex carries `s`, its world distance along
// the stroke. The pattern is laid out in screen px: the world length of one
// period at the fragment's depth is snapped up to a power of two, so the
// dashes stay put on the geometry while zooming (no crawling) and keep a
// steady on-screen size within a factor of two, like the grid's levels.
struct DashOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) s: f32,
    // Clip w: the eye depth in perspective, 1 in parallel views.
    @location(1) w: f32,
    @location(2) world: vec3<f32>,
};

// Whether the dash pattern is "on" at distance `s` along the stroke, for a
// fragment at clip depth `w`.
fn dash_on(s: f32, w: f32) -> bool {
    let pat = object.dash * max(camera.screen.w, 1.0);
    let period = pat.x + pat.y + pat.z + pat.w;
    if (period <= 0.0) {
        return true;
    }
    // World size of one device px at this depth: 2 w / (P11 * height). The
    // projection's y row (P11 times the unit view row) gives P11.
    var h = camera.screen.y;
    if (h <= 0.0) {
        h = 1000.0;
    }
    let row_y = vec3(camera.view_proj[0].y, camera.view_proj[1].y, camera.view_proj[2].y);
    let wpp = 2.0 * abs(w) / max(length(row_y) * h, 1e-12);
    let world_period = exp2(ceil(log2(max(period * wpp, 1e-12))));
    let phase = fract(s / world_period) * period;
    let first = pat.x + pat.y;
    return phase < pat.x || (phase >= first && phase < first + pat.z);
}

@vertex
fn vs_dash(@location(0) p: vec4<f32>) -> DashOut {
    var out: DashOut;
    // The dotted negative world axes use this too: same depth nudge as vs_axis.
    var clip = camera.view_proj * vec4(p.xyz, 1.0);
    clip.z -= 5e-7 * clip.w;
    out.pos = clip;
    out.s = p.w;
    out.w = clip.w;
    out.world = p.xyz;
    return out;
}

@fragment
fn fs_dash(in: DashOut) -> @location(0) vec4<f32> {
    if (stroke_cut(in.world)) {
        discard;
    }
    if (!dash_on(in.s, in.w)) {
        discard;
    }
    return fogged(ink(), in.world);
}

struct FatDashOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) @interpolate(linear) local: vec2<f32>,
    @location(1) @interpolate(flat) len_px: f32,
    @location(2) s: f32,
    @location(3) w: f32,
    @location(4) @interpolate(flat) width: f32,
    @location(5) world: vec3<f32>,
};

@vertex
fn vs_fat_dash(
    @builtin(vertex_index) vi: u32,
    @location(0) a: vec4<f32>,
    @location(1) b: vec4<f32>,
) -> FatDashOut {
    let q = fat_quad(vi, a.xyz, b.xyz, stroke_width(a.xyz, b.xyz));
    var out: FatDashOut;
    out.pos = q.pos;
    out.width = q.width;
    out.local = q.local;
    out.len_px = q.len_px;
    // Corners 1, 4 and 5 sit at the segment's end (see `corners` above).
    out.s = select(a.w, b.w, vi == 1u || vi == 4u || vi == 5u);
    out.w = q.pos.w;
    out.world = q.world;
    return out;
}

@fragment
fn fs_fat_dash(in: FatDashOut) -> @location(0) vec4<f32> {
    if (stroke_cut(in.world)) {
        discard;
    }
    if (!dash_on(in.s, in.w)) {
        discard;
    }
    return fogged(fat_ink(in.local, in.len_px, in.width), in.world);
}

// Back edges (#361, SketchUp): the edges hidden behind faces, drawn after the
// scene in a second pass whose depth test is reversed (GREATER, no depth
// writes), as faint dashes of the stroke's own colour. One fat quad per
// segment of the edge / curve LineList buffers, so no extra geometry: the
// dashes run in screen px along each segment (`local.y`). The segment is
// pulled toward the eye like every fat stroke, plus a little more in
// parallel views, so a visible edge (at its face's depth) fails the test.
@vertex
fn vs_back(
    @builtin(vertex_index) vi: u32,
    @location(0) a: vec3<f32>,
    @location(1) b: vec3<f32>,
) -> FatOut {
    var q = fat_quad(vi, a, b, max(camera.screen.z, 1.0));
    let eye_clip = camera.view_proj * vec4(camera.eye.xyz, 1.0);
    if (abs(eye_clip.w) >= 0.5) {
        q.pos.z -= 1e-4 * q.pos.w;
    }
    return q;
}

// Back-edge dash in logical px (on, off) and opacity (`BACK_EDGE_ALPHA`).
const BACK_DASH: vec2<f32> = vec2(4.0, 3.0);
const BACK_ALPHA: f32 = 0.4;

@fragment
fn fs_back(in: FatOut) -> @location(0) vec4<f32> {
    if (stroke_cut(in.world)) {
        discard;
    }
    let pat = BACK_DASH * max(camera.edges.z, 1.0);
    if (fract(max(in.local.y, 0.0) / (pat.x + pat.y)) * (pat.x + pat.y) >= pat.x) {
        discard;
    }
    let c = fogged(fat_ink(in.local, in.len_px, in.width), in.world);
    return vec4(c.rgb, c.a * BACK_ALPHA);
}
