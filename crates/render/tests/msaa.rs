// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Viewport MSAA (#356): every scene pipeline and the pane target agree on the
//! chosen sample count, and a diagonal edge comes out smoothed.
//!
//! `#[ignore]` like the golden tests: they need a real GPU adapter. Run with:
//!
//!   cargo test -p itsjustcad-render --test msaa -- --ignored

use glam::{Mat4, Vec3};
use itsjustcad_render::{
    camera_uniform, pick_sample_count, supported_sample_counts, DisplayMode, SceneData,
    SceneRenderer, UnderlayData, DEPTH_FORMAT,
};
use kernel_mesh::RenderMesh;

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const W: u32 = 160;
const H: u32 = 120;

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    counts: Vec<u32>,
}

/// A device with adapter-specific format features when offered (as the app
/// requests), so 2x/8x are usable where the GPU has them.
fn gpu() -> Gpu {
    let instance = wgpu::Instance::default();
    let adapter =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .expect("no GPU adapter");
    let required_features =
        adapter.features() & wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES;
    let (device, queue) = pollster::block_on(
        adapter.request_device(&wgpu::DeviceDescriptor { required_features, ..Default::default() }),
    )
    .expect("device");
    let counts = supported_sample_counts(&adapter, device.features(), FORMAT);
    Gpu { device, queue, counts }
}

fn quad_mesh(corners: [[f32; 3]; 3]) -> RenderMesh {
    // Both windings, so back-face culling never hides it.
    RenderMesh {
        positions: corners.to_vec(),
        normals: vec![[0.0, -1.0, 0.0]; 3],
        indices: vec![0, 1, 2, 0, 2, 1],
    }
}

/// Camera at the origin looking up and along +Y: the ground grid (and its
/// partially covered anti-aliased lines) stays below the frame.
fn view_proj() -> (Mat4, Vec3) {
    let eye = Vec3::new(0.0, 0.0, 10.0);
    let view = Mat4::look_at_rh(eye, Vec3::new(0.0, 5.0, 12.0), Vec3::Z);
    let proj = Mat4::perspective_rh(40f32.to_radians(), W as f32 / H as f32, 0.05, 1000.0);
    (proj * view, eye)
}

/// A scene touching every pipeline: meshes, face meshes, curves, edges,
/// profile ribbons, points, underlay and basemap.
fn full_scene() -> SceneData {
    let tri = || quad_mesh([[-1.5, 5.0, 10.5], [1.5, 5.0, 13.5], [1.5, 5.0, 10.5]]);
    let underlay = || UnderlayData {
        rgba: vec![200; 4 * 4 * 4],
        width_px: 4,
        height_px: 4,
        corners: [[-5.0, -5.0, 0.0], [5.0, -5.0, 0.0], [5.0, 5.0, 0.0], [-5.0, 5.0, 0.0]],
        opacity: 0.5,
    };
    SceneData {
        meshes: vec![(tri(), [0.8, 0.2, 0.2, 1.0], [0.5, 0.0])],
        face_meshes: vec![(tri(), vec![[0.1, 0.9, 0.1]; 2], [1.0; 4], [0.5, 0.0])],
        lines: vec![(vec![[-2.0, 5.0, 11.0], [2.0, 5.0, 13.0]], [0.0, 0.0, 0.0, 1.0], 0.5)],
        edges: vec![(vec![[-2.0, 5.0, 12.0], [2.0, 5.0, 12.0]], [0.0, 0.0, 0.0, 1.0], 0.25)],
        profile_edges: vec![(vec![[-2.0, 5.0, 11.5], [2.0, 5.0, 11.5]], [0.0, 0.0, 0.0, 1.0], 0.02)],
        points: vec![(vec![[0.0, 5.0, 12.0]], [0.0, 0.0, 1.0, 1.0])],
        underlay: Some(underlay()),
        basemap: Some(underlay()),
        show_lineweights: true,
    }
}

/// Copy a single-sample RGBA8 texture back to the CPU.
fn read_rgba(g: &Gpu, tex: &wgpu::Texture) -> Vec<u8> {
    let bpr = (W * 4).next_multiple_of(256);
    let buf = g.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: (bpr * H) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = g.device.create_command_encoder(&Default::default());
    enc.copy_texture_to_buffer(
        tex.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bpr),
                rows_per_image: None,
            },
        },
        wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
    );
    g.queue.submit([enc.finish()]);
    let slice = buf.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    g.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).expect("poll");
    let data = slice.get_mapped_range();
    (0..H).flat_map(|y| data[(y * bpr) as usize..][..(W * 4) as usize].to_vec()).collect()
}

