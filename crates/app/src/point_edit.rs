// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Control-point editor (#163): when a single Curve::Nurbs or Curve::Polyline
//! is selected, its control/vertex points are drawn as small FILLED CIRCLES
//! (theme-aware ink: black on light, white on dark; the selected / hovered
//! node switches to the accent color).
//!
//! - **Click** a handle: that vertex becomes the selected vertex (accent,
//!   larger dot). Clicking anywhere else, or Esc, clears it.
//! - **Press-drag** a handle (selected or not): the vertex follows the pointer
//!   on the view's work plane ([`drag_plane_normal`]), snapping to other
//!   objects' osnap points (and the grid, in-plane). On release ONE `setpoint`
//!   is emitted through the substrate (same drag-end-emits-command contract as
//!   the gumball) so the op-log stays authoritative and undo works. Esc
//!   cancels a drag.
//! - **Delete / Backspace** with a selected vertex: `deletepoint` (wired in
//!   the app's early hotkeys via [`PointEdit::delete_command`]).
//! - **Double-click** a polyline segment: `insertpoint` at that spot.
//!
//! Hit tests run at the PRESS position (egui only reports a drag after its
//! threshold, by which time the pointer has moved) and the NEAREST handle wins.

use glam::{DVec3, Mat4};
use itsjustcad_commands::osnap::{self, SnapSettings};
use itsjustcad_commands::{Command, Selector};
use itsjustcad_doc::{Document, Geometry, ObjectId};
use itsjustcad_render::Theme;
use kernel_curve::Curve;

/// Radius (logical px) of a control-point dot.
const HANDLE_PX: f32 = 4.0;
/// Radius (logical px) of the selected control-point dot.
const SELECTED_PX: f32 = 5.5;
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

/// The handle nearest `at` within `radius` px (not merely the first in
/// range). `handles[i]` is `None` when point `i` does not project.
pub fn nearest_handle(
    handles: &[Option<egui::Pos2>],
    at: egui::Pos2,
    radius: f32,
) -> Option<usize> {
    handles
        .iter()
        .enumerate()
        .filter_map(|(i, h)| Some((i, h.as_ref()?.distance(at))))
        .filter(|(_, d)| *d <= radius)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}

/// Normal of the plane a vertex drags on, from the view direction `forward`
/// (same rule as iOS #138 `VertexEdit.dragPlaneNormal`). A view looking along
/// a world axis (Top, Front, Right) drags on that axis's plane (XY, XZ, YZ).
/// An oblique view drags on the horizontal plane while it looks down enough
/// to keep a flat curve flat; a grazing view drags on the camera-facing plane
/// so the point never runs off to infinity.
pub fn drag_plane_normal(forward: DVec3) -> DVec3 {
    let Some(d) = forward.try_normalize() else {
        return DVec3::Z;
    };
    let cos5 = 5f64.to_radians().cos();
    if let Some(a) = [DVec3::X, DVec3::Y, DVec3::Z]
        .into_iter()
        .find(|a| d.dot(*a).abs() > cos5)
    {
        return a;
    }
    if d.z.abs() >= 0.3 { DVec3::Z } else { -d }
}

/// Intersection of the ray with the plane through `origin` with `normal`, in
/// front of the ray origin; `None` when the ray is parallel to the plane.
pub fn ray_plane(origin: DVec3, normal: DVec3, ro: DVec3, rd: DVec3) -> Option<DVec3> {
    let denom = rd.dot(normal);
    if denom.abs() < 1e-9 {
        return None;
    }
    let t = (origin - ro).dot(normal) / denom;
    (t > 0.0 && t.is_finite()).then(|| ro + rd * t)
}

/// Grid snap (10 cm, [`osnap::grid_snap`]) within an axis-aligned drag plane,
/// keeping the coordinate along the normal (the vertex's depth). A
/// camera-facing plane is left unsnapped.
pub fn grid_snap_in_plane(p: DVec3, normal: DVec3) -> DVec3 {
    let Some(k) = (0..3).find(|k| normal[*k].abs() > 0.999) else {
        return p;
    };
    let mut out = osnap::grid_snap(p);
    out[k] = p[k];
    out
}

