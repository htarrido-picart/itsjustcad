//! Gumball transform gizmo: shown at the selection's combined AABB center.
//! Arrows drag-move along X/Y/Z, the ring rotates about Z, the center square
//! scales uniformly. Drags preview as a ghost wireframe; on release ONE
//! substrate command (move/rotate/scale with explicit center) is emitted so
//! the op-log stays the single source of truth and undo works.

use glam::{DMat4, DVec3, Mat4};
use mydrafter_commands::{Command, Selector};
use mydrafter_doc::{Document, ObjectId};

/// Screen-space sizes (logical px, pre-zoom).
const AXIS_PX: f32 = 70.0;
const RING_PX: f32 = 52.0;
const HIT_PX: f32 = 7.0;
const CENTER_PX: f32 = 6.0;
const RING_SEGMENTS: usize = 48;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    MoveX,
    MoveY,
    MoveZ,
    RotZ,
    ScaleUniform,
}

impl Handle {
    fn axis(self) -> Option<DVec3> {
        match self {
            Handle::MoveX => Some(DVec3::X),
            Handle::MoveY => Some(DVec3::Y),
            Handle::MoveZ => Some(DVec3::Z),
            _ => None,
        }
    }
}

struct DragState {
    handle: Handle,
    /// Gizmo center at drag start; transform center for rotate/scale.
    center: DVec3,
    ids: Vec<ObjectId>,
    /// Move: axis param at grab. Rotate: pointer angle at grab (rad).
    /// Scale: pointer distance from projected center at grab (px).
    start: f64,
    /// Same quantity, current frame.
    current: f64,
}

#[derive(Default)]
pub struct Gumball {
    drag: Option<DragState>,
}

pub struct GumballOutput {
    /// Command to run through the substrate (drag just ended).
    pub command: Option<Command>,
    /// Pointer belongs to the gizmo this frame: suppress click-pick.
    pub consumed: bool,
}

impl Gumball {
    pub fn ui(
        &mut self,
        ui: &egui::Ui,
        rect: egui::Rect,
        response: &egui::Response,
        view_proj: Mat4,
        doc: &Document,
    ) -> GumballOutput {
        let mut out = GumballOutput { command: None, consumed: false };

        let ids: Vec<ObjectId> = doc
            .objects()
            .filter(|o| doc.selection.contains(&o.id))
            .map(|o| o.id)
            .collect();
        if ids.is_empty() {
            self.drag = None;
            return out;
        }
        let center = match &self.drag {
            Some(d) => d.center, // stable anchor while dragging
            None => selection_center(doc, &ids),
        };

        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.drag = None;
        }

        let project = |w: DVec3| crate::app::project(view_proj, rect, w);
        let Some(center_px) = project(center) else {
            return out;
        };

        // World length per axis so each arrow projects to ~AXIS_PX on screen.
        let axis_len = |axis: DVec3| -> Option<f64> {
            let tip = project(center + axis)?;
            let px = (tip - center_px).length();
            (px > 1e-3).then(|| (AXIS_PX / px) as f64)
        };
        let axes: Vec<(Handle, DVec3, f64)> = [
            (Handle::MoveX, DVec3::X),
            (Handle::MoveY, DVec3::Y),
            (Handle::MoveZ, DVec3::Z),
        ]
        .into_iter()
        .filter_map(|(h, a)| Some((h, a, axis_len(a)?)))
        .collect();
        let ring_radius = axis_len(DVec3::X).map(|l| l * (RING_PX / AXIS_PX) as f64);

        let pointer = response
            .hover_pos()
            .or_else(|| response.interact_pointer_pos());

        // -- hit test (hover, or drag start) --
        let hovered = pointer.and_then(|pos| {
            if self.drag.is_some() {
                return None;
            }
            for (handle, axis, len) in &axes {
                if let (Some(a), Some(b)) = (project(center), project(center + *axis * *len))
                    && dist_point_segment(pos, a, b) <= HIT_PX
                {
                    return Some(*handle);
                }
            }
            if egui::Rect::from_center_size(center_px, egui::Vec2::splat(CENTER_PX * 2.0))
                .expand(2.0)
                .contains(pos)
            {
                return Some(Handle::ScaleUniform);
            }
            if let Some(r) = ring_radius {
                let pts = ring_points(center, r, RING_SEGMENTS);
                let mut prev: Option<egui::Pos2> = None;
                for p in pts.iter().filter_map(|w| project(*w)) {
                    if let Some(q) = prev
                        && dist_point_segment(pos, q, p) <= HIT_PX
                    {
                        return Some(Handle::RotZ);
                    }
                    prev = Some(p);
                }
            }
            None
        });

