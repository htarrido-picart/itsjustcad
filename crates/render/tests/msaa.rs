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
    camera_uniform, camera_uniform_ex, pick_sample_count, supported_sample_counts, AoStyle, AoUnits, DisplayMode,
    EdgeStyle, FogColor, FogStyle, SceneData, ShadowStyle,
    SceneRenderer, UnderlayData, DEPTH_FORMAT,
};
use itsjustcad_doc::LineType;
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
/// dashed curves and edges, profile ribbons, points, underlay and basemap.
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
        // Dashed curve and dash-dot edge: the dash pipelines (#364).
        lines: vec![(vec![[-2.0, 5.0, 11.0], [2.0, 5.0, 13.0]], [0.0, 0.0, 0.0, 1.0], 0.5)],
        edges: vec![(vec![[-2.0, 5.0, 12.0], [2.0, 5.0, 12.0]], [0.0, 0.0, 0.0, 1.0], 0.25)],
        line_types: vec![LineType::Dashed],
        edge_types: vec![LineType::DashDot],
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
        line_types: vec![],
        edge_types: vec![],
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

#[test]
#[ignore = "needs a GPU adapter"]
fn dashed_lines_draw_gaps_at_every_sample_count() {
    // #364: a dashed curve across the frame covers clearly fewer pixels than
    // the same curve solid, as a hairline and as a fat stroke, with the dash
    // pipelines validating against every sample count.
    let g = gpu();
    let scene = |lt| SceneData {
        meshes: vec![],
        face_meshes: vec![],
        lines: vec![(vec![[-4.0, 5.0, 12.0], [4.0, 5.0, 12.0]], [1.0, 0.0, 0.0, 1.0], 0.25)],
        edges: vec![],
        line_types: vec![lt],
        edge_types: vec![],
        profile_edges: vec![],
        points: vec![],
        underlay: None,
        basemap: None,
        show_lineweights: false,
    };
    let drawn = |rgba: &[u8]| rgba.chunks(4).filter(|p| p[3] > 8).count();
    for &n in &g.counts {
        for mode in [DisplayMode::Shaded, DisplayMode::HiddenLine] {
            let scope = g.device.push_error_scope(wgpu::ErrorFilter::Validation);
            let draw = |lt| {
                let mut r = renderer(&g, &scene(lt), n);
                let (vp, eye) = view_proj();
                let mut cam = camera_uniform(vp, eye).with_overlays(false, false);
                cam.screen = [W as f32, H as f32, mode.line_px(1.0), 1.0];
                r.write_camera(&g.device, &g.queue, 0, &cam);
                if n > 1 {
                    let mut enc = g.device.create_command_encoder(&Default::default());
                    r.render_msaa_pane(&g.device, &mut enc, 0, [W, H], mode, true);
                    g.queue.submit([enc.finish()]);
                    return read_rgba(&g, r.msaa_target(0).expect("target").resolve_texture());
                }
                window_pass(&g, wgpu::Color::TRANSPARENT, |pass| r.paint(pass, 0, mode, true))
            };
            let solid = drawn(&draw(LineType::Continuous));
            let dashed = drawn(&draw(LineType::Dashed));
            let err = pollster::block_on(scope.pop());
            assert!(err.is_none(), "{n}x {mode:?}: {err:?}");
            assert!(solid > W as usize / 2, "{n}x {mode:?}: solid line in frame ({solid} px)");
            assert!(
                dashed * 10 < solid * 9 && dashed * 10 > solid * 3,
                "{n}x {mode:?}: dashed {dashed} px vs solid {solid} px"
            );
        }
    }
}

