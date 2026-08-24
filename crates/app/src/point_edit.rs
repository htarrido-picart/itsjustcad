//! Control-point editor: when a single Curve::Nurbs or Curve::Polyline is
//! selected, its control/vertex points are drawn as small draggable squares.
//! Dragging a handle previews the moved point; on release ONE `setpoint`
//! command is emitted through the substrate (same drag-end-emits-command
//! contract as the gumball) so the op-log stays authoritative and undo works.

use glam::{DVec3, Mat4};
use mydrafter_commands::{Command, Selector};
use kernel_curve::Curve;
use mydrafter_doc::{Document, Geometry, ObjectId};

const HANDLE_PX: f32 = 5.0;
const HIT_PX: f32 = 8.0;

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
            let color = if active_idx == Some(i) || hovered == Some(i) {
                egui::Color32::from_rgb(255, 210, 80)
            } else {
                egui::Color32::from_rgb(120, 200, 255)
            };
            painter.rect_filled(
                egui::Rect::from_center_size(sp, egui::Vec2::splat(HANDLE_PX * 2.0)),
                0.0,
                color,
            );
            painter.rect_stroke(
                egui::Rect::from_center_size(sp, egui::Vec2::splat(HANDLE_PX * 2.0)),
                0.0,
                egui::Stroke::new(1.0, egui::Color32::from_rgb(20, 20, 20)),
                egui::StrokeKind::Middle,
            );
        }
        if self.drag.is_some() {
            ui.ctx().request_repaint();
        }
        out
    }
}