        // -- drag lifecycle --
        if response.drag_started_by(egui::PointerButton::Primary)
            && let Some(handle) = hovered
            && let Some(pos) = pointer
            && let Some(start) = drag_param(handle, center, center_px, view_proj, rect, pos)
        {
            self.drag = Some(DragState {
                handle,
                center,
                ids: ids.clone(),
                start,
                current: start,
            });
        }
        if let Some(drag) = &mut self.drag {
            if let Some(pos) = pointer
                && let Some(p) = drag_param(drag.handle, drag.center, center_px, view_proj, rect, pos)
            {
                drag.current = p;
            }
            if response.drag_stopped_by(egui::PointerButton::Primary) {
                let drag = self.drag.take().expect("checked");
                out.command = commit_command(&drag);
            }
            out.consumed = true;
        } else {
            out.consumed = hovered.is_some();
        }

        // -- paint (foreground layer: on top of the wgpu scene) --
        let painter = ui
            .ctx()
            .layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("gumball"),
            ))
            .with_clip_rect(rect);
        let active = self.drag.as_ref().map(|d| d.handle).or(hovered);
        let colw = |h: Handle, base: egui::Color32| {
            if active == Some(h) {
                egui::Color32::from_rgb(255, 210, 80)
            } else {
                base
            }
        };
        for (handle, axis, len) in &axes {
            let color = colw(
                *handle,
                match handle {
                    Handle::MoveX => egui::Color32::from_rgb(225, 85, 85),
                    Handle::MoveY => egui::Color32::from_rgb(110, 200, 110),
                    _ => egui::Color32::from_rgb(95, 145, 255),
                },
            );
            if let (Some(a), Some(b)) = (project(center), project(center + *axis * *len)) {
                painter.line_segment([a, b], egui::Stroke::new(2.0, color));
                painter.circle_filled(b, 4.0, color);
            }
        }
        if let Some(r) = ring_radius {
            let color = colw(Handle::RotZ, egui::Color32::from_rgb(95, 145, 255));
            let pts: Vec<egui::Pos2> = ring_points(center, r, RING_SEGMENTS)
                .iter()
                .filter_map(|w| project(*w))
                .collect();
            for pair in pts.windows(2) {
                painter.line_segment([pair[0], pair[1]], egui::Stroke::new(1.5, color));
            }
        }
        painter.rect_filled(
            egui::Rect::from_center_size(center_px, egui::Vec2::splat(CENTER_PX * 2.0)),
            1.0,
            colw(Handle::ScaleUniform, egui::Color32::from_rgb(200, 200, 200)),
        );

        // -- ghost preview + readout while dragging --
        if let Some(drag) = &self.drag {
            let m = pending_matrix(drag);
            let stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(255, 210, 80));
            for id in &drag.ids {
                if let Some(obj) = doc.get(*id) {
                    let bb = obj.geometry.aabb();
                    draw_ghost_box(&painter, project, &m, bb.min, bb.max, stroke);
                }
            }
            if let Some(pos) = pointer {
                painter.text(
                    pos + egui::vec2(14.0, -14.0),
                    egui::Align2::LEFT_BOTTOM,
                    drag_readout(drag, doc.units),
                    egui::TextStyle::Small.resolve(ui.style()),
                    egui::Color32::from_rgb(255, 210, 80),
                );
            }
            ui.ctx().request_repaint();
        }
        out
    }
}

/// Combined AABB center of the given (resolved) selection.
fn selection_center(doc: &Document, ids: &[ObjectId]) -> DVec3 {
    let mut bb = doc.get(ids[0]).expect("selected exists").geometry.aabb();
    for id in &ids[1..] {
        bb = bb.union(doc.get(*id).expect("selected exists").geometry.aabb());
    }
    bb.center()
}

