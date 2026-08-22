use mydrafter_commands::Session;
use mydrafter_render::{OrbitCamera, SceneRenderer, ViewportCallback};

use crate::command_line::CommandLine;
use crate::deck_pane::DeckPane;
use crate::scene;

pub struct App {
    session: Session,
    command_line: CommandLine,
    deck_pane: DeckPane,
    tokio: tokio::runtime::Handle,
    camera: OrbitCamera,
    /// Generation of the last GPU upload; compare with `session.doc.generation`.
    uploaded_generation: Option<u64>,
    /// Dev self-verification: MYDRAFTER_SHOT=<path.png> captures a frame and exits.
    shot_path: Option<String>,
    /// Dev scripting: MYDRAFTER_RUN="cmd;cmd;..." executes on startup.
    startup_script: Option<String>,
    /// Dev scripting: MYDRAFTER_DECK_RUN="prompt" sends one deck message on
    /// startup; with MYDRAFTER_SHOT set, the shot waits for the turn to end.
    deck_script: Option<String>,
    frame_count: u64,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, tokio: tokio::runtime::Handle) -> Self {
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
            deck_pane: DeckPane::default(),
            tokio,
            camera: OrbitCamera::default(),
            uploaded_generation: None,
            shot_path: std::env::var("MYDRAFTER_SHOT").ok(),
            startup_script: std::env::var("MYDRAFTER_RUN").ok(),
            deck_script: std::env::var("MYDRAFTER_DECK_RUN").ok(),
            frame_count: 0,
        }
    }

    fn run_startup_script(&mut self) {
        if let Some(script) = self.startup_script.take() {
            for cmd in script.split(';') {
                self.execute_line(cmd.to_string());
            }
        }
        if let Some(prompt) = self.deck_script.take() {
            self.deck_pane
                .send_text(&prompt, &self.session, &self.tokio);
        }
    }

    /// App-level verbs (save/open, file dialogs) wrap the command substrate.
    fn execute_line(&mut self, line: String) {
        let line = line.trim();
        let mut words = line.split_whitespace();
        match words.next() {
            Some("save") => self.save(words.next().map(Into::into)),
            Some("open") => self.open(words.next().map(Into::into)),
            _ => {
                self.command_line.execute(&mut self.session, line);
            }
        }
    }

    fn save(&mut self, path: Option<std::path::PathBuf>) {
        let path = path.or_else(|| {
            rfd::FileDialog::new()
                .add_filter("mydrafter", &["mydrafter.json", "json"])
                .set_file_name("untitled.mydrafter.json")
                .save_file()
        });
        let Some(path) = path else { return };
        match mydrafter_commands::io::save_file(&self.session, &path) {
            Ok(()) => self
                .command_line
                .push_line(format!("saved {}", path.display())),
            Err(e) => self.command_line.push_line(format!("error: {e}")),
        }
    }

    fn open(&mut self, path: Option<std::path::PathBuf>) {
        let path = path.or_else(|| {
            rfd::FileDialog::new()
                .add_filter("mydrafter", &["mydrafter.json", "json"])
                .pick_file()
        });
        let Some(path) = path else { return };
        match mydrafter_commands::io::load_file(&path) {
            Ok(session) => {
                self.session = session;
                self.uploaded_generation = None;
                self.command_line
                    .push_line(format!("opened {} ({} objects)", path.display(), self.session.doc.len()));
            }
            Err(e) => self.command_line.push_line(format!("error: {e}")),
        }
    }

    fn handle_dev_screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = self.shot_path.clone() else {
            return;
        };
        ctx.request_repaint(); // keep frames flowing until the shot lands
        // With a deck script, wait for the LLM turn(s) to finish before shooting.
        let deck_ready =
            std::env::var("MYDRAFTER_DECK_RUN").is_err() || self.deck_pane.turns_completed();
        if deck_ready {
            self.frame_count += 1;
        }
        if self.frame_count == 20 {
            if let Ok(path) = std::env::var("MYDRAFTER_SAVE") {
                self.save(Some(path.into()));
            }
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

        let (save_key, open_key) = ui.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::S),
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::O),
            )
        });
        if save_key {
            self.save(None);
        }
        if open_key {
            self.open(None);
        }

        egui::Panel::bottom("command_line")
            .resizable(false)
            .show(ui, |ui| {
                if let Some(line) = self.command_line.ui(ui) {
                    self.execute_line(line);
                }
            });

        egui::Panel::right("deck")
            .resizable(true)
            .default_size(340.0)
            .show(ui, |ui| {
                self.deck_pane.ui(ui, &mut self.session, &self.tokio);
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| self.viewport(ui));
    }
}
