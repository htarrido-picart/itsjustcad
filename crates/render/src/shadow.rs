// SPDX-License-Identifier: AGPL-3.0-only

//! Real-time sun shadows for the viewport (#357).
//!
//! One directional shadow map, shared by every pane: the opaque faces
//! (`meshes`, `face_meshes` and `tex_meshes`) drawn depth-only from the sun with an
//! orthographic projection fitted to the faces' bounds ([`fit_light`]). Any
//! receiver outside that footprint has no caster between it and the sun, so
//! the fit loses nothing; one map with 3x3 PCF reads well from a room to a
//! site block. Cascades were not needed: the fit does not follow the camera,
//! so nothing shimmers while orbiting, and a 4096² map keeps texels at a
//! few centimetres on a building and about 0.25 m on a 1 km site.
//!
//! The map is recorded in the paint callback's `prepare`, before the AO and
//! the scene, and only when the face geometry ([`crate::batch::Batch`]
//! versions) or the sun direction changed since the last render
//! ([`SceneRenderer::shadow_map_renders`] counts them): a camera move costs
//! nothing. One draw per batch through absolute indices, as the AO prepass.
//!
//! The light matrix lives in a small uniform bound, with the map and a
//! comparison sampler, in every pane's camera group; the face shaders
//! (`sun_visibility()` in `shaders/common.wgsl`) and the ground
//! (`ground_shadow()`) look it up. With every pane's shadows off the map is
//! freed and a 1x1 placeholder is bound, so panes (and iOS, which never
//! turns them on) draw exactly as before.

use bytemuck::Zeroable as _;

use crate::ao::FlatIndices;
use crate::renderer::SceneRenderer;

const DEPTH: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// Shadow map side in texels (capped by the device limit).
pub const SHADOW_MAP_SIZE: u32 = 4096;

/// The light uniform: `light_view_proj`, then `params` (x = 1 when the map
/// holds casters, y = one texel in map uv, z = one texel in world units,
/// w = the depth bias in map depth units).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct ShadowUniform {
    pub light_view_proj: [[f32; 4]; 4],
    pub params: [f32; 4],
}

/// Layout entry of the light uniform in the camera group.
pub(crate) fn uniform_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

/// Layout entry of the shadow map (a depth texture).
pub(crate) fn map_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Depth,
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

/// Layout entry of the comparison sampler.
pub(crate) fn sampler_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Comparison),
        count: None,
    }
}

fn depth_texture(device: &wgpu::Device, label: &str, size: u32) -> wgpu::TextureView {
    device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
        .create_view(&Default::default())
}

/// The light's view-projection fitted to the casters' bounds `lo..hi` for a
/// sun toward `dir` (unit), and the world size of the fitted extent (the
/// larger side, and the depth range).
pub fn fit_light(dir: glam::Vec3, lo: glam::Vec3, hi: glam::Vec3) -> (glam::Mat4, f32, f32) {
    use glam::Vec3;
    let center = (lo + hi) * 0.5;
    let radius = ((hi - lo).length() * 0.5).max(1e-3);
    let up = if dir.z.abs() > 0.99 { Vec3::Y } else { Vec3::Z };
    let view = glam::Mat4::look_at_rh(center + dir * (radius * 2.0), center, up);
    let (mut a, mut b) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    for i in 0..8 {
        let c = Vec3::new(
            if i & 1 == 0 { lo.x } else { hi.x },
            if i & 2 == 0 { lo.y } else { hi.y },
            if i & 4 == 0 { lo.z } else { hi.z },
        );
        let v = view.transform_point3(c);
        a = a.min(v);
        b = b.max(v);
    }
    let m = radius * 0.01 + 1e-3;
    // Right-handed view space looks down -z: near is the largest z.
    let (near, far) = (-b.z - m, -a.z + m);
    let proj = glam::Mat4::orthographic_rh(a.x - m, b.x + m, a.y - m, b.y + m, near, far);
    let extent = (b.x - a.x).max(b.y - a.y) + 2.0 * m;
    (proj * view, extent, far - near)
}

/// What the map was last rendered from.
#[derive(Clone, Copy, PartialEq)]
struct ShadowKey {
    meshes: u64,
    faces: u64,
    textured: u64,
    dir: [u32; 3],
    bounds: Option<[u32; 6]>,
}

struct ShadowPipelines {
    mesh: wgpu::RenderPipeline,
    face: wgpu::RenderPipeline,
}

