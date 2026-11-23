// Textured quad on the ground plane (z = 0). Four world-space corners fed as
// vertices with uvs; depth is written so scene meshes occlude it and the grid
// blends over it. Opacity is a uniform multiplier.

// Camera and fogged() come from common.wgsl.
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;
@group(1) @binding(2) var<uniform> opacity: vec4<f32>; // x = opacity

struct VsIn {
    @location(0) pos: vec3<f32>,
    @location(1) uv: vec2<f32>,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) world: vec3<f32>,
};

@vertex
fn vs_main(in: VsIn) -> VsOut {
    var out: VsOut;
    out.clip = camera.view_proj * vec4(in.pos, 1.0);
    out.uv = in.uv;
    out.world = in.pos;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let c = fogged(textureSample(tex, samp, in.uv), in.world);
    let a = c.a * opacity.x;
    // Premultiplied output to match the viewport's blend state.
    return vec4(c.rgb * a, a);
}