/// Render `scene` once with `style` at `n` samples in `mode`, the camera
/// sized like a real pane; validation errors fail the test.
fn draw_styled(g: &Gpu, scene: &SceneData, n: u32, mode: DisplayMode, style: &EdgeStyle) -> Vec<u8> {
    let scope = g.device.push_error_scope(wgpu::ErrorFilter::Validation);
    let mut r = renderer(g, scene, n);
    let (vp, eye) = view_proj();
    let mut cam = camera_uniform(vp, eye).with_overlays(false, false);
    cam.screen = [W as f32, H as f32, mode.line_px(1.0), 1.0];
    r.write_camera(&g.device, &g.queue, 0, &cam.with_edge_style(style, 1.0));
    let rgba = if n > 1 {
        let mut enc = g.device.create_command_encoder(&Default::default());
        r.render_msaa_pane(&g.device, &mut enc, 0, [W, H], mode, true);
        g.queue.submit([enc.finish()]);
        read_rgba(g, r.msaa_target(0).expect("target").resolve_texture())
    } else {
        window_pass(g, wgpu::Color::TRANSPARENT, |pass| r.paint(pass, 0, mode, true))
    };
    let err = pollster::block_on(scope.pop());
    assert!(err.is_none(), "{n}x {mode:?}: {err:?}");
    rgba
}

#[test]
#[ignore = "needs a GPU adapter"]
fn back_edges_draw_hidden_edges_dashed_at_every_sample_count() {
    // #361: a red edge behind a grey face shows only with back edges on, and
    // then dashed and faint; never in X-ray. The back pipeline validates
    // against every sample count.
    let g = gpu();
    let face = || quad_mesh([[-6.0, 5.0, 6.0], [6.0, 5.0, 6.0], [0.0, 5.0, 20.0]]);
    let scene = SceneData {
        meshes: vec![(face(), [0.5, 0.5, 0.5, 1.0], [0.5, 0.0])],
        face_meshes: vec![],
        lines: vec![],
        // Red behind the face; blue in front of it, on its own plane.
        edges: vec![
            (vec![[-3.0, 9.0, 12.0], [3.0, 9.0, 12.0]], [1.0, 0.0, 0.0, 1.0], 0.25),
            (vec![[-3.0, 4.0, 10.0], [3.0, 4.0, 10.0]], [0.0, 0.0, 1.0, 1.0], 0.25),
        ],
        line_types: vec![],
        edge_types: vec![],
        profile_edges: vec![],
        points: vec![],
        underlay: None,
        basemap: None,
        show_lineweights: false,
    };
    let blue = |rgba: &[u8]| -> Vec<Vec<u8>> {
        rgba.chunks(4).filter(|p| p[2] as i32 > p[0] as i32 + 20).map(<[u8]>::to_vec).collect()
    };
    let red = |rgba: &[u8]| rgba.chunks(4).filter(|p| p[0] as i32 > p[1] as i32 + 20).count();
    let on = EdgeStyle { back_edges: true, ..Default::default() };
    for &n in &g.counts {
        for mode in [DisplayMode::Shaded, DisplayMode::HiddenLine] {
            let off = red(&draw_styled(&g, &scene, n, mode, &EdgeStyle::default()));
            let back = draw_styled(&g, &scene, n, mode, &on);
            assert_eq!(off, 0, "{n}x {mode:?}: hidden edge occluded by default");
            let plain = draw_styled(&g, &scene, n, mode, &EdgeStyle::default());
            assert_eq!(blue(&back), blue(&plain), "{n}x {mode:?}: visible edge untouched");
            assert!(red(&back) > 20, "{n}x {mode:?}: back edge drawn ({} px)", red(&back));
            // Faint: no fully red pixel over the grey face.
            assert!(back.chunks(4).all(|p| p[0] < 250 || p[1] > 100), "{n}x {mode:?}: faint");
        }
        // X-ray already shows every edge; the back pass stays off there.
        let xr = draw_styled(&g, &scene, n, DisplayMode::XRay, &on);
        assert_eq!(xr, draw_styled(&g, &scene, n, DisplayMode::XRay, &EdgeStyle::default()));
    }
}

#[test]
#[ignore = "needs a GPU adapter"]
fn depth_cue_widens_near_edges_at_every_sample_count() {
    // #361: an edge running from near to far covers more pixels with the
    // depth cue on, mostly at its near end.
    let g = gpu();
    let scene = SceneData {
        meshes: vec![],
        face_meshes: vec![],
        lines: vec![],
        edges: vec![(vec![[-1.0, 2.0, 11.0], [-1.0, 60.0, 30.0]], [1.0, 0.0, 0.0, 1.0], 0.25)],
        line_types: vec![],
        edge_types: vec![],
        profile_edges: vec![],
        points: vec![],
        underlay: None,
        basemap: None,
        show_lineweights: false,
    };
    let drawn = |rgba: &[u8]| rgba.chunks(4).filter(|p| p[3] > 8).count();
    let cue = EdgeStyle { depth_cue: true, depth_cue_px: 6.0, ..Default::default() };
    for &n in &g.counts {
        let plain = drawn(&draw_styled(&g, &scene, n, DisplayMode::Shaded, &EdgeStyle::default()));
        let wide = drawn(&draw_styled(&g, &scene, n, DisplayMode::Shaded, &cue));
        assert!(plain > 10, "{n}x: edge in frame ({plain} px)");
        assert!(wide > plain * 2, "{n}x: depth cue {wide} px vs {plain} px");
    }
}

