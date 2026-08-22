use mydrafter_commands::Session;
use mydrafter_render::{OrbitCamera, SceneRenderer, ViewportCallback};

use crate::command_line::CommandLine;
use crate::scene;

pub struct App {
    session: Session,
    command_line: CommandLine,
    camera: OrbitCamera,
    /// Generation of the last GPU upload; compare with `session.doc.generation`.
    uploaded_generation: Option<u64>,
    /// Dev self-verification: MYDRAFTER_SHOT=<path.png> captures a frame and exits.
    shot_path: Option<String>,
    /// Dev scripting: MYDRAFTER_RUN="cmd;cmd;..." executes on startup.
    startup_script: Option<String>,
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
            session: Session::default(),
            command_line: CommandLine::default(),
            camera: OrbitCamera::default(),
            uploaded_generation: None,
            shot_path: std::env::var("MYDRAFTER_SHOT").ok(),
            startup_script: std::env::var("MYDRAFTER_RUN").ok(),
            frame_count: 0,
        }
    }

    fn run_startup_script(&mut self) {
        if let Some(script) = self.startup_script.take() {
            for cmd in script.split(';') {
                self.command_line.execute(&mut self.session, cmd);
            }
        }
    }

    fn handle_dev_screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = self.shot_path.clone() else {
            return;
        };
        ctx.request_repaint(); // keep frames flowing until the shot lands
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
        let (rect, response) =
            ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());

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

        let generation = self.session.doc.generation;
        let scene = (self.uploaded_generation != Some(generation)).then(|| {
            self.uploaded_generation = Some(generation);
            scene::snapshot(&self.session.doc)
        });

        let aspect = rect.width() / rect.height().max(1.0);
        ui.painter().add(egui_wgpu::Callback::new_paint_callback(
            rect,
            ViewportCallback {
                view_proj: self.camera.view_proj(aspect),
                eye: self.camera.eye(),
                generation,
                scene,
            },
        ));
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.run_startup_script();
        self.handle_dev_screenshot(ui.ctx());

        egui::Panel::bottom("command_line")
            .resizable(false)
            .show(ui, |ui| {
                if let Some(line) = self.command_line.ui(ui) {
                    self.command_line.execute(&mut self.session, &line);
                }
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| self.viewport(ui));
    }
}
