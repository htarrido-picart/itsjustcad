// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! The walkthrough tools in the viewport (#369): Position Camera's click or
//! drag, Look Around's drag, Walk's drag and keys, typed eye heights, Esc.
//! The state and motion maths are [`crate::walkthrough`]; the floor and
//! collision rules are [`itsjustcad_doc::walk`]. Camera moves are view state:
//! the pane's view history records the result once it holds still.

use glam::{DVec3, Vec3};
use itsjustcad_doc::pick::{FacePlane, face_at};
use itsjustcad_doc::walk;

use super::{App, project, screen_ray};
use crate::walkthrough::{LOOK_DISTANCE, LOOK_RAD_PER_PX, Motion, WalkKeys, WalkMode};

/// The crosshair and look-line colour.
const MARK: egui::Color32 = egui::Color32::from_rgb(240, 130, 20);

impl App {
    /// Arm a walkthrough tool from the command line (`poscam [h]`,
    /// `positioncamera [h]`, `lookaround [h]`, `walk [h]`). Ends any other tool.
    pub(in crate::app) fn start_walkthrough(&mut self, line: &str) {
        let mut words = line.split_whitespace();
        let Some(mode) = words.next().and_then(WalkMode::from_verb) else { return };
        let height = match words.next() {
            Some(w) => match crate::walkthrough::parse_height(w) {
                Some(h) => Some(h),
                None => {
                    self.command_line.push_line(format!("usage: {} [eye height m]", mode.verb()));
                    return;
                }
            },
            None => None,
        };
        self.draw_tool.cancel();
        self.guided.cancel();
        self.pushpull.cancel();
        self.section_tool.cancel();
        self.zoom_window = None;
        let prompt = self.walkthrough.start(mode);
        self.command_line.push_line(prompt);
        if let Some(h) = height {
            self.walk_set_height(h);
        }
    }

    /// A typed eye height (`h` m): Position Camera's next placement; in Look
    /// Around and Walk the eye moves to `h` above its floor now.
    pub(in crate::app) fn walk_set_height(&mut self, h: f64) {
        self.walkthrough.eye_height = h;
        match self.walkthrough.mode() {
            Some(WalkMode::PositionCamera) => {
                self.command_line.push_line(format!("poscam: eye height {h} m"));
            }
            Some(mode) => {
                let doc = &self.session.doc;
                let cam = {
                    let idx = self.effective_camera_index(self.active_pane);
                    &mut self.cameras[idx]
                };
                let eye = walk::at_height(doc, cam.eye().as_dvec3(), h);
                cam.move_eye(eye.as_vec3());
                self.command_line.push_line(format!("{}: eye height {h} m", mode.verb()));
            }
            None => {}
        }
    }

    /// Where Position Camera stands for the cursor ray: the face under it,
    /// else the ground.
    fn walk_point(&self, origin: DVec3, dir: DVec3) -> Option<DVec3> {
        face_at(&self.session.doc, origin, dir)
            .map(|h| h.point)
            .or_else(|| FacePlane::horizontal(DVec3::ZERO).ray(origin, dir))
    }