/// Render `scene` shaded at `n` samples with `fog` (flat background `bg`,
/// or the sky/ground gradient with `gradient`); `fat` draws the strokes as
/// 3 px quads instead of hairlines.
fn draw_fogged(
    g: &Gpu,
    scene: &SceneData,
    n: u32,
    fog: &FogStyle,
    bg: [f32; 4],
    fat: bool,
    gradient: bool,
) -> Vec<u8> {
    let scope = g.device.push_error_scope(wgpu::ErrorFilter::Validation);
    let mut r = renderer(g, scene, n);
    let (vp, eye) = view_proj();
    let mut cam = camera_uniform(vp, eye).with_overlays(false, false);
    cam.screen = [W as f32, H as f32, if fat { 3.0 } else { 0.0 }, 1.0];
    cam.light[1] = if gradient { 1.0 } else { 0.0 };
    r.write_camera(&g.device, &g.queue, 0, &cam.with_fog(fog, bg));
    let mode = DisplayMode::Shaded;
    let rgba = if n > 1 {
        let mut enc = g.device.create_command_encoder(&Default::default());
        r.render_msaa_pane(&g.device, &mut enc, 0, [W, H], mode, true);
        g.queue.submit([enc.finish()]);
        read_rgba(g, r.msaa_target(0).expect("target").resolve_texture())
    } else {
        window_pass(g, wgpu::Color::TRANSPARENT, |pass| r.paint(pass, 0, mode, true))
    };
    let err = pollster::block_on(scope.pop());
    assert!(err.is_none(), "{n}x fog: {err:?}");
    rgba
}

