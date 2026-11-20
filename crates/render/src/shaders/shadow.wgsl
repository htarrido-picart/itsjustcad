// Sun shadow map (#357): the opaque faces, depth only, seen from the sun.
// See shadow.rs. Standalone: no camera, just the light.

struct ShadowMap {
    light_view_proj: mat4x4<f32>,
    params: vec4<f32>,
};

@group(0) @binding(0) var<uniform> shadow_map: ShadowMap;

@vertex
fn vs_shadow(@location(0) position: vec3<f32>) -> @builtin(position) vec4<f32> {
    return shadow_map.light_view_proj * vec4(position, 1.0);
}
