// SPDX-License-Identifier: AGPL-3.0-only

//! Screen-space ambient occlusion for the viewport (#358).
//!
//! Three small passes per pane, recorded in the paint callback's `prepare`
//! before the scene is drawn, into single-sample targets at half the pane's
//! pixel size ([`ao_resolution`]):
//!
//! 1. **Prepass**: the opaque face fills again, depth (`Depth32Float`) plus
//!    world normals (`Rgba8Unorm`), one draw per batch (absolute indices kept
//!    beside the batch, rebuilt only when its geometry changes, since the
//!    per-object draws cost CPU on large scenes). One path for every sample count: with MSAA
//!    the scene's own depth is multisampled and discarded, and with MSAA off
//!    the scene draws straight into egui's pass, so neither can be read back.
//!    A dedicated single-sample prepass is the simplest robust source, costs
//!    one more draw of the face batches, and needs no offscreen scene.
//! 2. **AO**: for each pixel, 16 points in the normal-oriented hemisphere of
//!    radius `camera.ao.w` (world units) are projected back onto the depth;
//!    those behind a nearer surface within the radius count as occluded. The
//!    kernel is rotated by a fixed 4x4 per-pixel pattern, never by time, so
//!    the result is stable while orbiting.
//! 3. **Blur**: a 4x4 depth- and normal-aware box blur that exactly averages
//!    out the 4x4 rotation pattern without bleeding across silhouettes.
//!
//! The blurred occlusion (`R8Unorm`) is bound in the pane's camera bind group;
//! the face shaders darken toward the AO colour before fogging
//! (`occluded()` in `shaders/common.wgsl`). The fragment finds its texel by
//! projecting its world point, so the same texture serves the MSAA target and
//! egui's pass (whatever the pane's offset in the window).
//!
//! Targets are kept per pane and rebuilt only when the pane's size changes;
//! setting changes only rewrite the camera. Panes without AO hold no target
//! and bind a 1x1 empty texture (no occlusion).

use crate::batch::{Batch, GrowBuffer};
use crate::renderer::{CameraUniform, SceneRenderer};

const DEPTH: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
const NORMAL: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const OCCLUSION: wgpu::TextureFormat = wgpu::TextureFormat::R8Unorm;

const AO_WGSL: &str = concat!(include_str!("shaders/common.wgsl"), include_str!("shaders/ao.wgsl"));

/// Layout entry of an unfilterable 2D float texture read in the fragment
/// stage (the occlusion in the camera group, the raw AO in the blur).
pub(crate) fn occlusion_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    texture_entry(binding, wgpu::TextureSampleType::Float { filterable: false })
}

fn texture_entry(binding: u32, sample_type: wgpu::TextureSampleType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type,
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn texture(
    device: &wgpu::Device,
    label: &str,
    format: wgpu::TextureFormat,
    size: [u32; 2],
) -> wgpu::TextureView {
    device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
        .create_view(&Default::default())
}

/// Size of the AO textures for a pane of `size` pixels: half each way
/// (rounded up), as the occlusion is soft and blurred anyway. A quarter of
/// the pixels keeps a Retina pane cheap; the fills look their texel up by
/// projection, so any resolution maps onto the pane.
pub fn ao_resolution(size: [u32; 2]) -> [u32; 2] {
    size.map(|n| n.div_ceil(2).max(1))
}

/// The AO pipelines, built on first use.
struct AoPipelines {
    /// Depth + normal prepass of the plain and the per-face-coloured meshes.
    prepass: wgpu::RenderPipeline,
    prepass_face: wgpu::RenderPipeline,
    occlusion: wgpu::RenderPipeline,
    blur: wgpu::RenderPipeline,
    /// Camera, prepass depth and normals (group 0 of the AO and blur passes).
    inputs_layout: wgpu::BindGroupLayout,
    /// The raw AO (group 1 of the blur).
    raw_layout: wgpu::BindGroupLayout,
}

impl AoPipelines {
    fn new(device: &wgpu::Device, camera_layout: &wgpu::BindGroupLayout) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ao_shader"),
            source: wgpu::ShaderSource::Wgsl(AO_WGSL.into()),
        });
        let inputs_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ao_inputs_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                texture_entry(2, wgpu::TextureSampleType::Depth),
                occlusion_entry(3),
            ],
        });
        let raw_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ao_raw_layout"),
            entries: &[occlusion_entry(0)],
        });
        let layout = |label, groups: &[Option<&wgpu::BindGroupLayout>]| {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: groups,
                immediate_size: 0,
            })
        };
        let prepass_layout = layout("ao_prepass_pl", &[Some(camera_layout)]);
        let prepass = |label, stride| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&prepass_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_prepass"),
                    compilation_options: Default::default(),
                    buffers: &[wgpu::VertexBufferLayout {
                        // Position + normal; the face meshes' colour is skipped.
                        array_stride: stride,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3],
                    }],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_prepass"),
                    compilation_options: Default::default(),
                    targets: &[Some(NORMAL.into())],
                }),
                // As the opaque mesh fill: back faces culled.
                primitive: wgpu::PrimitiveState { cull_mode: Some(wgpu::Face::Back), ..Default::default() },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH,
                    depth_write_enabled: Some(true),
                    depth_compare: Some(wgpu::CompareFunction::LessEqual),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let fullscreen = |label, pl: &wgpu::PipelineLayout, entry| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(pl),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_full"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    targets: &[Some(OCCLUSION.into())],
                }),
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let ao_layout = layout("ao_pl", &[Some(&inputs_layout)]);
        let blur_layout = layout("ao_blur_pl", &[Some(&inputs_layout), Some(&raw_layout)]);
        Self {
            prepass: prepass("ao_prepass", 24),
            prepass_face: prepass("ao_prepass_face", 36),
            occlusion: fullscreen("ao_pipeline", &ao_layout, "fs_ao"),
            blur: fullscreen("ao_blur_pipeline", &blur_layout, "fs_blur"),
            inputs_layout,
            raw_layout,
        }
    }
}

