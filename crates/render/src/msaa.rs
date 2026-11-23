// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Multisample anti-aliasing for the viewport (#356).
//!
//! egui owns the single-sample window pass, so MSAA cannot happen inside it.
//! Each pane instead draws the scene into its own multisampled colour + depth
//! target during the paint callback's `prepare` (cleared to transparent),
//! resolves it to a single-sample texture, and `paint` composites that texture
//! into egui's pass with premultiplied-alpha blending. The scene pipelines
//! blend with `ALPHA_BLENDING` / premultiplied blending, so a transparent clear
//! leaves premultiplied colour in the resolve and the composite equals drawing
//! straight onto egui's background. With MSAA off the scene still draws
//! directly into egui's pass (no extra target, no extra pass).
//!
//! Targets are kept per pane and rebuilt only when the pane's pixel size or
//! the sample count changes, never per frame.

use crate::renderer::{DisplayMode, SceneRenderer};
use crate::DEPTH_FORMAT;

/// The anti-aliasing choices offered to the user, in menu order (1 = off).
pub const MSAA_CHOICES: [u32; 4] = [1, 2, 4, 8];

/// Requested sample count when the user never chose one: 4x, which WebGPU
/// guarantees for the formats the viewport uses.
pub const DEFAULT_MSAA: u32 = 4;

/// Sample counts (ascending, always including 1) that a colour/depth pair with
/// these format features can render AND resolve at.
pub fn counts_from_flags(
    color: wgpu::TextureFormatFeatureFlags,
    depth: wgpu::TextureFormatFeatureFlags,
) -> Vec<u32> {
    MSAA_CHOICES
        .into_iter()
        .filter(|&n| {
            n == 1
                || (color.sample_count_supported(n)
                    && depth.sample_count_supported(n)
                    && color.contains(wgpu::TextureFormatFeatureFlags::MULTISAMPLE_RESOLVE))
        })
        .collect()
}

/// Sample counts the device can use for viewport targets of `color_format`
/// (with [`DEPTH_FORMAT`] depth). Uses the adapter's real format features when
/// the device enabled `TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES`, else the
/// WebGPU-guaranteed set (1 and 4) the device validates against.
pub fn supported_sample_counts(
    adapter: &wgpu::Adapter,
    device_features: wgpu::Features,
    color_format: wgpu::TextureFormat,
) -> Vec<u32> {
    let flags = |f: wgpu::TextureFormat| {
        if device_features.contains(wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES) {
            adapter.get_texture_format_features(f).flags
        } else {
            f.guaranteed_format_features(device_features).flags
        }
    };
    counts_from_flags(flags(color_format), flags(DEPTH_FORMAT))
}

/// The highest supported count not above `requested` (falls back to 1).
pub fn pick_sample_count(requested: u32, supported: &[u32]) -> u32 {
    supported.iter().copied().filter(|&n| n <= requested).max().unwrap_or(1)
}

/// A pane's multisampled colour + depth target and its single-sample resolve.
pub struct MsaaTarget {
    size: [u32; 2],
    samples: u32,
    color_view: wgpu::TextureView,
    depth_view: wgpu::TextureView,
    resolve: wgpu::Texture,
    resolve_view: wgpu::TextureView,
}

impl MsaaTarget {
    /// Allocate a `size`-pixel target of `samples` samples for `format`.
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat, size: [u32; 2], samples: u32) -> Self {
        let extent = wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 };
        let tex = |label, format, samples, usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: extent,
                mip_level_count: 1,
                sample_count: samples,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        let attach = wgpu::TextureUsages::RENDER_ATTACHMENT;
        let color = tex("msaa_color", format, samples, attach);
        let depth = tex("msaa_depth", DEPTH_FORMAT, samples, attach);
        let resolve = tex(
            "msaa_resolve",
            format,
            1,
            attach | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
        );
        Self {
            size,
            samples,
            color_view: color.create_view(&Default::default()),
            depth_view: depth.create_view(&Default::default()),
            resolve_view: resolve.create_view(&Default::default()),
            resolve,
        }
    }

    pub fn size(&self) -> [u32; 2] {
        self.size
    }

    pub fn samples(&self) -> u32 {
        self.samples
    }

    /// The resolved single-sample image (premultiplied alpha).
    pub fn resolve_texture(&self) -> &wgpu::Texture {
        &self.resolve
    }

    /// Begin a pass that clears to transparent and resolves on end.
    pub fn begin_pass(&self, encoder: &mut wgpu::CommandEncoder) -> wgpu::RenderPass<'static> {
        encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("msaa_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.color_view,
                    resolve_target: Some(&self.resolve_view),
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        // Only the resolve is read; the samples can be dropped.
                        store: wgpu::StoreOp::Discard,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            })
            .forget_lifetime()
    }
}

const COMPOSITE_WGSL: &str = r"
@group(0) @binding(0) var src: texture_2d<f32>;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    // Fullscreen triangle over the pane viewport.
    let x = f32((vi << 1u) & 2u);
    let y = f32(vi & 2u);
    var out: VsOut;
    out.pos = vec4(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    out.uv = vec2(x, y);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // The viewport is the pane rect at the resolve's exact pixel size, so the
    // uv lands on texel centres: a 1:1 copy, no filtering.
    let dims = vec2<f32>(textureDimensions(src));
    let p = clamp(in.uv * dims, vec2(0.0), dims - 1.0);
    return textureLoad(src, vec2<i32>(p), 0);
}
";

