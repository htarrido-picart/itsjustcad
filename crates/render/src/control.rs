// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Control-image export: from the current view, render three CAD-owned control
//! maps for a diffusion / image-editing hand-off.
//!
//!  * `<prefix>_depth.png` — grayscale near-to-far depth (white near, black far)
//!  * `<prefix>_edge.png`  — feature-edge linework (black ink on white)
//!  * `<prefix>_mask.png`  — a flat semantic color per layer (no shading)
//!
//! These are the CAD-side inputs for the later diffusion cassette but stand on
//! their own (drop into Photoshop / ControlNet). The depth pass reads the mesh
//! world position, the edge pass draws `kernel_mesh::feature_edges`, and the
//! mask pass paints one deterministic hue per layer — so the three outputs are
//! guaranteed distinct.

use bytemuck::{Pod, Zeroable};
use glam::Mat4;
use itsjustcad_doc::{Document, Geometry};
use wgpu::util::DeviceExt as _;

use crate::renderer::hue_from_seed;
use crate::DEPTH_FORMAT;

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ControlUniform {
    view_proj: [[f32; 4]; 4],
    eye: [f32; 4],
    /// x = near distance, y = far distance (for depth normalization); zw spare.
    range: [f32; 4],
    /// Flat color used by the mask pass; alpha spare.
    color: [f32; 4],
}

/// One triangle mesh flattened for the control passes.
struct CtrlMesh {
    vertex_buf: wgpu::Buffer,
    index_buf: wgpu::Buffer,
    index_count: u32,
    /// Deterministic flat color for the mask pass (per layer).
    mask_color: [f32; 4],
}

/// A flat edge-segment buffer (LineList) for the edge pass.
struct CtrlEdges {
    vertex_buf: wgpu::Buffer,
    vertex_count: u32,
}

/// Result of a control-image render: the three output paths that were written.
pub struct ControlImagePaths {
    pub depth: std::path::PathBuf,
    pub edge: std::path::PathBuf,
    pub mask: std::path::PathBuf,
}

