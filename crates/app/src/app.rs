use mydrafter_commands::Session;
use mydrafter_render::{OrbitCamera, SceneRenderer, StandardView, ViewportCallback};

use crate::command_line::CommandLine;
use crate::deck_pane::DeckPane;
use crate::draw_tool::DrawTool;
use crate::scene;

pub struct App {
    session: Session,
    command_line: CommandLine,
    deck_pane: DeckPane,
    draw_tool: DrawTool,
    tokio: tokio::runtime::Handle,
    camera: OrbitCamera,
    /// Generation of the last GPU upload; compare with `session.doc.generation`.
    uploaded_generation: Option<u64>,
    /// Theme of the last GPU upload; theme flips force a re-upload.
    uploaded_theme: Option<scene::Theme>,
    /// Last zoom factor written to ui.json (avoid rewriting every frame).
    saved_zoom: f32,
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

        // Accessibility: readable default text size, persisted across runs.
        // Cmd+= / Cmd+- / Cmd+0 also work (egui built-in zoom).
        let zoom = load_zoom().unwrap_or(1.3);
        cc.egui_ctx.set_zoom_factor(zoom);

        Self {
            session: Session::default(),
            command_line: CommandLine::default(),
            deck_pane: DeckPane::default(),
            draw_tool: DrawTool::default(),
            tokio,
            camera: OrbitCamera::default(),
            uploaded_generation: None,
            uploaded_theme: None,
            saved_zoom: zoom,
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

    /// App-level verbs (save/open, camera) wrap the command substrate.
    fn execute_line(&mut self, line: String) {
        let line = line.trim();
        let mut words = line.split_whitespace();
        match words.next() {
            Some("save") => self.save(words.next().map(Into::into)),
            Some("open") => self.open(words.next().map(Into::into)),
            Some("ze" | "zoomextents") => self.zoom_extents(),
            Some(view @ ("top" | "bottom" | "front" | "back" | "left" | "right" | "persp"
            | "perspective")) => {
                self.set_view(view);
                self.command_line.push_line(format!("view: {view}"));
            }
            _ => {
                if self.draw_tool.try_start(line) {
                    if let Some(prompt) = self.draw_tool.prompt() {
                        self.command_line.push_line(prompt);
                    }
                    return;
                }
                self.command_line.execute(&mut self.session, line);
            }
        }
    }

    fn set_view(&mut self, name: &str) {
        let view = match name {
            "top" => StandardView::Top,
            "bottom" => StandardView::Bottom,
            "front" => StandardView::Front,
            "back" => StandardView::Back,
            "left" => StandardView::Left,
            "right" => StandardView::Right,
            _ => StandardView::Perspective,
        };
        self.camera.set_view(view);
    }

    fn zoom_extents(&mut self) {
        if let Some(bb) = self.session.doc.scene_aabb() {
            let center = bb.center();
            self.camera.target =
                glam::Vec3::new(center.x as f32, center.y as f32, center.z as f32);
            self.camera.distance = (bb.size().length() as f32 * 1.2).max(5.0);
        }
    }

    /// Click-select: ray through the clicked pixel vs object AABBs.
    fn pick(&mut self, rect: egui::Rect, pos: egui::Pos2, additive: bool) {
        let aspect = rect.width() / rect.height().max(1.0);
        let view_proj = self.camera.view_proj(aspect);
        let inv = view_proj.inverse();
        let ndc = glam::Vec2::new(
            (pos.x - rect.left()) / rect.width() * 2.0 - 1.0,
            1.0 - (pos.y - rect.top()) / rect.height() * 2.0,
        );
        let unproject = |z: f32| {
            let p = inv * glam::Vec4::new(ndc.x, ndc.y, z, 1.0);
            (p.truncate() / p.w).as_dvec3()
        };
        let origin = unproject(0.0);
        let dir = (unproject(1.0) - origin).normalize();

        let mut best: Option<(f64, mydrafter_doc::ObjectId)> = None;
        for obj in self.session.doc.objects() {
            let bb = obj.geometry.aabb();
            if let Some(t) = ray_aabb(origin, dir, bb.min, bb.max)
                && best.is_none_or(|(bt, _)| t < bt)
            {
                best = Some((t, obj.id));
            }
        }
        let doc = &mut self.session.doc;
        if !additive {
            doc.selection.clear();
        }
        if let Some((_, id)) = best {
            if additive && doc.selection.contains(&id) {
                doc.selection.remove(&id);
            } else {
                doc.selection.insert(id);
            }
        }
        doc.generation += 1; // recolor selection
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
        let aspect = rect.width() / rect.height().max(1.0);
        let view_proj = self.camera.view_proj(aspect);

        if self.draw_tool.active() {
            self.drawing_input(ui, rect, &response, view_proj);
        } else if response.clicked()
            && let Some(pos) = response.interact_pointer_pos()
        {
            let additive = ui.input(|i| i.modifiers.shift);
            self.pick(rect, pos, additive);
        }

        let theme = if ui.visuals().dark_mode {
            scene::Theme::Dark
        } else {
            scene::Theme::Light
        };
        let generation = self.session.doc.generation;
        let stale = self.uploaded_generation != Some(generation)
            || self.uploaded_theme != Some(theme);
        let scene = stale.then(|| {
            self.uploaded_generation = Some(generation);
            self.uploaded_theme = Some(theme);
            scene::snapshot(&self.session.doc, theme)
        });

        ui.painter().add(egui_wgpu::Callback::new_paint_callback(
            rect,
            ViewportCallback {
                view_proj,
                eye: self.camera.eye(),
                generation,
                scene,
            },
        ));

        self.view_toolbar(ui, rect);
    }

    /// Interactive drawing: picks on the ground plane, ghost preview, prompt.
    fn drawing_input(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        response: &egui::Response,
        view_proj: glam::Mat4,
    ) {
        let (esc, enter) = ui.input(|i| {
            (
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::Enter),
            )
        });
        if esc {
            self.draw_tool.cancel();
            self.command_line.push_line("drawing cancelled");
            return;
        }
        if enter && let Some(cmd) = self.draw_tool.on_enter() {
            self.execute_line(cmd);
            return;
        }

        // Snap resolution: nearest object point within the screen-space
        // radius wins; empty space falls back to the ground-plane 10cm grid.
        let cursor_px = response
            .hover_pos()
            .or_else(|| response.interact_pointer_pos());
        let snap_hit = cursor_px.and_then(|pos| {
            crate::osnap::resolve(
                &crate::osnap::candidates(&self.session.doc),
                pos,
                crate::osnap::SNAP_RADIUS_PX,
                |w| project(view_proj, rect, w),
            )
        });
        let cursor_world = snap_hit.map(|(p, _)| p).or_else(|| {
            cursor_px
                .and_then(|pos| ground_point(view_proj, rect, pos))
                .map(crate::osnap::grid_snap)
        });

        if response.clicked() && let Some(world) = cursor_world {
            if let Some(cmd) = self.draw_tool.on_click(world) {
                self.execute_line(cmd);
                return;
            } else if let Some(prompt) = self.draw_tool.prompt() {
                self.command_line.push_line(prompt);
            }
        }

        // Ghost preview + prompt overlay
        let painter = ui.painter_at(rect);
        let stroke = egui::Stroke::new(1.5, egui::Color32::from_rgb(90, 160, 255));
        for strip in self.draw_tool.preview(cursor_world) {
            for pair in strip.windows(2) {
                if let (Some(a), Some(b)) = (
                    project(view_proj, rect, pair[0]),
                    project(view_proj, rect, pair[1]),
                ) {
                    painter.line_segment([a, b], stroke);
                }
            }
        }
        // Osnap marker: square on the snapped point + kind label (Rhino look).
        if let Some((world, kind)) = snap_hit
            && let Some(screen) = project(view_proj, rect, world)
        {
            let color = egui::Color32::from_rgb(255, 200, 60);
            painter.rect_stroke(
                egui::Rect::from_center_size(screen, egui::vec2(9.0, 9.0)),
                0.0,
                egui::Stroke::new(1.5, color),
                egui::StrokeKind::Middle,
            );
            painter.text(
                screen + egui::vec2(8.0, -8.0),
                egui::Align2::LEFT_BOTTOM,
                kind.label(),
                egui::TextStyle::Small.resolve(ui.style()),
                color,
            );
        }
        if let Some(prompt) = self.draw_tool.prompt() {
            painter.text(
                rect.center_top() + egui::vec2(0.0, 28.0),
                egui::Align2::CENTER_TOP,
                prompt,
                egui::TextStyle::Body.resolve(ui.style()),
                ui.visuals().strong_text_color(),
            );
        }
        ui.ctx().request_repaint(); // live rubber-band
    }