/// A pane's AO targets: prepass depth and normals, raw and blurred occlusion.
pub struct AoTarget {
    size: [u32; 2],
    depth: wgpu::TextureView,
    normal: wgpu::TextureView,
    raw: wgpu::TextureView,
    occlusion: wgpu::TextureView,
    inputs: wgpu::BindGroup,
    raw_group: wgpu::BindGroup,
}

impl AoTarget {
    fn new(device: &wgpu::Device, pipes: &AoPipelines, camera: &wgpu::Buffer, size: [u32; 2]) -> Self {
        let res = ao_resolution(size);
        let depth = texture(device, "ao_depth", DEPTH, res);
        let normal = texture(device, "ao_normal", NORMAL, res);
        let raw = texture(device, "ao_raw", OCCLUSION, res);
        let occlusion = texture(device, "ao_occlusion", OCCLUSION, res);
        let view = wgpu::BindingResource::TextureView;
        let inputs = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ao_inputs"),
            layout: &pipes.inputs_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: camera.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: view(&depth) },
                wgpu::BindGroupEntry { binding: 3, resource: view(&normal) },
            ],
        });
        let raw_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ao_raw"),
            layout: &pipes.raw_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: view(&raw) }],
        });
        Self { size, depth, normal, raw, occlusion, inputs, raw_group }
    }

    /// Pixel size of the pane the target was built for.
    pub fn size(&self) -> [u32; 2] {
        self.size
    }

    /// Pixel size of the AO textures ([`ao_resolution`] of the pane).
    pub fn resolution(&self) -> [u32; 2] {
        ao_resolution(self.size)
    }
}

/// Viewport AO state owned by [`SceneRenderer`]: the pipelines (built on
/// first use), one target per pane with AO on, and the empty occlusion.
pub(crate) struct ViewportAo {
    pipes: Option<AoPipelines>,
    panes: Vec<Option<AoTarget>>,
    /// 1x1 zero occlusion bound by panes without AO (wgpu zero-fills it).
    empty: wgpu::TextureView,
    /// Whole-batch index buffers of the meshes and the face meshes.
    flat: [FlatIndices; 2],
}

/// A batch's indices made absolute, for one draw of the whole batch (the
/// AO prepass, the shadow map #357).
pub(crate) struct FlatIndices {
    /// [`Batch::version`] the buffer was built from.
    version: Option<u64>,
    buf: GrowBuffer,
    count: u32,
}

impl FlatIndices {
    pub(crate) fn new(label: &'static str) -> Self {
        Self { version: None, buf: GrowBuffer::new(label, wgpu::BufferUsages::INDEX), count: 0 }
    }

    /// Bring the buffer up to date with `batch`.
    pub(crate) fn update(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, batch: &Batch) {
        if self.version != Some(batch.version) {
            let indices = batch.flat_indices();
            self.buf.write(device, queue, bytemuck::cast_slice(&indices));
            self.count = indices.len() as u32;
            self.version = Some(batch.version);
        }
    }

    /// Draw `batch` (its vertex buffer) through the flat indices.
    pub(crate) fn draw(&self, pass: &mut wgpu::RenderPass<'_>, batch: &Batch) {
        let (Some(vb), Some(ib)) = (batch.vertex_slice(), self.buf.buf.as_ref()) else {
            return;
        };
        if self.count > 0 {
            pass.set_vertex_buffer(0, vb);
            pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..self.count, 0, 0..1);
        }
    }
}