/// Render the three control images for `doc` from the given `view_proj` / `eye`
/// at `width` x `height`, writing `<prefix>_depth.png`, `<prefix>_edge.png` and
/// `<prefix>_mask.png`. Returns the written paths.
///
/// `eye` and `view_proj` come from the live/headless camera; `near`/`far` bound
/// the depth normalization (pass the scene's near/far distance from the eye).
#[allow(clippy::too_many_arguments)]
pub fn render_control_images(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    doc: &Document,
    view_proj: Mat4,
    eye: glam::Vec3,
    near: f32,
    far: f32,
    width: u32,
    height: u32,
    prefix: &str,
) -> Result<ControlImagePaths, String> {
    // ── Gather geometry ─────────────────────────────────────────────────────
    // Deterministic per-layer mask hue: stable hash of the layer name.
    let layer_hue = |layer: &str| -> [f32; 4] {
        let mut h: u64 = 1469598103934665603; // FNV-1a
        for b in layer.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(1099511628257);
        }
        hue_from_seed(h)
    };

    let mut meshes: Vec<CtrlMesh> = Vec::new();
    let mut all_edges: Vec<[f32; 3]> = Vec::new();
    for obj in doc.objects() {
        if !obj.visible {
            continue;
        }
        if doc.layers.get(&obj.layer).is_some_and(|s| !s.visible) {
            continue;
        }
        let mesh = match &obj.geometry {
            Geometry::Mesh(m) | Geometry::Frame { mesh: m, .. } | Geometry::Area { mesh: m, .. } => m,
            _ => continue,
        };
        let rm = mesh.to_render();
        let mut vertices = Vec::with_capacity(rm.positions.len() * 6);
        for (p, n) in rm.positions.iter().zip(&rm.normals) {
            vertices.extend_from_slice(p);
            vertices.extend_from_slice(n);
        }
        let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("ctrl_vb"),
            contents: bytemuck::cast_slice(&vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let index_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("ctrl_ib"),
            contents: bytemuck::cast_slice(&rm.indices),
            usage: wgpu::BufferUsages::INDEX,
        });
        meshes.push(CtrlMesh {
            vertex_buf,
            index_buf,
            index_count: rm.indices.len() as u32,
            mask_color: layer_hue(&obj.layer),
        });
        for (a, b) in kernel_mesh::feature_edges(mesh) {
            all_edges.push([a.x as f32, a.y as f32, a.z as f32]);
            all_edges.push([b.x as f32, b.y as f32, b.z as f32]);
        }
    }

    let edges = if all_edges.len() >= 2 {
        Some(CtrlEdges {
            vertex_buf: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("ctrl_edge_vb"),
                contents: bytemuck::cast_slice(&all_edges),
                usage: wgpu::BufferUsages::VERTEX,
            }),
            vertex_count: all_edges.len() as u32,
        })
    } else {
        None
    };

    // ── Pipelines ───────────────────────────────────────────────────────────
    let uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("ctrl_uniform_layout"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }],
    });
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("ctrl_shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shaders/control.wgsl").into()),
    });
    let pl_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("ctrl_pl"),
        bind_group_layouts: &[Some(&uniform_layout)],
        immediate_size: 0,
    });

    const MESH_ATTRS: [wgpu::VertexAttribute; 2] =
        wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3];
    const LINE_ATTRS: [wgpu::VertexAttribute; 1] = wgpu::vertex_attr_array![0 => Float32x3];
    let mesh_vb_layout = wgpu::VertexBufferLayout {
        array_stride: 24,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &MESH_ATTRS,
    };
    let line_vb_layout = wgpu::VertexBufferLayout {
        array_stride: 12,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &LINE_ATTRS,
    };

    let make_pipeline = |fs: &str, vb: wgpu::VertexBufferLayout<'static>, topo| {
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("ctrl_pipeline"),
            layout: Some(&pl_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[vb],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some(fs),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState { topology: topo, ..Default::default() },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        })
    };
    let depth_pipeline =
        make_pipeline("fs_depth", mesh_vb_layout.clone(), wgpu::PrimitiveTopology::TriangleList);
    let mask_pipeline =
        make_pipeline("fs_mask", mesh_vb_layout, wgpu::PrimitiveTopology::TriangleList);
    let edge_pipeline =
        make_pipeline("fs_edge", line_vb_layout, wgpu::PrimitiveTopology::LineList);

    // ── Per-mesh uniforms (mask needs a distinct color per mesh) ────────────
    let base_uniform = |color: [f32; 4]| ControlUniform {
        view_proj: view_proj.to_cols_array_2d(),
        eye: [eye.x, eye.y, eye.z, 1.0],
        range: [near, far.max(near + 1e-3), 0.0, 0.0],
        color,
    };
    let make_bg = |u: &ControlUniform| {
        let buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("ctrl_ubo"),
            contents: bytemuck::bytes_of(u),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ctrl_bg"),
            layout: &uniform_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: buf.as_entire_binding() }],
        })
    };
    let shared_bg = make_bg(&base_uniform([0.0, 0.0, 0.0, 1.0]));
    let mask_bgs: Vec<wgpu::BindGroup> =
        meshes.iter().map(|m| make_bg(&base_uniform(m.mask_color))).collect();

    // ── Render each of the three targets ────────────────────────────────────
    let depth_png =
        render_pass_to_png(device, queue, width, height, [0.0, 0.0, 0.0, 1.0], |pass| {
            pass.set_pipeline(&depth_pipeline);
            pass.set_bind_group(0, &shared_bg, &[]);
            for m in &meshes {
                pass.set_vertex_buffer(0, m.vertex_buf.slice(..));
                pass.set_index_buffer(m.index_buf.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..m.index_count, 0, 0..1);
            }
        });

    let edge_png =
        render_pass_to_png(device, queue, width, height, [1.0, 1.0, 1.0, 1.0], |pass| {
            if let Some(e) = &edges {
                pass.set_pipeline(&edge_pipeline);
                pass.set_bind_group(0, &shared_bg, &[]);
                pass.set_vertex_buffer(0, e.vertex_buf.slice(..));
                pass.draw(0..e.vertex_count, 0..1);
            }
        });

    let mask_png =
        render_pass_to_png(device, queue, width, height, [0.0, 0.0, 0.0, 1.0], |pass| {
            pass.set_pipeline(&mask_pipeline);
            for (m, bg) in meshes.iter().zip(&mask_bgs) {
                pass.set_bind_group(0, bg, &[]);
                pass.set_vertex_buffer(0, m.vertex_buf.slice(..));
                pass.set_index_buffer(m.index_buf.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..m.index_count, 0, 0..1);
            }
        });

    let paths = ControlImagePaths {
        depth: std::path::PathBuf::from(format!("{prefix}_depth.png")),
        edge: std::path::PathBuf::from(format!("{prefix}_edge.png")),
        mask: std::path::PathBuf::from(format!("{prefix}_mask.png")),
    };
    save_png(&depth_png, width, height, &paths.depth)?;
    save_png(&edge_png, width, height, &paths.edge)?;
    save_png(&mask_png, width, height, &paths.mask)?;
    Ok(paths)
}

/// Run one render pass with a `clear` color and a closure that records draws,
/// then read the color target back as tight RGBA8 bytes.
fn render_pass_to_png(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    width: u32,
    height: u32,
    clear: [f32; 4],
    record: impl FnOnce(&mut wgpu::RenderPass<'static>),
) -> Vec<u8> {
    let color = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("ctrl_color"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let depth = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("ctrl_depth"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
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
            label: Some("ctrl_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &color_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: clear[0] as f64,
                        g: clear[1] as f64,
                        b: clear[2] as f64,
                        a: clear[3] as f64,
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
        record(&mut pass);
    }

    let bytes_per_row = (width * 4).next_multiple_of(256);
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("ctrl_readback"),
        size: (bytes_per_row * height) as u64,
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
        wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
    );
    queue.submit([encoder.finish()]);

    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    device
        .poll(wgpu::PollType::Wait { submission_index: None, timeout: None })
        .expect("poll");
    let data = slice.get_mapped_range();
    let mut out = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        let row = &data[(y * bytes_per_row) as usize..][..(width * 4) as usize];
        out.extend_from_slice(row);
    }
    drop(data);
    out
}

/// Encode tight RGBA8 bytes as a PNG at `path`.
fn save_png(rgba: &[u8], width: u32, height: u32, path: &std::path::Path) -> Result<(), String> {
    let img = image::RgbaImage::from_raw(width, height, rgba.to_vec())
        .ok_or_else(|| "control image buffer size mismatch".to_string())?;
    img.save(path).map_err(|e| format!("write {}: {e}", path.display()))
}