struct DragState {
    id: ObjectId,
    index: usize,
    /// Point on the drag plane: the vertex's original position.
    origin: DVec3,
    normal: DVec3,
    current: DVec3,
}

#[derive(Default)]
pub struct PointEdit {
    drag: Option<DragState>,
    /// The selected vertex: (curve, point index).
    selected: Option<(ObjectId, usize)>,
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

/// The single selected curve with editable points, and those points.
fn target(doc: &Document) -> Option<(ObjectId, &[DVec3])> {
    let mut sel = doc.selection.iter();
    let (Some(&id), None) = (sel.next(), sel.next()) else {
        return None;
    };
    Some((id, editable_points(&doc.get(id)?.geometry)?))
}

/// Segment of polyline `points` nearest the screen position `at` within
/// `radius` px: `(insert index, world point on the segment)`.
fn segment_hit(
    points: &[DVec3],
    closed: bool,
    at: egui::Pos2,
    radius: f32,
    project: impl Fn(DVec3) -> Option<egui::Pos2>,
) -> Option<(usize, DVec3)> {
    let n = points.len();
    let segs = if closed && n > 2 {
        n
    } else {
        n.saturating_sub(1)
    };
    let mut best: Option<(f32, usize, DVec3)> = None;
    for i in 0..segs {
        let (a, b) = (points[i], points[(i + 1) % n]);
        let (Some(sa), Some(sb)) = (project(a), project(b)) else {
            continue;
        };
        let ab = sb - sa;
        let t = ((at - sa).dot(ab) / ab.length_sq().max(1e-6)).clamp(0.0, 1.0);
        let d = (sa + ab * t).distance(at);
        if d <= radius && best.is_none_or(|(bd, ..)| d < bd) {
            // Screen-space t is exact in ortho, close enough in perspective
            // for a click on the drawn segment.
            best = Some((d, i + 1, a.lerp(b, t as f64)));
        }
    }
    best.map(|(_, i, p)| (i, p))
}

impl PointEdit {
    /// The selected vertex, if it still exists on the single selected curve.
    pub fn selected(&self, doc: &Document) -> Option<(ObjectId, usize)> {
        let (id, points) = target(doc)?;
        self.selected
            .filter(|&(sid, i)| sid == id && i < points.len())
    }

    /// Esc: cancel a live drag, else clear the selected vertex. Returns
    /// whether it did either (so Esc does not also deselect the curve).
    pub fn escape(&mut self, doc: &Document) -> bool {
        if self.drag.take().is_some() {
            return true;
        }
        let had = self.selected(doc).is_some();
        self.selected = None;
        had
    }

