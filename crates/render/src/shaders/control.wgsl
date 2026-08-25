// Control-image passes: depth (grayscale), edge (flat ink), mask (flat color).

struct Ctrl {
    view_proj: mat4x4<f32>,
    eye: vec4<f32>,
    // x = near distance, y = far distance (world units from the eye).
    range: vec4<f32>,
    color: vec4<f32>,
};

@group(0) @binding(0) var<uniform> ctrl: Ctrl;

struct VsIn {
    @location(0) position: vec3<f32>,
    // Meshes bind a normal at location 1; the edge (line) buffer omits it, so
    // the pipeline for lines uses a vertex layout without location 1. Keeping
    // the field optional per-pipeline is handled on the Rust side; here we only
    // read position, which both layouts provide.
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
};

@vertex
fn vs_main(@location(0) position: vec3<f32>) -> VsOut {
    var out: VsOut;
    out.clip = ctrl.view_proj * vec4(position, 1.0);
    out.world = position;
    return out;
}

// Depth: distance from the eye normalized into [0,1] over [near, far], then
// inverted so NEAR is white and FAR is black (a readable depth gradient).
@fragment
fn fs_depth(in: VsOut) -> @location(0) vec4<f32> {
    let d = length(ctrl.eye.xyz - in.world);
    let t = clamp((d - ctrl.range.x) / (ctrl.range.y - ctrl.range.x), 0.0, 1.0);
    let g = 1.0 - t;
    return vec4(g, g, g, 1.0);
}

// Edge: flat black ink (background is cleared white by the pass).
@fragment
fn fs_edge(in: VsOut) -> @location(0) vec4<f32> {
    return vec4(0.0, 0.0, 0.0, 1.0);
}

// Mask: the per-object flat color, no shading.
@fragment
fn fs_mask(in: VsOut) -> @location(0) vec4<f32> {
    return vec4(ctrl.color.rgb, 1.0);
}