/// Current scalar parameter of a drag: axis param (world units), pointer
/// angle about the center (rad), or pointer distance from the center (px).
fn drag_param(
    handle: Handle,
    center: DVec3,
    center_px: egui::Pos2,
    view_proj: Mat4,
    rect: egui::Rect,
    pos: egui::Pos2,
) -> Option<f64> {
    match handle {
        Handle::MoveX | Handle::MoveY | Handle::MoveZ => {
            let (ro, rd) = crate::app::screen_ray(view_proj, rect, pos);
            closest_axis_t(center, handle.axis().expect("move handle"), ro, rd)
        }
        Handle::RotZ => {
            let (ro, rd) = crate::app::screen_ray(view_proj, rect, pos);
            let p = ray_plane_z(center.z, ro, rd)?;
            Some(plane_angle(center, p))
        }
        Handle::ScaleUniform => Some((pos - center_px).length() as f64),
    }
}

fn commit_command(drag: &DragState) -> Option<Command> {
    let targets = Selector::Ids { ids: drag.ids.clone() };
    match drag.handle {
        Handle::MoveX | Handle::MoveY | Handle::MoveZ => {
            let delta = drag.handle.axis().expect("move handle") * (drag.current - drag.start);
            (delta.length() > 1e-9).then_some(Command::Move { targets, delta })
        }
        Handle::RotZ => {
            let angle_deg = wrap_angle_deg((drag.current - drag.start).to_degrees());
            (angle_deg.abs() > 1e-6).then_some(Command::Rotate {
                targets,
                angle_deg,
                axis: DVec3::Z,
                center: Some(drag.center),
            })
        }
        Handle::ScaleUniform => {
            let f = scale_factor(drag.start, drag.current)?;
            ((f - 1.0).abs() > 1e-9).then_some(Command::Scale {
                targets,
                factors: DVec3::splat(f),
                center: Some(drag.center),
            })
        }
    }
}

fn pending_matrix(drag: &DragState) -> DMat4 {
    let c = drag.center;
    match drag.handle {
        Handle::MoveX | Handle::MoveY | Handle::MoveZ => DMat4::from_translation(
            drag.handle.axis().expect("move handle") * (drag.current - drag.start),
        ),
        Handle::RotZ => {
            DMat4::from_translation(c)
                * DMat4::from_rotation_z(drag.current - drag.start)
                * DMat4::from_translation(-c)
        }
        Handle::ScaleUniform => {
            let f = scale_factor(drag.start, drag.current).unwrap_or(1.0);
            DMat4::from_translation(c)
                * DMat4::from_scale(DVec3::splat(f))
                * DMat4::from_translation(-c)
        }
    }
}

fn drag_readout(drag: &DragState, units: mydrafter_doc::Units) -> String {
    let d = || mydrafter_doc::format_length(units, drag.current - drag.start);
    match drag.handle {
        Handle::MoveX => format!("dx {}", d()),
        Handle::MoveY => format!("dy {}", d()),
        Handle::MoveZ => format!("dz {}", d()),
        Handle::RotZ => format!(
            "{:.1}°",
            wrap_angle_deg((drag.current - drag.start).to_degrees())
        ),
        Handle::ScaleUniform => {
            format!("×{:.3}", scale_factor(drag.start, drag.current).unwrap_or(1.0))
        }
    }
}

fn draw_ghost_box(
    painter: &egui::Painter,
    project: impl Fn(DVec3) -> Option<egui::Pos2>,
    m: &DMat4,
    min: DVec3,
    max: DVec3,
    stroke: egui::Stroke,
) {
    let c = |x: f64, y: f64, z: f64| m.transform_point3(DVec3::new(x, y, z));
    let v = [
        c(min.x, min.y, min.z),
        c(max.x, min.y, min.z),
        c(max.x, max.y, min.z),
        c(min.x, max.y, min.z),
        c(min.x, min.y, max.z),
        c(max.x, min.y, max.z),
        c(max.x, max.y, max.z),
        c(min.x, max.y, max.z),
    ];
    const EDGES: [(usize, usize); 12] = [
        (0, 1), (1, 2), (2, 3), (3, 0),
        (4, 5), (5, 6), (6, 7), (7, 4),
        (0, 4), (1, 5), (2, 6), (3, 7),
    ];
    for (a, b) in EDGES {
        if let (Some(pa), Some(pb)) = (project(v[a]), project(v[b])) {
            painter.line_segment([pa, pb], stroke);
        }
    }
}

