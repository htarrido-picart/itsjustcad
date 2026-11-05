// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Metal plumbing: the device gate, runtime shader compilation, bounding-box
//! and triangle acceleration structures and batched compute dispatch, shared
//! by [`crate::ShadowCaster`] (#317) and [`crate::Tracer`] (#318).
//!
//! Device: `MTLCreateSystemDefaultDevice`, not wgpu's `MTLDevice`. Analysis
//! commands run in `itsjustcad-commands`, which also runs headless (CLI,
//! tests, the FFI core) with no wgpu context; threading a renderer handle
//! into command execution would couple the two. On Apple silicon there is
//! one GPU, and the system default device is that same GPU.
//!
//! Shaders: compiled from source at runtime (`newLibraryWithSource`) with
//! fast math off. A build-time `.metallib` needs the `metal` compiler, which
//! ships with full Xcode only (not the Command Line Tools), so it would add
//! a toolchain dependency to every macOS build; runtime compilation costs
//! a few tens of ms once per process (reported in the stats).

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::{NSArray, NSString};
use objc2_metal::{
    MTLAccelerationStructure, MTLAccelerationStructureBoundingBoxGeometryDescriptor,
    MTLAccelerationStructureCommandEncoder, MTLAccelerationStructureGeometryDescriptor,
    MTLAccelerationStructureTriangleGeometryDescriptor, MTLAttributeFormat, MTLBuffer, MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandEncoder, MTLCommandQueue,
    MTLCompileOptions, MTLComputeCommandEncoder, MTLComputePipelineState,
    MTLCreateSystemDefaultDevice, MTLDevice, MTLDispatchType, MTLGPUFamily, MTLLibrary, MTLMathMode,
    MTLPrimitiveAccelerationStructureDescriptor, MTLResourceOptions, MTLSize,
};

pub type Device = ProtocolObject<dyn MTLDevice>;
pub type Buffer = Retained<ProtocolObject<dyn MTLBuffer>>;
pub type Accel = Retained<ProtocolObject<dyn MTLAccelerationStructure>>;
pub type Pipeline = Retained<ProtocolObject<dyn MTLComputePipelineState>>;
type CommandBuffer = Retained<ProtocolObject<dyn MTLCommandBuffer>>;

/// A Metal object shared across threads. Devices, queues, pipelines,
/// buffers and acceleration structures are thread-safe in Metal (Apple:
/// "Metal Objects and Thread Safety"); command buffers and encoders are not
/// and never leave the function or [`Job`] that made them.
pub struct Shared<T>(pub T);
// SAFETY: only used for the thread-safe Metal object kinds listed above.
unsafe impl<T> Send for Shared<T> {}
unsafe impl<T> Sync for Shared<T> {}

/// The hardware ray-tracing GPU: present only on Apple9-family devices that
/// support ray tracing.
pub struct Gpu {
    device: Retained<Device>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
}

// SAFETY: MTLDevice and MTLCommandQueue are thread-safe (see [`Shared`]).
unsafe impl Send for Gpu {}
unsafe impl Sync for Gpu {}

