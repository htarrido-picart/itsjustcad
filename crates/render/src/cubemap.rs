//! Cubemap capture + panorama/fisheye remap.
//!
//! Real panoramic and fisheye lenses need a >180° or curved image that no
//! single pinhole `view_proj` can express. So we render the scene six times —
//! once per cube face, a 90° pinhole looking along ±X/±Y/±Z from the eye — into
//! the six layers of a cube texture, then run one fullscreen post pass
//! (`pano.wgsl`) that turns every output pixel into a ray direction and samples
//! the cube. The ray math lives in [`crate::pano`] and is unit-tested there;
//! this file is the GPU plumbing.
//!
//! `render_pano_image` is self-contained (creates its own textures / passes)
//! and returns an `RgbaImage`, so the headless shot path can call it directly
//! for pano/fisheye cameras instead of the ordinary single-pass renderer.

use glam::{Mat4, Vec3};

use crate::pano::{CubeFace, PanoProjection};
use crate::renderer::{DisplayMode, SceneRenderer};
use crate::{camera_uniform_with_mode, snapshot::Theme, OrbitCamera, DEPTH_FORMAT};

const CUBE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Tightly-packed RGBA8 output of a panorama/fisheye remap (row-major, no
/// padding). The `image` crate lives in the app layer, so we hand back raw
/// pixels the caller wraps.
pub struct PanoImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct PanoParams {
    // mat3x3 in a uniform is laid out as 3 x vec4 (std140 column padding).
    basis: [[f32; 4]; 3],
    mode: [f32; 4],
    bg: [f32; 4],
}

/// Render a panorama or fisheye image of `renderer`'s scene from `camera`'s eye.
///
/// `camera.pano` selects the projection; when it is `None` this still works and
/// produces an equirectangular view (callers gate on pano themselves). The
/// output is `width x height` RGBA8. `face_size` is the per-face resolution of
/// the cubemap (higher = sharper remap, slower); 1024 is a good default.
#[allow(clippy::too_many_arguments)]
pub fn render_pano_image(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    renderer: &mut SceneRenderer,
    camera: &OrbitCamera,
    theme: Theme,
    mode: DisplayMode,
    width: u32,
    height: u32,
    face_size: u32,
) -> PanoImage {
    let eye = camera.eye();
    let bg = theme.background();

    // ── 1. Capture the six faces into a cube texture ────────────────────────
    let cube = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("pano_cube"),
        size: wgpu::Extent3d { width: face_size, height: face_size, depth_or_array_layers: 6 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: CUBE_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let depth = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("pano_cube_depth"),
        size: wgpu::Extent3d { width: face_size, height: face_size, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let depth_view = depth.create_view(&Default::default());

    // We reuse the caller's SceneRenderer for geometry (scene already uploaded),
    // but drive our own per-face camera uniform through viewport slot 0.
    let (right, up, forward) = camera.view_basis();
    // Cube-space basis columns (right, up, forward) for the remap. The faces are
    // captured in this same basis, so the remap rotates view rays back into it.
    let basis_cols = [right, up, forward];

    for (layer, face) in CubeFace::ALL.iter().enumerate() {
        // 90° pinhole down the face axis, expressed in the camera's view basis.
        let f_local = face.forward();
        let u_local = face.up();
        let f_world = local_to_world(f_local, right, up, forward);
        let u_world = local_to_world(u_local, right, up, forward);
        let view = Mat4::look_at_rh(eye, eye + f_world, u_world);
        let proj = Mat4::perspective_rh(std::f32::consts::FRAC_PI_2, 1.0, 0.05, 5.0e4);
        let vp = proj * view;
        let cam = camera_uniform_with_mode(vp, eye, mode);
        renderer.write_camera(device, queue, 0, &cam);

        let face_view = cube.create_view(&wgpu::TextureViewDescriptor {
            label: Some("pano_face"),
            dimension: Some(wgpu::TextureViewDimension::D2),
            base_array_layer: layer as u32,
            array_layer_count: Some(1),
            ..Default::default()
        });

        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("pano_face_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &face_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: bg[0] as f64,
                            g: bg[1] as f64,
                            b: bg[2] as f64,
                            a: bg[3] as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let mut pass = pass.forget_lifetime();
            renderer.paint(&mut pass, 0, mode);
        }
        queue.submit([encoder.finish()]);
    }

    // ── 2. Post pass: remap the cube into the output image ──────────────────
    let (mode_id, fov) = match camera.pano {
        Some(PanoProjection::Fisheye { fov }) => (1.0f32, fov),
        _ => (0.0f32, 0.0), // equirect (also the fallback)
    };
    let params = PanoParams {
        basis: [
            [basis_cols[0].x, basis_cols[0].y, basis_cols[0].z, 0.0],
            [basis_cols[1].x, basis_cols[1].y, basis_cols[1].z, 0.0],
            [basis_cols[2].x, basis_cols[2].y, basis_cols[2].z, 0.0],
        ],
        // mode.z carries the output aspect (w/h) so the fisheye disc stays
        // circular; equirect ignores it.
        mode: [mode_id, fov, width as f32 / height.max(1) as f32, 0.0],
        bg,
    };
    remap_cube(device, queue, &cube, &params, width, height)
}

/// Rotate a face-local direction into cube space using the view basis columns.
fn local_to_world(local: Vec3, right: Vec3, up: Vec3, forward: Vec3) -> Vec3 {
    (right * local.x + up * local.y + forward * local.z).normalize()
}

/// Run the fullscreen remap post pass and read back the result as an image.
fn remap_cube(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    cube: &wgpu::Texture,
    params: &PanoParams,
    width: u32,
    height: u32,
) -> PanoImage {
    use wgpu::util::DeviceExt as _;

    let cube_view = cube.create_view(&wgpu::TextureViewDescriptor {
        label: Some("pano_cube_view"),
        dimension: Some(wgpu::TextureViewDimension::Cube),
        ..Default::default()
    });
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("pano_sampler"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        ..Default::default()
    });
    let param_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("pano_params"),
        contents: bytemuck::bytes_of(params),
        usage: wgpu::BufferUsages::UNIFORM,
    });

    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("pano_layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::Cube,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
        ],
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("pano_bg"),
        layout: &layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&cube_view) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
            wgpu::BindGroupEntry { binding: 2, resource: param_buf.as_entire_binding() },
        ],
    });

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("pano_shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shaders/pano.wgsl").into()),
    });
    let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("pano_pl"),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    let out_format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("pano_pipeline"),
        layout: Some(&pl),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: out_format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });

    let out = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("pano_out"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: out_format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let out_view = out.create_view(&Default::default());

    let mut encoder = device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("pano_post_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &out_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.draw(0..3, 0..1);
    }

    let bytes_per_row = (width * 4).next_multiple_of(256);
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("pano_readback"),
        size: (bytes_per_row * height) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        out.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: None,
            },
        },
        wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
    );
    queue.submit([encoder.finish()]);

    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map pano readback"));
    device
        .poll(wgpu::PollType::Wait { submission_index: None, timeout: None })
        .expect("poll pano");
    let data = slice.get_mapped_range();
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        let row = &data[(y * bytes_per_row) as usize..][..(width * 4) as usize];
        for x in 0..width {
            let px = &row[(x * 4) as usize..][..4];
            rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
        }
    }
    drop(data);
    PanoImage { width, height, rgba }
}
