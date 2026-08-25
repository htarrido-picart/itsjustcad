// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Control-point editor: when a single Curve::Nurbs or Curve::Polyline is
//! selected, its control/vertex points are drawn as small draggable FILLED
//! CIRCLES (theme-aware ink: black on light, white on dark; the active /
//! hovered node switches to the accent color). Dragging a handle previews the
//! moved point; on release ONE `setpoint` command is emitted through the
//! substrate (same drag-end-emits-command contract as the gumball) so the
//! op-log stays authoritative and undo works.

use glam::{DVec3, Mat4};
use itsjustcad_commands::{Command, Selector};
use kernel_curve::Curve;
use itsjustcad_doc::{Document, Geometry, ObjectId};
use itsjustcad_render::Theme;

/// Radius (logical px) of a control-point dot.
const HANDLE_PX: f32 = 4.0;
const HIT_PX: f32 = 8.0;

/// Fill color of a control-point dot. An idle node reads as theme ink
/// (near-black on light, near-white on dark); the selected / hovered node
/// switches to the accent selection color so the active grip stands out.
pub fn node_color(theme: Theme, active: bool) -> egui::Color32 {
    if active {
        let [r, g, b, _] = theme.selected();
        egui::Color32::from_rgb(to_u8(r), to_u8(g), to_u8(b))
    } else {
        match theme {
            // Near-white ink on the dark viewport, near-black on the light one.
            Theme::Dark => egui::Color32::from_rgb(235, 238, 245),
            Theme::Light => egui::Color32::from_rgb(20, 20, 24),
        }
    }
}

fn to_u8(c: f32) -> u8 {
    (c.clamp(0.0, 1.0) * 255.0).round() as u8
}

struct DragState {
    id: ObjectId,
    index: usize,
    /// Plane height the point is dragged on (its original z).
    plane_z: f64,
    current: DVec3,
}

#[derive(Default)]
pub struct PointEdit {
    drag: Option<DragState>,
}

pub struct PointEditOutput {
    pub command: Option<Command>,
    /// Pointer belongs to a handle this frame: suppress click-pick / box-select.
    pub consumed: bool,
}

/// The editable control/vertex points of a curve, or `None` for curve types
/// without draggable points (line/arc/ellipse are parametric).
pub fn editable_points(geometry: &Geometry) -> Option<&[DVec3]> {
    match geometry {
        Geometry::Curve(Curve::Nurbs { control, .. }) => Some(control),
        Geometry::Curve(Curve::Polyline { points, .. }) => Some(points),
        _ => None,
    }
}

impl PointEdit {
    pub fn ui(
        &mut self,
        ui: &egui::Ui,
        rect: egui::Rect,
        response: &egui::Response,
        view_proj: Mat4,
        doc: &Document,
        theme: Theme,
    ) -> PointEditOutput {
        let mut out = PointEditOutput { command: None, consumed: false };

        // Only when exactly one object is selected and it has editable points.
        let sel: Vec<ObjectId> = doc.selection.iter().copied().collect();
        let [id] = sel[..] else {
            self.drag = None;
            return out;
        };
        let Some(points) = doc.get(id).and_then(|o| editable_points(&o.geometry)) else {
            self.drag = None;
            return out;
        };
        let points = points.to_vec();

        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.drag = None;
        }

        let project = |w: DVec3| crate::app::project(view_proj, rect, w);
        let pointer = response
            .hover_pos()
            .or_else(|| response.interact_pointer_pos());

        // Hit test a handle (only when not already dragging).
        let hovered = pointer.and_then(|pos| {
            if self.drag.is_some() {
                return None;
            }
            points.iter().enumerate().find_map(|(i, p)| {
                let sp = project(*p)?;
                ((sp - pos).length() <= HIT_PX).then_some(i)
            })
        });

        // Drag lifecycle.
        if response.drag_started_by(egui::PointerButton::Primary)
            && let Some(i) = hovered
        {
            self.drag = Some(DragState {
                id,
                index: i,
                plane_z: points[i].z,
                current: points[i],
            });
        }
        if let Some(drag) = &mut self.drag {
            if let Some(pos) = pointer {
                let (ro, rd) = crate::app::screen_ray(view_proj, rect, pos);
                if let Some(hit) = crate::gumball::ray_plane_z(drag.plane_z, ro, rd) {
                    drag.current = hit;
                }
            }
            if response.drag_stopped_by(egui::PointerButton::Primary) {
                let drag = self.drag.take().expect("checked");
                let original = points[drag.index];
                if drag.current.distance(original) > 1e-9 {
                    out.command = Some(Command::SetPoint {
                        target: Selector::Ids { ids: vec![drag.id] },
                        index: drag.index as u32,
                        position: drag.current,
                    });
                }
            }
            out.consumed = true;
        } else {
            out.consumed = hovered.is_some();
        }

        // Paint the handles (foreground layer, over the wgpu scene).
        let painter = ui
            .ctx()
            .layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("point_edit"),
            ))
            .with_clip_rect(rect);
        let active_idx = self.drag.as_ref().map(|d| d.index);
        for (i, p) in points.iter().enumerate() {
            // While dragging the active handle, draw it at the live position.
            let world = match &self.drag {
                Some(d) if d.index == i => d.current,
                _ => *p,
            };
            let Some(sp) = project(world) else { continue };
            let active = active_idx == Some(i) || hovered == Some(i);
            let fill = node_color(theme, active);
            // Small filled circle with a thin contrasting outline so the dot
            // reads on both the shaded fill and the background.
            painter.circle_filled(sp, HANDLE_PX, fill);
            let ring = match theme {
                Theme::Dark => egui::Color32::from_rgb(20, 20, 24),
                Theme::Light => egui::Color32::from_rgb(235, 238, 245),
            };
            painter.circle_stroke(sp, HANDLE_PX, egui::Stroke::new(1.0, ring));
        }
        if self.drag.is_some() {
            ui.ctx().request_repaint();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_node_is_theme_ink() {
        // Near-black on light, near-white on dark; the two differ.
        let light = node_color(Theme::Light, false);
        let dark = node_color(Theme::Dark, false);
        assert!(light.r() < 60 && light.g() < 60 && light.b() < 60, "light ink is dark");
        assert!(dark.r() > 200 && dark.g() > 200 && dark.b() > 200, "dark ink is light");
        assert_ne!(light, dark);
    }

    #[test]
    fn selected_node_switches_to_accent() {
        // Active node uses the accent selection color, distinct from idle ink,
        // and identical on both themes' active state up to the accent value.
        for theme in [Theme::Dark, Theme::Light] {
            let idle = node_color(theme, false);
            let active = node_color(theme, true);
            assert_ne!(idle, active, "selection must change the node color");
            let [r, g, b, _] = theme.selected();
            assert_eq!(
                active,
                egui::Color32::from_rgb(
                    (r * 255.0).round() as u8,
                    (g * 255.0).round() as u8,
                    (b * 255.0).round() as u8
                )
            );
        }
    }
}