#[test]
#[ignore = "needs a GPU adapter"]
fn fog_fades_far_geometry_and_leaves_near_untouched_at_every_sample_count() {
    // #363: a green face near the eye and a blue face plus a red edge far
    // away. With fog from 30% to 60% of the view depth, the far ones turn
    // the fog colour and the near face keeps every pixel, at every sample
    // count, hairline or fat strokes.
    let g = gpu();
    let wall = |x0: f32, x1: f32, y: f32, z0: f32, z1: f32| RenderMesh {
        positions: vec![[x0, y, z0], [x1, y, z0], [x1, y, z1], [x0, y, z1]],
        normals: vec![[0.0, -1.0, 0.0]; 4],
        indices: vec![0, 1, 2, 0, 2, 3, 0, 2, 1, 0, 3, 2],
    };
    let scene = SceneData {
        meshes: vec![
            (wall(-1.5, -0.2, 4.0, 10.5, 12.5), [0.1, 0.8, 0.1, 1.0], [0.5, 0.0]),
            (wall(2.0, 20.0, 60.0, 25.0, 45.0), [0.1, 0.1, 0.8, 1.0], [0.5, 0.0]),
        ],
        face_meshes: vec![],
        lines: vec![],
        edges: vec![(vec![[-20.0, 60.0, 55.0], [-5.0, 60.0, 55.0]], [1.0, 0.0, 0.0, 1.0], 0.25)],
        line_types: vec![],
        edge_types: vec![],
        profile_edges: vec![],
        points: vec![],
        underlay: None,
        basemap: None,
        show_lineweights: false,
    };
    let green = |p: &[u8]| p[1] as i32 > p[0] as i32 + 40 && p[1] as i32 > p[2] as i32 + 40;
    let blue = |p: &[u8]| p[2] as i32 > p[0] as i32 + 40 && p[2] as i32 > p[1] as i32 + 40;
    let red = |p: &[u8]| p[0] as i32 > p[1] as i32 + 40 && p[0] as i32 > p[2] as i32 + 40;
    let count = |rgba: &[u8], f: &dyn Fn(&[u8]) -> bool| rgba.chunks(4).filter(|p| f(p)).count();
    let custom = FogStyle {
        on: true,
        start: 0.3,
        end: 0.6,
        color: FogColor::Custom,
        custom_rgb: [255, 255, 0],
    };
    let bg = [0.2, 0.2, 0.2, 1.0];
    for &n in &g.counts {
        for fat in [false, true] {
            let plain = draw_fogged(&g, &scene, n, &FogStyle::default(), bg, fat, false);
            let tag = format!("{n}x fat={fat}");
            assert!(count(&plain, &green) > 50, "{tag}: near face in frame");
            assert!(count(&plain, &blue) > 50, "{tag}: far face in frame");
            assert!(count(&plain, &red) > 5, "{tag}: far edge in frame");
            // Fog off is the default camera: identical to no fog at all.
            let off = FogStyle { on: false, ..custom };
            assert_eq!(draw_fogged(&g, &scene, n, &off, bg, fat, false), plain, "{tag}: off");

            let fogged = draw_fogged(&g, &scene, n, &custom, bg, fat, false);
            for (a, b) in plain.chunks(4).zip(fogged.chunks(4)) {
                if green(a) {
                    assert_eq!(a, b, "{tag}: near face untouched");
                }
                if blue(a) && a[3] == 255 {
                    assert!(b[0] > 240 && b[1] > 240 && b[2] < 15, "{tag}: far face is fog {b:?}");
                }
            }
            assert_eq!(count(&fogged, &blue), 0, "{tag}: no blue left");
            assert_eq!(count(&fogged, &red), 0, "{tag}: far edge fogged");

            // Background colour (gradient off): the flat background.
            let bg_fog = FogStyle { color: FogColor::Background, ..custom };
            let into_bg = draw_fogged(&g, &scene, n, &bg_fog, bg, fat, false);
            for (a, b) in plain.chunks(4).zip(into_bg.chunks(4)) {
                if blue(a) && a[3] == 255 {
                    assert!(b[..3].iter().all(|&c| (50..=53).contains(&c)), "{tag}: far face is bg {b:?}");
                }
            }
            // Background colour over the gradient: the sky behind the face
            // (light, bluish), not the flat colour.
            let sky = draw_fogged(&g, &scene, n, &bg_fog, bg, fat, true);
            for (a, b) in plain.chunks(4).zip(sky.chunks(4)) {
                if blue(a) && a[3] == 255 {
                    assert!(b[0] > 150 && b[2] > b[0] && b[2] > 200, "{tag}: far face is sky {b:?}");
                }
            }
        }
    }
}

/// A box-corner scene for ambient occlusion (#358): a floor (z = 0) meeting
/// a wall (y = 4) that faces the camera, plus the camera looking down at the
/// inside corner from the front.
fn corner_scene() -> (SceneData, Mat4, Vec3) {
    let quad = |c: [[f32; 3]; 4], n: [f32; 3]| RenderMesh {
        positions: c.to_vec(),
        normals: vec![n; 4],
        // Both windings, so back-face culling never hides it.
        indices: vec![0, 1, 2, 0, 2, 3, 0, 2, 1, 0, 3, 2],
    };
    let floor = quad([[-4.0, -2.0, 0.0], [4.0, -2.0, 0.0], [4.0, 4.0, 0.0], [-4.0, 4.0, 0.0]], [0.0, 0.0, 1.0]);
    let wall = quad([[-4.0, 4.0, 0.0], [4.0, 4.0, 0.0], [4.0, 4.0, 4.0], [-4.0, 4.0, 4.0]], [0.0, -1.0, 0.0]);
    let grey = [0.8, 0.8, 0.8, 1.0];
    let scene = SceneData {
        meshes: vec![(floor, grey, [0.5, 0.0]), (wall, grey, [0.5, 0.0])],
        face_meshes: vec![],
        lines: vec![],
        edges: vec![],
        line_types: vec![],
        edge_types: vec![],
        profile_edges: vec![],
        points: vec![],
        underlay: None,
        basemap: None,
        show_lineweights: false,
    };
    let eye = Vec3::new(0.0, -5.0, 5.0);
    let view = Mat4::look_at_rh(eye, Vec3::new(0.0, 3.0, 1.0), Vec3::Z);
    let proj = Mat4::perspective_rh(50f32.to_radians(), W as f32 / H as f32, 0.05, 1000.0);
    (scene, proj * view, eye)
}

