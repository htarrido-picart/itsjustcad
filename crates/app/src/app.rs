use glam::DVec3;
use mydrafter_render::{OrbitCamera, SceneRenderer, ViewportCallback};

pub struct App {
    camera: OrbitCamera,
    /// Bumped whenever scene geometry changes; drives GPU buffer rebuilds.
    generation: u64,
    gpu_dirty: bool,
    /// Dev self-verification: MYDRAFTER_SHOT=<path.png> captures a frame and exits.
    shot_path: Option<String>,
    frame_count: u64,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let rs = cc
            .wgpu_render_state
            .as_ref()
            .expect("wgpu render state (eframe must run with the wgpu backend)");
        rs.renderer
            .write()
            .callback_resources
            .insert(SceneRenderer::new(&rs.device, rs.target_format));
        Self {
            camera: OrbitCamera::default(),
            generation: 0,
            gpu_dirty: true,
            shot_path: std::env::var("MYDRAFTER_SHOT").ok(),
            frame_count: 0,
        }
    }

    fn handle_dev_screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = self.shot_path.clone() else {
            return;
        };
        ctx.request_repaint();
        self.frame_count += 1;
        if self.frame_count == 20 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        let image = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = image {
            let png = image::RgbaImage::from_raw(
                img.width() as u32,
                img.height() as u32,
                img.as_raw().to_vec(),
            )
            .expect("screenshot buffer size");
            png.save(&path).expect("write screenshot");
            std::process::exit(0);
        }
    }

    fn viewport(&mut self, ui: &mut egui::Ui) {
        let (rect, response) = ui.allocate_exact_size(
            ui.available_size(),
            egui::Sense::click_and_drag(),
        );

        // Rhino muscle memory: RMB orbit, Shift+RMB pan, scroll dolly.
        if response.dragged_by(egui::PointerButton::Secondary)
            || response.dragged_by(egui::PointerButton::Middle)
        {
            let delta = response.drag_delta();
            let shift = ui.input(|i| i.modifiers.shift);
            if shift {
                self.camera.pan(delta.x, delta.y);
            } else {
                self.camera.orbit(delta.x, delta.y);
            }
        }
        if response.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                self.camera.dolly(scroll);
            }
        }

        let aspect = rect.width() / rect.height().max(1.0);
        let meshes = self.gpu_dirty.then(|| {
            self.gpu_dirty = false;
            // Phase 1: hardcoded test box, replaced by the document in Phase 2.
            let mesh = kernel_mesh::make_box(DVec3::new(-2.5, -2.5, 0.0), DVec3::new(5.0, 5.0, 3.0));
            vec![(mesh.to_render(), [0.72, 0.73, 0.78, 1.0f32])]
        });

        ui.painter().add(egui_wgpu::Callback::new_paint_callback(
            rect,
            ViewportCallback {
                view_proj: self.camera.view_proj(aspect),
                eye: self.camera.eye(),
                generation: self.generation,
                meshes,
            },
        ));
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.handle_dev_screenshot(ui.ctx());
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| self.viewport(ui));
    }
}