impl Gpu {
    /// The process-wide GPU, or `None` when the gate fails.
    pub fn get() -> Option<&'static Gpu> {
        static GPU: OnceLock<Option<Gpu>> = OnceLock::new();
        GPU.get_or_init(Gpu::new).as_ref()
    }

    fn new() -> Option<Self> {
        let device = MTLCreateSystemDefaultDevice()?;
        if !(device.supportsFamily(MTLGPUFamily::Apple9) && device.supportsRaytracing()) {
            return None;
        }
        let queue = device.newCommandQueue()?;
        Some(Self { device, queue })
    }

    /// Compile `src` and make a compute pipeline for `function`, fast math
    /// off (IEEE `f32` rounding the margins rely on).
    pub fn pipeline(&self, src: &str, function: &str) -> Result<Pipeline, String> {
        Ok(self.pipelines(src, &[function])?.remove(0))
    }

    /// [`Gpu::pipeline`] for several functions of one source, compiled once.
    pub fn pipelines(&self, src: &str, functions: &[&str]) -> Result<Vec<Pipeline>, String> {
        let opts = MTLCompileOptions::new();
        opts.setMathMode(MTLMathMode::Safe);
        let lib = self
            .device
            .newLibraryWithSource_options_error(&NSString::from_str(src), Some(&opts))
            .map_err(|e| e.localizedDescription().to_string())?;
        functions
            .iter()
            .map(|function| {
                let f = lib
                    .newFunctionWithName(&NSString::from_str(function))
                    .ok_or_else(|| format!("no function {function}"))?;
                self.device
                    .newComputePipelineStateWithFunction_error(&f)
                    .map_err(|e| e.localizedDescription().to_string())
            })
            .collect()
    }

    /// A shared-storage buffer holding a copy of `data` (at least 16 bytes).
    pub fn buffer<T: Copy>(&self, data: &[T]) -> Option<Buffer> {
        let buf = self.buffer_len(std::mem::size_of_val(data))?;
        // SAFETY: the buffer is at least `size_of_val(data)` bytes, CPU
        // visible, and not yet used by the GPU.
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr().cast::<u8>(),
                buf.contents().as_ptr().cast::<u8>(),
                std::mem::size_of_val(data),
            );
        }
        Some(buf)
    }

    /// An uninitialised shared-storage buffer of `bytes` (at least 16).
    pub fn buffer_len(&self, bytes: usize) -> Option<Buffer> {
        self.device.newBufferWithLength_options(bytes.max(16), MTLResourceOptions::StorageModeShared)
    }

    /// Build a primitive acceleration structure over `MTLAxisAlignedBoundingBox`
    /// records (24 bytes each). Blocks until built.
    pub fn build_box_accel(&self, boxes: &Buffer, count: usize) -> Result<Accel, String> {
        let geo = MTLAccelerationStructureBoundingBoxGeometryDescriptor::descriptor();
        geo.setBoundingBoxBuffer(Some(boxes));
        geo.setBoundingBoxCount(count);
        // SAFETY: 24 = size_of(MTLAxisAlignedBoundingBox), the buffer's layout.
        unsafe { geo.setBoundingBoxStride(24) };
        self.build_accel(Retained::into_super(geo))
    }

    /// Build a primitive acceleration structure over `count` triangles stored
    /// as consecutive packed `float3` vertices (36 bytes per triangle, no
    /// index buffer). Opaque, so `intersector` closest/any-hit queries run in
    /// hardware. Blocks until built.
    pub fn build_triangle_accel(&self, vertices: &Buffer, count: usize) -> Result<Accel, String> {
        let geo = MTLAccelerationStructureTriangleGeometryDescriptor::descriptor();
        geo.setVertexBuffer(Some(vertices));
        geo.setVertexFormat(MTLAttributeFormat::Float3);
        geo.setVertexStride(12);
        geo.setTriangleCount(count);
        geo.setOpaque(true);
        self.build_accel(Retained::into_super(geo))
    }

    fn build_accel(&self, geo: Retained<MTLAccelerationStructureGeometryDescriptor>) -> Result<Accel, String> {
        let desc = MTLPrimitiveAccelerationStructureDescriptor::descriptor();
        let geos: Retained<NSArray<MTLAccelerationStructureGeometryDescriptor>> =
            NSArray::from_retained_slice(&[geo]);
        desc.setGeometryDescriptors(Some(&geos));
        let sizes = self.device.accelerationStructureSizesWithDescriptor(&desc);
        let accel = self
            .device
            .newAccelerationStructureWithSize(sizes.accelerationStructureSize)
            .ok_or("acceleration structure allocation failed")?;
        let scratch = self
            .device
            .newBufferWithLength_options(sizes.buildScratchBufferSize.max(16), MTLResourceOptions::StorageModePrivate)
            .ok_or("scratch allocation failed")?;
        let cb = self.queue.commandBuffer().ok_or("no command buffer")?;
        let enc = cb.accelerationStructureCommandEncoder().ok_or("no AS encoder")?;
        enc.buildAccelerationStructure_descriptor_scratchBuffer_scratchBufferOffset(&accel, &desc, &scratch, 0);
        enc.endEncoding();
        cb.commit();
        Job::finish(&cb)?;
        Ok(accel)
    }

    /// Encode one 1-D dispatch of `n` threads and commit it without waiting.
    /// `buffers[k]` binds at index `k + 1` (index 0 is the acceleration
    /// structure); `n` itself is passed as a `uint` after the buffers.
    pub fn dispatch(&self, pipe: &Pipeline, accel: &Accel, buffers: &[&Buffer], n: u32) -> Result<Job, String> {
        self.dispatch_all(accel, &[(pipe, buffers, n)])
    }

    /// [`Gpu::dispatch`] for several independent kernels in one command
    /// buffer and one concurrent encoder, so they may overlap on the GPU and
    /// cost one round trip. Dispatches with `n == 0` are skipped.
    pub fn dispatch_all(&self, accel: &Accel, work: &[(&Pipeline, &[&Buffer], u32)]) -> Result<Job, String> {
        let cb = self.queue.commandBuffer().ok_or("no command buffer")?;
        let enc = cb
            .computeCommandEncoderWithDispatchType(MTLDispatchType::Concurrent)
            .ok_or("no compute encoder")?;
        for &(pipe, buffers, n) in work {
            if n == 0 {
                continue;
            }
            enc.setComputePipelineState(pipe);
            let width = pipe.threadExecutionWidth().max(1);
            let group = (width * 2).min(pipe.maxTotalThreadsPerThreadgroup()).max(1);
            // SAFETY: indices match the kernel signature; buffers outlive the
            // command buffer because the caller keeps them with the [`Job`].
            unsafe {
                enc.setAccelerationStructure_atBufferIndex(Some(accel), 0);
                for (k, b) in buffers.iter().enumerate() {
                    enc.setBuffer_offset_atIndex(Some(b), 0, k + 1);
                }
                let nn = n;
                enc.setBytes_length_atIndex(
                    NonNull::from(&nn).cast::<c_void>(),
                    std::mem::size_of::<u32>(),
                    buffers.len() + 1,
                );
            }
            enc.dispatchThreads_threadsPerThreadgroup(
                MTLSize { width: n.max(1) as usize, height: 1, depth: 1 },
                MTLSize { width: group, height: 1, depth: 1 },
            );
        }
        enc.endEncoding();
        cb.commit();
        Ok(Job { cb })
    }
}

