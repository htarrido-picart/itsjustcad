// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Section planes in the viewport (#359): the Section Plane tool's input and
//! preview, the plane widgets (frame, arrows into the kept side, name; the
//! active cut bold), double-click to toggle a cut, and Align View. The tool's
//! state and geometry are [`crate::section_tool`]; the cut itself is the
//! renderer's (a camera uniform, no geometry upload).

use glam::DVec3;
use itsjustcad_commands::{Command, SectionPlaneOp};
use itsjustcad_doc::SectionPlane;

use super::{App, ARROW_AXES, axis_color, project, screen_ray};
use crate::section_tool::{Placement, outline_distance, widget};

/// The active cut's colour (SketchUp's orange section plane).
const ACTIVE: egui::Color32 = egui::Color32::from_rgb(240, 130, 20);
/// An inactive plane's colour.
const INACTIVE: egui::Color32 = egui::Color32::from_rgb(130, 130, 140);
/// How near (px) a double-click must land to a frame to toggle its cut.
const FRAME_PICK_PX: f32 = 6.0;

impl App {
    pub(in crate::app) fn section_tool_input(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        response: &egui::Response,
        view_proj: glam::Mat4,
    ) {
        // The canvas owns the keyboard: the arrows lock the normal.
        if let Some(id) = ui.ctx().memory(|m| m.focused()) {
            ui.ctx().memory_mut(|m| m.surrender_focus(id));
        }
        let (esc, shift, arrows) = ui.input(|i| {
            let arrows: Vec<char> =
                ARROW_AXES.iter().filter(|(k, _)| i.key_pressed(*k)).map(|(_, a)| *a).collect();
            (i.key_pressed(egui::Key::Escape), i.modifiers.shift, arrows)
        });
        if esc {
            self.section_tool.cancel();
            self.command_line.push_line("sectionplane: cancelled");
            return;
        }
        for a in arrows {
            self.section_tool.toggle_lock(a);
        }
        let ray = response.hover_pos().or_else(|| response.interact_pointer_pos()).map(|p| screen_ray(view_proj, rect, p));
        let place = |tool: &crate::section_tool::SectionPlaneTool, doc| ray.and_then(|(o, d)| tool.placement(doc, o, d));
        let free = place(&self.section_tool, &self.session.doc);
        self.section_tool.set_hold(shift, free);
        let at = if self.section_tool.held() { place(&self.section_tool, &self.session.doc) } else { free };
        if response.clicked_by(egui::PointerButton::Primary)
            && let Some(p) = at
        {
            let cmd = self.section_tool.place(p);
            self.run_viewport_edit(cmd);
            return;
        }
        let painter = ui.painter_at(rect);
        if let Some(p) = at {
            let colour = match self.section_tool.lock() {
                Some(a) if !self.section_tool.held() => axis_color(a),
                _ => ACTIVE,
            };
            self.paint_placement(&painter, rect, view_proj, p, colour);
        }
        if let Some(prompt) = self.section_tool.prompt() {
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

    /// The plane a click would place, as a dashed-looking ghost widget.
    fn paint_placement(
        &mut self,
        painter: &egui::Painter,
        rect: egui::Rect,
        view_proj: glam::Mat4,
        p: Placement,
        colour: egui::Color32,
    ) {
        let Some(plane) = SectionPlane::new("", p.point, p.normal) else { return };
        let (_, half) = self.section_tool.frame(&self.session.doc);
        // Centred on the cursor point: the frame follows the hover.
        paint_widget(painter, rect, view_proj, &plane, p.point, half * 0.5, colour, 1.5, None);
    }

    /// Every section plane's widget (`sections planes on`).
    pub(in crate::app) fn paint_section_planes(&mut self, ui: &egui::Ui, rect: egui::Rect, view_proj: glam::Mat4) {
        if self.session.doc.section_planes.is_empty() {
            return;
        }
        let (center, half) = self.section_tool.frame(&self.session.doc);
        let painter = ui.painter_at(rect);
        let font = egui::TextStyle::Small.resolve(ui.style());
        let doc = &self.session.doc;
        for plane in &doc.section_planes {
            let active = doc.active_section.as_deref() == Some(plane.name.as_str());
            let (colour, width) = if active { (ACTIVE, 2.0) } else { (INACTIVE, 1.2) };
            paint_widget(&painter, rect, view_proj, plane, center, half, colour, width, Some(&font));
        }
    }

    /// Double-click on a plane's frame (#359): toggle its cut. Returns
    /// whether a frame was hit.
    pub(in crate::app) fn toggle_section_at(&mut self, view_proj: glam::Mat4, rect: egui::Rect, pos: egui::Pos2) -> bool {
        if !self.section_style.planes || self.session.doc.section_planes.is_empty() {
            return false;
        }
        let (center, half) = self.section_tool.frame(&self.session.doc);
        let doc = &self.session.doc;
        // Frames can overlap on screen (parallel planes in a parallel
        // view): the active one wins.
        let is_active = |p: &SectionPlane| doc.active_section.as_deref() == Some(p.name.as_str());
        let mut hits = doc.section_planes.iter().filter(|plane| {
            let w = widget(plane, center, half);
            let px: Option<Vec<egui::Pos2>> = w.corners.iter().map(|c| project(view_proj, rect, *c)).collect();
            px.is_some_and(|px| outline_distance(&px, pos) <= FRAME_PICK_PX)
        });
        let first = hits.next();
        let hit = first.filter(|p| is_active(p)).or_else(|| hits.find(|p| is_active(p))).or(first);
        let Some(plane) = hit else { return false };
        let op = if doc.active_section.as_deref() == Some(plane.name.as_str()) {
            SectionPlaneOp::Deactivate
        } else {
            SectionPlaneOp::Activate { name: plane.name.clone() }
        };
        self.run_viewport_edit(Command::SectionPlane(op));
        true
    }

    /// `sectionplane align [name]` (#359, SketchUp's Align View): the active
    /// pane looks straight at the plane from its cut side, in parallel
    /// projection, framing the model. View state, never logged.
    pub(in crate::app) fn align_view_to_section(&mut self, name: Option<&str>) {
        let doc = &self.session.doc;
        let plane = match name {
            Some(n) => doc.section_plane(n),
            None => doc.active_section_plane(),
        };
        let Some(plane) = plane.cloned() else {
            self.command_line.push_line(match name {
                Some(n) => format!("sectionplane: no section plane named '{n}'"),
                None => "sectionplane: no active section plane; name one".to_string(),
            });
            return;
        };
        let (center, half) = self.section_tool.frame(&self.session.doc);
        let target = center - plane.normal * plane.signed(center);
        let (yaw, pitch) = view_angles(plane.normal);
        let aspect = self.active_aspect;
        let cam = self.active_camera();
        cam.set_parallel(true);
        cam.yaw = yaw;
        cam.pitch = pitch;
        cam.frame(target.as_vec3(), (half / 0.6) as f32, aspect, 5.0);
        self.command_line.push_line(format!("view aligned to section plane {}", plane.name));
    }
}

/// Camera yaw and pitch putting the eye along `n` from the target (the eye
/// on the cut side, looking into the kept part). A vertical normal takes the
/// Top/Bottom views' yaw.
fn view_angles(n: DVec3) -> (f32, f32) {
    let n = n.normalize_or_zero();
    let pitch = n.z.clamp(-1.0, 1.0).asin() as f32;
    let yaw = if n.x.hypot(n.y) < 1e-9 { -std::f32::consts::FRAC_PI_2 } else { n.y.atan2(n.x) as f32 };
    (yaw, pitch)
}

/// Draw one plane's widget: a translucent frame, its outline, the arrows into
/// the kept side, and its name (when `font` is given).
#[allow(clippy::too_many_arguments)]
fn paint_widget(
    painter: &egui::Painter,
    rect: egui::Rect,
    view_proj: glam::Mat4,
    plane: &SectionPlane,
    center: DVec3,
    half: f64,
    colour: egui::Color32,
    width: f32,
    font: Option<&egui::FontId>,
) {
    let w = widget(plane, center, half);
    let stroke = egui::Stroke::new(width, colour);
    let corners: Option<Vec<egui::Pos2>> = w.corners.iter().map(|c| project(view_proj, rect, *c)).collect();
    if let Some(px) = &corners {
        painter.add(egui::Shape::convex_polygon(px.clone(), colour.gamma_multiply(0.08), egui::Stroke::NONE));
        painter.add(egui::Shape::closed_line(px.clone(), stroke));
    }
    for [tail, head, b1, b2] in &w.arrows {
        if let [Some(t), Some(h), Some(a), Some(b)] = [tail, head, b1, b2].map(|p| project(view_proj, rect, *p)) {
            painter.line_segment([t, h], stroke);
            painter.line_segment([h, a], stroke);
            painter.line_segment([h, b], stroke);
        }
    }
    if let (Some(font), Some(px)) = (font, &corners)
        && !plane.name.is_empty()
    {
        // The name at the frame's top-most corner, just above it.
        let top = px.iter().copied().fold(px[0], |a, b| if b.y < a.y { b } else { a });
        painter.text(top + egui::vec2(6.0, -4.0), egui::Align2::LEFT_BOTTOM, &plane.name, font.clone(), colour);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_angles_put_the_eye_on_the_normal() {
        for n in [DVec3::X, DVec3::NEG_Y, DVec3::new(1.0, 1.0, 1.0), DVec3::Z, DVec3::NEG_Z] {
            let (yaw, pitch) = view_angles(n);
            let cam = itsjustcad_render::OrbitCamera { yaw, pitch, target: glam::Vec3::ZERO, distance: 1.0, ..Default::default() };
            let eye = cam.eye().as_dvec3();
            assert!((eye - n.normalize()).length() < 1e-5, "{n}: {eye}");
        }
    }
}
