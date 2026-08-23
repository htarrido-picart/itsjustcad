use mydrafter_commands::Session;
use mydrafter_render::{
    DisplayMode, OrbitCamera, SceneRenderer, StandardView, ViewportCallback, ViewportLayout,
};

use crate::command_line::CommandLine;
use crate::deck_pane::DeckPane;
use crate::draw_tool::DrawTool;
use crate::gumball::Gumball;
use crate::journal::{self, Journal};
use crate::keymap;
use crate::scene;

pub struct App {
    session: Session,
    command_line: CommandLine,
    deck_pane: DeckPane,
    draw_tool: DrawTool,
    gumball: Gumball,
    tokio: tokio::runtime::Handle,
    /// Camera slots shared across layouts: 0 Persp, 1 Top, 2 Front, 3 Right.
    cameras: [OrbitCamera; 4],
    /// Display mode per camera slot (view state, follows the camera across
    /// layout switches; never logged).
    display_modes: [DisplayMode; 4],
    layout: ViewportLayout,
    /// Last hovered pane; view commands and tools target its camera.
    active_pane: usize,
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
    /// Layer color being edited in the panel; the `layercolor` command is
    /// issued once, when the mouse is released (avoids one op per drag frame).
    pending_layer_color: Option<(String, [f32; 3])>,
    /// Last executed command line; Enter/Space on the canvas repeats it.
    last_line: Option<String>,
    /// Cmd+C pressed with a selection; Cmd+V then runs `copy sel 1,1,0`.
    clipboard_armed: bool,
    /// In-progress drag-box selection: anchor position of the drag.
    box_drag: Option<egui::Pos2>,
    /// Crash-recovery journal mirroring the op-log; deleted on save/clean exit.
    journal: Option<Journal>,
    /// Doc generation of the last journal sync (skip serializing every frame).
    journaled_generation: Option<u64>,
    /// Cursor ground-plane position in the active pane, for the status bar
    /// (written during the viewport pass, read next frame by the strip).
    status_cursor: Option<glam::DVec3>,
    /// Snap kind currently hit by the draw tool, for the status bar.
    status_snap: Option<&'static str>,
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

        let journal = Journal::open_default();
        let mut command_line = CommandLine::default();
        // Leftover journals mean a crashed session; offer recovery up front.
        if let (Some(dir), Some(j)) = (journal::default_dir(), &journal) {
            let n = journal::recoverable(&dir, j.path()).len();
            if n > 0 {
                command_line.push_line(format!(
                    "{n} crash journal(s) found — type 'recover' to restore the latest"
                ));
            }
        }

