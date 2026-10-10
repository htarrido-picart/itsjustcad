// Curves: tessellated line strips, uniform color.
// In pencil mode (camera.misc.x < 0) unselected lines render black (ink on
// paper); high-saturation colours (selection highlights) are preserved.

struct Camera {
    view_proj: mat4x4<f32>,
    inv_view_proj: mat4x4<f32>,
    eye: vec4<f32>,
    misc: vec4<f32>,
    light: vec4<f32>,
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
