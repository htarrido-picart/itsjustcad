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
}

impl DisplayMode {
    pub const ALL: [DisplayMode; 4] = [
        DisplayMode::Shaded,
        DisplayMode::Wireframe,
        DisplayMode::XRay,
        DisplayMode::Ghosted,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DisplayMode::Shaded => "Shaded",
            DisplayMode::Wireframe => "Wireframe",
            DisplayMode::XRay => "X-Ray",
            DisplayMode::Ghosted => "Ghosted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "shaded" => Some(DisplayMode::Shaded),
            "wireframe" | "wire" => Some(DisplayMode::Wireframe),
            "xray" | "x-ray" => Some(DisplayMode::XRay),
            "ghosted" | "ghost" => Some(DisplayMode::Ghosted),
            _ => None,
        }
    }

    /// Mesh fill alpha multiplier fed to the shader via the camera UBO.
    pub fn fill_alpha(self) -> f32 {
        match self {
            DisplayMode::Shaded => 1.0,
            DisplayMode::Wireframe => 0.0, // fill pass skipped entirely
            DisplayMode::XRay => 0.18,
            DisplayMode::Ghosted => 0.55,
        }
    }

    pub fn draws_fill(self) -> bool {
        self != DisplayMode::Wireframe
    }

    pub fn draws_edges(self) -> bool {
        self != DisplayMode::Shaded
    }

    /// Transparent modes skip depth writes so geometry reads through.
    pub fn depth_writes(self) -> bool {
        self == DisplayMode::Shaded
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

/// CPU-side scene snapshot handed to the renderer when the document changes.
pub struct SceneData {
    pub meshes: Vec<(RenderMesh, [f32; 4])>,
    /// Line strips: consecutive points, `closed` handled by repeating the seam
    /// point on the CPU side.
    pub lines: Vec<(Vec<[f32; 3]>, [f32; 4])>,
    /// Mesh feature edges as flat segment lists (point pairs), drawn in the
    /// wireframe/x-ray/ghosted display modes.
    pub edges: Vec<(Vec<[f32; 3]>, [f32; 4])>,
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
    meshes: Vec<GpuMesh>,
    lines: Vec<GpuLine>,
    edges: Vec<GpuLine>,
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

        Self {
            grid_pipeline,
            mesh_pipeline,
            mesh_xray_pipeline,
            curve_pipeline,
            edge_pipeline,
            camera_layout,
            cameras: Vec::new(),
            object_layout,
            meshes: Vec::new(),
            lines: Vec::new(),
            edges: Vec::new(),
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

    /// Rebuild all GPU buffers from a scene snapshot.
    pub fn set_scene(&mut self, device: &wgpu::Device, scene: &SceneData, generation: u64) {
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
