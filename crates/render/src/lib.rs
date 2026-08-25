// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Viewport rendering: wgpu pipelines inside egui paint callbacks.

mod camera;
mod control;
mod cubemap;
mod layout;
mod pano;
mod renderer;
mod snapshot;
mod viewport_callback;

pub use camera::{fov_for_focal_mm, preset_focal_mm, OrbitCamera, StandardView};
pub use control::{render_control_images, ControlImagePaths};
pub use cubemap::{render_pano_image, PanoImage};
pub use pano::{
    equirect_dir_to_ndc, equirect_ndc_to_dir, fisheye_ndc_to_dir, fisheye_radius, pixel_to_ndc,
    CubeFace, Ndc, PanoProjection,
};
pub use layout::ViewportLayout;
pub use renderer::{
    hue_from_seed, CameraUniform, ColorMode, DisplayMode, LightMode, SceneData, SceneRenderer,
    UnderlayData,
};
pub use snapshot::{snapshot, snapshot_with_mode, ColorModeSnapshot, Theme};
pub use viewport_callback::ViewportCallback;

pub fn camera_uniform(view_proj: glam::Mat4, eye: glam::Vec3) -> CameraUniform {
    camera_uniform_with_mode(view_proj, eye, DisplayMode::default())
}

/// Build a camera uniform encoding the given display mode, the default
/// (Working) lighting mode, and no sun.
pub fn camera_uniform_with_mode(
    view_proj: glam::Mat4,
    eye: glam::Vec3,
    mode: DisplayMode,
) -> CameraUniform {
    camera_uniform_full(view_proj, eye, mode, LightMode::default(), None)
}

/// Build a camera uniform with an explicit lighting mode and optional sun
/// direction (unit vector, X=East Y=North Z=Up). `sun_dir` is only consulted
/// by the Sun lighting mode in the shader; pass `None` for the others.
pub fn camera_uniform_full(
    view_proj: glam::Mat4,
    eye: glam::Vec3,
    mode: DisplayMode,
    light: LightMode,
    sun_dir: Option<[f32; 3]>,
) -> CameraUniform {
    camera_uniform_ex(view_proj, eye, mode, light, sun_dir, false)
}

/// Full camera-uniform builder. `background_gradient` turns on the sky/ground
/// gradient the grid shader paints behind the scene (the SketchUp look);
/// when false the flat clear colour shows through.
pub fn camera_uniform_ex(
    view_proj: glam::Mat4,
    eye: glam::Vec3,
    mode: DisplayMode,
    light: LightMode,
    sun_dir: Option<[f32; 3]>,
    background_gradient: bool,
) -> CameraUniform {
    let (sx, sy, sz) = sun_dir.map(|d| (d[0], d[1], d[2])).unwrap_or((0.0, 0.0, 0.0));
    CameraUniform {
        view_proj: view_proj.to_cols_array_2d(),
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        eye: [eye.x, eye.y, eye.z, 1.0],
        misc: [mode.fill_alpha(), sx, sy, sz],
        light: [light.shader_flag(), if background_gradient { 1.0 } else { 0.0 }, 0.0, 0.0],
    }
}

/// Must match `NativeOptions::depth_buffer = 24` in the app.
pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth24Plus;
