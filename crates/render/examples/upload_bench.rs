// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Scene-upload benchmark (#269): times the snapshot build and the GPU upload
//! on open, after a selection recolor and after one geometry edit, then the
//! frame time while orbiting, and prints wgpu's buffer / bind-group counts.
//! Writes PNGs of a fixed view after open and after the recolor so two builds
//! can be compared pixel for pixel.
//!
//! Usage: cargo run --release -p itsjustcad-render --example upload_bench -- scene.itsjustcad.json out_dir

use std::time::Instant;

use itsjustcad_render::{OrbitCamera, SceneRenderer, Theme, DEPTH_FORMAT};

const W: u32 = 1280;
const H: u32 = 800;
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Process CPU seconds (user + system, all threads).
fn cpu_s() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 * 1e-6;
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

/// Run `f`, then wait for the GPU; print wall and CPU time.
fn timed<T>(label: &str, device: &wgpu::Device, f: impl FnOnce() -> T) -> T {
    let (t, c) = (Instant::now(), cpu_s());
    let out = f();
    device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).expect("poll");
    println!("{label}: {:.1} ms wall, {:.3} cpu-s", t.elapsed().as_secs_f64() * 1e3, cpu_s() - c);
    out
}

fn counts(label: &str, device: &wgpu::Device) {
    let c = device.get_internal_counters().hal;
    println!(
        "{label}: buffers {} bind_groups {} buffer_memory {:.1} MB",
        c.buffers.read(),
        c.bind_groups.read(),
        c.buffer_memory.read() as f64 / 1e6
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let scene_path = args.get(1).expect("scene path");
    let out_dir = std::path::PathBuf::from(args.get(2).map_or(".", |s| s.as_str()));
    let mut session =
        itsjustcad_commands::io::load_file(std::path::Path::new(scene_path)).expect("load scene");
    let theme = Theme::Dark;

    let instance = wgpu::Instance::default();
    let adapter =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .expect("adapter");
    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .expect("device");
    let mut renderer = SceneRenderer::new(&device, FORMAT);
    counts("empty", &device);

    let upload = |label: &str, r: &mut SceneRenderer, doc: &itsjustcad_doc::Document| {
        let scene = timed(&format!("{label} snapshot"), &device, || itsjustcad_render::snapshot(doc, theme));
        timed(&format!("{label} upload"), &device, || {
            r.set_scene(&device, &queue, &scene, 0);
            queue.submit([]);
        });
    };
    upload("open", &mut renderer, &session.doc);
    counts("open", &device);

    let mut camera = OrbitCamera::default();
    if let Some(bb) = session.doc.scene_aabb() {
        let c = bb.center();
        camera.target = glam::Vec3::new(c.x as f32, c.y as f32, c.z as f32);
        camera.distance = (bb.size().length() as f32 * 0.6).max(5.0);
    }
    let target = Target::new(&device);
    target.render(&device, &queue, &mut renderer, &camera, theme);
    target.save(&device, &queue, &out_dir.join("open.png"));

    // Selection recolor: select every 50th object (a generation bump).
    let ids: Vec<_> = session.doc.objects().map(|o| o.id).step_by(50).collect();
    session.doc.selection.extend(ids);
    upload("select", &mut renderer, &session.doc);
    counts("select", &device);
    target.render(&device, &queue, &mut renderer, &camera, theme);
    target.save(&device, &queue, &out_dir.join("select.png"));

    // One geometry edit: add a box.
    session.doc.selection.clear();
    let cmd = itsjustcad_commands::parse("box 0,0,0 1,1,1").expect("parse box");
    session.run(cmd).expect("run box");
    upload("edit", &mut renderer, &session.doc);
    counts("edit", &device);

    // Orbit: 120 frames around the target, each waited on.
    let mut frames = Vec::new();
    let (t, c) = (Instant::now(), cpu_s());
    for i in 0..120 {
        camera.yaw = i as f32 * std::f32::consts::TAU / 120.0;
        let f = Instant::now();
        target.render(&device, &queue, &mut renderer, &camera, theme);
        frames.push(f.elapsed().as_secs_f64() * 1e3);
    }
    frames.sort_by(f64::total_cmp);
    println!(
        "orbit 120 frames: median {:.2} ms, p90 {:.2} ms, total {:.0} ms wall, {:.3} cpu-s",
        frames[60],
        frames[108],
        t.elapsed().as_secs_f64() * 1e3,
        cpu_s() - c
    );
}

struct Target {
    color: wgpu::Texture,
    color_view: wgpu::TextureView,
    depth_view: wgpu::TextureView,
}

impl Target {
    fn new(device: &wgpu::Device) -> Self {
        let tex = |format, usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        let color = tex(FORMAT, wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC);
        let depth = tex(DEPTH_FORMAT, wgpu::TextureUsages::RENDER_ATTACHMENT);
        Self {
            color_view: color.create_view(&Default::default()),
            depth_view: depth.create_view(&Default::default()),
            color,
        }
    }

    fn render(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        renderer: &mut SceneRenderer,
        camera: &OrbitCamera,
        theme: Theme,
    ) {
        let cam = itsjustcad_render::camera_uniform(camera.view_proj(W as f32 / H as f32), camera.eye());
        renderer.write_camera(device, queue, 0, &cam);
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let [r, g, b, a] = theme.background();
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.color_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: r as f64,
                            g: g as f64,
                            b: b as f64,
                            a: a as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth_view,
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
            renderer.paint(&mut pass, 0, itsjustcad_render::DisplayMode::Shaded, true);
        }
        queue.submit([encoder.finish()]);
        device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).expect("poll");
    }

    fn save(&self, device: &wgpu::Device, queue: &wgpu::Queue, path: &std::path::Path) {
        let bytes_per_row = W * 4;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (bytes_per_row * H) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            self.color.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(H),
                },
            },
            wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        );
        queue.submit([encoder.finish()]);
        let slice = readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
        device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).expect("poll");
        let img = image::RgbaImage::from_raw(W, H, slice.get_mapped_range().to_vec()).expect("image");
        img.save(path).expect("write png");
    }
}