/// Draws a pane's resolved image into egui's single-sample pass.
struct Compositor {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
}

impl Compositor {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("msaa_composite_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("msaa_composite_shader"),
            source: wgpu::ShaderSource::Wgsl(COMPOSITE_WGSL.into()),
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("msaa_composite_pl"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("msaa_composite_pipeline"),
            layout: Some(&pl),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            // egui's pass carries the window depth buffer; the composite
            // neither tests nor writes it.
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        Self { pipeline, layout }
    }

    fn bind(&self, device: &wgpu::Device, target: &MsaaTarget) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("msaa_composite_bg"),
            layout: &self.layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&target.resolve_view),
            }],
        })
    }
}

/// Viewport MSAA state owned by [`SceneRenderer`]: the composite pipeline
/// (built on first use) and one target per pane.
#[derive(Default)]
pub(crate) struct ViewportMsaa {
    compositor: Option<Compositor>,
    panes: Vec<Option<(MsaaTarget, wgpu::BindGroup)>>,
}

impl ViewportMsaa {
    /// Drop every pane target (sample count changed, or MSAA turned off).
    pub(crate) fn clear(&mut self) {
        self.panes.clear();
    }
}

impl SceneRenderer {
    /// Pane `viewport`'s current MSAA target, if the MSAA path built one.
    pub fn msaa_target(&self, viewport: usize) -> Option<&MsaaTarget> {
        self.msaa.panes.get(viewport)?.as_ref().map(|(t, _)| t)
    }

    /// MSAA path, `prepare` half: draw pane `viewport` into its multisampled
    /// target (`size_px` = the pane's viewport in pixels) and resolve it. The
    /// target is reallocated only when the size or sample count changed.
    /// No-op when MSAA is off or the pane has no area.
    #[allow(clippy::too_many_arguments)]
    pub fn render_msaa_pane(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        viewport: usize,
        size_px: [u32; 2],
        mode: DisplayMode,
        edges_enabled: bool,
    ) {
        let samples = self.sample_count();
        if samples <= 1 || size_px[0] == 0 || size_px[1] == 0 {
            return;
        }
        let format = self.target_format();
        let msaa = &mut self.msaa;
        let compositor = msaa.compositor.get_or_insert_with(|| Compositor::new(device, format));
        if msaa.panes.len() <= viewport {
            msaa.panes.resize_with(viewport + 1, || None);
        }
        let slot = &mut msaa.panes[viewport];
        if !slot.as_ref().is_some_and(|(t, _)| t.size == size_px && t.samples == samples) {
            let target = MsaaTarget::new(device, format, size_px, samples);
            let bg = compositor.bind(device, &target);
            *slot = Some((target, bg));
        }
        let Some((target, _)) = &self.msaa.panes[viewport] else { return };
        let mut pass = target.begin_pass(encoder);
        self.paint(&mut pass, viewport, mode, edges_enabled);
    }

    /// MSAA path, `paint` half: copy the pane's resolved image into egui's
    /// pass (whose viewport is the pane rect). False if nothing was drawn.
    pub fn composite_msaa_pane(&self, pass: &mut wgpu::RenderPass<'static>, viewport: usize) -> bool {
        let (Some(c), Some(Some((_, bg)))) = (&self.msaa.compositor, self.msaa.panes.get(viewport)) else {
            return false;
        };
        pass.set_pipeline(&c.pipeline);
        pass.set_bind_group(0, bg, &[]);
        pass.draw(0..3, 0..1);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wgpu::TextureFormatFeatureFlags as F;

    #[test]
    fn pick_falls_back_to_highest_supported() {
        let s = [1, 2, 4];
        assert_eq!(pick_sample_count(8, &s), 4);
        assert_eq!(pick_sample_count(4, &s), 4);
        assert_eq!(pick_sample_count(2, &s), 2);
        assert_eq!(pick_sample_count(1, &s), 1);
        assert_eq!(pick_sample_count(4, &[1]), 1);
        assert_eq!(pick_sample_count(8, &[]), 1);
    }

    #[test]
    fn counts_need_colour_depth_and_resolve() {
        let all = F::MULTISAMPLE_X2 | F::MULTISAMPLE_X4 | F::MULTISAMPLE_X8;
        assert_eq!(counts_from_flags(all | F::MULTISAMPLE_RESOLVE, all), vec![1, 2, 4, 8]);
        // Depth only does 4x → only 4x is usable.
        assert_eq!(counts_from_flags(all | F::MULTISAMPLE_RESOLVE, F::MULTISAMPLE_X4), vec![1, 4]);
        // No resolve support → MSAA unusable.
        assert_eq!(counts_from_flags(all, all), vec![1]);
    }

    #[test]
    fn webgpu_guarantees_default_count_for_viewport_formats() {
        // Without adapter-specific features the device validates against the
        // WebGPU guarantees; the default must be usable there.
        for fmt in [wgpu::TextureFormat::Bgra8Unorm, wgpu::TextureFormat::Rgba8Unorm] {
            let f = wgpu::Features::empty();
            let counts = counts_from_flags(
                fmt.guaranteed_format_features(f).flags,
                DEPTH_FORMAT.guaranteed_format_features(f).flags,
            );
            assert_eq!(pick_sample_count(DEFAULT_MSAA, &counts), DEFAULT_MSAA, "{fmt:?}");
        }
    }
}