/// Points on the Z ring (world space, plane z = center.z), closed.
fn ring_points(center: DVec3, radius: f64, segments: usize) -> Vec<DVec3> {
    (0..=segments)
        .map(|i| {
            let a = i as f64 / segments as f64 * std::f64::consts::TAU;
            center + DVec3::new(a.cos() * radius, a.sin() * radius, 0.0)
        })
        .collect()
}

// ---- pure math (unit-tested) ----

/// Parameter `s` of the point on the axis line `axis_o + s*axis_d` closest to
/// the ray `ray_o + t*ray_d`. `None` when the lines are (near-)parallel.
pub fn closest_axis_t(axis_o: DVec3, axis_d: DVec3, ray_o: DVec3, ray_d: DVec3) -> Option<f64> {
    let r = axis_o - ray_o;
    let a = axis_d.dot(axis_d);
    let b = axis_d.dot(ray_d);
    let c = ray_d.dot(ray_d);
    let d = axis_d.dot(r);
    let e = ray_d.dot(r);
    let denom = a * c - b * b;
    if denom.abs() < 1e-12 {
        return None;
    }
    Some((b * e - c * d) / denom)
}

/// Ray vs the horizontal plane at height `z` (any hit direction, t > 0).
pub fn ray_plane_z(z: f64, ray_o: DVec3, ray_d: DVec3) -> Option<DVec3> {
    if ray_d.z.abs() < 1e-12 {
        return None;
    }
    let t = (z - ray_o.z) / ray_d.z;
    (t > 0.0).then(|| ray_o + ray_d * t)
}

/// Angle of `p` about `center` in the XY plane (radians, atan2 convention).
pub fn plane_angle(center: DVec3, p: DVec3) -> f64 {
    (p.y - center.y).atan2(p.x - center.x)
}

/// Wrap to (-180, 180] so a rotate drag never logs a >half-turn detour.
pub fn wrap_angle_deg(deg: f64) -> f64 {
    let mut a = deg % 360.0;
    if a > 180.0 {
        a -= 360.0;
    } else if a <= -180.0 {
        a += 360.0;
    }
    a
}

/// Uniform scale factor from pointer distances (px) to the gizmo center.
/// `None` when the grab was too close to the center to be meaningful.
pub fn scale_factor(start_px: f64, current_px: f64) -> Option<f64> {
    if start_px < 4.0 {
        return None;
    }
    Some((current_px / start_px).clamp(0.01, 1000.0))
}