        Self {
            session: Session::default(),
            command_line,
            deck_pane: DeckPane::default(),
            draw_tool: DrawTool::default(),
            gumball: Gumball::default(),
            tokio,
            cameras: {
                let mut cams = [OrbitCamera::default(); 4];
                cams[1].set_view(StandardView::Top);
                cams[2].set_view(StandardView::Front);
                cams[3].set_view(StandardView::Right);
                cams
            },
            display_modes: [DisplayMode::default(); 4],
            layout: ViewportLayout::Single,
            active_pane: 0,
            uploaded_generation: None,
            uploaded_theme: None,
            saved_zoom: zoom,
            shot_path: std::env::var("MYDRAFTER_SHOT").ok(),
            startup_script: std::env::var("MYDRAFTER_RUN").ok(),
            deck_script: std::env::var("MYDRAFTER_DECK_RUN").ok(),
            frame_count: 0,
            pending_layer_color: None,
            last_line: None,
            clipboard_armed: false,
            box_drag: None,
            journal,
            journaled_generation: None,
            status_cursor: None,
            status_snap: None,
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
        if !line.is_empty() {
            self.last_line = Some(line.to_string()); // Enter/Space repeat
        }
        let mut words = line.split_whitespace();
        match words.next() {
            Some("save") => self.save(words.next().map(Into::into)),
            Some("copyselection") => {
                let n = self.session.doc.selection.len();
                if n == 0 {
                    self.command_line.push_line("nothing selected to copy");
                } else {
                    self.clipboard_armed = true;
                    self.command_line
                        .push_line(format!("copied {n} object(s) — Cmd+V pastes with offset"));
                }
            }
            Some("pasteselection") => {
                if self.clipboard_armed {
                    self.command_line.execute(&mut self.session, "copy sel 1,1,0");
                } else {
                    self.command_line.push_line("nothing to paste");
                }
            }
            Some("open") => self.open(words.next().map(Into::into)),
            Some("recover") => self.recover(),
            Some("ze" | "zoomextents") => self.zoom_extents(),
            // Display mode of the active viewport. View state, never logged.
            Some("display") => match words.next().and_then(DisplayMode::parse) {
                Some(mode) => {
                    self.display_modes[self.layout.camera_index(self.active_pane)] = mode;
                    self.command_line
                        .push_line(format!("display: {}", mode.label().to_lowercase()));
                }
                None => {
                    self.command_line
                        .push_line("usage: display shaded|wireframe|xray|ghosted");
                }
            },
            Some("viewports" | "vp") => {
                match words.next() {
                    Some("1") => self.set_layout(ViewportLayout::Single),
                    Some("2") => self.set_layout(ViewportLayout::Two),
                    Some("4") => self.set_layout(ViewportLayout::Four),
                    _ => {
                        self.command_line.push_line("usage: viewports 1|2|4");
                        return;
                    }
                }
                self.command_line
                    .push_line(format!("viewports: {}", self.layout.pane_count()));
            }
            Some(view @ ("top" | "bottom" | "front" | "back" | "left" | "right" | "persp"
            | "perspective")) => {
                self.set_view(view);
                self.command_line.push_line(format!("view: {view}"));
            }
            // `view save` captures the active camera — only the app can; the
            // parser leaves `camera: None`. Other `view ...` forms parse as-is.
            Some("view") => {
                if let ["save", name] = words.collect::<Vec<_>>().as_slice() {
                    let camera = named_view_of(self.active_camera());
                    let cmd = mydrafter_commands::Command::ViewSave {
                        name: (*name).to_string(),
                        camera: Some(camera),
                    };
                    self.command_line.execute_command(&mut self.session, line, cmd);
                } else {
                    self.command_line.execute(&mut self.session, line);
                }
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

    fn set_layout(&mut self, layout: ViewportLayout) {
        self.layout = layout;
        self.active_pane = 0;
    }

    /// Camera of the active (last hovered) pane.
    fn active_camera(&mut self) -> &mut OrbitCamera {
        &mut self.cameras[self.layout.camera_index(self.active_pane)]
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
        self.active_camera().set_view(view);
    }

    fn zoom_extents(&mut self) {
        if let Some(bb) = self.session.doc.scene_aabb() {
            let center = bb.center();
            let cam = self.active_camera();
            cam.target = glam::Vec3::new(center.x as f32, center.y as f32, center.z as f32);
            cam.distance = (bb.size().length() as f32 * 1.2).max(5.0);
        }
    }

    /// Click-select: ray through the clicked pixel vs object AABBs.
    fn pick(&mut self, view_proj: glam::Mat4, rect: egui::Rect, pos: egui::Pos2, additive: bool) {
        let (origin, dir) = screen_ray(view_proj, rect, pos);

        let mut best: Option<(f64, mydrafter_doc::ObjectId)> = None;
        for obj in self.session.doc.objects() {
            if !obj.visible || !self.session.doc.layer_visible(&obj.layer) {
                continue; // hidden objects/layers are unpickable
            }
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

    /// Apply a finished drag-box: project visible object AABBs to screen
    /// rects, run the pure window/crossing test, update the selection.
    fn box_select(
        &mut self,
        view_proj: glam::Mat4,
        rect: egui::Rect,
        drag: egui::Rect,
        mode: crate::boxsel::BoxMode,
        additive: bool,
    ) {
        let items: Vec<(mydrafter_doc::ObjectId, egui::Rect)> = self
            .session
            .doc
            .objects()
            .filter(|obj| obj.visible && self.session.doc.layer_visible(&obj.layer))
            .filter_map(|obj| {
                let bb = obj.geometry.aabb();
                Some((obj.id, projected_rect(view_proj, rect, bb.min, bb.max)?))
            })
            .collect();
        let ids = crate::boxsel::box_select(&items, drag, mode);
        let doc = &mut self.session.doc;
        if !additive {
            doc.selection.clear();
        }
        let n = ids.len();
        for id in ids {
            doc.selection.insert(id);
        }
        doc.generation += 1; // recolor selection
        let kind = match mode {
            crate::boxsel::BoxMode::Window => "window",
            crate::boxsel::BoxMode::Crossing => "crossing",
        };
        self.command_line
            .push_line(format!("{kind} select: {n} object(s)"));
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
            Ok(()) => {
                // Ops are safe in the file now; drop the crash journal.
                if let Some(j) = &mut self.journal {
                    j.discard();
                }
                self.journaled_generation = Some(self.session.doc.generation);
                self.command_line
                    .push_line(format!("saved {}", path.display()));
            }
            Err(e) => self.command_line.push_line(format!("error: {e}")),
        }
    }

    /// Replay the newest crash journal from another session into this one.
    fn recover(&mut self) {
        let (Some(dir), Some(own)) = (
            journal::default_dir(),
            self.journal.as_ref().map(|j| j.path().to_path_buf()),
        ) else {
            self.command_line.push_line("error: no journal directory");
            return;
        };
        let Some(path) = journal::recoverable(&dir, &own).into_iter().next() else {
            self.command_line.push_line("no crash journal to recover");
            return;
        };
        match journal::load(&path) {
            Ok(session) => {
                self.session = session;
                self.uploaded_generation = None;
                // The recovered ops now live in THIS session's journal (next
                // sync writes them); the crashed one has served its purpose.
                let _ = std::fs::remove_file(&path);
                self.journaled_generation = None;
                self.command_line.push_line(format!(
                    "recovered {} op(s) from {}",
                    self.session.save_log().len(),
                    path.display()
                ));
            }
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
        let full = ui.available_rect_before_wrap();
        if !self.draw_tool.active() {
            self.status_snap = None; // no tool, no snap marker to report
        }
        if self.active_pane >= self.layout.pane_count() {
            self.active_pane = 0;
        }
        // Active viewport = last hovered: tools and view commands follow the cursor.
        if let Some(pos) = ui.ctx().pointer_latest_pos()
            && let Some(pane) = self.layout.pane_at(full, pos)
        {
            self.active_pane = pane;
        }

        let theme = if ui.visuals().dark_mode {
            scene::Theme::Dark
        } else {
            scene::Theme::Light
        };
        let generation = self.session.doc.generation;
        let stale = self.uploaded_generation != Some(generation)
            || self.uploaded_theme != Some(theme);
        // Scene is uploaded once (renderer shared); only the first pane's
        // callback carries the snapshot, the rest just set their camera.
        let mut scene = stale.then(|| {
            self.uploaded_generation = Some(generation);
            self.uploaded_theme = Some(theme);
            scene::snapshot(&self.session.doc, theme)
        });

        let panes = self.layout.split(full);
        for (pane, rect) in panes.iter().copied().enumerate() {
            let cam_idx = self.layout.camera_index(pane);
            let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());

            // Rhino muscle memory: RMB orbit, Shift+RMB pan, scroll dolly.
            if response.dragged_by(egui::PointerButton::Secondary)
                || response.dragged_by(egui::PointerButton::Middle)
            {
                let delta = response.drag_delta();
                let shift = ui.input(|i| i.modifiers.shift);
                if shift {
                    self.cameras[cam_idx].pan(delta.x, delta.y);
                } else {
                    self.cameras[cam_idx].orbit(delta.x, delta.y);
                }
            }
            if response.hovered() {
                let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                if scroll != 0.0 {
                    self.cameras[cam_idx].dolly(scroll);
                }
            }
            let aspect = rect.width() / rect.height().max(1.0);
            let view_proj = self.cameras[cam_idx].view_proj(aspect);

            // Status-bar cursor readout follows the active pane's hover.
            if pane == self.active_pane {
                self.status_cursor = response
                    .hover_pos()
                    .and_then(|pos| ground_point(view_proj, rect, pos));
            }

            if self.draw_tool.active() {
                // Draw/osnap only in the active pane; one prompt, one ghost.
                if pane == self.active_pane {
                    self.drawing_input(ui, rect, &response, view_proj);
                }
            } else {
                // Gumball on the selection (active pane only). A completed
                // drag emits ONE substrate command through Session::run so
                // the op-log stays the single source of truth.
                let mut consumed = false;
                if pane == self.active_pane {
                    let out =
                        self.gumball
                            .ui(ui, rect, &response, view_proj, &self.session.doc);
                    consumed = out.consumed;
                    if let Some(cmd) = out.command {
                        match self.session.run(cmd) {
                            Ok(outcome) => self.command_line.push_line(outcome.message),
                            Err(e) => self.command_line.push_line(format!("error: {e}")),
                        }
                    }
                }
                if !consumed
                    && response.clicked()
                    && let Some(pos) = response.interact_pointer_pos()
                {
                    let additive = ui.input(|i| i.modifiers.shift);
                    self.pick(view_proj, rect, pos, additive);
                }
                // Drag-box selection (no tool, gumball idle): left→right is a
                // window (solid box, fully-inside only), right→left a crossing
                // (dashed box, touch counts). Shift adds to the selection.
                if !consumed
                    && pane == self.active_pane
                    && response.drag_started_by(egui::PointerButton::Primary)
                    && let Some(pos) = response.interact_pointer_pos()
                {
                    self.box_drag = Some(pos);
                }
                if pane == self.active_pane
                    && let Some(start) = self.box_drag
                    && let Some(pos) = response.interact_pointer_pos()
                {
                    let mode = crate::boxsel::mode(start, pos);
                    let drag_rect = egui::Rect::from_two_pos(start, pos);
                    draw_rubber_box(&ui.painter_at(rect), drag_rect, mode, ui.visuals());
                    if response.drag_stopped_by(egui::PointerButton::Primary) {
                        self.box_drag = None;
                        let additive = ui.input(|i| i.modifiers.shift);
                        self.box_select(view_proj, rect, drag_rect, mode, additive);
                    }
                    ui.ctx().request_repaint(); // live rubber box
                }
            }

            ui.painter().add(egui_wgpu::Callback::new_paint_callback(
                rect,
                ViewportCallback {
                    view_proj,
                    eye: self.cameras[cam_idx].eye(),
                    generation,
                    scene: scene.take(),
                    viewport: pane,
                    mode: self.display_modes[cam_idx],
                },
            ));

            // Dimensions and text are 2D overlay drawing (egui text cannot go
            // through wgpu); hatches render in the scene itself.
            self.draw_annotations(ui, rect, view_proj, theme);

            if panes.len() > 1 {
                let color = if pane == self.active_pane {
                    ui.visuals().selection.stroke.color
                } else {
                    ui.visuals().weak_text_color()
                };
                ui.painter().rect_stroke(
                    rect.shrink(0.5),
                    0.0,
                    egui::Stroke::new(1.0, color),
                    egui::StrokeKind::Inside,
                );
            }
        }

        // Safety net: a drag that ends without a pointer position (or in
        // another pane) must not leave a stale rubber box behind.
        if self.box_drag.is_some() && !ui.input(|i| i.pointer.any_down()) {
            self.box_drag = None;
        }

        self.view_toolbar(ui, full);
        self.layers_panel(ui, full, theme);
        self.history_panel(ui, full);
    }

    /// Overlay pass for dimension and text annotations: world points project
    /// through `view_proj`, text sizes track world-space heights on screen.
    fn draw_annotations(
        &self,
        ui: &egui::Ui,
        rect: egui::Rect,
        view_proj: glam::Mat4,
        theme: scene::Theme,
    ) {
        use mydrafter_doc::{Annotation, Geometry};
        let painter = ui.painter_at(rect);
        let doc = &self.session.doc;
        for obj in doc.objects() {
            if !obj.visible || !doc.layer_visible(&obj.layer) {
                continue;
            }
            let Geometry::Annotation(ann) = &obj.geometry else {
                continue;
            };
            let c = if doc.selection.contains(&obj.id) {
                theme.selected()
            } else {
                doc.layers
                    .get(&obj.layer)
                    .and_then(|s| s.color)
                    .unwrap_or(theme.curve())
            };
            let color = egui::Color32::from_rgb(
                (c[0] * 255.0).round() as u8,
                (c[1] * 255.0).round() as u8,
                (c[2] * 255.0).round() as u8,
            );
            let stroke = egui::Stroke::new(1.0, color);
            // World height -> on-screen pixels at `at`, for text sizing.
            let px_height = |at: glam::DVec3, h: f64| -> f32 {
                let Some(p) = project(view_proj, rect, at) else {
                    return 12.0;
                };
                // Whichever world axis is visible in this view carries the size
                // (Z collapses in Top view, Y in Front view).
                let len = |axis: glam::DVec3| {
                    project(view_proj, rect, at + axis * h)
                        .map(|q| (p - q).length())
                        .unwrap_or(0.0)
                };
                len(glam::DVec3::Z).max(len(glam::DVec3::Y)).clamp(8.0, 60.0)
            };
            match ann {
                Annotation::LinearDim { a, b, offset } => {
                    let dir = (*b - *a).normalize_or_zero();
                    let perp = glam::DVec3::new(-dir.y, dir.x, 0.0).normalize_or(glam::DVec3::X);
                    let (a2, b2) = (*a + perp * *offset, *b + perp * *offset);
                    let segs = [(*a, a2), (*b, b2), (a2, b2)];
                    let mut px: Option<(egui::Pos2, egui::Pos2)> = None;
                    for (w0, w1) in segs {
                        if let (Some(p), Some(q)) = (
                            project(view_proj, rect, w0),
                            project(view_proj, rect, w1),
                        ) {
                            painter.line_segment([p, q], stroke);
                            px = Some((p, q)); // last segment = dimension line
                        }
                    }
                    if let Some((p, q)) = px {
                        // 45° tick marks at the dimension line ends.
                        let d = (q - p).normalized();
                        let tick = egui::vec2(d.x - d.y, d.x + d.y) * 3.5;
                        painter.line_segment([p - tick, p + tick], stroke);
                        painter.line_segment([q - tick, q + tick], stroke);
                        let mid = egui::pos2((p.x + q.x) * 0.5, (p.y + q.y) * 0.5);
                        // Measured value: derived, formatted in document units.
                        let label =
                            mydrafter_doc::format_length(doc.units, (*b - *a).length());
                        let size = px_height((a2 + b2) * 0.5, 0.2);
                        painter.text(
                            mid,
                            egui::Align2::CENTER_BOTTOM,
                            label,
                            egui::FontId::proportional(size),
                            color,
                        );
                    }
                }
                Annotation::Text { pos, text, height } => {
                    if let Some(p) = project(view_proj, rect, *pos) {
                        painter.text(
                            p,
                            egui::Align2::LEFT_BOTTOM,
                            text,
                            egui::FontId::proportional(px_height(*pos, *height)),
                            color,
                        );
                    }
                }
                Annotation::Hatch { .. } => {} // rendered in the wgpu scene
            }
        }
    }

    /// Undo history: op list newest-last, current position highlighted.
    /// Clicking an entry jumps there by running undo/redo through the
    /// session, so the op-log stays the single source of truth.
    fn history_panel(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        let (entries, cursor) = self.session.history();
        let mut jump: Option<usize> = None;
        egui::Area::new(egui::Id::new("history_panel"))
            .fixed_pos(rect.left_top() + egui::vec2(8.0, 48.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(130.0);
                    egui::CollapsingHeader::new(format!("History ({})", entries.len()))
                        .id_salt("history_panel_header")
                        .default_open(true)
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .max_height(260.0)
                                .show(ui, |ui| {
                                    if ui.selectable_label(cursor == 0, "(start)").clicked() {
                                        jump = Some(0);
                                    }
                                    for (i, name) in entries.iter().enumerate() {
                                        let step = i + 1; // state after op i
                                        let label = format!("{step}. {name}");
                                        let text = if step > cursor {
                                            egui::RichText::new(label).weak() // undone
                                        } else {
                                            egui::RichText::new(label)
                                        };
                                        if ui.selectable_label(step == cursor, text).clicked() {
                                            jump = Some(step);
                                        }
                                    }
                                });
                        });
                });
            });
        if let Some(step) = jump {
            match self.session.jump_to(step) {
                Ok(moved) if moved > 0 => self
                    .command_line
                    .push_line(format!("history: jumped to step {step} ({moved} op(s))")),
                Ok(_) => {}
                Err(e) => self.command_line.push_line(format!("error: {e}")),
            }
        }
    }