impl ShadowPipelines {
    fn new(device: &wgpu::Device, layout: &wgpu::BindGroupLayout) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("shadow_shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/shadow.wgsl").into()),
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("shadow_pl"),
            bind_group_layouts: &[Some(layout)],
            immediate_size: 0,
        });
        let pipeline = |label, stride| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&pl),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_shadow"),
                    compilation_options: Default::default(),
                    buffers: &[wgpu::VertexBufferLayout {
                        // Position only; normals (and face colours) skipped.
                        array_stride: stride,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &wgpu::vertex_attr_array![0 => Float32x3],
                    }],
                },
                fragment: None,
                // Both sides cast: single-sided faces and open shells too.
                primitive: wgpu::PrimitiveState { cull_mode: None, ..Default::default() },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH,
                    depth_write_enabled: Some(true),
                    depth_compare: Some(wgpu::CompareFunction::LessEqual),
                    stencil: Default::default(),
                    // Slope-scaled bias against acne on grazing faces; the
                    // lookup adds a normal offset (common.wgsl).
                    bias: wgpu::DepthBiasState { constant: 2, slope_scale: 2.0, clamp: 0.0 },
                }),
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        Self { mesh: pipeline("shadow_mesh", 24), face: pipeline("shadow_face", 36) }
    }
}

/// Viewport shadow state owned by [`SceneRenderer`].
pub(crate) struct ViewportShadow {
    pipes: Option<ShadowPipelines>,
    layout: wgpu::BindGroupLayout,
    group: wgpu::BindGroup,
    ubo: wgpu::Buffer,
    sampler: wgpu::Sampler,
    /// 1x1 placeholder bound while no pane has shadows.
    empty: wgpu::TextureView,
    map: Option<wgpu::TextureView>,
    size: u32,
    flat: [FlatIndices; 3],
    /// Per pane: whether its last camera asked for shadows.
    wants: Vec<bool>,
    key: Option<ShadowKey>,
    renders: u64,
}

impl ViewportShadow {
    pub(crate) fn new(device: &wgpu::Device) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("shadow_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                visibility: wgpu::ShaderStages::VERTEX,
                ..uniform_entry(0)
            }],
        });
        let ubo = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("shadow_ubo"),
            size: std::mem::size_of::<ShadowUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("shadow_group"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: ubo.as_entire_binding() }],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("shadow_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            compare: Some(wgpu::CompareFunction::LessEqual),
            ..Default::default()
        });
        Self {
            pipes: None,
            layout,
            group,
            ubo,
            sampler,
            empty: depth_texture(device, "shadow_empty", 1),
            map: None,
            size: SHADOW_MAP_SIZE.min(device.limits().max_texture_dimension_2d),
            flat: std::array::from_fn(|_| FlatIndices::new("shadow_flat_indices")),
            wants: Vec::new(),
            key: None,
            renders: 0,
        }
    }

    pub(crate) fn uniform(&self) -> &wgpu::Buffer {
        &self.ubo
    }

    /// The map every pane binds: the real one, else the placeholder.
    pub(crate) fn map_view(&self) -> &wgpu::TextureView {
        self.map.as_ref().unwrap_or(&self.empty)
    }

    pub(crate) fn sampler(&self) -> &wgpu::Sampler {
        &self.sampler
    }
}

impl SceneRenderer {
    /// How many times the shadow map has been drawn (#357): it changes with
    /// the geometry or the sun, never with the camera alone.
    pub fn shadow_map_renders(&self) -> u64 {
        self.shadow.renders
    }

    /// Whether the shadow map is allocated (some pane has shadows on).
    pub fn has_shadow_map(&self) -> bool {
        self.shadow.map.is_some()
    }