    pub(in crate::app) fn walkthrough_input(
        &mut self,
        ui: &mut egui::Ui,
        cam_idx: usize,
        rect: egui::Rect,
        response: &egui::Response,
        view_proj: glam::Mat4,
    ) {
        let Some(mode) = self.walkthrough.mode() else { return };
        // Started from the command line: hand the keyboard to the canvas once.
        // Afterwards the keys are the tool's only while nothing else (the
        // command line, a text field) has focus, so typing is never stolen.
        if std::mem::take(&mut self.walkthrough.take_keys)
            && let Some(id) = ui.ctx().memory(|m| m.focused())
        {
            ui.ctx().memory_mut(|m| m.surrender_focus(id));
        }
        let canvas_keys = ui.ctx().memory(|m| m.focused()).is_none();
        let (esc, enter, typed, keys, shift, run, alt, dt) = ui.input(|i| {
            let typed: Vec<egui::Event> = i
                .events
                .iter()
                .filter(|e| {
                    matches!(e, egui::Event::Text(_))
                        || matches!(e, egui::Event::Key { key: egui::Key::Backspace, pressed: true, .. })
                })
                .cloned()
                .collect();
            let down = |k: egui::Key| canvas_keys && i.key_down(k);
            let keys = WalkKeys {
                forward: down(egui::Key::ArrowUp) || down(egui::Key::W),
                back: down(egui::Key::ArrowDown) || down(egui::Key::S),
                turn_left: down(egui::Key::ArrowLeft),
                turn_right: down(egui::Key::ArrowRight),
                strafe_left: down(egui::Key::A),
                strafe_right: down(egui::Key::D),
            };
            (
                i.key_pressed(egui::Key::Escape),
                canvas_keys && i.key_pressed(egui::Key::Enter),
                typed,
                keys,
                i.modifiers.shift,
                i.modifiers.ctrl || i.modifiers.command,
                i.modifiers.alt,
                i.stable_dt.clamp(0.0, 0.1),
            )
        });
        if esc {
            self.walkthrough.cancel();
            self.command_line.push_line(format!("{}: done", mode.verb()));
            return;
        }
        if canvas_keys {
            for e in typed {
                match e {
                    // W/A/S/D walk; they are not eye-height characters anyway.
                    egui::Event::Text(t) => t.chars().for_each(|c| self.walkthrough.push_input(c)),
                    _ => self.walkthrough.pop_input(),
                }
            }
            if enter && let Some(h) = self.walkthrough.take_input() {
                self.walk_set_height(h);
            }
        }
        let primary = egui::PointerButton::Primary;
        let cursor = response.hover_pos().or_else(|| response.interact_pointer_pos());
        let press = ui.input(|i| i.pointer.press_origin());
        let painter = ui.painter_at(rect);
        if response.hovered() {
            ui.ctx().set_cursor_icon(match mode {
                WalkMode::PositionCamera => egui::CursorIcon::Crosshair,
                WalkMode::LookAround => egui::CursorIcon::Grab,
                WalkMode::Walk => egui::CursorIcon::Move,
            });
        }
        match mode {
            WalkMode::PositionCamera => {
                let at = |app: &Self, pos: egui::Pos2| {
                    let (o, d) = screen_ray(view_proj, rect, pos);
                    app.walk_point(o, d)
                };
                let h = self.walkthrough.eye_height;
                if response.clicked_by(primary)
                    && let Some(p) = cursor.and_then(|pos| at(self, pos))
                {
                    // Level, the way the view faces.
                    let (ahead, _) = self.cameras[cam_idx].walk_axes();
                    self.place_camera(cam_idx, walk::stand(p, h), ahead, LOOK_DISTANCE);
                } else if response.drag_started_by(primary) {
                    self.walkthrough.press = press.or(cursor).and_then(|pos| at(self, pos));
                } else if let Some(p) = self.walkthrough.press {
                    let end = cursor.and_then(|pos| at(self, pos));
                    if let (Some(a), Some(b)) = (project(view_proj, rect, p), cursor) {
                        painter.line_segment([a, b], egui::Stroke::new(1.5, MARK));
                        crosshair(&painter, a);
                    }
                    if response.drag_stopped_by(primary) {
                        self.walkthrough.press = None;
                        let eye = walk::stand(p, h);
                        let (look, dist) = match end {
                            Some(e) if (e - p).length() > 1e-3 => {
                                ((e - eye).as_vec3(), (e - eye).length() as f32)
                            }
                            _ => (self.cameras[cam_idx].walk_axes().0, LOOK_DISTANCE),
                        };
                        self.place_camera(cam_idx, eye, look, dist);
                    }
                }
            }
            WalkMode::LookAround => {
                if response.dragged_by(primary) {
                    let d = response.drag_delta();
                    self.cameras[cam_idx].look(-d.x * LOOK_RAD_PER_PX, -d.y * LOOK_RAD_PER_PX);
                }
            }
            WalkMode::Walk => {
                if response.drag_started_by(primary) {
                    self.walkthrough.drag_origin = press.or(cursor);
                }
                if !ui.input(|i| i.pointer.button_down(primary)) {
                    self.walkthrough.drag_origin = None;
                }
                let drag = match (self.walkthrough.drag_origin, ui.input(|i| i.pointer.latest_pos())) {
                    (Some(o), Some(p)) => {
                        crosshair(&painter, o);
                        painter.line_segment([o, p], egui::Stroke::new(1.0, MARK.gamma_multiply(0.6)));
                        Some((p.x - o.x, p.y - o.y))
                    }
                    _ => None,
                };
                let m = Motion::from_input(keys, drag, shift, run, dt);
                if !m.is_zero() {
                    self.walk_camera(cam_idx, m, !alt);
                    ui.ctx().request_repaint();
                }
            }
        }
        if let Some(prompt) = self.walkthrough.prompt() {
            painter.text(
                rect.center_top() + egui::vec2(0.0, 28.0),
                egui::Align2::CENTER_TOP,
                prompt,
                egui::TextStyle::Body.resolve(ui.style()),
                ui.visuals().strong_text_color(),
            );
        }
    }

    /// Position Camera's result: the eye at `eye` looking along `look`, in
    /// perspective; Look Around follows (SketchUp).
    fn place_camera(&mut self, cam_idx: usize, eye: DVec3, look: Vec3, dist: f32) {
        self.cameras[cam_idx].place_eye(eye.as_vec3(), look, dist);
        self.walkthrough.start(WalkMode::LookAround);
        self.walkthrough.take_keys = false;
        self.command_line.push_line(format!(
            "poscam: eye at {:.2}, {:.2}, {:.2}",
            eye.x, eye.y, eye.z
        ));
        if let Some(p) = self.walkthrough.prompt() {
            self.command_line.push_line(p);
        }
    }

    /// One Walk frame on camera `cam_idx`: turn, then step with the floor
    /// and (when `collide`) the faces ahead.
    pub(in crate::app) fn walk_camera(&mut self, cam_idx: usize, m: Motion, collide: bool) {
        let cam = &mut self.cameras[cam_idx];
        if m.turn_left != 0.0 {
            cam.look(m.turn_left, 0.0);
        }
        let (ahead, right) = cam.walk_axes();
        let delta = (ahead * m.ahead + right * m.right + Vec3::Z * m.up).as_dvec3();
        if delta != DVec3::ZERO {
            let eye = walk::step(&self.session.doc, cam.eye().as_dvec3(), delta, collide);
            cam.move_eye(eye.as_vec3());
        }
    }
}

/// A small crosshair at `p`.
fn crosshair(painter: &egui::Painter, p: egui::Pos2) {
    let s = egui::Stroke::new(1.5, MARK);
    painter.line_segment([p - egui::vec2(8.0, 0.0), p + egui::vec2(8.0, 0.0)], s);
    painter.line_segment([p - egui::vec2(0.0, 8.0), p + egui::vec2(0.0, 8.0)], s);
}
