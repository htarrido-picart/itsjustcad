// Infinite ground grid: fullscreen triangle, ray vs plane z=0 in the fragment
// shader, analytic anti-aliased lines at 1m and 10m, depth written so meshes
// occlude the grid correctly.

struct Camera {
    view_proj: mat4x4<f32>,
    inv_view_proj: mat4x4<f32>,
    eye: vec4<f32>,
    misc: vec4<f32>,
    // x = lighting mode, y = background gradient flag (1 = sky/ground gradient).
    light: vec4<f32>,
};

@group(0) @binding(0) var<uniform> camera: Camera;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) ndc: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    // Fullscreen triangle
    var p = array<vec2<f32>, 3>(
        vec2(-1.0, -1.0),
        vec2(3.0, -1.0),
        vec2(-1.0, 3.0),
    );
    var out: VsOut;
    out.clip = vec4(p[vi], 0.9999, 1.0);
    out.ndc = p[vi];
    return out;
}

fn unproject(ndc: vec2<f32>, depth: f32) -> vec3<f32> {
    let p = camera.inv_view_proj * vec4(ndc, depth, 1.0);
    return p.xyz / p.w;
}

struct FsOut {
    @location(0) color: vec4<f32>,
    @builtin(frag_depth) depth: f32,
};

fn grid_factor(coord: vec2<f32>, spacing: f32) -> f32 {
    let c = coord / spacing;
    let d = abs(fract(c - 0.5) - 0.5) / fwidth(c);
    let line = min(d.x, d.y);
    return 1.0 - min(line, 1.0);
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

@fragment
fn fs_main(in: VsOut) -> FsOut {
    let near = unproject(in.ndc, 0.0);
    let far = unproject(in.ndc, 1.0);
    let dir = far - near;

    let gradient = camera.light.y > 0.5;

    // Intersect z = 0
    if (abs(dir.z) < 1e-9) {
        if (gradient) {
            var out: FsOut;
            out.color = vec4(sky_ground(normalize(dir).z), 1.0);
            out.depth = 0.9999;
            return out;
        }
        discard;
    }
    let t = -near.z / dir.z;
    if (t <= 0.0 || t >= 1.0) {
        // Ray does not hit the ground within the view frustum → sky region.
        if (gradient) {
            var out: FsOut;
            out.color = vec4(sky_ground(normalize(dir).z), 1.0);
            out.depth = 0.9999;
            return out;
        }
        discard;
    }
    let hit = near + dir * t;

    let clip = camera.view_proj * vec4(hit, 1.0);
    let depth = clip.z / clip.w;

    let minor = grid_factor(hit.xy, 1.0);
    let major = grid_factor(hit.xy, 10.0);

    // Axes tint: X red-ish, Y green-ish along the origin lines
    let ax = 1.0 - min(abs(hit.y) / fwidth(hit.y), 1.0);
    let ay = 1.0 - min(abs(hit.x) / fwidth(hit.x), 1.0);

    let dist = length(hit - camera.eye.xyz);
    let fade = exp(-dist * 0.008);

    var alpha = max(minor * 0.18, major * 0.42) * fade;
    var color = vec3(0.62, 0.64, 0.68);
    if (ax > 0.0) {
        color = mix(color, vec3(0.85, 0.25, 0.25), ax);
        alpha = max(alpha, ax * 0.8 * fade);
    }
    if (ay > 0.0) {
        color = mix(color, vec3(0.25, 0.75, 0.30), ay);
        alpha = max(alpha, ay * 0.8 * fade);
    }
    if (gradient) {
        // Composite the grid lines over an opaque gradient ground so the sky
        // and ground read as one continuous background (SketchUp look). The
        // ground colour comes from the down-facing gradient, lifted slightly.
        let ground = sky_ground(-clamp(fade, 0.0, 1.0)) + vec3<f32>(0.03, 0.03, 0.03);
        let lit = mix(ground, color, clamp(alpha, 0.0, 1.0));
        var out: FsOut;
        out.color = vec4(lit, 1.0); // opaque
        out.depth = depth;
        return out;
    }

    if (alpha < 0.003) {
        discard;
    }

    var out: FsOut;
    out.color = vec4(color * alpha, alpha); // premultiplied
    out.depth = depth;
    return out;
}