    /// Delete key: `deletepoint` for the selected vertex. Clears the vertex
    /// selection (the indices shift).
    pub fn delete_command(&mut self, doc: &Document) -> Option<Command> {
        let (id, index) = self.selected(doc)?;
        self.selected = None;
        self.drag = None;
        Some(Command::DeletePoint {
            target: Selector::Ids { ids: vec![id] },
            index: index as u32,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn ui(
        &mut self,
        ui: &egui::Ui,
        rect: egui::Rect,
        response: &egui::Response,
        view_proj: Mat4,
        doc: &Document,
        theme: Theme,
        snap: &SnapSettings,
    ) -> PointEditOutput {
        let mut out = PointEditOutput {
            command: None,
            consumed: false,
        };

        // Only when exactly one object is selected and it has editable points.
        let Some((id, points)) = target(doc) else {
            self.drag = None;
            self.selected = None;
            return out;
        };
        let closed = matches!(
            doc.get(id).map(|o| &o.geometry),
            Some(Geometry::Curve(Curve::Polyline { closed: true, .. }))
        );
        self.selected = self.selected(doc);
        if self
            .drag
            .as_ref()
            .is_some_and(|d| d.id != id || d.index >= points.len())
        {
            self.drag = None;
        }

        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.escape(doc);
        }

        let project = |w: DVec3| crate::app::project(view_proj, rect, w);
        let screen: Vec<Option<egui::Pos2>> = points.iter().map(|p| project(*p)).collect();
        let (primary_down, press_origin) =
            ui.input(|i| (i.pointer.primary_down(), i.pointer.press_origin()));
        // The handle under the PRESS (not the moved pointer), while held.
        let pressed = press_origin
            .filter(|p| primary_down && rect.contains(*p))
            .and_then(|p| nearest_handle(&screen, p, HIT_PX));

        // Drag lifecycle.
        if response.drag_started_by(egui::PointerButton::Primary)
            && let Some(i) = pressed
        {
            let (_, forward) = crate::app::screen_ray(view_proj, rect, rect.center());
            self.selected = Some((id, i));
            self.drag = Some(DragState {
                id,
                index: i,
                origin: points[i],
                normal: drag_plane_normal(forward),
                current: points[i],
            });
        }
        let pointer = ui.input(|i| i.pointer.latest_pos());
        if let Some(drag) = &mut self.drag {
            if let Some(pos) = pointer {
                if let Some(p) = Self::snap_point(doc, snap, id, pos, view_proj, rect) {
                    drag.current = p;
                } else {
                    let (ro, rd) = crate::app::screen_ray(view_proj, rect, pos);
                    if let Some(hit) = ray_plane(drag.origin, drag.normal, ro, rd) {
                        drag.current = if snap.grid {
                            grid_snap_in_plane(hit, drag.normal)
                        } else {
                            hit
                        };
                    }
                }
            }
            if response.drag_stopped_by(egui::PointerButton::Primary) || !primary_down {
                let drag = self.drag.take().expect("checked");
                if drag.current.distance(drag.origin) > 1e-9 {
                    out.command = Some(Command::SetPoint {
                        target: Selector::Ids { ids: vec![drag.id] },
                        index: drag.index as u32,
                        position: drag.current,
                    });
                }
            }
            out.consumed = true;
        } else if response.clicked_by(egui::PointerButton::Primary) {
            // Release frame: press_origin is gone; the click is within egui's
            // click distance of the press, so the release position stands in.
            let hit = response
                .interact_pointer_pos()
                .and_then(|p| nearest_handle(&screen, p, HIT_PX));
            self.selected = hit.map(|i| (id, i));
            out.consumed = hit.is_some();
        } else {
            out.consumed = pressed.is_some();
        }

        // Double-click a polyline segment (not a handle): insert a vertex.
        if out.command.is_none()
            && response.double_clicked()
            && let Some(pos) = response.interact_pointer_pos()
            && nearest_handle(&screen, pos, HIT_PX).is_none()
            && matches!(
                doc.get(id).map(|o| &o.geometry),
                Some(Geometry::Curve(Curve::Polyline { .. }))
            )
            && let Some((index, position)) = segment_hit(points, closed, pos, HIT_PX, project)
        {
            out.command = Some(Command::InsertPoint {
                target: Selector::Ids { ids: vec![id] },
                index: index as u32,
                position,
            });
            self.selected = Some((id, index));
            out.consumed = true;
        }

        let hovered = if self.drag.is_some() {
            None
        } else {
            response
                .hover_pos()
                .and_then(|p| nearest_handle(&screen, p, HIT_PX))
        };

        // Paint the handles (foreground layer, over the wgpu scene).
        let painter = ui
            .ctx()
            .layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("point_edit"),
            ))
            .with_clip_rect(rect);
        let active_idx = self.drag.as_ref().map(|d| d.index);
        let selected_idx = self.selected.map(|(_, i)| i);
        let ring = match theme {
            Theme::Dark => egui::Color32::from_rgb(20, 20, 24),
            Theme::Light => egui::Color32::from_rgb(235, 238, 245),
        };
        for (i, p) in points.iter().enumerate() {
            // While dragging the active handle, draw it at the live position.
            let world = match &self.drag {
                Some(d) if d.index == i => d.current,
                _ => *p,
            };
            let Some(sp) = project(world) else { continue };
            let selected = selected_idx == Some(i) || active_idx == Some(i);
            let r = if selected { SELECTED_PX } else { HANDLE_PX };
            // Small filled circle with a thin contrasting outline so the dot
            // reads on both the shaded fill and the background.
            painter.circle_filled(sp, r, node_color(theme, selected || hovered == Some(i)));
            painter.circle_stroke(sp, r, egui::Stroke::new(1.0, ring));
        }
        if self.drag.is_some() {
            ui.ctx().request_repaint();
        }
        out
    }