/// Render the corner scene at `n` samples in `mode` with `ao`; validation
/// errors fail the test.
fn draw_ao(g: &Gpu, n: u32, mode: DisplayMode, ao: &AoStyle) -> Vec<u8> {
    let (scene, vp, eye) = corner_scene();
    let scope = g.device.push_error_scope(wgpu::ErrorFilter::Validation);
    let mut r = SceneRenderer::with_samples(&g.device, FORMAT, n);
    r.set_scene(&g.device, &g.queue, &scene, 1);
    let mut cam = camera_uniform(vp, eye).with_overlays(false, false);
    cam.screen = [W as f32, H as f32, 0.0, 1.0];
    r.write_camera(&g.device, &g.queue, 0, &cam.with_ao(ao, mode));
    let mut enc = g.device.create_command_encoder(&Default::default());
    r.render_ao(&g.device, &g.queue, &mut enc, 0, [W, H]);
    assert_eq!(r.ao_target(0).is_some(), ao.on && mode.takes_ao(), "{n}x {mode:?}: target only with AO");
    let rgba = if n > 1 {
        r.render_msaa_pane(&g.device, &mut enc, 0, [W, H], mode, false);
        g.queue.submit([enc.finish()]);
        read_rgba(g, r.msaa_target(0).expect("target").resolve_texture())
    } else {
        g.queue.submit([enc.finish()]);
        window_pass(g, wgpu::Color::TRANSPARENT, |pass| r.paint(pass, 0, mode, false))
    };
    let err = pollster::block_on(scope.pop());
    assert!(err.is_none(), "{n}x {mode:?} ao: {err:?}");
    rgba
}

#[test]
#[ignore = "needs a GPU adapter"]
fn ao_darkens_the_concave_corner_not_open_faces_at_every_sample_count() {
    // #358: with AO on, the floor/wall crease darkens clearly while the open
    // middle of the floor and the top of the wall keep their shade, at every
    // sample count. AO off (or a mode without AO) is today's image exactly.
    let g = gpu();
    let (_, vp, _) = corner_scene();
    let pixel = |p: Vec3| {
        let ndc = vp.project_point3(p);
        let x = ((ndc.x * 0.5 + 0.5) * W as f32) as usize;
        let y = ((0.5 - ndc.y * 0.5) * H as f32) as usize;
        (y * W as usize + x) * 4
    };
    let luma = |rgba: &[u8], p: Vec3| {
        let i = pixel(p);
        rgba[i] as i32 + rgba[i + 1] as i32 + rgba[i + 2] as i32
    };
    let on = AoStyle { on: true, distance: 1.0, units: AoUnits::World, intensity: 1.0, ..Default::default() };
    let corner = [Vec3::new(-1.0, 3.95, 0.08), Vec3::new(1.0, 3.9, 0.05), Vec3::new(0.0, 3.97, 0.25)];
    let open = [Vec3::new(0.0, 0.5, 0.0), Vec3::new(-1.5, 1.0, 0.0), Vec3::new(0.5, 4.0, 3.0)];
    for &n in &g.counts {
        for mode in [DisplayMode::Shaded, DisplayMode::Pencil] {
            let plain = draw_ao(&g, n, mode, &AoStyle::default());
            let ao = draw_ao(&g, n, mode, &on);
            for p in corner {
                let (a, b) = (luma(&plain, p), luma(&ao, p));
                assert!(b * 10 < a * 8, "{n}x {mode:?}: corner {p} darkens ({a} -> {b})");
            }
            for p in open {
                let (a, b) = (luma(&plain, p), luma(&ao, p));
                assert!(a.abs_diff(b) <= 6, "{n}x {mode:?}: open face {p} untouched ({a} -> {b})");
            }
            // Off with other settings changed is the default image.
            assert_eq!(draw_ao(&g, n, mode, &AoStyle { on: false, ..on }), plain, "{n}x {mode:?}: off");
        }
        for mode in [DisplayMode::XRay, DisplayMode::HiddenLine, DisplayMode::Wireframe, DisplayMode::Ghosted] {
            let plain = draw_ao(&g, n, mode, &AoStyle::default());
            assert_eq!(draw_ao(&g, n, mode, &on), plain, "{n}x {mode:?}: skipped");
        }
    }
}

