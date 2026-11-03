// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Shared GPU buffers for scene geometry and per-object parameters (#269).
//!
//! Every object kind (meshes, curves, edges, profile ribbons, points) packs
//! its geometry into one shared vertex buffer (plus one index buffer for the
//! indexed kinds) and draws each object as a range of it. All per-object
//! color/material parameters live in ONE uniform buffer addressed with a
//! dynamic offset, so the whole scene binds a single object bind group.
//!
//! Uploads are diffed against a CPU copy of what the GPU holds: a scene whose
//! geometry did not change (selection recolor, theme flip, color mode) only
//! rewrites the parameter buffer in place with `queue.write_buffer`.

use std::num::NonZeroU64;

use bytemuck::{Pod, Zeroable};

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Pod, Zeroable)]
pub(crate) struct ObjectParams {
    pub color: [f32; 4],
    /// x = roughness (0 smooth .. 1 matte), y = metallic (0 dielectric .. 1
    /// metal), z,w spare. Drives the specular term in the mesh shader so
    /// `material2` presets read differently (glass shiny, concrete matte).
    pub material: [f32; 4],
}

impl ObjectParams {
    /// Parameters for an unlit stroke or point: color plus the default
    /// mid-rough dielectric material.
    pub fn stroke(color: [f32; 4]) -> Self {
        Self { color, material: [0.5, 0.0, 0.0, 0.0] }
    }
}

/// One object's draw out of a shared batch: an index range with a base vertex
/// (indexed batches) or a vertex range (line and point batches), plus the
/// object's slot in the batch's parameter list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Draw {
    pub first: u32,
    pub count: u32,
    pub base_vertex: i32,
    pub slot: u32,
}

impl Draw {
    /// Segment (instance) range for the fat-stroke pass, which reads the
    /// vertex buffer as one `[a, b]` pair per instance. Line-list batches keep
    /// every draw's `first` even (see [`Staged::push_segments`]), so a range
    /// never straddles two objects.
    pub fn segments(&self) -> std::ops::Range<u32> {
        self.first / 2..self.first / 2 + self.count / 2
    }

    pub fn range(&self) -> std::ops::Range<u32> {
        self.first..self.first + self.count
    }
}

/// CPU-side packing of one batch, built from the scene snapshot before
/// anything touches the GPU.
#[derive(Default, Debug, PartialEq)]
pub(crate) struct Staged {
    /// Flat vertex floats (`[x, y, z]` or `[x, y, z, nx, ny, nz]` per vertex).
    pub vertices: Vec<f32>,
    pub indices: Vec<u32>,
    pub draws: Vec<Draw>,
    pub params: Vec<ObjectParams>,
}

impl std::fmt::Debug for ObjectParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ObjectParams({:?}, {:?})", self.color, self.material)
    }
}

impl Staged {
    /// Append an indexed triangle mesh with interleaved position + normal
    /// vertices. Meshes with no triangles are skipped (they drew nothing).
    pub fn push_mesh(
        &mut self,
        positions: &[[f32; 3]],
        normals: &[[f32; 3]],
        indices: &[u32],
        params: ObjectParams,
    ) {
        if indices.is_empty() {
            return;
        }
        let base_vertex = (self.vertices.len() / 6) as i32;
        for (p, n) in positions.iter().zip(normals) {
            self.vertices.extend_from_slice(p);
            self.vertices.extend_from_slice(n);
        }
        self.draws.push(Draw {
            first: self.indices.len() as u32,
            count: indices.len() as u32,
            base_vertex,
            slot: self.params.len() as u32,
        });
        self.indices.extend_from_slice(indices);
        self.params.push(params);
    }

    /// Append a run of `[x, y, z]` points drawn as one vertex range.
    pub fn push_points(&mut self, points: &[[f32; 3]], params: ObjectParams) {
        self.draws.push(Draw {
            first: (self.vertices.len() / 3) as u32,
            count: points.len() as u32,
            base_vertex: 0,
            slot: self.params.len() as u32,
        });
        self.vertices.extend(points.iter().flatten());
        self.params.push(params);
    }

    /// Append a line-list segment soup. Pads an odd point count with one
    /// unused vertex so the next object still starts on a pair boundary for
    /// the fat-stroke instance range.
    pub fn push_segments(&mut self, points: &[[f32; 3]], params: ObjectParams) {
        self.push_points(points, params);
        if points.len() % 2 == 1 {
            self.vertices.extend_from_slice(&[0.0; 3]);
        }
    }
}

/// A GPU buffer kept across uploads: rewritten in place while the data fits,
/// reallocated only when it must grow.
pub(crate) struct GrowBuffer {
    label: &'static str,
    usage: wgpu::BufferUsages,
    pub buf: Option<wgpu::Buffer>,
}

