//! Headless smoke render: draws a scene to an offscreen texture and writes a
//! PNG. Lets CI (and agent sessions without a WindowServer) verify pixels.
//!
//! Usage: cargo run -p mydrafter-render --example headless [-- out.png [scene.mydrafter.json]]

use glam::DVec3;
use mydrafter_render::{OrbitCamera, SceneRenderer, ViewportCallback, DEPTH_FORMAT};

const W: u32 = 1280;
const H: u32 = 800;
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

fn main() {
    let out = std::env::args().nth(1).unwrap_or("/tmp/mydrafter_headless.png".into());
    let theme = match std::env::var("MYDRAFTER_THEME").as_deref() {
        Ok("light") => mydrafter_render::Theme::Light,
        _ => mydrafter_render::Theme::Dark,
    };

    let instance = wgpu::Instance::default();
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
        .expect("adapter");
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
        .expect("device");

    let mut renderer = SceneRenderer::new(&device, FORMAT);

    let mut camera = OrbitCamera::default();
    if let Some(scene_path) = std::env::args().nth(2) {
        let session = mydrafter_commands::io::load_file(std::path::Path::new(&scene_path))
            .expect("load scene");
        let scene = mydrafter_render::snapshot(&session.doc, theme);
        renderer.set_scene(&device, &queue, &scene, 0);
        if let Some(bb) = session.doc.scene_aabb() {
            let center = bb.center();
            camera.target = glam::Vec3::new(center.x as f32, center.y as f32, center.z as f32);
            camera.distance = (bb.size().length() as f32 * 1.2).max(5.0);
        }
    } else {
        let meshes = vec![(
            kernel_mesh::make_box(DVec3::new(-2.5, -2.5, 0.0), DVec3::new(5.0, 5.0, 3.0))
                .to_render(),
            [0.72, 0.73, 0.78, 1.0f32],
        )];
        renderer.set_meshes(&device, &meshes, 0);
    }

    let aspect = W as f32 / H as f32;
    let view_proj = camera.view_proj(aspect);
    let eye = camera.eye();
    let cam = mydrafter_render::camera_uniform(view_proj, eye);
    renderer.write_camera(&device, &queue, 0, &cam);

    let color = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("headless_color"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let depth = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("headless_depth"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let color_view = color.create_view(&Default::default());
    let depth_view = depth.create_view(&Default::default());

    let mut encoder = device.create_command_encoder(&Default::default());
    {
        let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("headless_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &color_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear({
                        let [r, g, b, a] = theme.background();
                        wgpu::Color { r: r as f64, g: g as f64, b: b as f64, a: a as f64 }
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
        renderer.paint(&mut pass, 0, mydrafter_render::DisplayMode::Shaded);
    }

    let bytes_per_row = (W * 4).next_multiple_of(256);
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (bytes_per_row * H) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        color.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: None,
            },
        },
        wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
    );
    queue.submit([encoder.finish()]);

    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .expect("poll");
    let data = slice.get_mapped_range();

    let mut img = image::RgbaImage::new(W, H);
    for y in 0..H {
        let row = &data[(y * bytes_per_row) as usize..][..(W * 4) as usize];
        for x in 0..W {
            let px = &row[(x * 4) as usize..][..4];
            img.put_pixel(x, y, image::Rgba([px[0], px[1], px[2], 255]));
        }
    }
    img.save(&out).expect("write png");
    println!("wrote {out}");

    // keep ViewportCallback linked so the example exercises the public API surface
    let _ = ViewportCallback {
        view_proj,
        eye,
        generation: 0,
        scene: None,
        viewport: 0,
        mode: mydrafter_render::DisplayMode::Shaded,
        sun_dir: None,
    };
}