    fn view_toolbar(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        egui::Area::new(egui::Id::new("view_toolbar"))
            .fixed_pos(rect.left_top() + egui::vec2(8.0, 8.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for (label, name) in [
                            ("Persp", "persp"),
                            ("Top", "top"),
                            ("Front", "front"),
                            ("Right", "right"),
                            ("Left", "left"),
                            ("Back", "back"),
                            ("Bottom", "bottom"),
                        ] {
                            if ui.small_button(label).clicked() {
                                self.set_view(name);
                            }
                        }
                        if ui.small_button("ZE").on_hover_text("zoom extents").clicked() {
                            self.zoom_extents();
                        }
                    });
                });
            });
    }
}

fn ui_config_path() -> Option<std::path::PathBuf> {
    Some(dirs::home_dir()?.join(".config").join("mydrafter").join("ui.json"))
}

fn load_zoom() -> Option<f32> {
    let json = std::fs::read_to_string(ui_config_path()?).ok()?;
    let value: serde_json::Value = serde_json::from_str(&json).ok()?;
    value["zoom"].as_f64().map(|z| (z as f32).clamp(0.5, 3.0))
}

fn save_zoom(zoom: f32) {
    if let Some(path) = ui_config_path() {
        let _ = std::fs::create_dir_all(path.parent().expect("has parent"));
        let _ = std::fs::write(path, format!("{{\n  \"zoom\": {zoom}\n}}\n"));
    }
}

