// Shared by the scene shaders (mesh, curve, grid, underlay): prepended to each
// when its module is built (see `scene_shader` in renderer.rs). Holds the
// camera uniform, the background gradient and the fog (#363).

struct Camera {
    view_proj: mat4x4<f32>,
    inv_view_proj: mat4x4<f32>,
    eye: vec4<f32>,
    // x  = fill alpha multiplier (display mode; < 0 = pencil paper fill)
    // yzw = sun direction (X=East,Y=North,Z=Up)
    misc: vec4<f32>,
    // x = lighting mode (0 working, 1 sun, 2 presentation), y = background
    // gradient flag (1 = sky/ground gradient), z = 1 hides the grid lines
    // (`grid off`, #364), w = 1 hides the axes.
    light: vec4<f32>,
    // x,y = target size (device px), z = stroke width (device px); see vs_fat.
    // w = device px per logical px, scaling the dash patterns (0 reads as 1).
    screen: vec4<f32>,
    // Edge styles (#361): x = depth-cue width of the nearest strokes (device
    // px, 0 = off), y = 1 draws back edges, z = device px per logical px.
    edges: vec4<f32>,
    // Depth ramp (depth cue and fog): dot(p, xyz) + w is 0 at the nearest
    // scene point, 1 at the farthest.
    cue: vec4<f32>,
    // Fog (#363): x = start, y = end (fractions of the depth ramp), z = 1 on,
    // w = 1 takes the colour from the background behind each pixel.
    fog: vec4<f32>,
    // Fog colour: the custom colour, or the flat background colour.
    fog_color: vec4<f32>,
};

@group(0) @binding(0) var<uniform> camera: Camera;

fn unproject(ndc: vec2<f32>, depth: f32) -> vec3<f32> {
    let p = camera.inv_view_proj * vec4(ndc, depth, 1.0);
    return p.xyz / p.w;
}

// Sky/ground gradient for the SketchUp-style background. `up` is the world-ray
// z component in [-1,1]: a light sky above the horizon fading to a warm ground
// haze below.
fn sky_ground(up: f32) -> vec3<f32> {
    let sky = vec3<f32>(0.62, 0.72, 0.86);
    let horizon = vec3<f32>(0.86, 0.88, 0.90);
    let ground = vec3<f32>(0.52, 0.50, 0.47);
    if (up >= 0.0) {
        return mix(horizon, sky, clamp(up * 1.4, 0.0, 1.0));
    }
    return mix(horizon, ground, clamp(-up * 1.4, 0.0, 1.0));
}

// The opaque gradient ground the grid paints `dist` from the eye, before its
// lines: the down-facing gradient, lifted slightly, hazing with distance.
fn ground_haze(dist: f32) -> vec3<f32> {
    let fade = exp(-dist * 0.008);
    return sky_ground(-clamp(fade, 0.0, 1.0)) + vec3<f32>(0.03, 0.03, 0.03);
}

// The gradient background behind world point `p`, as the grid paints it
// (without its lines): the ground where the view ray through `p` meets z = 0
// inside the frustum, else the sky.
fn gradient_behind(p: vec3<f32>) -> vec3<f32> {
    let c = camera.view_proj * vec4(p, 1.0);
    let ndc = c.xy / c.w;
    let near = unproject(ndc, 0.0);
    let far = unproject(ndc, 1.0);
    let dir = far - near;
    if (abs(dir.z) >= 1e-9) {
        let t = -near.z / dir.z;
        if (t > 0.0 && t < 1.0) {
            return ground_haze(length(near + dir * t - camera.eye.xyz));
        }
    }
    return sky_ground(normalize(dir).z);
}

// How much fog covers world point `p`: 0 before the start, 1 past the end,
// linear between (FogStyle::amount on the CPU). 0 with fog off.
fn fog_amount(p: vec3<f32>) -> f32 {
    if (camera.fog.z < 0.5) {
        return 0.0;
    }
    let t = dot(p, camera.cue.xyz) + camera.cue.w;
    return clamp((t - camera.fog.x) / max(camera.fog.y - camera.fog.x, 1e-4), 0.0, 1.0);
}

// The fog colour at world point `p`.
fn fog_colour(p: vec3<f32>) -> vec3<f32> {
    if (camera.fog.w > 0.5 && camera.light.y > 0.5) {
        return gradient_behind(p);
    }
    return camera.fog_color.rgb;
}

// Straight-alpha colour `c` of world point `p`, fogged. With fog off the
// colour is returned untouched.
fn fogged(c: vec4<f32>, p: vec3<f32>) -> vec4<f32> {
    let f = fog_amount(p);
    if (f <= 0.0) {
        return c;
    }
    return vec4(mix(c.rgb, fog_colour(p), f), c.a);
}