    /// Osnap while dragging: the nearest enabled snap point of any OTHER
    /// object (the dragged curve's own points move with it) within the snap
    /// radius of `pos`. Respects the app's osnap toggles (master + per-kind).
    fn snap_point(
        doc: &Document,
        snap: &SnapSettings,
        id: ObjectId,
        pos: egui::Pos2,
        view_proj: Mat4,
        rect: egui::Rect,
    ) -> Option<DVec3> {
        let r = osnap::SNAP_RADIUS_PX;
        let cands: Vec<_> = osnap::candidates_tagged(doc, snap, None, |bb| {
            match crate::app::projected_rect(view_proj, rect, bb.min, bb.max) {
                Some(s) => s.expand(r).contains(pos),
                None => true,
            }
        })
        .into_iter()
        .filter(|c| !c.objects.contains(&Some(id)))
        .map(|c| (c.point, c.kind))
        .collect();
        let project =
            |w| crate::app::project(view_proj, rect, w).map(|p| glam::Vec2::new(p.x, p.y));
        osnap::resolve(&cands, glam::Vec2::new(pos.x, pos.y), r, snap, project).map(|(p, _)| p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::pos2;

    #[test]
    fn idle_node_is_theme_ink() {
        // Near-black on light, near-white on dark; the two differ.
        let light = node_color(Theme::Light, false);
        let dark = node_color(Theme::Dark, false);
        assert!(
            light.r() < 60 && light.g() < 60 && light.b() < 60,
            "light ink is dark"
        );
        assert!(
            dark.r() > 200 && dark.g() > 200 && dark.b() > 200,
            "dark ink is light"
        );
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

    #[test]
    fn nearest_handle_wins_not_first() {
        // Two handles 5 px apart: the one nearer the press is picked, even
        // though both are in range and the other comes first.
        let h = [Some(pos2(100.0, 100.0)), Some(pos2(105.0, 100.0))];
        assert_eq!(nearest_handle(&h, pos2(104.0, 100.0), HIT_PX), Some(1));
        assert_eq!(nearest_handle(&h, pos2(101.0, 100.0), HIT_PX), Some(0));
        assert_eq!(nearest_handle(&h, pos2(150.0, 100.0), HIT_PX), None);
        assert_eq!(
            nearest_handle(&[None, Some(pos2(0.0, 0.0))], pos2(1.0, 1.0), HIT_PX),
            Some(1)
        );
    }

    #[test]
    fn hit_test_uses_press_origin_not_moved_pointer() {
        // egui reports a drag only after its threshold: by then the pointer is
        // >8 px from the handle. The press position still hits it.
        let h = [Some(pos2(100.0, 100.0))];
        let press = pos2(102.0, 101.0);
        let moved = pos2(115.0, 101.0);
        assert_eq!(nearest_handle(&h, press, HIT_PX), Some(0));
        assert_eq!(nearest_handle(&h, moved, HIT_PX), None);
    }

    #[test]
    fn drag_plane_per_view() {
        // Top / Front / Right look along an axis → XY / XZ / YZ.
        assert_eq!(drag_plane_normal(DVec3::new(0.0, 0.0, -1.0)), DVec3::Z);
        assert_eq!(drag_plane_normal(DVec3::new(0.0, 1.0, 0.0)), DVec3::Y);
        assert_eq!(drag_plane_normal(DVec3::new(-1.0, 0.0, 0.0)), DVec3::X);
        // Nearly axis-aligned (within 5°) still counts.
        assert_eq!(drag_plane_normal(DVec3::new(0.05, 1.0, 0.0)), DVec3::Y);
        // Oblique perspective looking down: horizontal plane.
        assert_eq!(drag_plane_normal(DVec3::new(1.0, 1.0, -1.0)), DVec3::Z);
        // Grazing: camera-facing plane (never parallel to the view ray).
        let f = DVec3::new(1.0, 1.0, -0.1).normalize();
        let n = drag_plane_normal(f);
        assert!((n + f).length() < 1e-12, "camera-facing: {n:?}");
        // Degenerate input falls back to XY.
        assert_eq!(drag_plane_normal(DVec3::ZERO), DVec3::Z);
    }

    #[test]
    fn front_view_drag_moves_in_xz() {
        // Front view ray along +Y: a horizontal plane is parallel (the old
        // bug); the XZ plane through the vertex is hit.
        let v = DVec3::new(1.0, 2.0, 3.0);
        let (ro, rd) = (DVec3::new(4.0, -50.0, 7.0), DVec3::Y);
        assert!(ray_plane(v, DVec3::Z, ro, rd).is_none());
        let hit = ray_plane(v, drag_plane_normal(rd), ro, rd).unwrap();
        assert_eq!(hit, DVec3::new(4.0, 2.0, 7.0));
    }

    #[test]
    fn grid_snap_keeps_depth() {
        let p = DVec3::new(1.234, 5.678, 9.876);
        assert_eq!(grid_snap_in_plane(p, DVec3::Y), DVec3::new(1.2, 5.678, 9.9));
        let tilted = DVec3::new(1.0, 1.0, 0.0).normalize();
        assert_eq!(grid_snap_in_plane(p, tilted), p);
    }

    fn doc_with_polyline(points: Vec<DVec3>) -> (Document, ObjectId) {
        let mut doc = Document::default();
        let id = ObjectId::new();
        doc.insert(itsjustcad_doc::SceneObject {
            visible: true,
            id,
            name: None,
            layer: itsjustcad_doc::DEFAULT_LAYER.to_string(),
            color: None,
            material: None,
            lineweight_mm: None,
            edge_tags: None,
            street: None,
            geometry: Geometry::Curve(Curve::Polyline {
                points,
                closed: false,
            }),
        });
        (doc, id)
    }

    #[test]
    fn selection_state_escape_and_delete() {
        let (mut doc, id) = doc_with_polyline(vec![DVec3::ZERO, DVec3::X, DVec3::Y]);
        let mut pe = PointEdit {
            selected: Some((id, 1)),
            ..Default::default()
        };
        // Curve not selected → no selected vertex.
        assert_eq!(pe.selected(&doc), None);
        doc.selection.insert(id);
        assert_eq!(pe.selected(&doc), Some((id, 1)));
        // Esc clears the vertex first (and reports it), then has nothing to do.
        assert!(pe.escape(&doc));
        assert_eq!(pe.selected(&doc), None);
        assert!(!pe.escape(&doc));
        // Delete emits deletepoint for the selected vertex and clears it.
        pe.selected = Some((id, 2));
        match pe.delete_command(&doc) {
            Some(Command::DeletePoint {
                index: 2,
                target: Selector::Ids { ids },
            }) => {
                assert_eq!(ids, vec![id])
            }
            other => panic!("expected deletepoint, got {other:?}"),
        }
        assert_eq!(pe.selected(&doc), None);
        assert!(pe.delete_command(&doc).is_none());
        // An out-of-range index (curve shrank) is not a selection.
        pe.selected = Some((id, 7));
        assert_eq!(pe.selected(&doc), None);
    }

    #[test]
    fn segment_hit_picks_insert_index() {
        let pts = [
            DVec3::ZERO,
            DVec3::new(10.0, 0.0, 0.0),
            DVec3::new(10.0, 10.0, 0.0),
        ];
        let project = |w: DVec3| Some(pos2(w.x as f32 * 10.0, w.y as f32 * 10.0));
        let (i, p) = segment_hit(&pts, false, pos2(50.0, 2.0), HIT_PX, project).unwrap();
        assert_eq!(i, 1);
        assert!((p - DVec3::new(5.0, 0.0, 0.0)).length() < 1e-6);
        assert_eq!(
            segment_hit(&pts, false, pos2(102.0, 50.0), HIT_PX, project)
                .unwrap()
                .0,
            2
        );
        // The closing segment only exists on a closed curve.
        assert!(segment_hit(&pts, false, pos2(50.0, 50.0), HIT_PX, project).is_none());
        assert_eq!(
            segment_hit(&pts, true, pos2(50.0, 50.0), HIT_PX, project)
                .unwrap()
                .0,
            3
        );
    }
}
