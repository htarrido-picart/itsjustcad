//! Golden-image regression tests.
//!
//! These tests are `#[ignore]` so they do not run in normal `cargo test`. Run
//! them with:
//!
//!   cargo test --workspace -- --ignored golden
//!
//! The first time (or after intentional visual changes), bless the images:
//!
//!   BLESS=1 cargo test --workspace -- --ignored golden
//!
//! Blessed PNGs live in `tests/golden/`. Commit them. The pixel tolerance is
//! ±2 per channel to absorb driver/platform rounding on the same scene.

use std::path::{Path, PathBuf};

use glam::DVec3;
use mydrafter_render::{OrbitCamera, SceneRenderer, Theme, DEPTH_FORMAT};
use wgpu::TextureFormat;

const W: u32 = 640;
const H: u32 = 400;
const FORMAT: TextureFormat = TextureFormat::Rgba8UnormSrgb;
/// Maximum per-channel absolute difference allowed before a pixel fails.
const PIXEL_TOLERANCE: u8 = 2;

// ── helpers ──────────────────────────────────────────────────────────────────

fn golden_dir() -> PathBuf {
    // crates/render/tests/golden.rs → repo root → tests/golden/
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("tests/golden")
}

struct GpuContext {
    device: wgpu::Device,
    queue: wgpu::Queue,
}

impl GpuContext {
    fn new() -> Self {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(
            instance.request_adapter(&wgpu::RequestAdapterOptions::default()),
        )
        .expect("no GPU adapter — run with a software renderer (LIBGL_ALWAYS_SOFTWARE=1 on Linux)");
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .expect("device");
        Self { device, queue }
    }
}

/// Render `renderer` to an RGBA image.
fn render_to_image(ctx: &GpuContext, renderer: &SceneRenderer, theme: Theme) -> image::RgbaImage {
    let color = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("golden_color"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let depth = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("golden_depth"),
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

    let mut encoder = ctx.device.create_command_encoder(&Default::default());
    {
        let bg = theme.background();
        let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("golden_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &color_view,
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
        renderer.paint(&mut pass, 0, mydrafter_render::DisplayMode::Shaded);
    }

    let bytes_per_row = (W * 4).next_multiple_of(256);
    let readback = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("golden_readback"),
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
    ctx.queue.submit([encoder.finish()]);

    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    ctx.device
        .poll(wgpu::PollType::Wait { submission_index: None, timeout: None })
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
    img
}

/// Compare `actual` against the blessed PNG at `path`.
/// If `BLESS=1` is set, write the blessed file instead of comparing.
fn check_or_bless(actual: &image::RgbaImage, name: &str) {
    let path = golden_dir().join(format!("{name}.png"));
    if std::env::var("BLESS").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).expect("create golden dir");
        actual.save(&path).expect("write blessed PNG");
        println!("blessed {}", path.display());
        return;
    }
    assert!(
        path.exists(),
        "no blessed image at {path:?} — run with BLESS=1 to create it"
    );
    let blessed = image::open(&path).expect("read blessed PNG").into_rgba8();
    assert_eq!(
        (actual.width(), actual.height()),
        (blessed.width(), blessed.height()),
        "{name}: image dimensions changed"
    );
    let mut failures = 0u64;
    for (a, b) in actual.pixels().zip(blessed.pixels()) {
        for ch in 0..3 {
            if a.0[ch].abs_diff(b.0[ch]) > PIXEL_TOLERANCE {
                failures += 1;
                break;
            }
        }
    }
    let total = (W * H) as u64;
    let pct = failures as f64 / total as f64 * 100.0;
    assert!(
        failures == 0,
        "{name}: {failures}/{total} pixels ({pct:.2}%) differ by more than \
         {PIXEL_TOLERANCE} — run with BLESS=1 to re-bless after intentional changes"
    );
}

// ── test utilities ────────────────────────────────────────────────────────────

/// Build a renderer from raw mesh + line data and a fixed camera orbit.
fn make_renderer(
    ctx: &GpuContext,
    meshes: Vec<(kernel_mesh::RenderMesh, [f32; 4])>,
    lines: Vec<(Vec<[f32; 3]>, [f32; 4])>,
    camera: &OrbitCamera,
) -> SceneRenderer {
    let mut renderer = SceneRenderer::new(&ctx.device, FORMAT);
    renderer.set_meshes(&ctx.device, &meshes, 0);
    if !lines.is_empty() {
        let data = mydrafter_render::SceneData {
            meshes: vec![],
            lines,
            edges: vec![],
            underlay: None,
        };
        renderer.set_scene(&ctx.device, &ctx.queue, &data, 0);
    }
    let aspect = W as f32 / H as f32;
    let vp = camera.view_proj(aspect);
    let eye = camera.eye();
    let cam = mydrafter_render::camera_uniform(vp, eye);
    renderer.write_camera(&ctx.device, &ctx.queue, 0, &cam);
    renderer
}