impl GrowBuffer {
    pub fn new(label: &'static str, usage: wgpu::BufferUsages) -> Self {
        Self { label, usage: usage | wgpu::BufferUsages::COPY_DST, buf: None }
    }

    /// Upload `bytes` to the start of the buffer.
    pub fn write(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, bytes: &[u8]) {
        if !self.alloc_init(device, bytes)
            && let Some(buf) = &self.buf
        {
            queue.write_buffer(buf, 0, bytes);
        }
    }

    /// Allocate a new buffer holding `bytes` when the current one is missing
    /// or too small; returns `false` (nothing done) when `bytes` already fit,
    /// in which case the caller writes them through the queue. Empty data
    /// keeps the old buffer: no draw reads it.
    pub fn alloc_init(&mut self, device: &wgpu::Device, bytes: &[u8]) -> bool {
        let len = bytes.len() as u64;
        if len == 0 || self.buf.as_ref().is_some_and(|b| b.size() >= len) {
            return false;
        }
        // Exact size on first upload; a quarter of headroom when growing so a
        // run of small additive edits does not reallocate every time.
        let size = if self.buf.is_some() { len + len / 4 } else { len }
            .next_multiple_of(wgpu::COPY_BUFFER_ALIGNMENT);
        let buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(self.label),
            size,
            usage: self.usage,
            mapped_at_creation: true,
        });
        buf.slice(..len).get_mapped_range_mut().copy_from_slice(bytes);
        buf.unmap();
        self.buf = Some(buf);
        true
    }
}

/// Shared geometry for one object kind: a vertex buffer, an optional index
/// buffer, and the per-object draws and parameters.
pub(crate) struct Batch {
    vertex_buf: GrowBuffer,
    index_buf: GrowBuffer,
    /// What the GPU buffers currently hold, compared on upload so unchanged
    /// geometry is never re-sent.
    vertices: Vec<f32>,
    indices: Vec<u32>,
    pub draws: Vec<Draw>,
    pub params: Vec<ObjectParams>,
    /// First slot of `params` in the shared [`ParamBuffer`].
    pub slot_base: u32,
}

impl Batch {
    pub fn new(label: &'static str) -> Self {
        Self {
            vertex_buf: GrowBuffer::new(label, wgpu::BufferUsages::VERTEX),
            index_buf: GrowBuffer::new(label, wgpu::BufferUsages::INDEX),
            vertices: Vec::new(),
            indices: Vec::new(),
            draws: Vec::new(),
            params: Vec::new(),
            slot_base: 0,
        }
    }

    /// Take a staged packing; geometry goes to the GPU only when it differs
    /// from what is already there. Returns whether geometry was uploaded.
    pub fn set(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, s: Staged) -> bool {
        let mut uploaded = false;
        if bytemuck::cast_slice::<f32, u32>(&s.vertices)
            != bytemuck::cast_slice::<f32, u32>(&self.vertices)
        {
            self.vertex_buf.write(device, queue, bytemuck::cast_slice(&s.vertices));
            self.vertices = s.vertices;
            uploaded = true;
        }
        if s.indices != self.indices {
            self.index_buf.write(device, queue, bytemuck::cast_slice(&s.indices));
            self.indices = s.indices;
            uploaded = true;
        }
        self.draws = s.draws;
        self.params = s.params;
        uploaded
    }

    /// Like [`Batch::set`] for a batch built before any queue exists (the
    /// world axes in `SceneRenderer::new`).
    pub fn init(&mut self, device: &wgpu::Device, s: Staged) {
        self.vertex_buf.alloc_init(device, bytemuck::cast_slice(&s.vertices));
        self.index_buf.alloc_init(device, bytemuck::cast_slice(&s.indices));
        self.vertices = s.vertices;
        self.indices = s.indices;
        self.draws = s.draws;
        self.params = s.params;
    }

    pub fn vertex_slice(&self) -> Option<wgpu::BufferSlice<'_>> {
        if self.draws.is_empty() {
            return None;
        }
        self.vertex_buf.buf.as_ref().map(|b| b.slice(..))
    }

    pub fn index_slice(&self) -> Option<wgpu::BufferSlice<'_>> {
        self.index_buf.buf.as_ref().map(|b| b.slice(..))
    }
}

/// One uniform buffer holding every object's [`ObjectParams`] at a fixed
/// stride, bound once through a dynamic-offset bind group.
pub(crate) struct ParamBuffer {
    stride: u64,
    buf: GrowBuffer,
    bind_group: Option<wgpu::BindGroup>,
    /// Padded bytes the GPU buffer holds, compared to skip no-op writes.
    bytes: Vec<u8>,
}