impl ViewportAo {
    pub(crate) fn new(device: &wgpu::Device) -> Self {
        Self {
            pipes: None,
            panes: Vec::new(),
            empty: texture(device, "ao_empty", OCCLUSION, [1, 1]),
            flat: [FlatIndices::new("ao_flat_indices"), FlatIndices::new("ao_flat_indices")],
        }
    }

    /// The occlusion pane `viewport` binds: its target's, else the empty one.
    pub(crate) fn occlusion_view(&self, viewport: usize) -> &wgpu::TextureView {
        match self.panes.get(viewport) {
            Some(Some(t)) => &t.occlusion,
            _ => &self.empty,
        }
    }
}

/// A pass clearing `view` to zero (and `depth` to far), stored.
fn pass<'a>(
    encoder: &'a mut wgpu::CommandEncoder,
    label: &str,
    view: &wgpu::TextureView,
    depth: Option<&wgpu::TextureView>,
) -> wgpu::RenderPass<'a> {
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(label),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                store: wgpu::StoreOp::Store,
            },
            depth_slice: None,
        })],
        depth_stencil_attachment: depth.map(|view| wgpu::RenderPassDepthStencilAttachment {
            view,
            depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Store }),
            stencil_ops: None,
        }),
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    })
}

impl SceneRenderer {
    /// Pane `viewport`'s AO target, if its camera has AO on and
    /// [`Self::render_ao`] built one.
    pub fn ao_target(&self, viewport: usize) -> Option<&AoTarget> {
        self.ao.panes.get(viewport)?.as_ref()
    }

    /// Compute pane `viewport`'s ambient occlusion (#358) for a pane of
    /// `size_px` pixels, before the scene is drawn (MSAA or not). Uses the
    /// pane's last camera ([`Self::write_camera`]); a camera without AO frees
    /// the pane's target. The target is reallocated only when the size
    /// changed.
    pub fn render_ao(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        viewport: usize,
        size_px: [u32; 2],
    ) {
        let on = size_px[0] > 0
            && size_px[1] > 0
            && viewport < self.cameras.len()
            && self.last_cameras.get(viewport).is_some_and(CameraUniform::has_ao);
        if !on {
            if let Some(slot @ Some(_)) = self.ao.panes.get_mut(viewport) {
                *slot = None;
                self.rebind_camera(device, viewport);
                if self.ao.panes.iter().all(Option::is_none) {
                    self.ao.flat = [FlatIndices::new("ao_flat_indices"), FlatIndices::new("ao_flat_indices")];
                }
            }
            return;
        }
        let pipes = self.ao.pipes.get_or_insert_with(|| AoPipelines::new(device, &self.camera_layout));
        if self.ao.panes.len() <= viewport {
            self.ao.panes.resize_with(viewport + 1, || None);
        }
        if !self.ao.panes[viewport].as_ref().is_some_and(|t| t.size == size_px) {
            self.ao.panes[viewport] = Some(AoTarget::new(device, pipes, &self.cameras[viewport].0, size_px));
            self.rebind_camera(device, viewport);
        }
        let [flat_meshes, flat_faces] = &mut self.ao.flat;
        flat_meshes.update(device, queue, &self.meshes);
        flat_faces.update(device, queue, &self.face_meshes);
        let (Some(pipes), Some(Some(t))) = (&self.ao.pipes, self.ao.panes.get(viewport)) else {
            return;
        };
        {
            let mut p = pass(encoder, "ao_prepass", &t.normal, Some(&t.depth));
            p.set_bind_group(0, &self.cameras[viewport].1, &[]);
            p.set_pipeline(&pipes.prepass);
            self.ao.flat[0].draw(&mut p, &self.meshes);
            p.set_pipeline(&pipes.prepass_face);
            self.ao.flat[1].draw(&mut p, &self.face_meshes);
        }
        {
            let mut p = pass(encoder, "ao_pass", &t.raw, None);
            p.set_pipeline(&pipes.occlusion);
            p.set_bind_group(0, &t.inputs, &[]);
            p.draw(0..3, 0..1);
        }
        let mut p = pass(encoder, "ao_blur", &t.occlusion, None);
        p.set_pipeline(&pipes.blur);
        p.set_bind_group(0, &t.inputs, &[]);
        p.set_bind_group(1, &t.raw_group, &[]);
        p.draw(0..3, 0..1);
    }

    /// Rebuild pane `viewport`'s camera bind group after its occlusion (or
    /// the shadow map, #357) changed.
    pub(crate) fn rebind_camera(&mut self, device: &wgpu::Device, viewport: usize) {
        if let Some((buf, _)) = self.cameras.get(viewport) {
            let group = self.camera_bind_group(device, buf, viewport);
            self.cameras[viewport].1 = group;
        }
    }
}
