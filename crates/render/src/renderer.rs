use bytemuck::{Pod, Zeroable};
use kernel_mesh::RenderMesh;

use crate::DEPTH_FORMAT;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct CameraUniform {
    pub view_proj: [[f32; 4]; 4],
    pub inv_view_proj: [[f32; 4]; 4],
    pub eye: [f32; 4],
    /// x = mesh fill alpha multiplier (display mode); y,z,w spare.
    pub misc: [f32; 4],
}

/// Per-viewport display mode. View state, not model state: never logged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DisplayMode {
    #[default]
    Shaded,
    /// Feature edges only, no fill.
    Wireframe,
    /// Nearly transparent fill without depth writes + edges show through.
    XRay,
    /// Half-transparent fill + edges.
    Ghosted,
    /// Architect's hidden-line drawing: paper-white fill with depth writes
    /// (occlusion) and black feature edges on top. Background and grid also
    /// switch to paper white regardless of the egui theme.
    Pencil,
}

impl DisplayMode {
    pub const ALL: [DisplayMode; 5] = [
        DisplayMode::Shaded,
        DisplayMode::Wireframe,
        DisplayMode::XRay,
        DisplayMode::Ghosted,
        DisplayMode::Pencil,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DisplayMode::Shaded => "Shaded",
            DisplayMode::Wireframe => "Wireframe",
            DisplayMode::XRay => "X-Ray",
            DisplayMode::Ghosted => "Ghosted",
            DisplayMode::Pencil => "Pencil",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "shaded" => Some(DisplayMode::Shaded),
            "wireframe" | "wire" => Some(DisplayMode::Wireframe),
            "xray" | "x-ray" => Some(DisplayMode::XRay),
            "ghosted" | "ghost" => Some(DisplayMode::Ghosted),
            "pencil" => Some(DisplayMode::Pencil),
            _ => None,
        }
    }

    /// Mesh fill alpha multiplier fed to the shader via the camera UBO.
    /// Pencil uses -1.0 as a sentinel: the shader interprets negative values
    /// as "pencil mode — render paper-white, ignore object color".
    pub fn fill_alpha(self) -> f32 {
        match self {
            DisplayMode::Shaded => 1.0,
            DisplayMode::Wireframe => 0.0, // fill pass skipped entirely
            DisplayMode::XRay => 0.18,
            DisplayMode::Ghosted => 0.55,
            DisplayMode::Pencil => -1.0, // sentinel: pencil white fill
        }
    }

    pub fn draws_fill(self) -> bool {
        self != DisplayMode::Wireframe
    }

    pub fn draws_edges(self) -> bool {
        self != DisplayMode::Shaded
    }

    /// Transparent modes skip depth writes so geometry reads through.
    /// Pencil uses depth writes: occlusion is the whole point of hidden-line.
    pub fn depth_writes(self) -> bool {
        matches!(self, DisplayMode::Shaded | DisplayMode::Pencil)
    }

    /// Pencil mode forces the viewport clear colour to paper white regardless
    /// of the egui dark/light theme.
    pub fn pencil_background() -> [f32; 4] {
        [0.97, 0.97, 0.95, 1.0]
    }
}

/// How object colors are resolved in the viewport. View state — never logged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorMode {
    /// Object color → layer color → theme default (current behavior).
    #[default]
    ByLayer,
    /// Per-object color override wins; falls back to layer then theme.
    ByObject,
    /// Fixed hue per geometry type: teal for meshes, white/dark for curves,
    /// amber for annotations.
    ByType,
    /// Stable hash of the object id → unique hue; great for untangling imports.
    Random,
}

impl ColorMode {
    pub const ALL: [ColorMode; 4] = [
        ColorMode::ByLayer,
        ColorMode::ByObject,
        ColorMode::ByType,
        ColorMode::Random,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ColorMode::ByLayer => "By Layer",
            ColorMode::ByObject => "By Object",
            ColorMode::ByType => "By Type",
            ColorMode::Random => "Random",
        }
    }
}

