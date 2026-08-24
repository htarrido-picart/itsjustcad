//! Viewport rendering: wgpu pipelines inside egui paint callbacks.

mod camera;
mod layout;
mod renderer;
mod snapshot;
mod viewport_callback;

pub use camera::{OrbitCamera, StandardView};
pub use layout::ViewportLayout;
pub use renderer::{
    hue_from_seed, CameraUniform, ColorMode, DisplayMode, SceneData, SceneRenderer, UnderlayData,
};
pub use snapshot::{snapshot, snapshot_with_mode, ColorModeSnapshot, Theme};
pub use viewport_callback::ViewportCallback;

pub fn camera_uniform(view_proj: glam::Mat4, eye: glam::Vec3) -> CameraUniform {
    camera_uniform_with_mode(view_proj, eye, DisplayMode::default())
}

/// Build a camera uniform encoding the given display mode and no sun.
pub fn camera_uniform_with_mode(
    view_proj: glam::Mat4,
    eye: glam::Vec3,
    mode: DisplayMode,
) -> CameraUniform {
    CameraUniform {
        view_proj: view_proj.to_cols_array_2d(),
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        eye: [eye.x, eye.y, eye.z, 1.0],
        misc: [mode.fill_alpha(), 0.0, 0.0, 0.0],
    }
}

/// Must match `NativeOptions::depth_buffer = 24` in the app.
pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth24Plus;