/// Distance from point `p` to segment `ab` in screen space.
pub fn dist_point_segment(p: egui::Pos2, a: egui::Pos2, b: egui::Pos2) -> f32 {
    let ab = b - a;
    let len2 = ab.length_sq();
    if len2 < 1e-9 {
        return (p - a).length();
    }
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    (p - (a + ab * t)).length()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axis_param_recovers_point_on_axis() {
        // Ray straight down onto (3, 0, 0); X axis through the origin.
        let t = closest_axis_t(
            DVec3::ZERO,
            DVec3::X,
            DVec3::new(3.0, 0.0, 10.0),
            DVec3::new(0.0, 0.0, -1.0),
        )
        .unwrap();
        assert!((t - 3.0).abs() < 1e-12);
    }

    #[test]
    fn axis_param_skew_ray() {
        // Oblique ray: closest point on the Z axis to the ray through
        // (5, 0, 2) with direction (-1, 0, 0) is z = 2.
        let t = closest_axis_t(
            DVec3::ZERO,
            DVec3::Z,
            DVec3::new(5.0, 0.0, 2.0),
            DVec3::new(-1.0, 0.0, 0.0),
        )
        .unwrap();
        assert!((t - 2.0).abs() < 1e-12);
    }

    #[test]
    fn axis_param_parallel_is_none() {
        assert!(closest_axis_t(DVec3::ZERO, DVec3::X, DVec3::new(0.0, 1.0, 0.0), DVec3::X).is_none());
    }

    #[test]
    fn plane_hit_and_angle() {
        let p = ray_plane_z(
            1.0,
            DVec3::new(2.0, 2.0, 11.0),
            DVec3::new(0.0, 0.0, -1.0),
        )
        .unwrap();
        assert!((p - DVec3::new(2.0, 2.0, 1.0)).length() < 1e-12);
        let a = plane_angle(DVec3::new(1.0, 1.0, 1.0), p);
        assert!((a - std::f64::consts::FRAC_PI_4).abs() < 1e-12);
    }

    #[test]
    fn plane_parallel_or_behind_is_none() {
        assert!(ray_plane_z(0.0, DVec3::new(0.0, 0.0, 1.0), DVec3::X).is_none());
        assert!(ray_plane_z(5.0, DVec3::ZERO, DVec3::new(0.0, 0.0, -1.0)).is_none());
    }

    #[test]
    fn angle_wrapping() {
        assert_eq!(wrap_angle_deg(0.0), 0.0);
        assert!((wrap_angle_deg(190.0) + 170.0).abs() < 1e-12);
        assert!((wrap_angle_deg(-190.0) - 170.0).abs() < 1e-12);
        assert_eq!(wrap_angle_deg(180.0), 180.0);
        assert!((wrap_angle_deg(540.0) - 180.0).abs() < 1e-12);
    }

    #[test]
    fn scale_factor_guard_and_clamp() {
        assert!(scale_factor(2.0, 100.0).is_none()); // grabbed at the center
        assert!((scale_factor(50.0, 100.0).unwrap() - 2.0).abs() < 1e-12);
        assert_eq!(scale_factor(100.0, 0.0).unwrap(), 0.01);
    }

    #[test]
    fn segment_distance() {
        let a = egui::pos2(0.0, 0.0);
        let b = egui::pos2(10.0, 0.0);
        assert!((dist_point_segment(egui::pos2(5.0, 3.0), a, b) - 3.0).abs() < 1e-6);
        assert!((dist_point_segment(egui::pos2(-4.0, 3.0), a, b) - 5.0).abs() < 1e-6);
        assert!((dist_point_segment(egui::pos2(3.0, 4.0), a, a) - 5.0).abs() < 1e-6);
    }

    fn drag(handle: Handle, start: f64, current: f64) -> DragState {
        DragState {
            handle,
            center: DVec3::new(1.0, 2.0, 0.0),
            ids: vec![ObjectId::new()],
            start,
            current,
        }
    }

    #[test]
    fn commit_move_maps_axis_param_to_delta() {
        let cmd = commit_command(&drag(Handle::MoveY, 1.5, 4.0)).unwrap();
        match cmd {
            Command::Move { delta, .. } => assert!((delta - DVec3::new(0.0, 2.5, 0.0)).length() < 1e-12),
            other => panic!("expected move, got {other:?}"),
        }
    }

    #[test]
    fn commit_rotate_uses_explicit_center_and_z_axis() {
        let d = drag(Handle::RotZ, 0.0, std::f64::consts::FRAC_PI_2);
        match commit_command(&d).unwrap() {
            Command::Rotate { angle_deg, axis, center, .. } => {
                assert!((angle_deg - 90.0).abs() < 1e-9);
                assert_eq!(axis, DVec3::Z);
                assert_eq!(center, Some(DVec3::new(1.0, 2.0, 0.0)));
            }
            other => panic!("expected rotate, got {other:?}"),
        }
    }

    #[test]
    fn commit_scale_uniform_with_center() {
        match commit_command(&drag(Handle::ScaleUniform, 40.0, 80.0)).unwrap() {
            Command::Scale { factors, center, .. } => {
                assert!((factors - DVec3::splat(2.0)).length() < 1e-12);
                assert_eq!(center, Some(DVec3::new(1.0, 2.0, 0.0)));
            }
            other => panic!("expected scale, got {other:?}"),
        }
    }

    #[test]
    fn no_op_drags_emit_nothing() {
        assert!(commit_command(&drag(Handle::MoveX, 2.0, 2.0)).is_none());
        assert!(commit_command(&drag(Handle::RotZ, 1.0, 1.0)).is_none());
        assert!(commit_command(&drag(Handle::ScaleUniform, 50.0, 50.0)).is_none());
        assert!(commit_command(&drag(Handle::ScaleUniform, 1.0, 80.0)).is_none());
    }
}