#[test]
#[ignore = "needs a GPU adapter"]
fn ao_targets_rebuild_only_on_resize_and_free_when_off() {
    let g = gpu();
    let (scene, vp, eye) = corner_scene();
    let mut r = SceneRenderer::new(&g.device, FORMAT);
    r.set_scene(&g.device, &g.queue, &scene, 1);
    let on = AoStyle { on: true, ..Default::default() };
    let cam = camera_uniform(vp, eye);
    r.write_camera(&g.device, &g.queue, 0, &cam.with_ao(&on, DisplayMode::Shaded));
    let mut enc = g.device.create_command_encoder(&Default::default());
    r.render_ao(&g.device, &g.queue, &mut enc, 0, [W, H]);
    let before = r.ao_target(0).expect("target") as *const _;
    // A setting change only rewrites the camera.
    let stronger = AoStyle { intensity: 1.0, ..on };
    r.write_camera(&g.device, &g.queue, 0, &cam.with_ao(&stronger, DisplayMode::Shaded));
    r.render_ao(&g.device, &g.queue, &mut enc, 0, [W, H]);
    assert!(std::ptr::eq(before, r.ao_target(0).unwrap()), "kept across frames and settings");
    r.render_ao(&g.device, &g.queue, &mut enc, 0, [W / 2, H]);
    assert_eq!(r.ao_target(0).unwrap().size(), [W / 2, H], "rebuilt on resize");
    r.write_camera(&g.device, &g.queue, 0, &cam);
    r.render_ao(&g.device, &g.queue, &mut enc, 0, [W / 2, H]);
    assert!(r.ao_target(0).is_none(), "freed when off");
    g.queue.submit([enc.finish()]);
}

/// A closed box `lo..hi`, both windings per face (outward normals), so back
/// faces never cull it away.
fn cuboid(lo: Vec3, hi: Vec3) -> RenderMesh {
    let (mut positions, mut normals, mut indices) = (Vec::new(), Vec::new(), Vec::new());
    for axis in 0..3 {
        for side in [0.0f32, 1.0] {
            let (b, c) = ((axis + 1) % 3, (axis + 2) % 3);
            let i = positions.len() as u32;
            for (u, v) in [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)] {
                let mut p = lo;
                p[axis] = lo[axis] + (hi[axis] - lo[axis]) * side;
                p[b] = lo[b] + (hi[b] - lo[b]) * u;
                p[c] = lo[c] + (hi[c] - lo[c]) * v;
                positions.push(p.to_array());
                let mut n = [0.0; 3];
                n[axis] = side * 2.0 - 1.0;
                normals.push(n);
            }
            indices.extend([i, i + 1, i + 2, i, i + 2, i + 3, i, i + 2, i + 1, i, i + 3, i + 2]);
        }
    }
    RenderMesh { positions, normals, indices }
}

/// A 1 x 1 x 2 box standing at the origin, optionally on a floor face, seen
/// from the south above; the sun stands in the east at 45°.
fn box_scene(floor: bool) -> (SceneData, Mat4, Vec3) {
    let grey = [0.8, 0.8, 0.8, 1.0];
    let mut meshes = vec![(cuboid(Vec3::new(-0.5, -0.5, 0.0), Vec3::new(0.5, 0.5, 2.0)), grey, [0.5, 0.0])];
    if floor {
        meshes.push((cuboid(Vec3::new(-4.0, -4.0, -0.2), Vec3::new(4.0, 4.0, 0.0)), grey, [0.5, 0.0]));
    }
    let scene = SceneData {
        meshes,
        face_meshes: vec![],
        lines: vec![],
        edges: vec![],
        line_types: vec![],
        edge_types: vec![],
        profile_edges: vec![],
        points: vec![],
        underlay: None,
        basemap: None,
        show_lineweights: false,
    };
    let eye = Vec3::new(0.0, -7.0, 6.0);
    let view = Mat4::look_at_rh(eye, Vec3::new(0.0, 0.0, 0.5), Vec3::Z);
    let proj = Mat4::perspective_rh(50f32.to_radians(), W as f32 / H as f32, 0.05, 1000.0);
    (scene, proj * view, eye)
}

