use egui_wgpu::CallbackTrait;
use glam::Mat4;

use crate::renderer::{CameraUniform, SceneData, SceneRenderer};

/// Per-frame paint callback for the 3D viewport. Uploads the camera, rebuilds
/// scene buffers when the document generation changed, then draws the scene.
pub struct ViewportCallback {
    pub view_proj: Mat4,
    pub eye: glam::Vec3,
    pub generation: u64,
    /// Present only on frames where `generation` differs from the GPU copy.
    pub scene: Option<SceneData>,
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
        let cam = CameraUniform {
            view_proj: self.view_proj.to_cols_array_2d(),
            inv_view_proj: self.view_proj.inverse().to_cols_array_2d(),
            eye: [self.eye.x, self.eye.y, self.eye.z, 1.0],
        };
        renderer.write_camera(queue, &cam);
        if let Some(scene) = &self.scene
            && renderer.generation != self.generation
        {
            renderer.set_scene(device, scene, self.generation);
        }
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        let renderer: &SceneRenderer = resources.get().expect("SceneRenderer registered");
        renderer.paint(render_pass);
    }
}
