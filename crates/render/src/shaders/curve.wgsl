// Curves: tessellated line strips, uniform color.

struct Camera {
    view_proj: mat4x4<f32>,
    inv_view_proj: mat4x4<f32>,
    eye: vec4<f32>,
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

@fragment
fn fs_main() -> @location(0) vec4<f32> {
    return object.color;
}
