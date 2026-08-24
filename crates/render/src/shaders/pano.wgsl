// Fullscreen remap of a cubemap into a panorama / fisheye image.
//
// Each output pixel becomes a ray direction (see crate::pano for the CPU-side
// math this mirrors), rotated from the camera view basis into world/cube space,
// then used to sample the cube. mode.x selects the projection:
//   0 = equirectangular (lat/long), full 360x180 sphere
//   1 = fisheye (equidistant), mode.y = fov in radians; pixels outside the
//       image circle are painted with the background colour.

struct Params {
    // column 0 = right, 1 = up, 2 = forward (view basis, in cube space)
    basis: mat3x3<f32>,
    // x = mode (0 equirect, 1 fisheye), y = fisheye fov (rad), z,w spare
    mode: vec4<f32>,
    // background colour for out-of-circle fisheye pixels
    bg: vec4<f32>,
};

@group(0) @binding(0) var cube_tex: texture_cube<f32>;
@group(0) @binding(1) var cube_samp: sampler;
@group(0) @binding(2) var<uniform> params: Params;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>, // 0..1 across the target
};

// Fullscreen triangle.
@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VsOut {
    var out: VsOut;
    let x = f32((vid << 1u) & 2u); // 0,2,0
    let y = f32(vid & 2u);         // 0,0,2
    out.uv = vec2<f32>(x, y);
    out.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    return out;
}

const PI: f32 = 3.14159265358979;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Centred NDC, +y up.
    let ndc = vec2<f32>(in.uv.x * 2.0 - 1.0, 1.0 - in.uv.y * 2.0);

    var dir_view: vec3<f32>;
    if (params.mode.x < 0.5) {
        // Equirectangular.
        let lon = ndc.x * PI;
        let lat = ndc.y * (PI * 0.5);
        let cla = cos(lat);
        dir_view = vec3<f32>(cla * sin(lon), sin(lat), cla * cos(lon));
    } else {
        // Fisheye (equidistant). Correct for aspect so the image disc stays a
        // true circle inscribed in the shorter (vertical) axis; the wider frame
        // shows background at the sides. mode.z = aspect (w/h).
        let fndc = vec2<f32>(ndc.x * params.mode.z, ndc.y);
        let r = length(fndc);
        if (r > 1.0) {
            return params.bg; // outside the image circle
        }
        let theta = r * (params.mode.y * 0.5);
        if (r < 1e-6) {
            dir_view = vec3<f32>(0.0, 0.0, 1.0);
        } else {
            let a = fndc / r;
            dir_view = normalize(vec3<f32>(a.x * sin(theta), a.y * sin(theta), cos(theta)));
        }
    }

    // Rotate view-space ray into cube space: cols are right/up/forward.
    let dir = normalize(params.basis * dir_view);
    return textureSample(cube_tex, cube_samp, dir);
}
