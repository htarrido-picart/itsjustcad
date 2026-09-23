// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

use egui_wgpu::CallbackTrait;
use glam::Mat4;

use crate::renderer::{CameraUniform, DisplayMode, LightMode, SceneData, SceneRenderer};

/// Per-frame paint callback for the 3D viewport. Uploads the camera, rebuilds
/// scene buffers when the document generation changed, then draws the scene.
pub struct ViewportCallback {
    pub view_proj: Mat4,
    pub eye: glam::Vec3,
    pub generation: u64,
    /// Present only on frames where `generation` differs from the GPU copy.
    pub scene: Option<SceneData>,
    /// Pane index — selects the per-viewport camera UBO so multiple panes in
    /// one frame do not clobber each other's camera during `prepare`.
    pub viewport: usize,
    /// Display mode of this pane (shaded/wireframe/x-ray/ghosted).
    pub mode: DisplayMode,
    /// Unit vector toward the sun (X=East, Y=North, Z=Up). Only consulted by
    /// the `Sun` lighting mode; `None` leaves the sun slot zeroed.
    pub sun_dir: Option<[f32; 3]>,
    /// Lighting model for the mesh fill (Working / Sun / Presentation).
    pub light: LightMode,
    /// When true the grid shader paints a sky/ground gradient background behind
    /// the scene (SketchUp look) instead of the flat clear colour.
    pub background_gradient: bool,
    /// Draw mesh feature edges. On by default (the Shaded "+ edges" look);
    /// user-toggleable. Ignored by modes that always need edges.
    pub edges_enabled: bool,
}

impl CallbackTrait for ViewportCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let renderer: &mut SceneRenderer = resources.get_mut().expect("SceneRenderer registered");
        // misc[0] = fill_alpha (display mode); misc[1..3] = sun direction xyz.
        // light[0] = lighting mode flag.
        let (sx, sy, sz) = self
            .sun_dir
            .map(|d| (d[0], d[1], d[2]))
            .unwrap_or((0.0, 0.0, 0.0));
        let cam = CameraUniform {
            view_proj: self.view_proj.to_cols_array_2d(),
            inv_view_proj: self.view_proj.inverse().to_cols_array_2d(),
            eye: [self.eye.x, self.eye.y, self.eye.z, 1.0],
            misc: [self.mode.fill_alpha(), sx, sy, sz],
            light: [
                self.light.shader_flag(),
                if self.background_gradient { 1.0 } else { 0.0 },
                0.0,
                0.0,
            ],
        };
        renderer.write_camera(device, queue, self.viewport, &cam);
        if let Some(scene) = &self.scene
            && renderer.generation != self.generation
        {
            renderer.set_scene(device, queue, scene, self.generation);
        }
        Vec::new()
    }

    fn paint(
        &self,
        info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        // Confine the 3D to THIS pane's rect, in physical pixels. egui-wgpu sets a
        // per-callback `viewport` but scissors only to the wider egui `clip_rect`
        // (the whole central panel), and a fullscreen-triangle grid rasterises
        // across the viewport regardless — so without our own scissor the grid
        // bleeds past the pane and, when the pane momentarily overlaps the dock on
        // a Retina relayout, paints under the chat. Scissoring to the pane rect
        // (clamped to the clip rect and framebuffer) fixes the overlap at any DPI.
        let vp = info.viewport_in_pixels();
        let clip = info.clip_rect_in_pixels();
        let x0 = vp.left_px.max(clip.left_px).max(0);
        let y0 = vp.top_px.max(clip.top_px).max(0);
        let x1 = (vp.left_px + vp.width_px).min(clip.left_px + clip.width_px);
        let y1 = (vp.top_px + vp.height_px).min(clip.top_px + clip.height_px);
        if x1 <= x0 || y1 <= y0 {
            return; // pane fully clipped away this frame — nothing to draw
        }
        render_pass.set_scissor_rect(
            x0 as u32,
            y0 as u32,
            (x1 - x0) as u32,
            (y1 - y0) as u32,
        );

        let renderer: &SceneRenderer = resources.get().expect("SceneRenderer registered");
        renderer.paint(render_pass, self.viewport, self.mode, self.edges_enabled);
    }
}