const EAST_SUN: [f32; 3] = [std::f32::consts::FRAC_1_SQRT_2, 0.0, std::f32::consts::FRAC_1_SQRT_2];

/// Render `scene` at `n` samples in `mode` with `shadows` from `sun`, over
/// the gradient ground (or the flat clear); validation errors fail the test.
fn draw_shadow(
    g: &Gpu,
    n: u32,
    mode: DisplayMode,
    (scene, vp, eye): &(SceneData, Mat4, Vec3),
    shadows: Option<&ShadowStyle>,
    gradient: bool,
) -> Vec<u8> {
    let scope = g.device.push_error_scope(wgpu::ErrorFilter::Validation);
    let mut r = SceneRenderer::with_samples(&g.device, FORMAT, n);
    r.set_scene(&g.device, &g.queue, scene, 1);
    let mut cam = camera_uniform_ex(*vp, *eye, mode, Default::default(), Some(EAST_SUN), gradient)
        .with_overlays(false, false);
    cam.screen = [W as f32, H as f32, 0.0, 1.0];
    if let Some(s) = shadows {
        cam = cam.with_shadows(s, mode, Some(EAST_SUN));
    }
    r.write_camera(&g.device, &g.queue, 0, &cam);
    let mut enc = g.device.create_command_encoder(&Default::default());
    r.render_shadow(&g.device, &g.queue, &mut enc, 0);
    assert_eq!(r.has_shadow_map(), cam.has_shadows(), "{n}x {mode:?}: map only with shadows");
    let rgba = if n > 1 {
        r.render_msaa_pane(&g.device, &mut enc, 0, [W, H], mode, false);
        g.queue.submit([enc.finish()]);
        read_rgba(g, r.msaa_target(0).expect("target").resolve_texture())
    } else {
        g.queue.submit([enc.finish()]);
        window_pass(g, wgpu::Color::TRANSPARENT, |pass| r.paint(pass, 0, mode, false))
    };
    let err = pollster::block_on(scope.pop());
    assert!(err.is_none(), "{n}x {mode:?} shadows: {err:?}");
    rgba
}