/// Screen position -> point on the z=0 ground plane.
fn ground_point(view_proj: glam::Mat4, rect: egui::Rect, pos: egui::Pos2) -> Option<glam::DVec3> {
    let inv = view_proj.inverse();
    let ndc = glam::Vec2::new(
        (pos.x - rect.left()) / rect.width() * 2.0 - 1.0,
        1.0 - (pos.y - rect.top()) / rect.height() * 2.0,
    );
    let unproject = |z: f32| {
        let p = inv * glam::Vec4::new(ndc.x, ndc.y, z, 1.0);
        (p.truncate() / p.w).as_dvec3()
    };
    let origin = unproject(0.0);
    let dir = unproject(1.0) - origin;
    if dir.z.abs() < 1e-12 {
        return None;
    }
    let t = -origin.z / dir.z;
    (t > 0.0).then(|| origin + dir * t)
}

/// World point -> screen position (None when behind the camera).
fn project(view_proj: glam::Mat4, rect: egui::Rect, world: glam::DVec3) -> Option<egui::Pos2> {
    let clip = view_proj * glam::Vec4::new(world.x as f32, world.y as f32, world.z as f32, 1.0);
    if clip.w <= 0.0 {
        return None;
    }
    let ndc = clip.truncate() / clip.w;
    Some(egui::pos2(
        rect.left() + (ndc.x + 1.0) * 0.5 * rect.width(),
        rect.top() + (1.0 - ndc.y) * 0.5 * rect.height(),
    ))
}

fn ray_aabb(origin: glam::DVec3, dir: glam::DVec3, min: glam::DVec3, max: glam::DVec3) -> Option<f64> {
    let inv = dir.recip();
    let t1 = (min - origin) * inv;
    let t2 = (max - origin) * inv;
    let t_min = t1.min(t2).max_element();
    let t_max = t1.max(t2).min_element();
    (t_max >= t_min.max(0.0)).then_some(t_min.max(0.0))
}

impl eframe::App for App {
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        if visuals.dark_mode {
            scene::Theme::Dark.background()
        } else {
            scene::Theme::Light.background()
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.run_startup_script();
        self.handle_dev_screenshot(ui.ctx());

        // Persist zoom changes from any source (buttons or Cmd+=/Cmd+-).
        let zoom = ui.ctx().zoom_factor();
        if (zoom - self.saved_zoom).abs() > 0.01 {
            self.saved_zoom = zoom;
            save_zoom(zoom);
        }

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
