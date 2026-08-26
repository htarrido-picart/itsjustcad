// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! End-to-end sanity for the diffusion RenderDeck pipeline, driven by the MOCK
//! backend so it never touches the network. Proves the whole plumbing:
//!
//!   owned control images (real wgpu render) → ControlImages::from_prefix →
//!   RenderRequest → MockRenderDeck::render → RenderedImage → overlay PNG on disk
//!
//! `#[ignore]` like the control-image golden test: it needs a real GPU adapter.
//! Run with:
//!
//!   cargo test -p itsjustcad --test diffusion_pipeline -- --ignored

use glam::{DVec3, Vec3};
use itsjustcad_deck::{
    make_render_deck, ControlImages, MockRenderDeck, RenderConfig, RenderDeck, RenderRequest,
};
use itsjustcad_doc::{Document, Geometry, LayerStyle, ObjectId, SceneObject};
use itsjustcad_render::{render_control_images, OrbitCamera};

const W: u32 = 320;
const H: u32 = 200;

fn gpu() -> (wgpu::Device, wgpu::Queue) {
    let instance = wgpu::Instance::default();
    let adapter =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .expect("no GPU adapter");
    pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).expect("device")
}

fn box_obj(min: DVec3, size: DVec3, layer: &str) -> SceneObject {
    SceneObject {
        id: ObjectId::new(),
        name: None,
        layer: layer.to_string(),
        visible: true,
        color: None,
        material: None,
        lineweight_mm: None,
        geometry: Geometry::Mesh(kernel_mesh::make_box(min, size)),
    }
}

/// The full owned-control-image → mock-diffusion → overlay pipeline.
#[test]
#[ignore = "needs a GPU adapter"]
fn mock_diffusion_pipeline_produces_overlay() {
    let (device, queue) = gpu();

    // Small massing on two layers, so the control maps carry real content.
    let mut doc = Document::default();
    doc.layers.insert("towers".into(), LayerStyle::default());
    doc.layers.insert("podium".into(), LayerStyle::default());
    doc.insert(box_obj(DVec3::new(0.0, 0.0, 0.0), DVec3::new(4.0, 4.0, 12.0), "towers"));
    doc.insert(box_obj(DVec3::new(6.0, 0.0, 0.0), DVec3::new(8.0, 8.0, 2.0), "podium"));

    let bb = doc.scene_aabb().expect("scene has extents");
    let c = bb.center();
    let cam = OrbitCamera {
        target: Vec3::new(c.x as f32, c.y as f32, c.z as f32),
        distance: (bb.size().length() as f32 * 1.2).max(5.0),
        pitch: 0.55,
        yaw: -0.6,
        ..OrbitCamera::default()
    };
    let aspect = W as f32 / H as f32;
    let view_proj = cam.view_proj(aspect);
    let eye = cam.eye();
    let center = Vec3::new(c.x as f32, c.y as f32, c.z as f32);
    let radius = (bb.size().length() as f32 * 0.5).max(0.5);
    let d = (eye - center).length();
    let (near, far) = ((d - radius).max(0.01), d + radius);

    let dir = std::env::temp_dir().join(format!("itsjustcad_diffuse_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let prefix = dir.join("scene");
    let prefix_str = prefix.to_str().unwrap();

    // 1) CAD OWNS the control images: real wgpu render → three PNGs.
    let paths = render_control_images(
        &device, &queue, &doc, view_proj, eye, near, far, W, H, prefix_str,
    )
    .expect("render control images");
    for p in [&paths.depth, &paths.edge, &paths.mask] {
        assert!(std::fs::metadata(p).unwrap().len() > 0, "empty {}", p.display());
    }

    // 2) Load them into the request payload (the deck-side owned inputs).
    let control = ControlImages::from_prefix(prefix_str).expect("load control images");
    assert!(!control.depth.is_empty() && !control.edge.is_empty() && !control.mask.is_empty());

    // 3) Assemble the request (a deck-authored or literal prompt) and run the
    //    MOCK backend — no network, canned image back.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let deck = MockRenderDeck::new();
    let req = RenderRequest::new("a glass pavilion at golden hour, architectural render", control, W, H);
    let rendered = rt.block_on(deck.render(req)).expect("mock render");

    // 4) Response is a real PNG; the mock recorded the control images it saw.
    assert_eq!(rendered.backend, "mock");
    assert!(rendered.png.starts_with(&[0x89, 0x50, 0x4E, 0x47]), "PNG signature");
    {
        let seen = deck.last.lock().unwrap();
        let seen = seen.as_ref().expect("mock saw a request");
        assert!(seen.prompt.contains("glass pavilion"));
        assert!(!seen.control.depth.is_empty(), "depth control reached the backend");
        assert!(!seen.control.edge.is_empty(), "edge control reached the backend");
    }

    // 5) OVERLAY IS SET: write the diffused image where the viewport overlay
    //    would load it, and confirm it landed on disk.
    let overlay = dir.join("scene_diffused.png");
    rendered.save(&overlay).expect("write overlay");
    assert!(std::fs::metadata(&overlay).unwrap().len() > 0, "overlay written");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Without a backend configured (the ship default), the pipeline fails with
/// clear guidance rather than a panic or silent no-op. No GPU needed.
#[test]
fn unconfigured_backend_guides_the_user() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let deck = make_render_deck(&RenderConfig::none());
    let control = ControlImages { depth: vec![1], edge: vec![2], mask: vec![3] };
    let req = RenderRequest::new("anything", control, W, H);
    let err = rt.block_on(deck.render(req)).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("ComfyUI"), "{msg}");
    assert!(msg.contains("cloud key"), "{msg}");
}
