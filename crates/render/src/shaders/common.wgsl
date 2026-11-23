// Shared by the scene shaders (mesh, curve, grid, underlay, ao): prepended to
// each when its module is built (see `scene_shader` in renderer.rs). Holds the
// camera uniform, the background gradient, the fog (#363), the ambient
// occlusion lookup (#358), the sun shadow lookup (#357) and the live
// section cut (#359).

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
    // px, 0 = off), y = 1 draws back edges, z = device px per logical px,
    // w = 1 hides material textures (`textures off`, #360).
    edges: vec4<f32>,
    // Depth ramp (depth cue and fog): dot(p, xyz) + w is 0 at the nearest
    // scene point, 1 at the farthest.
    cue: vec4<f32>,
    // Fog (#363): x = start, y = end (fractions of the depth ramp), z = 1 on,
    // w = 1 takes the colour from the background behind each pixel.
    fog: vec4<f32>,
    // Fog colour: the custom colour, or the flat background colour.
    fog_color: vec4<f32>,
    // Ambient occlusion (#358): x = 1 on, y = intensity, z = requested
    // distance, w = sampling radius in world units.
    ao: vec4<f32>,
    // Colour occluded faces darken toward.
    ao_color: vec4<f32>,
    // Sun shadows (#357): x = 1 on, y = direct light (0..1), z = ambient
    // (0..1), w = 1 the ground catches them.
    shadow: vec4<f32>,
    // Unit direction toward the sun (xyz): the shadows' and the key light's.
    shadow_dir: vec4<f32>,
    // Live section cut (#359): xyz = unit normal toward the removed side,
    // w = minus the offset. All zero = no cut.
    clip: vec4<f32>,
    // Section fill colour (rgb), w = 1 caps the cut solids.
    clip_fill: vec4<f32>,
};

@group(0) @binding(0) var<uniform> camera: Camera;
// The pane's blurred occlusion (0 = open, 1 = fully occluded), see ao.rs.
@group(0) @binding(1) var ao_tex: texture_2d<f32>;

// The sun shadow map shared by every pane (#357), see shadow.rs: the light's
// view-projection and params (x = 1 when it holds casters, y = one texel in
// uv, z = one texel in world units, w = depth bias).
struct ShadowMap {
    light_view_proj: mat4x4<f32>,
    params: vec4<f32>,
};
@group(0) @binding(5) var<uniform> shadow_map: ShadowMap;
@group(0) @binding(6) var shadow_tex: texture_depth_2d;
@group(0) @binding(7) var shadow_cmp: sampler_comparison;

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

// Fill colour `c` of world point `p` darkened by the pane's ambient occlusion
// (#358) toward the AO colour. The texel is found by projecting `p`, so the
// lookup is the same in the MSAA target and in egui's pass. With AO off the
// colour is returned untouched.
fn occluded(c: vec4<f32>, p: vec3<f32>) -> vec4<f32> {
    if (camera.ao.x < 0.5) {
        return c;
    }
    let clip = camera.view_proj * vec4(p, 1.0);
    let ndc = clip.xy / clip.w;
    let dims = vec2<f32>(textureDimensions(ao_tex));
    let px = clamp(vec2(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5) * dims, vec2(0.0), dims - 1.0);
    let occ = textureLoad(ao_tex, vec2<i32>(px), 0).r;
    return vec4(mix(c.rgb, camera.ao_color.rgb, occ * camera.ao.y), c.a);
}

// The Light and Dark sliders (#357) as gains on the direct key and the
// ambient floor: 1 at their defaults (SHADOW_LIGHT 0.8, SHADOW_DARK 0.45),
// so turning shadows on keeps the sunlit faces' shading.
fn shadow_direct() -> f32 {
    return camera.shadow.y / 0.8;
}

fn shadow_ambient() -> f32 {
    return camera.shadow.z / 0.45;
}

// How much sun reaches world point `p` with normal `n` (#357): 1 lit, 0 in
// shadow, 3x3 PCF between. Faces turned from the sun, and every face when it
// is below the horizon, are in shadow. With shadows off, 1.
fn sun_visibility(p: vec3<f32>, n: vec3<f32>) -> f32 {
    if (camera.shadow.x < 0.5) {
        return 1.0;
    }
    let l = camera.shadow_dir.xyz;
    if (l.z <= 0.0 || dot(n, l) <= 0.0) {
        return 0.0;
    }
    if (shadow_map.params.x < 0.5) {
        return 1.0;
    }
    // Normal offset: look up a point lifted off the surface by a texel and a
    // half, against acne on the receiver.
    let c = shadow_map.light_view_proj * vec4(p + n * (shadow_map.params.z * 1.5), 1.0);
    let uv = vec2(c.x * 0.5 + 0.5, 0.5 - c.y * 0.5);
    // Outside the casters' footprint, or nearer the sun than all of them:
    // nothing is in the way.
    if (any(uv < vec2(0.0)) || any(uv > vec2(1.0)) || c.z < 0.0) {
        return 1.0;
    }
    let d = min(c.z, 1.0) - shadow_map.params.w;
    let t = shadow_map.params.y;
    var lit = 0.0;
    for (var y = -1; y <= 1; y++) {
        for (var x = -1; x <= 1; x++) {
            lit += textureSampleCompareLevel(shadow_tex, shadow_cmp, uv + vec2(f32(x), f32(y)) * t, d);
        }
    }
    return lit / 9.0;
}

// How much the ground at world point `p` darkens in shadow (#357): 0 lit, up
// to the share the sun adds to an up-facing face (the same light/dark
// balance as the faces). 0 with shadows or the ground catcher off.
fn ground_shadow(p: vec3<f32>) -> f32 {
    if (camera.shadow.x < 0.5 || camera.shadow.w < 0.5) {
        return 0.0;
    }
    let direct = 0.55 * shadow_direct() * max(camera.shadow_dir.z, 0.0);
    let total = 0.65 * shadow_ambient() + direct;
    if (direct <= 0.0 || total <= 0.0) {
        return 0.0;
    }
    return (1.0 - sun_visibility(p, vec3(0.0, 0.0, 1.0))) * direct / total;
}

// Whether the live section cut (#359) removes world point `p`: on the side
// the plane's normal points at, past a hair of tolerance so a face the plane
// was placed on stays. Always false without a cut (all-zero plane). The
// scene shaders discard such fragments; the grid, the world axes and the
// sun's shadow map are not cut (cut-away geometry still casts shadows).
fn cut_away(p: vec3<f32>) -> bool {
    let d = dot(p, camera.clip.xyz) + camera.clip.w;
    return d > 1e-4 * (1.0 + abs(camera.clip.w));
}
