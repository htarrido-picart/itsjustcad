// Infinite ground grid: fullscreen triangle, ray vs plane z=0 in the fragment
// shader, analytic anti-aliased lines at 1m and 10m, depth written so meshes
// occlude the grid correctly.

// Camera, unproject() and sky_ground() come from common.wgsl.

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

struct FsOut {
    @location(0) color: vec4<f32>,
    @builtin(frag_depth) depth: f32,
};

fn grid_factor(coord: vec2<f32>, spacing: f32) -> f32 {
    let c = coord / spacing;
    // Clamp the derivative away from zero: f32 precision in the unproject can
    // collapse fwidth to 0 in spots, and the resulting NaN discards the pixel
    // (reads as dotted/broken lines).
    let fw = max(fwidth(c), vec2(1e-9));
    let d = abs(fract(c - 0.5) - 0.5) / fw;
    let line = min(d.x, d.y);
    return 1.0 - min(line, 1.0);
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

    // NOTE: the origin axes (the z=0 datum lines, incl. the horizontal datum in
    // Front/Back elevations) are NOT tinted here. An analytic tint from the
    // per-fragment unprojected hit point wanders at far zoom (f32 precision
    // makes hit.xy jitter by meters), rendering the axes as jagged dotted
    // staircases. The renderer draws them as real depth-tested line geometry
    // instead (see `world_axis_lines` in renderer.rs).

    let dist = length(hit - camera.eye.xyz);
    let fade = exp(-dist * 0.008);

    // `grid off` keeps the ground (and its depth) but drops the lines.
    let shown = select(1.0, 0.0, camera.light.z > 0.5);
    let alpha = max(minor * 0.18, major * 0.42) * fade * shown;
    let color = vec3(0.62, 0.64, 0.68);
    if (gradient) {
        // Composite the grid lines over an opaque gradient ground so the sky
        // and ground read as one continuous background (SketchUp look). The
        // ground colour comes from the down-facing gradient, lifted slightly.
        // Fog (#363) fades the lines into the ground, or toward its colour.
        let ground = ground_haze(dist);
        let lit = fogged(vec4(mix(ground, color, clamp(alpha, 0.0, 1.0)), 1.0), hit).rgb;
        var out: FsOut;
        out.color = vec4(lit, 1.0); // opaque
        out.depth = depth;
        return out;
    }

    if (alpha < 0.003) {
        discard;
    }

    var out: FsOut;
    out.color = vec4(fogged(vec4(color, 1.0), hit).rgb * alpha, alpha); // premultiplied
    out.depth = depth;
    return out;
}