/// A committed command buffer.
pub struct Job {
    cb: CommandBuffer,
}

// SAFETY: a committed command buffer is only waited on and queried, which
// Metal allows from any thread.
unsafe impl Send for Job {}

impl Job {
    /// Wait for completion; returns GPU seconds.
    pub fn wait(&self) -> Result<f64, String> {
        Self::finish(&self.cb)
    }

    fn finish(cb: &CommandBuffer) -> Result<f64, String> {
        cb.waitUntilCompleted();
        if cb.status() != MTLCommandBufferStatus::Completed {
            let why = cb.error().map(|e| e.localizedDescription().to_string()).unwrap_or_default();
            return Err(format!("command buffer failed: {why}"));
        }
        Ok(cb.GPUEndTime() - cb.GPUStartTime())
    }
}

/// View a shared buffer's contents as `n` values of `T`.
///
/// # Safety
/// The buffer must hold at least `n` valid `T`s, no GPU work may be
/// writing it, and no other view of it may be alive (the `&mut` comes from
/// the buffer's shared memory, not from the `&Buffer`).
#[allow(clippy::mut_from_ref)]
pub unsafe fn contents<T>(buf: &Buffer, n: usize) -> &mut [T] {
    unsafe { std::slice::from_raw_parts_mut(buf.contents().as_ptr().cast::<T>(), n) }
}
