// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! The Push/Pull tool's viewport side (#370): hover tint, click or drag to
//! grab a face, the ghost of the moved face, the inference tip, typed
//! distances, Enter/click to commit, double-click to repeat, Esc. The state
//! machine is [`crate::pushpull`]; every commit is one `pushpull` op.

use super::{App, project, screen_ray};
use crate::pushpull::target_at;

/// The ghost's colour (the face-cue blue, as #371's hover tint).
const GHOST: egui::Color32 = egui::Color32::from_rgb(90, 160, 255);

impl App {
    pub(in crate::app) fn pushpull_input(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        response: &egui::Response,
        view_proj: glam::Mat4,
    ) {
        // The canvas owns the keyboard: typed digits are the distance.
        if let Some(id) = ui.ctx().memory(|m| m.focused()) {
            ui.ctx().memory_mut(|m| m.surrender_focus(id));
        }
        let (esc, enter, keep, events) = ui.input(|i| {
            let typed: Vec<egui::Event> = i
                .events
                .iter()
                .filter(|e| {
                    matches!(e, egui::Event::Text(_))
                        || matches!(e, egui::Event::Key { key: egui::Key::Backspace, pressed: true, .. })
                })
                .cloned()
                .collect();
            (
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::Enter),
                i.modifiers.alt || i.modifiers.ctrl,
                typed,
            )
        });
        if esc {
            if !self.pushpull.escape() {
                self.command_line.push_line("pushpull: done");
            }
            return;
        }
        for e in events {
            match e {
                egui::Event::Text(t) => t.chars().for_each(|c| {
                    self.pushpull.push_input(c);
                }),
                _ => {
                    self.pushpull.pop_input();
                }
            }
        }
        let cursor_px = response.hover_pos().or_else(|| response.interact_pointer_pos());
        let ray = cursor_px.map(|p| screen_ray(view_proj, rect, p));
        let target = ray.and_then(|(o, d)| target_at(&self.session.doc, o, d));
        if self.pushpull.drag().is_some() {
            let snap = cursor_px.and_then(|pos| self.pushpull_snap(view_proj, rect, pos));
            self.pushpull.update(ray, snap, target.map(|t| (t.point, t.normal)));
        }

        let mut commit = None;
        let primary = egui::PointerButton::Primary;
        if enter && self.pushpull.drag().is_some() {
            commit = self.pushpull.commit(keep);
        } else if response.double_clicked_by(primary)
            && let Some(cmd) = target.and_then(|t| self.pushpull.repeat(t, keep))
        {
            commit = Some(cmd);
        } else if response.clicked_by(primary) {
            match (self.pushpull.drag().is_some(), target) {
                (true, _) => commit = self.pushpull.commit(keep),
                (false, Some(t)) => self.pushpull.begin(&self.session.doc, t, ray),
                (false, None) => {}
            }
        } else if response.drag_started_by(primary)
            && self.pushpull.drag().is_none()
            && let Some(t) = target
        {
            self.pushpull.begin(&self.session.doc, t, ray);
        } else if response.drag_stopped_by(primary) && self.pushpull.drag().is_some() {
            commit = self.pushpull.commit(keep);
        }
        if let Some(cmd) = commit {
            self.run_viewport_edit(cmd);
        }

        let painter = ui.painter_at(rect);
        match self.pushpull.drag() {
            None => {
                // Hover: tint the face the next click grabs, or ring the region.
                let face = ray.and_then(|(o, d)| itsjustcad_doc::pick::face_at(&self.session.doc, o, d));
                match (target, face) {
                    (Some(t), Some(f)) if f.object == t.object => {
                        let fill = egui::Color32::from_rgba_unmultiplied(90, 160, 255, 48);
                        let mut mesh = egui::Mesh::default();
                        for tri in crate::face_draw::face_cue(&mut self.face_cue, &self.session.doc, &f) {
                            if let [Some(a), Some(b), Some(c)] = tri.map(|p| project(view_proj, rect, p)) {
                                let i = mesh.vertices.len() as u32;
                                for p in [a, b, c] {
                                    mesh.colored_vertex(p, fill);
                                }
                                mesh.add_triangle(i, i + 1, i + 2);
                            }
                        }
                        painter.add(egui::Shape::mesh(mesh));
                    }
                    (Some(t), _) => {
                        for [a, b] in crate::pushpull::outline(&self.session.doc, &t) {
                            line(&painter, view_proj, rect, a, b, egui::Stroke::new(2.5, GHOST));
                        }
                    }
                    _ => {}
                }
            }
            Some(d) => {
                // Ghost: the old outline, the moved one, and the sides.
                let by = d.target.normal * d.distance;
                let thin = egui::Stroke::new(1.0, GHOST.gamma_multiply(0.7));
                let bold = egui::Stroke::new(2.0, GHOST);
                for [a, b] in &d.outline {
                    line(&painter, view_proj, rect, *a, *b, thin);
                    line(&painter, view_proj, rect, *a + by, *b + by, bold);
                    line(&painter, view_proj, rect, *a, *a + by, thin);
                }
                if let (Some(tip), Some(pos)) = (d.tip, cursor_px) {
                    painter.text(
                        pos + egui::vec2(14.0, 14.0),
                        egui::Align2::LEFT_TOP,
                        tip.text(),
                        egui::TextStyle::Small.resolve(ui.style()),
                        GHOST,
                    );
                }
            }
        }
        if let Some(prompt) = self.pushpull.prompt() {
            painter.text(
                rect.center_top() + egui::vec2(0.0, 28.0),
                egui::Align2::CENTER_TOP,
                prompt,
                egui::TextStyle::Body.resolve(ui.style()),
                ui.visuals().strong_text_color(),
            );
        }
        ui.ctx().request_repaint();
    }

    /// The object snap under the cursor, for the height inference.
    fn pushpull_snap(&self, view_proj: glam::Mat4, rect: egui::Rect, pos: egui::Pos2) -> Option<glam::DVec3> {
        let settings = self.snap_settings;
        let cands = crate::osnap::candidates_filtered(&self.session.doc, &settings, None, |bb| {
            match super::projected_rect(view_proj, rect, bb.min, bb.max) {
                Some(r) => r.expand(crate::osnap::SNAP_RADIUS_PX).contains(pos),
                None => true,
            }
        });
        crate::osnap::resolve(
            &cands,
            glam::Vec2::new(pos.x, pos.y),
            crate::osnap::SNAP_RADIUS_PX,
            &settings,
            |w| project(view_proj, rect, w).map(|p| glam::Vec2::new(p.x, p.y)),
        )
        .map(|(p, _)| p)
    }
}

fn line(
    painter: &egui::Painter,
    view_proj: glam::Mat4,
    rect: egui::Rect,
    a: glam::DVec3,
    b: glam::DVec3,
    stroke: egui::Stroke,
) {
    if let (Some(a), Some(b)) = (project(view_proj, rect, a), project(view_proj, rect, b)) {
        painter.line_segment([a, b], stroke);
    }
}