    /// Bring the sun shadow map (#357) up to date for pane `viewport`'s last
    /// camera ([`Self::write_camera`]), before the scene is drawn (MSAA or
    /// not). The map is drawn only when the face geometry or the sun moved
    /// since its last render; it is allocated when the first pane turns
    /// shadows on and freed when none has them.
    pub fn render_shadow(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        viewport: usize,
    ) {
        let cam = self
            .last_cameras
            .get(viewport)
            .copied()
            .filter(|c| c.has_shadows() && viewport < self.cameras.len());
        let sh = &mut self.shadow;
        if sh.wants.len() <= viewport {
            sh.wants.resize(viewport + 1, false);
        }
        sh.wants[viewport] = cam.is_some();
        if !sh.wants.iter().any(|w| *w) {
            if sh.map.take().is_some() {
                sh.key = None;
                sh.flat = std::array::from_fn(|_| FlatIndices::new("shadow_flat_indices"));
                self.rebind_cameras(device);
            }
            return;
        }
        if sh.map.is_none() {
            sh.map = Some(depth_texture(device, "shadow_map", sh.size));
            sh.key = None;
            self.rebind_cameras(device);
        }
        let Some(cam) = cam else {
            return;
        };
        let dir = glam::Vec3::new(cam.shadow_dir[0], cam.shadow_dir[1], cam.shadow_dir[2]);
        // A sun below the horizon casts nothing: the faces fall to ambient.
        let casters = self.caster_bounds.filter(|_| dir.z > 0.0);
        let key = ShadowKey {
            meshes: self.meshes.version,
            faces: self.face_meshes.version,
            textured: self.tex_meshes.version,
            dir: dir.to_array().map(f32::to_bits),
            bounds: casters.map(|(lo, hi)| {
                let [a, b, c] = lo.to_array().map(f32::to_bits);
                let [d, e, f] = hi.to_array().map(f32::to_bits);
                [a, b, c, d, e, f]
            }),
        };
        let sh = &mut self.shadow;
        if sh.key == Some(key) {
            return;
        }
        sh.key = Some(key);
        let Some((lo, hi)) = casters else {
            queue.write_buffer(&sh.ubo, 0, bytemuck::bytes_of(&ShadowUniform::zeroed()));
            return;
        };
        let (light, extent, depth_range) = fit_light(dir, lo, hi);
        let texel = extent / sh.size as f32;
        let u = ShadowUniform {
            light_view_proj: light.to_cols_array_2d(),
            params: [1.0, 1.0 / sh.size as f32, texel, texel / depth_range.max(1e-6)],
        };
        queue.write_buffer(&sh.ubo, 0, bytemuck::bytes_of(&u));
        let pipes = sh.pipes.get_or_insert_with(|| ShadowPipelines::new(device, &sh.layout));
        let [flat_meshes, flat_faces, flat_tex] = &mut sh.flat;
        flat_meshes.update(device, queue, &self.meshes);
        flat_faces.update(device, queue, &self.face_meshes);
        flat_tex.update(device, queue, &self.tex_meshes);
        let Some(map) = &sh.map else {
            return;
        };
        let mut p = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("shadow_pass"),
            color_attachments: &[],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: map,
                depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Store }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        p.set_bind_group(0, &sh.group, &[]);
        p.set_pipeline(&pipes.mesh);
        sh.flat[0].draw(&mut p, &self.meshes);
        p.set_pipeline(&pipes.face);
        sh.flat[1].draw(&mut p, &self.face_meshes);
        sh.flat[2].draw(&mut p, &self.tex_meshes);
        sh.renders += 1;
    }

    /// Rebuild every pane's camera group after the shadow map changed.
    fn rebind_cameras(&mut self, device: &wgpu::Device) {
        for i in 0..self.cameras.len() {
            self.rebind_camera(device, i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Vec3;

    #[test]
    fn fit_covers_the_casters_and_looks_down_the_sun() {
        let (lo, hi) = (Vec3::new(-10.0, -5.0, 0.0), Vec3::new(30.0, 15.0, 12.0));
        for dir in [Vec3::new(0.3, -0.4, 0.8).normalize(), Vec3::Z, Vec3::new(-1.0, 0.2, 0.05).normalize()] {
            let (m, extent, range) = fit_light(dir, lo, hi);
            assert!(extent > 0.0 && range > 0.0);
            for i in 0..8 {
                let c = Vec3::new(
                    if i & 1 == 0 { lo.x } else { hi.x },
                    if i & 2 == 0 { lo.y } else { hi.y },
                    if i & 4 == 0 { lo.z } else { hi.z },
                );
                let p = m.project_point3(c);
                assert!(p.x.abs() <= 1.0 && p.y.abs() <= 1.0, "{dir}: {c} in the map");
                assert!((0.0..=1.0).contains(&p.z), "{dir}: {c} in depth");
            }
            // Nearer the sun is nearer in the map.
            let mid = (lo + hi) * 0.5;
            assert!(m.project_point3(mid + dir).z < m.project_point3(mid).z, "{dir}");
        }
    }
}
