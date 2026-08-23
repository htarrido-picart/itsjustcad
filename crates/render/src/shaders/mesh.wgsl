// Flat-shaded solid with a simple headlight lambert term.

struct Camera {
    view_proj: mat4x4<f32>,
    inv_view_proj: mat4x4<f32>,
    eye: vec4<f32>,
    // x = fill alpha multiplier (display mode), yzw spare
    misc: vec4<f32>,
};

@group(0) @binding(0) var<uniform> camera: Camera;

struct ObjectParams {
    color: vec4<f32>,
};

@group(1) @binding(0) var<uniform> object: ObjectParams;

struct VsIn {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) normal: vec3<f32>,
    @location(1) world: vec3<f32>,
};

@vertex
fn vs_main(in: VsIn) -> VsOut {
    var out: VsOut;
    out.clip = camera.view_proj * vec4(in.position, 1.0);
    out.normal = in.normal;
    out.world = in.position;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let n = normalize(in.normal);
    let l = normalize(camera.eye.xyz - in.world); // headlight
    let lambert = max(dot(n, l), 0.0);
    let shade = 0.35 + 0.65 * lambert;
    return vec4(object.color.rgb * shade, object.color.a * camera.misc.x);
}