/// A renderer at `samples` with `scene` uploaded and the test camera set.
fn renderer(g: &Gpu, scene: &SceneData, samples: u32) -> SceneRenderer {
    let mut r = SceneRenderer::with_samples(&g.device, FORMAT, samples);
    r.set_scene(&g.device, &g.queue, scene, 1);
    let (vp, eye) = view_proj();
    let mut cam = camera_uniform(vp, eye);
    cam.screen = [W as f32, H as f32, 0.0, 0.0];
    r.write_camera(&g.device, &g.queue, 0, &cam);
    r
}

/// A single-sample colour + depth pass shaped like egui's window pass,
/// cleared to `clear`; returns the colour read back.
fn window_pass(g: &Gpu, clear: wgpu::Color, draw: impl FnOnce(&mut wgpu::RenderPass<'static>)) -> Vec<u8> {
    let tex = |format, usage| {
        g.device.create_texture(&wgpu::TextureDescriptor {
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
    let (cv, dv) = (color.create_view(&Default::default()), depth.create_view(&Default::default()));
    let mut enc = g.device.create_command_encoder(&Default::default());
    {
        let mut pass = enc
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &cv,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(clear), store: wgpu::StoreOp::Store },
                    depth_slice: None,
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &dv,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            })
            .forget_lifetime();
        draw(&mut pass);
    }
    g.queue.submit([enc.finish()]);
    read_rgba(g, &color)
}

/// Draw the scene at `samples` into a transparent pane; returns the resolved
/// (premultiplied) RGBA. 1 sample draws directly, as the app does with MSAA off.
fn render(g: &Gpu, scene: &SceneData, samples: u32, mode: DisplayMode) -> Vec<u8> {
    let mut r = renderer(g, scene, samples);
    if samples > 1 {
        let mut enc = g.device.create_command_encoder(&Default::default());
        r.render_msaa_pane(&g.device, &mut enc, 0, [W, H], mode, true);
        g.queue.submit([enc.finish()]);
        let t = r.msaa_target(0).expect("pane target");
        assert_eq!(t.samples(), samples, "target built with the chosen count");
        assert_eq!(t.size(), [W, H]);
        return read_rgba(g, t.resolve_texture());
    }
    window_pass(g, wgpu::Color::TRANSPARENT, |pass| r.paint(pass, 0, mode, true))
}

#[test]
#[ignore = "needs a GPU adapter"]
fn composite_matches_direct_draw_over_background() {
    // The MSAA image composited into an egui-like pass (with its depth
    // attachment) must look like drawing straight onto the background, apart
    // from the smoothed edges.
    let g = gpu();
    let scene = full_scene();
    let bg = wgpu::Color { r: 0.2, g: 0.25, b: 0.3, a: 1.0 };
    let direct_r = renderer(&g, &scene, 1);
    let direct = window_pass(&g, bg, |p| direct_r.paint(p, 0, DisplayMode::Shaded, true));
    let mut msaa_r = renderer(&g, &scene, 4);
    let mut enc = g.device.create_command_encoder(&Default::default());
    msaa_r.render_msaa_pane(&g.device, &mut enc, 0, [W, H], DisplayMode::Shaded, true);
    g.queue.submit([enc.finish()]);
    let comp = window_pass(&g, bg, |p| assert!(msaa_r.composite_msaa_pane(p, 0)));
    let differ = |a: &[u8], b: &[u8]| {
        a.chunks(4)
            .zip(b.chunks(4))
            .filter(|(p, q)| p.iter().zip(q.iter()).any(|(x, y)| x.abs_diff(*y) > 8))
            .count()
    };
    let total = (W * H) as usize;
    let empty = window_pass(&g, bg, |_| {});
    let drawn = differ(&direct, &empty);
    assert!(drawn * 10 > total, "scene barely visible ({drawn}/{total} px)");
    let differ = differ(&direct, &comp);
    eprintln!("drawn {drawn}, msaa-vs-direct differ {differ} of {total}");
    assert!(differ * 20 < total, "{differ}/{total} px differ beyond edge smoothing");
}

#[test]
#[ignore = "needs a GPU adapter"]
fn every_pipeline_and_target_uses_the_chosen_count() {
    let g = gpu();
    assert!(g.counts.contains(&1) && g.counts.contains(&4), "counts {:?}", g.counts);
    let scene = full_scene();
    for &n in &g.counts {
        for mode in [
            DisplayMode::Shaded,
            DisplayMode::Wireframe,
            DisplayMode::XRay,
            DisplayMode::Ghosted,
            DisplayMode::Pencil,
            DisplayMode::HiddenLine,
            DisplayMode::Sketchy,
        ] {
            // A pipeline whose count disagreed with the target would raise a
            // validation error here.
            let scope = g.device.push_error_scope(wgpu::ErrorFilter::Validation);
            render(&g, &scene, n, mode);
            let err = pollster::block_on(scope.pop());
            assert!(err.is_none(), "{n}x {mode:?}: {err:?}");
        }
    }
}

#[test]
#[ignore = "needs a GPU adapter"]
fn sample_count_switch_rebuilds_and_drops_targets() {
    let g = gpu();
    let mut r = SceneRenderer::with_samples(&g.device, FORMAT, 4);
    let (vp, eye) = view_proj();
    r.write_camera(&g.device, &g.queue, 0, &camera_uniform(vp, eye));
    let mut enc = g.device.create_command_encoder(&Default::default());
    r.render_msaa_pane(&g.device, &mut enc, 0, [W, H], DisplayMode::Shaded, true);
    assert_eq!(r.msaa_target(0).map(|t| t.samples()), Some(4));
    // Same size and count: the target is kept (no per-frame reallocation).
    let before = r.msaa_target(0).unwrap() as *const _;
    r.render_msaa_pane(&g.device, &mut enc, 0, [W, H], DisplayMode::Shaded, true);
    assert!(std::ptr::eq(before, r.msaa_target(0).unwrap()));
    // Resize: rebuilt at the new size.
    r.render_msaa_pane(&g.device, &mut enc, 0, [W / 2, H], DisplayMode::Shaded, true);
    assert_eq!(r.msaa_target(0).unwrap().size(), [W / 2, H]);
    // Off: pipelines rebuilt for 1 sample, pane targets freed.
    r.set_sample_count(&g.device, 1);
    assert_eq!(r.sample_count(), 1);
    assert!(r.msaa_target(0).is_none());
    g.queue.submit([enc.finish()]);
}

/// Pixels whose alpha is neither empty nor full: edge coverage.
fn partial(rgba: &[u8]) -> usize {
    rgba.chunks(4).filter(|p| p[3] > 8 && p[3] < 247).count()
}

#[test]
#[ignore = "needs a GPU adapter"]
fn diagonal_edge_is_smoothed() {
    let g = gpu();
    // One opaque triangle with a long diagonal edge, nothing else.
    let scene = SceneData {
        meshes: vec![(
            quad_mesh([[-1.5, 5.0, 10.5], [1.5, 5.0, 13.5], [1.5, 5.0, 10.5]]),
            [0.8, 0.2, 0.2, 1.0],
            [0.5, 0.0],
        )],
        face_meshes: vec![],
        lines: vec![],
        edges: vec![],
        profile_edges: vec![],
        points: vec![],
        underlay: None,
        basemap: None,
        show_lineweights: false,
    };
    let off = render(&g, &scene, 1, DisplayMode::Shaded);
    let solid = off.chunks(4).filter(|p| p[3] == 255).count();
    assert!(solid > 500, "triangle not in frame ({solid} px)");
    assert_eq!(partial(&off), 0, "1x must be aliased (hard 0/255 coverage)");
    let n = pick_sample_count(4, &g.counts);
    let on = render(&g, &scene, n, DisplayMode::Shaded);
    // The diagonal spans ~40+ rows; MSAA blends most of its pixels.
    assert!(partial(&on) >= 30, "{n}x left the edge jagged ({} partial px)", partial(&on));
    // Interior coverage is unchanged by MSAA.
    let solid_on = on.chunks(4).filter(|p| p[3] == 255).count();
    assert!(solid_on.abs_diff(solid) < solid / 10, "{solid_on} vs {solid}");
}