#[test]
#[ignore = "needs a GPU adapter"]
fn box_casts_a_shadow_on_the_anti_sun_side_at_every_sample_count() {
    // #357: with the sun in the east, the box shades the ground (the grid's
    // catcher, and a floor face) to its west and not to its east, at every
    // sample count. Shadows off (or a mode without them) is today's image.
    let g = gpu();
    let on = ShadowStyle { on: true, ..Default::default() };
    let px = |vp: Mat4, p: Vec3| {
        let ndc = vp.project_point3(p);
        let x = ((ndc.x * 0.5 + 0.5) * W as f32) as usize;
        let y = ((0.5 - ndc.y * 0.5) * H as f32) as usize;
        (y * W as usize + x) * 4
    };
    let luma = |rgba: &[u8], i: usize| rgba[i] as i32 + rgba[i + 1] as i32 + rgba[i + 2] as i32;
    // West (in the shadow) and east (sunlit) of the box, on the ground.
    let (west, east) = (Vec3::new(-1.6, 0.0, 0.0), Vec3::new(1.6, 0.0, 0.0));
    for &n in &g.counts {
        for floor in [false, true] {
            let s = box_scene(floor);
            let vp = s.1;
            for mode in [DisplayMode::Shaded, DisplayMode::Pencil] {
                let plain = draw_shadow(&g, n, mode, &s, None, true);
                let lit = draw_shadow(&g, n, mode, &s, Some(&on), true);
                let (a, b) = (luma(&plain, px(vp, west)), luma(&lit, px(vp, west)));
                assert!(b * 10 < a * 9, "{n}x {mode:?} floor {floor}: west in shadow ({a} -> {b})");
                let (a, b) = (luma(&plain, px(vp, east)), luma(&lit, px(vp, east)));
                // Sunlit: only the key's angle moves (the sun replaces the
                // Working key), a few percent at most.
                assert!(a.abs_diff(b) * 100 <= a.unsigned_abs() * 3 + 6, "{n}x {mode:?} floor {floor}: east sunlit ({a} -> {b})");
                let west_east = (luma(&lit, px(vp, west)), luma(&lit, px(vp, east)));
                assert!(west_east.0 * 10 < west_east.1 * 9, "{n}x {mode:?} floor {floor}: {west_east:?}");
                // Off with other settings changed is the default image.
                let off = ShadowStyle { on: false, light: 0.1, dark: 0.9, ground: false };
                assert_eq!(draw_shadow(&g, n, mode, &s, Some(&off), true), plain, "{n}x {mode:?}: off");
            }
            for mode in [DisplayMode::XRay, DisplayMode::HiddenLine, DisplayMode::Wireframe, DisplayMode::Ghosted] {
                let plain = draw_shadow(&g, n, mode, &s, None, true);
                assert_eq!(draw_shadow(&g, n, mode, &s, Some(&on), true), plain, "{n}x {mode:?}: skipped");
            }
        }
        // Over a flat background the ground's shadow is translucent black;
        // `shadows ground off` leaves the background alone.
        let s = box_scene(false);
        let plain = draw_shadow(&g, n, DisplayMode::Shaded, &s, None, false);
        let lit = draw_shadow(&g, n, DisplayMode::Shaded, &s, Some(&on), false);
        let i = px(s.1, west) + 3;
        assert!(lit[i] > plain[i] + 40, "{n}x flat: west alpha {} -> {}", plain[i], lit[i]);
        let no_ground = ShadowStyle { ground: false, ..on };
        let bare = draw_shadow(&g, n, DisplayMode::Shaded, &s, Some(&no_ground), false);
        assert_eq!(bare[i - 3..=i], plain[i - 3..=i], "{n}x: ground off leaves the background");
    }
}

#[test]
#[ignore = "needs a GPU adapter"]
fn shadow_map_redraws_only_for_geometry_or_sun() {
    // #357: orbiting never redraws the map; a sun or geometry change does,
    // and turning every pane's shadows off frees it.
    let g = gpu();
    let (scene, vp, eye) = box_scene(true);
    let mut r = SceneRenderer::new(&g.device, FORMAT);
    r.set_scene(&g.device, &g.queue, &scene, 1);
    let on = ShadowStyle { on: true, ..Default::default() };
    let mut enc = g.device.create_command_encoder(&Default::default());
    let mut frame = |r: &mut SceneRenderer, vp: Mat4, sun: [f32; 3], on: &ShadowStyle| {
        let cam = camera_uniform(vp, eye).with_shadows(on, DisplayMode::Shaded, Some(sun));
        r.write_camera(&g.device, &g.queue, 0, &cam);
        r.render_shadow(&g.device, &g.queue, &mut enc, 0);
    };
    frame(&mut r, vp, EAST_SUN, &on);
    assert_eq!(r.shadow_map_renders(), 1);
    for yaw in 1..10 {
        frame(&mut r, vp * Mat4::from_rotation_z(yaw as f32 * 0.3), EAST_SUN, &on);
    }
    frame(&mut r, vp, EAST_SUN, &ShadowStyle { light: 0.2, dark: 0.7, ..on });
    assert_eq!(r.shadow_map_renders(), 1, "camera and slider changes keep the map");
    r.set_scene(&g.device, &g.queue, &scene, 2);
    frame(&mut r, vp, EAST_SUN, &on);
    assert_eq!(r.shadow_map_renders(), 1, "an unchanged re-upload keeps the map");
    frame(&mut r, vp, [0.0, -0.6, 0.8], &on);
    assert_eq!(r.shadow_map_renders(), 2, "a new sun redraws");
    r.set_scene(&g.device, &g.queue, &box_scene(false).0, 3);
    frame(&mut r, vp, [0.0, -0.6, 0.8], &on);
    assert_eq!(r.shadow_map_renders(), 3, "new geometry redraws");
    frame(&mut r, vp, [0.0, -0.6, 0.8], &ShadowStyle::default());
    assert!(!r.has_shadow_map(), "freed when off");
    g.queue.submit([enc.finish()]);
}
