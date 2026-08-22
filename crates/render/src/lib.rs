//! Viewport rendering: wgpu pipelines inside egui paint callbacks.

mod camera;
mod renderer;
mod snapshot;
mod viewport_callback;

pub use camera::OrbitCamera;
pub use renderer::{CameraUniform, SceneData, SceneRenderer};
pub use snapshot::{snapshot, CURVE_COLOR, CURVE_SELECTED, MESH_COLOR, MESH_SELECTED};
pub use viewport_callback::ViewportCallback;

pub fn camera_uniform(view_proj: glam::Mat4, eye: glam::Vec3) -> CameraUniform {
    CameraUniform {
        view_proj: view_proj.to_cols_array_2d(),
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        eye: [eye.x, eye.y, eye.z, 1.0],
    }
}

/// Must match `NativeOptions::depth_buffer = 24` in the app.
pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth24Plus;