impl ParamBuffer {
    pub fn new(device: &wgpu::Device) -> Self {
        let align = device.limits().min_uniform_buffer_offset_alignment as u64;
        Self {
            stride: (std::mem::size_of::<ObjectParams>() as u64).next_multiple_of(align),
            buf: GrowBuffer::new("object_params", wgpu::BufferUsages::UNIFORM),
            bind_group: None,
            bytes: Vec::new(),
        }
    }

    /// Byte size the shader binds at each dynamic offset.
    pub fn binding_size() -> NonZeroU64 {
        NonZeroU64::new(std::mem::size_of::<ObjectParams>() as u64).expect("nonzero")
    }

    fn pack<'a>(&self, params: impl Iterator<Item = &'a ObjectParams>) -> Vec<u8> {
        let stride = self.stride as usize;
        let mut bytes = Vec::new();
        for p in params {
            let at = bytes.len();
            bytes.resize(at + stride, 0);
            bytes[at..at + std::mem::size_of::<ObjectParams>()]
                .copy_from_slice(bytemuck::bytes_of(p));
        }
        bytes
    }

    /// Write all parameters (slot order) in place; skipped when unchanged.
    pub fn write<'a>(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        layout: &wgpu::BindGroupLayout,
        params: impl Iterator<Item = &'a ObjectParams>,
    ) {
        let bytes = self.pack(params);
        if bytes == self.bytes {
            return;
        }
        if self.buf.alloc_init(device, &bytes) {
            self.bind_group = Some(self.make_bind_group(device, layout));
        } else if let Some(buf) = &self.buf.buf {
            queue.write_buffer(buf, 0, &bytes);
        }
        self.bytes = bytes;
    }

    /// [`ParamBuffer::write`] without a queue, for the first upload.
    pub fn init<'a>(
        &mut self,
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        params: impl Iterator<Item = &'a ObjectParams>,
    ) {
        let bytes = self.pack(params);
        if self.buf.alloc_init(device, &bytes) {
            self.bind_group = Some(self.make_bind_group(device, layout));
        }
        self.bytes = bytes;
    }

    fn make_bind_group(&self, device: &wgpu::Device, layout: &wgpu::BindGroupLayout) -> wgpu::BindGroup {
        let buffer = self.buf.buf.as_ref().expect("allocated");
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("object_params_bg"),
            layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer,
                    offset: 0,
                    size: Some(Self::binding_size()),
                }),
            }],
        })
    }

    pub fn bind_group(&self) -> Option<&wgpu::BindGroup> {
        self.bind_group.as_ref()
    }

    /// Dynamic offset of a slot.
    pub fn offset(&self, slot: u32) -> u32 {
        (slot as u64 * self.stride) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(r: f32) -> ObjectParams {
        ObjectParams::stroke([r, 0.0, 0.0, 1.0])
    }

    #[test]
    fn meshes_pack_with_base_vertex_and_skip_empty() {
        let mut s = Staged::default();
        let tri = [[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let n = [[0.0, 0.0, 1.0]; 3];
        s.push_mesh(&tri, &n, &[0, 1, 2], p(0.1));
        s.push_mesh(&[], &[], &[], p(0.2));
        s.push_mesh(&tri, &n, &[2, 1, 0], p(0.3));
        assert_eq!(s.vertices.len(), 6 * 6);
        assert_eq!(s.indices, vec![0, 1, 2, 2, 1, 0]);
        assert_eq!(
            s.draws,
            vec![
                Draw { first: 0, count: 3, base_vertex: 0, slot: 0 },
                Draw { first: 3, count: 3, base_vertex: 3, slot: 1 },
            ]
        );
        assert_eq!(s.params, vec![p(0.1), p(0.3)]);
    }

    #[test]
    fn odd_segment_lists_keep_pair_alignment() {
        let mut s = Staged::default();
        s.push_segments(&[[0.0; 3], [1.0; 3], [2.0; 3]], p(0.1));
        s.push_segments(&[[3.0; 3], [4.0; 3]], p(0.2));
        assert_eq!(s.draws[0].range(), 0..3);
        assert_eq!(s.draws[0].segments(), 0..1);
        assert_eq!(s.draws[1].first, 4, "padded to a pair boundary");
        assert_eq!(s.draws[1].segments(), 2..3);
        // The second object's pair is where the fat pass reads instance 2.
        assert_eq!(&s.vertices[12..18], &[3.0, 3.0, 3.0, 4.0, 4.0, 4.0]);
    }
}