/// Stable hue for a given u64 seed (e.g. hash of object id bytes).
/// Returns a fully-saturated, medium-lightness RGBA color.
pub fn hue_from_seed(seed: u64) -> [f32; 4] {
    // Mix the bits to spread similar ids far apart on the hue wheel.
    let h = seed.wrapping_mul(0x9e3779b97f4a7c15).wrapping_add(seed >> 16);
    let hue = (h & 0xFFFF) as f32 / 65536.0; // 0..1
    hsv_to_rgb(hue, 0.75, 0.85)
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [f32; 4] {
    let i = (h * 6.0).floor() as u32 % 6;
    let f = h * 6.0 - (h * 6.0).floor();
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    let (r, g, b) = match i {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    [r, g, b, 1.0]
}

#[cfg(test)]
mod tests {
    use super::DisplayMode;

    #[test]
    fn pencil_parse_round_trip() {
        assert_eq!(DisplayMode::parse("pencil"), Some(DisplayMode::Pencil));
        assert_eq!(DisplayMode::Pencil.label(), "Pencil");
    }

    #[test]
    fn pencil_in_all() {
        assert!(DisplayMode::ALL.contains(&DisplayMode::Pencil));
    }

    #[test]
    fn pencil_fill_alpha_is_negative_sentinel() {
        assert!(DisplayMode::Pencil.fill_alpha() < 0.0);
        // Other modes are non-negative
        for mode in DisplayMode::ALL {
            if mode != DisplayMode::Pencil {
                assert!(mode.fill_alpha() >= 0.0, "{mode:?} should have non-negative alpha");
            }
        }
    }

    #[test]
    fn pencil_draws_fill_and_edges_with_depth_writes() {
        assert!(DisplayMode::Pencil.draws_fill());
        assert!(DisplayMode::Pencil.draws_edges());
        assert!(DisplayMode::Pencil.depth_writes());
    }

    #[test]
    fn pencil_background_is_near_white() {
        let bg = DisplayMode::pencil_background();
        // All channels > 0.9 (near white)
        assert!(bg[0] > 0.9 && bg[1] > 0.9 && bg[2] > 0.9);
        assert_eq!(bg[3], 1.0);
    }

    #[test]
    fn all_modes_parse() {
        for (s, expected) in [
            ("shaded", DisplayMode::Shaded),
            ("wireframe", DisplayMode::Wireframe),
            ("wire", DisplayMode::Wireframe),
            ("xray", DisplayMode::XRay),
            ("x-ray", DisplayMode::XRay),
            ("ghosted", DisplayMode::Ghosted),
            ("ghost", DisplayMode::Ghosted),
            ("pencil", DisplayMode::Pencil),
        ] {
            assert_eq!(DisplayMode::parse(s), Some(expected), "parse({s:?}) failed");
        }
        assert_eq!(DisplayMode::parse("unknown"), None);
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ObjectParams {
    color: [f32; 4],
}

struct GpuMesh {
    vertex_buf: wgpu::Buffer,
    index_buf: wgpu::Buffer,
    index_count: u32,
    object_bind_group: wgpu::BindGroup,
}

struct GpuLine {
    vertex_buf: wgpu::Buffer,
    vertex_count: u32,
    object_bind_group: wgpu::BindGroup,
}

struct GpuUnderlay {
    vertex_buf: wgpu::Buffer,
    index_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

/// A decoded raster underlay ready for GPU upload: RGBA8 pixels plus the four
/// ground-plane corners (CCW from lower-left) and blend opacity.
pub struct UnderlayData {
    pub rgba: Vec<u8>,
    pub width_px: u32,
    pub height_px: u32,
    /// World-space quad corners (z included), CCW from lower-left.
    pub corners: [[f32; 3]; 4],
    pub opacity: f32,
}

/// CPU-side scene snapshot handed to the renderer when the document changes.
pub struct SceneData {
    pub meshes: Vec<(RenderMesh, [f32; 4])>,
    /// Line strips: consecutive points, `closed` handled by repeating the seam
    /// point on the CPU side.
    pub lines: Vec<(Vec<[f32; 3]>, [f32; 4])>,
    /// Mesh feature edges as flat segment lists (point pairs), drawn in the
    /// wireframe/x-ray/ghosted display modes.
    pub edges: Vec<(Vec<[f32; 3]>, [f32; 4])>,
    /// Optional raster reference image on the ground plane.
    pub underlay: Option<UnderlayData>,
}

/// Owns all wgpu resources; lives in egui-wgpu's `CallbackResources` type map.
pub struct SceneRenderer {
    grid_pipeline: wgpu::RenderPipeline,
    mesh_pipeline: wgpu::RenderPipeline,
    /// Mesh fill without depth writes, for the transparent display modes.
    mesh_xray_pipeline: wgpu::RenderPipeline,
    curve_pipeline: wgpu::RenderPipeline,
    /// LineList variant of the curve pipeline for mesh feature edges.
    edge_pipeline: wgpu::RenderPipeline,
    camera_layout: wgpu::BindGroupLayout,
    /// One camera UBO + bind group per viewport pane. egui-wgpu runs every
    /// callback's `prepare` before any `paint`, so panes must not share a
    /// single camera buffer — the last write would win for all of them.
    cameras: Vec<(wgpu::Buffer, wgpu::BindGroup)>,
    object_layout: wgpu::BindGroupLayout,
    /// Textured-quad pipeline + its (texture, sampler, opacity) bind layout.
    underlay_pipeline: wgpu::RenderPipeline,
    underlay_layout: wgpu::BindGroupLayout,
    meshes: Vec<GpuMesh>,
    lines: Vec<GpuLine>,
    edges: Vec<GpuLine>,
    underlay: Option<GpuUnderlay>,
    /// Document generation the GPU buffers were built from.
    pub generation: u64,
}

impl SceneRenderer {
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let camera_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("camera_layout"),
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
        let object_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("object_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let grid_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("grid_shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/grid.wgsl").into()),
        });
        let mesh_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("mesh_shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/mesh.wgsl").into()),
        });

        let grid_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("grid_pl"),
            bind_group_layouts: &[Some(&camera_layout)],
            immediate_size: 0,
        });
        let grid_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("grid_pipeline"),
            layout: Some(&grid_layout),
            vertex: wgpu::VertexState {
                module: &grid_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &grid_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    // Premultiplied alpha
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
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
        });

        let mesh_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("mesh_pl"),
            bind_group_layouts: &[Some(&camera_layout), Some(&object_layout)],
            immediate_size: 0,
        });
        let mesh_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("mesh_pipeline"),
            layout: Some(&mesh_layout),
            vertex: wgpu::VertexState {
                module: &mesh_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 24, // 3 f32 position + 3 f32 normal
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &mesh_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                cull_mode: Some(wgpu::Face::Back),
                ..Default::default()
            },
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
        });

        // Same shading, but no depth writes and no culling: transparent modes
        // must not occlude, and back faces keep x-ray solids readable.
        let mesh_xray_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("mesh_xray_pipeline"),
            layout: Some(&mesh_layout),
            vertex: wgpu::VertexState {
                module: &mesh_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 24,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &mesh_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let curve_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("curve_shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/curve.wgsl").into()),
        });
        let curve_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("curve_pipeline"),
            layout: Some(&mesh_layout),
            vertex: wgpu::VertexState {
                module: &curve_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 12,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x3],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &curve_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::LineStrip,
                ..Default::default()
            },
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
        });

        // Feature edges: same shader as curves, LineList topology (segment
        // soup instead of strips).
        let edge_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("edge_pipeline"),
            layout: Some(&mesh_layout),
            vertex: wgpu::VertexState {
                module: &curve_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 12,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x3],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &curve_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::LineList,
                ..Default::default()
            },
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
        });

        // Underlay: a textured quad. Bind group 1 = texture + sampler + opacity.
        let underlay_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("underlay_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let underlay_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("underlay_shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/underlay.wgsl").into()),
        });
        let underlay_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("underlay_pl"),
            bind_group_layouts: &[Some(&camera_layout), Some(&underlay_layout)],
            immediate_size: 0,
        });
        let underlay_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("underlay_pipeline"),
            layout: Some(&underlay_pl),
            vertex: wgpu::VertexState {
                module: &underlay_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 20, // 3 f32 position + 2 f32 uv
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x2],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &underlay_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
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
        });

        Self {
            grid_pipeline,
            mesh_pipeline,
            mesh_xray_pipeline,
            curve_pipeline,
            edge_pipeline,
            camera_layout,
            cameras: Vec::new(),
            object_layout,
            underlay_pipeline,
            underlay_layout,
            meshes: Vec::new(),
            lines: Vec::new(),
            edges: Vec::new(),
            underlay: None,
            generation: u64::MAX,
        }
    }

    /// Upload the camera for one viewport pane, growing the slot list on demand.
    pub fn write_camera(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        viewport: usize,
        cam: &CameraUniform,
    ) {
        while self.cameras.len() <= viewport {
            let buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("camera_ubo"),
                size: std::mem::size_of::<CameraUniform>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("camera_bind_group"),
                layout: &self.camera_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buf.as_entire_binding(),
                }],
            });
            self.cameras.push((buf, bind_group));
        }
        queue.write_buffer(&self.cameras[viewport].0, 0, bytemuck::bytes_of(cam));
    }

    /// Rebuild all mesh buffers. Called when the document generation changes;
    /// fine at MVP scale, batch/diff later.
    pub fn set_meshes(
        &mut self,
        device: &wgpu::Device,
        meshes: &[(RenderMesh, [f32; 4])],
        generation: u64,
    ) {
        use wgpu::util::DeviceExt as _;
        self.meshes.clear();
        for (mesh, color) in meshes {
            let mut vertices = Vec::with_capacity(mesh.positions.len() * 6);
            for (p, n) in mesh.positions.iter().zip(&mesh.normals) {
                vertices.extend_from_slice(p);
                vertices.extend_from_slice(n);
            }
            let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("mesh_vb"),
                contents: bytemuck::cast_slice(&vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });
            let index_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("mesh_ib"),
                contents: bytemuck::cast_slice(&mesh.indices),
                usage: wgpu::BufferUsages::INDEX,
            });
            let params = ObjectParams { color: *color };
            let object_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("object_ubo"),
                contents: bytemuck::bytes_of(&params),
                usage: wgpu::BufferUsages::UNIFORM,
            });
            let object_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("object_bg"),
                layout: &self.object_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: object_buf.as_entire_binding(),
                }],
            });
            self.meshes.push(GpuMesh {
                vertex_buf,
                index_buf,
                index_count: mesh.indices.len() as u32,
                object_bind_group,
            });
        }
        self.generation = generation;
    }

    fn object_bind_group(&self, device: &wgpu::Device, color: [f32; 4]) -> wgpu::BindGroup {
        use wgpu::util::DeviceExt as _;
        let params = ObjectParams { color };
        let object_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("object_ubo"),
            contents: bytemuck::bytes_of(&params),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("object_bg"),
            layout: &self.object_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: object_buf.as_entire_binding(),
            }],
        })
    }

    /// Rebuild all GPU buffers from a scene snapshot. The queue uploads texture
    /// pixels for the underlay (buffers go through create_buffer_init).
    pub fn set_scene(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        scene: &SceneData,
        generation: u64,
    ) {
        use wgpu::util::DeviceExt as _;
        self.set_meshes(device, &scene.meshes, generation);
        self.lines.clear();
        for (points, color) in &scene.lines {
            if points.len() < 2 {
                continue;
            }
            let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("curve_vb"),
                contents: bytemuck::cast_slice(points),
                usage: wgpu::BufferUsages::VERTEX,
            });
            self.lines.push(GpuLine {
                vertex_buf,
                vertex_count: points.len() as u32,
                object_bind_group: self.object_bind_group(device, *color),
            });
        }
        self.edges.clear();
        for (points, color) in &scene.edges {
            if points.len() < 2 {
                continue;
            }
            let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("edge_vb"),
                contents: bytemuck::cast_slice(points),
                usage: wgpu::BufferUsages::VERTEX,
            });
            self.edges.push(GpuLine {
                vertex_buf,
                vertex_count: points.len() as u32,
                object_bind_group: self.object_bind_group(device, *color),
            });
        }

        self.underlay = scene
            .underlay
            .as_ref()
            .map(|u| self.build_underlay(device, queue, u));
    }

    /// Upload one underlay: an RGBA8 texture, a quad (two triangles) with uvs,
    /// and an opacity UBO, wired into one bind group.
    fn build_underlay(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        u: &UnderlayData,
    ) -> GpuUnderlay {
        use wgpu::util::DeviceExt as _;
        let size = wgpu::Extent3d {
            width: u.width_px.max(1),
            height: u.height_px.max(1),
            depth_or_array_layers: 1,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("underlay_tex"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &u.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * size.width),
                rows_per_image: Some(size.height),
            },
            size,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("underlay_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let opacity_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("underlay_opacity"),
            contents: bytemuck::bytes_of(&[u.opacity, 0.0, 0.0, 0.0]),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("underlay_bg"),
            layout: &self.underlay_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
                wgpu::BindGroupEntry { binding: 2, resource: opacity_buf.as_entire_binding() },
            ],
        });
        // Two triangles. uv v flipped so the image top maps to +y (north-up).
        let [c0, c1, c2, c3] = u.corners; // ll, lr, ur, ul
        let verts: [[f32; 5]; 4] = [
            [c0[0], c0[1], c0[2], 0.0, 1.0],
            [c1[0], c1[1], c1[2], 1.0, 1.0],
            [c2[0], c2[1], c2[2], 1.0, 0.0],
            [c3[0], c3[1], c3[2], 0.0, 0.0],
        ];
        let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("underlay_vb"),
            contents: bytemuck::cast_slice(&verts),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let indices: [u16; 6] = [0, 1, 2, 0, 2, 3];
        let index_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("underlay_ib"),
            contents: bytemuck::cast_slice(&indices),
            usage: wgpu::BufferUsages::INDEX,
        });
        GpuUnderlay { vertex_buf, index_buf, bind_group }
    }

    pub fn paint(
        &self,
        render_pass: &mut wgpu::RenderPass<'static>,
        viewport: usize,
        mode: DisplayMode,
    ) {
        let Some((_, camera_bind_group)) = self.cameras.get(viewport) else {
            return; // paint before any prepare for this pane — nothing to draw yet
        };
        render_pass.set_bind_group(0, camera_bind_group, &[]);

        // Underlay first: on the ground plane, depth-written so meshes occlude
        // it and the grid blends over it.
        if let Some(u) = &self.underlay {
            render_pass.set_pipeline(&self.underlay_pipeline);
            render_pass.set_bind_group(1, &u.bind_group, &[]);
            render_pass.set_vertex_buffer(0, u.vertex_buf.slice(..));
            render_pass.set_index_buffer(u.index_buf.slice(..), wgpu::IndexFormat::Uint16);
            render_pass.draw_indexed(0..6, 0, 0..1);
        }

        if mode.draws_fill() {
            render_pass.set_pipeline(if mode.depth_writes() {
                &self.mesh_pipeline
            } else {
                &self.mesh_xray_pipeline
            });
            for mesh in &self.meshes {
                render_pass.set_bind_group(1, &mesh.object_bind_group, &[]);
                render_pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
                render_pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
                render_pass.draw_indexed(0..mesh.index_count, 0, 0..1);
            }
        }

        if mode.draws_edges() {
            render_pass.set_pipeline(&self.edge_pipeline);
            for edge in &self.edges {
                render_pass.set_bind_group(1, &edge.object_bind_group, &[]);
                render_pass.set_vertex_buffer(0, edge.vertex_buf.slice(..));
                render_pass.draw(0..edge.vertex_count, 0..1);
            }
        }

        render_pass.set_pipeline(&self.curve_pipeline);
        for line in &self.lines {
            render_pass.set_bind_group(1, &line.object_bind_group, &[]);
            render_pass.set_vertex_buffer(0, line.vertex_buf.slice(..));
            render_pass.draw(0..line.vertex_count, 0..1);
        }

        // Grid last: blends over background, depth-tested against meshes.
        render_pass.set_pipeline(&self.grid_pipeline);
        render_pass.draw(0..3, 0..1);
    }
}