// ── golden tests ──────────────────────────────────────────────────────────────

/// Scene 1: single box, dark theme.
#[test]
#[ignore = "golden"]
fn golden_single_box_dark() {
    let ctx = GpuContext::new();
    let mesh = kernel_mesh::make_box(DVec3::new(-2.5, -2.5, 0.0), DVec3::new(5.0, 5.0, 3.0));
    let theme = Theme::Dark;
    let mut camera = OrbitCamera::default();
    camera.distance = 14.0;
    camera.yaw = -0.6;
    camera.pitch = 0.55;

    let mut renderer = SceneRenderer::new(&ctx.device, FORMAT);
    renderer.set_meshes(&ctx.device, &[(mesh.to_render(), theme.mesh())], 0);
    let aspect = W as f32 / H as f32;
    let cam = mydrafter_render::camera_uniform(camera.view_proj(aspect), camera.eye());
    renderer.write_camera(&ctx.device, &ctx.queue, 0, &cam);

    let img = render_to_image(&ctx, &renderer, theme);
    check_or_bless(&img, "single-box-dark");
}

/// Scene 2: two boxes, light theme.
#[test]
#[ignore = "golden"]
fn golden_two_boxes_light() {
    let ctx = GpuContext::new();
    let theme = Theme::Light;
    let box_a = kernel_mesh::make_box(DVec3::ZERO, DVec3::new(4.0, 4.0, 3.0));
    let box_b =
        kernel_mesh::make_box(DVec3::new(6.0, 0.0, 0.0), DVec3::new(3.0, 3.0, 5.0));
    let mut camera = OrbitCamera::default();
    camera.target = glam::Vec3::new(4.5, 2.0, 2.5);
    camera.distance = 18.0;
    camera.yaw = -0.9;
    camera.pitch = 0.45;

    let meshes = vec![
        (box_a.to_render(), theme.mesh()),
        (box_b.to_render(), [0.6, 0.2, 0.1, 1.0]),
    ];
    let renderer = make_renderer(&ctx, meshes, vec![], &camera);
    let img = render_to_image(&ctx, &renderer, theme);
    check_or_bless(&img, "two-boxes-light");
}

/// Scene 4: a raster underlay on the ground plane, viewed top-down, dark theme.
/// A 4x4 red/blue checkerboard placed at corner (-5,-5), 10 m square; the grid
/// blends over it. Proves the textured quad renders under the grid at depth.
#[test]
#[ignore = "golden"]
fn golden_underlay_dark() {
    let ctx = GpuContext::new();
    let theme = Theme::Dark;
    let mut camera = OrbitCamera::default();
    camera.target = glam::Vec3::ZERO;
    camera.distance = 20.0;
    camera.yaw = 0.0;
    camera.pitch = 1.4; // near top-down

    // 4x4 checkerboard, 64x64 px.
    let n = 64u32;
    let mut rgba = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let cell = ((x / 16) + (y / 16)) % 2 == 0;
            let c = if cell { [220, 60, 60, 255] } else { [60, 90, 220, 255] };
            rgba.extend_from_slice(&c);
        }
    }
    let underlay = mydrafter_render::UnderlayData {
        rgba,
        width_px: n,
        height_px: n,
        corners: [
            [-5.0, -5.0, 0.0],
            [5.0, -5.0, 0.0],
            [5.0, 5.0, 0.0],
            [-5.0, 5.0, 0.0],
        ],
        opacity: 0.8,
    };
    let data = mydrafter_render::SceneData {
        meshes: vec![],
        lines: vec![],
        edges: vec![],
        underlay: Some(underlay),
    };
    let mut renderer = SceneRenderer::new(&ctx.device, FORMAT);
    renderer.set_scene(&ctx.device, &ctx.queue, &data, 0);
    let aspect = W as f32 / H as f32;
    let cam = mydrafter_render::camera_uniform(camera.view_proj(aspect), camera.eye());
    renderer.write_camera(&ctx.device, &ctx.queue, 0, &cam);

    let img = render_to_image(&ctx, &renderer, theme);
    check_or_bless(&img, "underlay-dark");
}

/// Scene 3: empty scene (no geometry), dark theme — verifies clear-color only.
#[test]
#[ignore = "golden"]
fn golden_empty_dark() {
    let ctx = GpuContext::new();
    let theme = Theme::Dark;
    let camera = OrbitCamera::default();

    let renderer = make_renderer(&ctx, vec![], vec![], &camera);
    let img = render_to_image(&ctx, &renderer, theme);
    check_or_bless(&img, "empty-dark");
}