    /// Layers panel: visibility toggle, color swatch, current-layer switch.
    /// Every edit goes through the command substrate so it is logged/undoable.
    fn layers_panel(&mut self, ui: &mut egui::Ui, rect: egui::Rect, theme: scene::Theme) {
        let mut lines: Vec<String> = Vec::new();
        egui::Area::new(egui::Id::new("layers_panel"))
            .fixed_pos(rect.right_top() + egui::vec2(-190.0, 8.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(170.0);
                    egui::CollapsingHeader::new("Layers")
                        .default_open(true)
                        .show(ui, |ui| {
                            let layers: Vec<(String, mydrafter_doc::LayerStyle)> = self
                                .session
                                .doc
                                .layers
                                .iter()
                                .map(|(n, s)| (n.clone(), s.clone()))
                                .collect();
                            let current = self.session.doc.current_layer.clone();
                            for (name, style) in layers {
                                ui.horizontal(|ui| {
                                    let mut visible = style.visible;
                                    if ui
                                        .checkbox(&mut visible, "")
                                        .on_hover_text("visible")
                                        .changed()
                                    {
                                        let verb = if visible { "show" } else { "hide" };
                                        lines.push(format!("{verb} {name}"));
                                    }
                                    let fallback = theme.mesh();
                                    let mut rgb = self
                                        .pending_layer_color
                                        .as_ref()
                                        .filter(|(n, _)| *n == name)
                                        .map(|(_, c)| *c)
                                        .or_else(|| style.color.map(|c| [c[0], c[1], c[2]]))
                                        .unwrap_or([fallback[0], fallback[1], fallback[2]]);
                                    if ui.color_edit_button_rgb(&mut rgb).changed() {
                                        self.pending_layer_color = Some((name.clone(), rgb));
                                    }
                                    let is_current = name == current;
                                    if ui
                                        .selectable_label(is_current, &name)
                                        .on_hover_text("set current layer")
                                        .clicked()
                                        && !is_current
                                    {
                                        lines.push(format!("layer {name}"));
                                    }
                                });
                            }
                        });
                });
            });
        // Commit the color edit once the mouse is released — one logged op
        // per edit instead of one per drag frame.
        if let Some((name, c)) = self.pending_layer_color.clone()
            && !ui.input(|i| i.pointer.any_down())
        {
            lines.push(format!(
                "layercolor {name} {:.3},{:.3},{:.3}",
                c[0], c[1], c[2]
            ));
            self.pending_layer_color = None;
        }
        for line in lines {
            self.execute_line(line);
        }
    }

    /// Interactive drawing: picks on the ground plane, ghost preview, prompt.
    fn drawing_input(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        response: &egui::Response,
        view_proj: glam::Mat4,
    ) {
        // The canvas owns the keyboard while picking: typed digits build the
        // precise-input buffer, so no text field may hold focus underneath.
        if let Some(id) = ui.ctx().memory(|m| m.focused()) {
            ui.ctx().memory_mut(|m| m.surrender_focus(id));
        }

        let (esc, enter, shift) = ui.input(|i| {
            (
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::Enter),
                i.modifiers.shift,
            )
        });
        if esc {
            self.draw_tool.cancel();
            self.command_line.push_line("drawing cancelled");
            return;
        }
        // Typed characters feed the numeric buffer; Backspace edits it
        // (keymap keeps delete-selection off while drawing).
        let typed: Vec<egui::Event> = ui.input(|i| {
            i.events
                .iter()
                .filter(|e| {
                    matches!(
                        e,
                        egui::Event::Text(_)
                            | egui::Event::Key {
                                key: egui::Key::Backspace,
                                pressed: true,
                                ..
                            }
                    )
                })
                .cloned()
                .collect()
        });
        for event in typed {
            match event {
                egui::Event::Text(t) => {
                    for c in t.chars() {
                        self.draw_tool.push_input(c);
                    }
                }
                _ => {
                    self.draw_tool.pop_input();
                }
            }
        }

        // Snap resolution: nearest object point within the screen-space
        // radius wins; empty space falls back to the ground-plane 10cm grid.
        let cursor_px = response
            .hover_pos()
            .or_else(|| response.interact_pointer_pos());
        let mut snap_hit = cursor_px.and_then(|pos| {
            crate::osnap::resolve(
                &crate::osnap::candidates(&self.session.doc),
                pos,
                crate::osnap::SNAP_RADIUS_PX,
                |w| project(view_proj, rect, w),
            )
        });
        let mut cursor_world = snap_hit.map(|(p, _)| p).or_else(|| {
            cursor_px
                .and_then(|pos| ground_point(view_proj, rect, pos))
                .map(crate::osnap::grid_snap)
        });
        // Shift = ortho lock: 0°/90° from the last picked point overrides
        // osnap (marker off, the constrained point is what a click commits).
        if shift && let (Some(last), Some(c)) = (self.draw_tool.last_point(), cursor_world) {
            cursor_world = Some(crate::precise::ortho_lock(last, c));
            snap_hit = None;
        }
        self.status_snap = snap_hit.map(|(_, kind)| kind.label());

        if enter {
            let buffer = self.draw_tool.take_input();
            if !buffer.is_empty() {
                // Precise input: resolve the typed point, feed it as a pick.
                match crate::precise::resolve_input(
                    &buffer,
                    self.draw_tool.last_point(),
                    cursor_world,
                ) {
                    Ok(world) => {
                        if let Some(cmd) = self.draw_tool.on_click(world) {
                            self.execute_line(cmd);
                            return;
                        } else if let Some(prompt) = self.draw_tool.prompt() {
                            self.command_line.push_line(prompt);
                        }
                    }
                    Err(e) => self.command_line.push_line(format!("error: {e}")),
                }
            } else if let Some(cmd) = self.draw_tool.on_enter() {
                self.execute_line(cmd);
                return;
            }
        }

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

    /// Bottom strip: cursor coords, active layer, counts, snap state, view.
    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let doc = &self.session.doc;
        let cam = &self.cameras[self.layout.camera_index(self.active_pane)];
        ui.horizontal(|ui| {
            ui.monospace(crate::statusbar::format_cursor(doc.units, self.status_cursor));
            ui.separator();
            ui.label(format!("layer: {}", doc.current_layer));
            ui.separator();
            ui.label(crate::statusbar::format_counts(doc.selection.len(), doc.len()));
            ui.separator();
            ui.label(crate::statusbar::snap_label(
                self.draw_tool.active(),
                self.status_snap,
            ));
            ui.separator();
            ui.label(format!(
                "view: {}",
                crate::statusbar::view_label(cam.yaw, cam.pitch, cam.ortho)
            ));
        });
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
                        ui.separator();
                        // Display mode of the active pane's camera slot.
                        let slot = self.layout.camera_index(self.active_pane);
                        egui::ComboBox::from_id_salt("display_mode")
                            .selected_text(self.display_modes[slot].label())
                            .show_ui(ui, |ui| {
                                for mode in DisplayMode::ALL {
                                    ui.selectable_value(
                                        &mut self.display_modes[slot],
                                        mode,
                                        mode.label(),
                                    );
                                }
                            });
                        ui.separator();
                        for (label, layout) in [
                            ("1", ViewportLayout::Single),
                            ("2", ViewportLayout::Two),
                            ("4", ViewportLayout::Four),
                        ] {
                            if ui
                                .selectable_label(self.layout == layout, label)
                                .on_hover_text(format!("{label} viewport(s)"))
                                .clicked()
                            {
                                self.set_layout(layout);
                            }
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

/// Screen position -> world-space pick ray (origin on the near plane).
pub(crate) fn screen_ray(
    view_proj: glam::Mat4,
    rect: egui::Rect,
    pos: egui::Pos2,
) -> (glam::DVec3, glam::DVec3) {
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
    (origin, dir)
}

/// Screen position -> point on the z=0 ground plane.
fn ground_point(view_proj: glam::Mat4, rect: egui::Rect, pos: egui::Pos2) -> Option<glam::DVec3> {
    let (origin, dir) = screen_ray(view_proj, rect, pos);
    if dir.z.abs() < 1e-12 {
        return None;
    }
    let t = -origin.z / dir.z;
    (t > 0.0).then(|| origin + dir * t)
}

/// World point -> screen position (None when behind the camera).
pub(crate) fn project(view_proj: glam::Mat4, rect: egui::Rect, world: glam::DVec3) -> Option<egui::Pos2> {
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

/// World AABB -> covering screen rect; None when any corner sits behind the
/// camera (the object is then skipped rather than mis-boxed).
fn projected_rect(
    view_proj: glam::Mat4,
    rect: egui::Rect,
    min: glam::DVec3,
    max: glam::DVec3,
) -> Option<egui::Rect> {
    let mut out: Option<egui::Rect> = None;
    for i in 0..8 {
        let corner = glam::DVec3::new(
            if i & 1 == 0 { min.x } else { max.x },
            if i & 2 == 0 { min.y } else { max.y },
            if i & 4 == 0 { min.z } else { max.z },
        );
        let p = project(view_proj, rect, corner)?;
        out = Some(match out {
            Some(r) => r.union(egui::Rect::from_min_max(p, p)),
            None => egui::Rect::from_min_max(p, p),
        });
    }
    out
}

/// Rubber box: solid stroke for a window drag, dashed for a crossing drag.
fn draw_rubber_box(
    painter: &egui::Painter,
    drag: egui::Rect,
    mode: crate::boxsel::BoxMode,
    visuals: &egui::Visuals,
) {
    let stroke = egui::Stroke::new(1.0, visuals.selection.stroke.color);
    match mode {
        crate::boxsel::BoxMode::Window => {
            painter.rect_stroke(drag, 0.0, stroke, egui::StrokeKind::Middle);
        }
        crate::boxsel::BoxMode::Crossing => {
            let corners = [
                drag.left_top(),
                drag.right_top(),
                drag.right_bottom(),
                drag.left_bottom(),
                drag.left_top(),
            ];
            for pair in corners.windows(2) {
                painter.extend(egui::Shape::dashed_line(pair, stroke, 4.0, 4.0));
            }
        }
    }
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

    fn on_exit(&mut self) {
        // Clean exit: nothing crashed, nothing to recover.
        if let Some(j) = &mut self.journal {
            j.discard();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.run_startup_script();

        // Mirror the op-log to the crash journal. One hook covers every
        // mutation path (command line, gumball, deck, history jumps); the
        // generation check keeps idle frames free.
        if self.journaled_generation != Some(self.session.doc.generation) {
            self.journaled_generation = Some(self.session.doc.generation);
            if let Some(j) = &mut self.journal {
                j.sync(&self.session);
            }
        }

        self.handle_dev_screenshot(ui.ctx());

        // Persist zoom changes from any source (buttons or Cmd+=/Cmd+-).
        let zoom = ui.ctx().zoom_factor();
        if (zoom - self.saved_zoom).abs() > 0.01 {
            self.saved_zoom = zoom;
            save_zoom(zoom);
        }

        let open_key =
            ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::O));
        if open_key {
            self.open(None);
        }

        // Canvas shortcuts: pure keymap resolves each key press to a command
        // line; nothing fires while a text field owns the keyboard.
        let typing = ui.ctx().memory(|m| m.focused().is_some());
        let pressed: Vec<(egui::Key, egui::Modifiers)> = ui.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        repeat: false,
                        modifiers,
                        ..
                    } => Some((*key, *modifiers)),
                    _ => None,
                })
                .collect()
        });
        for (key, mods) in pressed {
            // Context is rebuilt per key: an earlier press this frame may have
            // started a tool or changed the selection.
            let line = keymap::keymap(
                key,
                mods,
                keymap::KeyContext {
                    typing,
                    draw_active: self.draw_tool.active(),
                    has_selection: !self.session.doc.selection.is_empty(),
                    last_command: self.last_line.as_deref(),
                },
            );
            if let Some(line) = line {
                self.execute_line(line);
            }
        }

        // Status strip sits below the command line (first bottom panel is
        // outermost); all strings come from the pure fns in `statusbar`.
        egui::Panel::bottom("statusbar")
            .resizable(false)
            .show(ui, |ui| self.status_bar(ui));

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

        // A `view <name>` restore (command line, deck or script) parks the
        // saved camera in the document mailbox; drive the active viewport.
        if let Some(view) = self.session.doc.pending_view.take() {
            apply_named_view(self.active_camera(), &view);
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| self.viewport(ui));
    }
}

/// Snapshot the orbit camera as document-storable named-view parameters.
fn named_view_of(cam: &OrbitCamera) -> mydrafter_doc::NamedView {
    mydrafter_doc::NamedView {
        target: cam.target.to_array(),
        distance: cam.distance,
        yaw: cam.yaw,
        pitch: cam.pitch,
        fov_y: cam.fov_y,
        ortho: cam.ortho,
    }
}

fn apply_named_view(cam: &mut OrbitCamera, view: &mydrafter_doc::NamedView) {
    cam.target = glam::Vec3::from_array(view.target);
    cam.distance = view.distance;
    cam.yaw = view.yaw;
    cam.pitch = view.pitch;
    cam.fov_y = view.fov_y;
    cam.ortho = view.ortho;
}
