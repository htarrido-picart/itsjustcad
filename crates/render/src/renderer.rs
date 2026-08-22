use bytemuck::{Pod, Zeroable};
use kernel_mesh::RenderMesh;

use crate::DEPTH_FORMAT;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct CameraUniform {
    pub view_proj: [[f32; 4]; 4],
    pub inv_view_proj: [[f32; 4]; 4],
    pub eye: [f32; 4],
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

/// Owns all wgpu resources; lives in egui-wgpu's `CallbackResources` type map.
pub struct SceneRenderer {
    grid_pipeline: wgpu::RenderPipeline,
    mesh_pipeline: wgpu::RenderPipeline,
    camera_buf: wgpu::Buffer,
    camera_bind_group: wgpu::BindGroup,
    object_layout: wgpu::BindGroupLayout,
    meshes: Vec<GpuMesh>,
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

        let camera_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("camera_ubo"),
            size: std::mem::size_of::<CameraUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let camera_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("camera_bind_group"),
            layout: &camera_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: camera_buf.as_entire_binding(),
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

        Self {
            grid_pipeline,
            mesh_pipeline,
            camera_buf,
            camera_bind_group,
            object_layout,
            meshes: Vec::new(),
            generation: u64::MAX,
        }
    }

    pub fn write_camera(&self, queue: &wgpu::Queue, cam: &CameraUniform) {
        queue.write_buffer(&self.camera_buf, 0, bytemuck::bytes_of(cam));
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

    pub fn paint(&self, render_pass: &mut wgpu::RenderPass<'static>) {
        render_pass.set_bind_group(0, &self.camera_bind_group, &[]);

        render_pass.set_pipeline(&self.mesh_pipeline);
        for mesh in &self.meshes {
            render_pass.set_bind_group(1, &mesh.object_bind_group, &[]);
            render_pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
            render_pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
            render_pass.draw_indexed(0..mesh.index_count, 0, 0..1);
        }

        // Grid last: blends over background, depth-tested against meshes.
        render_pass.set_pipeline(&self.grid_pipeline);
        render_pass.draw(0..3, 0..1);
    }
}
