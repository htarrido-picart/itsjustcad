// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Integration test for the control-image export: build a small massing, render
//! the depth / edge / mask control maps, and assert the three PNGs are written,
//! nonzero, correctly sized, and DISTINCT from one another.
//!
//! `#[ignore]` like the golden tests: it needs a real GPU adapter. Run with:
//!
//!   cargo test -p itsjustcad-render -- --ignored control_images

use glam::{DVec3, Vec3};
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

/// Load a PNG file into (width, height, rgba bytes).
fn load(path: &std::path::Path) -> (u32, u32, Vec<u8>) {
    let img = image::open(path).expect("open png").to_rgba8();
    let (w, h) = img.dimensions();
    (w, h, img.into_raw())
}

#[test]
#[ignore = "needs a GPU adapter"]
fn control_images_written_nonzero_and_distinct() {
    let (device, queue) = gpu();

    // Small massing: two boxes at different depths, on two distinct layers so
    // the mask has more than one region.
    let mut doc = Document::default();
    doc.layers.insert("towers".into(), LayerStyle::default());
    doc.layers.insert("podium".into(), LayerStyle::default());
    doc.insert(box_obj(DVec3::new(0.0, 0.0, 0.0), DVec3::new(4.0, 4.0, 12.0), "towers"));
    doc.insert(box_obj(DVec3::new(6.0, 0.0, 0.0), DVec3::new(8.0, 8.0, 2.0), "podium"));

    // A three-quarter perspective framed on the scene.
    let bb = doc.scene_aabb().expect("scene has extents");
    let c = bb.center();
    let mut cam = OrbitCamera::default();
    cam.target = Vec3::new(c.x as f32, c.y as f32, c.z as f32);
    cam.distance = (bb.size().length() as f32 * 1.2).max(5.0);
    cam.pitch = 0.55;
    cam.yaw = -0.6;
    let aspect = W as f32 / H as f32;
    let view_proj = cam.view_proj(aspect);
    let eye = cam.eye();
    let center = Vec3::new(c.x as f32, c.y as f32, c.z as f32);
    let radius = (bb.size().length() as f32 * 0.5).max(0.5);
    let d = (eye - center).length();
    let (near, far) = ((d - radius).max(0.01), d + radius);

    let dir = std::env::temp_dir().join(format!("itsjustcad_ctrl_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let prefix = dir.join("scene");
    let paths = render_control_images(
        &device,
        &queue,
        &doc,
        view_proj,
        eye,
        near,
        far,
        W,
        H,
        prefix.to_str().unwrap(),
    )
    .expect("render control images");

    // All three files exist and are nonzero on disk.
    for p in [&paths.depth, &paths.edge, &paths.mask] {
        let meta = std::fs::metadata(p).unwrap_or_else(|_| panic!("missing {}", p.display()));
        assert!(meta.len() > 0, "empty file {}", p.display());
    }

    let (dw, dh, depth) = load(&paths.depth);
    let (ew, eh, edge) = load(&paths.edge);
    let (mw, mh, mask) = load(&paths.mask);

    // Correct dimensions.
    for (w, h) in [(dw, dh), (ew, eh), (mw, mh)] {
        assert_eq!((w, h), (W, H), "unexpected image size");
    }

    // Each image has actual drawn content (not a flat clear).
    let distinct_values = |rgba: &[u8]| {
        let mut set = std::collections::HashSet::new();
        for px in rgba.chunks_exact(4) {
            set.insert((px[0], px[1], px[2]));
            if set.len() > 3 {
                break;
            }
        }
        set.len()
    };
    assert!(distinct_values(&depth) >= 2, "depth map should have a gradient");
    assert!(distinct_values(&edge) >= 2, "edge map should have ink on background");
    assert!(distinct_values(&mask) >= 2, "mask should have >1 region color");

    // The three maps must differ from one another (different byte content).
    assert_ne!(depth, edge, "depth and edge must differ");
    assert_ne!(depth, mask, "depth and mask must differ");
    assert_ne!(edge, mask, "edge and mask must differ");

    let _ = std::fs::remove_dir_all(&dir);
}
