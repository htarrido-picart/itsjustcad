// Solid mesh shading.
//
// Lighting model is selected by `camera.light.x`:
//   0 = Working (DEFAULT): hemispheric sky/ground ambient FILL + one soft 3/4
//       directional key, matte (no specular). The ambient floor guarantees no
//       face reads fully black so form is always legible while orbiting.
//   1 = Sun: the SPA solar direction (camera.misc.yzw) is the key light, on top
//       of the SAME hemispheric ambient floor — grazing faces never go black.
//   2 = Presentation: Working fill + Blinn-Phong specular from the material
//       presets so glass/metal/concrete read distinctly.
//
// Pencil hidden-line mode is orthogonal: signalled by camera.misc.x < 0.

struct Camera {
    view_proj: mat4x4<f32>,
    inv_view_proj: mat4x4<f32>,
    eye: vec4<f32>,
    // x  = fill alpha multiplier (display mode)
    // yzw = sun direction (X=East,Y=North,Z=Up)
    misc: vec4<f32>,
    // x = lighting mode (0 working, 1 sun, 2 presentation); yzw spare.
    light: vec4<f32>,
};

@group(0) @binding(0) var<uniform> camera: Camera;

struct ObjectParams {
    color: vec4<f32>,
    // x = roughness (0 smooth .. 1 matte), y = metallic (0 .. 1), zw spare.
    material: vec4<f32>,
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

// Hemispheric ambient fill: sky colour when the normal faces up, ground colour
// when it faces down, blended by the up-facing fraction. This is the ambient
// FLOOR — even a face turned fully away from the key light gets sky/ground
// light, so nothing reads pure black (accessibility min-luminance floor).
fn hemispheric_ambient(n: vec3<f32>) -> f32 {
    // n.z in [-1,1]; remap to [0,1] up-fraction.
    let up = clamp(n.z * 0.5 + 0.5, 0.0, 1.0);
    // Ground bounce 0.30, sky fill 0.65: guarantees a >0 floor on every face.
    return mix(0.30, 0.65, up);
}

// World-fixed 3/4 key direction (from upper-front-right). Not a headlight, so
// shading is stable as the camera orbits and reads form/depth.
fn working_key_dir() -> vec3<f32> {
    return normalize(vec3<f32>(0.4, -0.5, 0.75));
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let n = normalize(in.normal);

    // Pencil mode: misc.x < 0 signals hidden-line paper-white fill.
    if (camera.misc.x < 0.0) {
        let l = normalize(camera.eye.xyz - in.world);
        let lambert = max(dot(n, l), 0.0);
        let shade = 0.90 + 0.07 * lambert;
        return vec4(shade, shade, shade * 0.98, 1.0);
    }

    let mode = camera.light.x;
    let view_dir = normalize(camera.eye.xyz - in.world);
    let ambient = hemispheric_ambient(n);

    // Pick the key-light direction: Sun mode uses the real solar vector when it
    // is set; otherwise the world-fixed 3/4 key.
    let sun = camera.misc.yzw;
    let has_sun = dot(sun, sun) > 0.01;
    var key_dir = working_key_dir();
    if (mode == 1.0 && has_sun) {
        key_dir = normalize(sun);
    }
    let key = max(dot(n, key_dir), 0.0);

    // Diffuse = hemispheric ambient floor + soft directional key. The key is
    // deliberately gentle (0.55) so the ambient floor dominates and no face
    // crushes to black. Result is a matte, readable surface.
    let diffuse = ambient + 0.55 * key;
    var lit = object.color.rgb * diffuse;

    // Presentation mode only: Blinn-Phong specular from the material presets.
    if (mode == 2.0) {
        let roughness = clamp(object.material.x, 0.02, 1.0);
        let metallic = clamp(object.material.y, 0.0, 1.0);
        let half_v = normalize(key_dir + view_dir);
        let n_dot_h = max(dot(n, half_v), 0.0);
        let shininess = mix(4.0, 256.0, 1.0 - roughness);
        let spec_strength = mix(0.15, 0.9, 1.0 - roughness);
        let spec = pow(n_dot_h, shininess) * spec_strength * key;
        let spec_tint = mix(vec3(1.0, 1.0, 1.0), object.color.rgb, metallic);
        // Metals have little diffuse; dielectrics keep the matte base.
        let diffuse_amt = mix(1.0, 0.25, metallic);
        lit = object.color.rgb * diffuse * diffuse_amt + spec_tint * spec;
    }

    return vec4(lit, object.color.a * camera.misc.x);
}
