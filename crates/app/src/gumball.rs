// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Gumball transform gizmo: shown at the selection's combined AABB center.
//! Arrows drag-move along X/Y/Z, three rings (X red, Y green, Z blue, the
//! SketchUp axis colours) rotate about their axis through the gizmo centre,
//! the center square scales uniformly. Drags preview as a ghost wireframe; on release ONE
//! substrate command (move/rotate/scale with explicit center) is emitted so
//! the op-log stays the single source of truth and undo works.

use glam::{DMat4, DVec3, Mat4};
use itsjustcad_commands::{Command, Selector};
use itsjustcad_doc::{Document, ObjectId};

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
    RotX,
    RotY,
    RotZ,
    ScaleUniform,
}

const RINGS: [(Handle, DVec3); 3] = [
    (Handle::RotX, DVec3::X),
    (Handle::RotY, DVec3::Y),
    (Handle::RotZ, DVec3::Z),
];

impl Handle {
    fn axis(self) -> Option<DVec3> {
        match self {
            Handle::MoveX => Some(DVec3::X),
            Handle::MoveY => Some(DVec3::Y),
            Handle::MoveZ => Some(DVec3::Z),
            _ => None,
        }
    }

    fn rot_axis(self) -> Option<DVec3> {
        match self {
            Handle::RotX => Some(DVec3::X),
            Handle::RotY => Some(DVec3::Y),
            Handle::RotZ => Some(DVec3::Z),
            _ => None,
        }
    }

    /// SketchUp axis colour (red X, green Y, blue Z); grey for scale.
    fn color(self) -> egui::Color32 {
        match self {
            Handle::MoveX | Handle::RotX => egui::Color32::from_rgb(225, 85, 85),
            Handle::MoveY | Handle::RotY => egui::Color32::from_rgb(110, 200, 110),
            Handle::MoveZ | Handle::RotZ => egui::Color32::from_rgb(95, 145, 255),
            Handle::ScaleUniform => egui::Color32::from_rgb(200, 200, 200),
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

        let ids = gumball_targets(doc);
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
        // Rings share one world radius: the least-foreshortened axis gives the
        // true px scale, so the rings stay ~RING_PX from any view angle.
        let ring_radius = [DVec3::X, DVec3::Y, DVec3::Z]
            .into_iter()
            .filter_map(axis_len)
            .reduce(f64::min)
            .map(|l| l * (RING_PX / AXIS_PX) as f64);

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
            // Nearest ring wins where rings cross (screen-space, so an
            // edge-on ring, a line through the centre, still hits).
            let r = ring_radius?;
            RINGS
                .iter()
                .filter_map(|&(handle, axis)| {
                    let pts: Vec<egui::Pos2> = ring_points(center, axis, r, RING_SEGMENTS)
                        .iter()
                        .filter_map(|w| project(*w))
                        .collect();
                    let d = pts
                        .windows(2)
                        .map(|w| dist_point_segment(pos, w[0], w[1]))
                        .fold(f32::INFINITY, f32::min);
                    (d <= HIT_PX).then_some((handle, d))
                })
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(h, _)| h)
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
            let color = colw(*handle, handle.color());
            if let (Some(a), Some(b)) = (project(center), project(center + *axis * *len)) {
                painter.line_segment([a, b], egui::Stroke::new(2.0, color));
                painter.circle_filled(b, 4.0, color);
            }
        }
        if let Some(r) = ring_radius {
            for (handle, axis) in RINGS {
                // While dragging, only the active ring (the protractor) shows.
                if self.drag.as_ref().is_some_and(|d| d.handle != handle) {
                    continue;
                }
                let color = colw(handle, handle.color());
                let pts: Vec<egui::Pos2> = ring_points(center, axis, r, RING_SEGMENTS)
                    .iter()
                    .filter_map(|w| project(*w))
                    .collect();
                for pair in pts.windows(2) {
                    painter.line_segment([pair[0], pair[1]], egui::Stroke::new(1.5, color));
                }
            }
        }
        painter.rect_filled(
            egui::Rect::from_center_size(center_px, egui::Vec2::splat(CENTER_PX * 2.0)),
            1.0,
            colw(Handle::ScaleUniform, Handle::ScaleUniform.color()),
        );

        // -- ghost preview + readout while dragging --
        if let Some(drag) = &self.drag {
            let m = pending_matrix(drag);
            let stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(255, 210, 80));
            for id in &drag.ids {
                if let Some(obj) = doc.get(*id) {
                    let bb = doc.geometry_aabb(&obj.geometry);
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

/// The selected objects the gumball manipulates — none (gumball hidden) when
/// the selection holds a generated building: it is a parametric output of its
/// subdivision link, driven by the building parameters. Since #199 no
/// generated child (road, block, lot, building) gets it: only hand-drawn
/// objects and top-level parents.
fn gumball_targets(doc: &Document) -> Vec<ObjectId> {
    let ids: Vec<ObjectId> = doc.objects().filter(|o| doc.selection.contains(&o.id)).map(|o| o.id).collect();
    // Only hand-drawn objects and top-level parents, never generated
    // children (#199).
    if ids
        .iter()
        .any(|id| !itsjustcad_commands::lot::hierarchy::gumball_allowed(doc, *id))
    {
        return Vec::new();
    }
    ids
}

/// Combined AABB center of the given (resolved) selection.
fn selection_center(doc: &Document, ids: &[ObjectId]) -> DVec3 {
    let mut bb = doc.geometry_aabb(&doc.get(ids[0]).expect("selected exists").geometry);
    for id in &ids[1..] {
        bb = bb.union(doc.geometry_aabb(&doc.get(*id).expect("selected exists").geometry));
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
        Handle::RotX | Handle::RotY | Handle::RotZ => {
            let axis = handle.rot_axis().expect("ring handle");
            let (ro, rd) = crate::app::screen_ray(view_proj, rect, pos);
            let p = ray_plane(center, axis, ro, rd)?;
            Some(plane_angle(center, axis, p))
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
        Handle::RotX | Handle::RotY | Handle::RotZ => {
            let angle_deg = wrap_angle_deg((drag.current - drag.start).to_degrees());
            (angle_deg.abs() > 1e-6).then_some(Command::Rotate {
                targets,
                angle_deg,
                axis: drag.handle.rot_axis().expect("ring handle"),
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
        Handle::RotX | Handle::RotY | Handle::RotZ => {
            DMat4::from_translation(c)
                * DMat4::from_axis_angle(
                    drag.handle.rot_axis().expect("ring handle"),
                    drag.current - drag.start,
                )
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

fn drag_readout(drag: &DragState, units: itsjustcad_doc::Units) -> String {
    let d = || itsjustcad_doc::format_length(units, drag.current - drag.start);
    match drag.handle {
        Handle::MoveX => format!("dx {}", d()),
        Handle::MoveY => format!("dy {}", d()),
        Handle::MoveZ => format!("dz {}", d()),
        Handle::RotX | Handle::RotY | Handle::RotZ => format!(
            "{} {:.1}°",
            match drag.handle {
                Handle::RotX => "X",
                Handle::RotY => "Y",
                _ => "Z",
            },
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

/// Points on the ring about `axis` (world space, plane through `center`
/// normal to `axis`), closed.
fn ring_points(center: DVec3, axis: DVec3, radius: f64, segments: usize) -> Vec<DVec3> {
    let (u, v) = itsjustcad_guided::rotation_basis(axis);
    (0..=segments)
        .map(|i| {
            let a = i as f64 / segments as f64 * std::f64::consts::TAU;
            center + (u * a.cos() + v * a.sin()) * radius
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

/// Ray vs the plane through `origin` with normal `n` (t > 0). `None` when
/// the ray runs parallel to the plane or points away from it.
pub fn ray_plane(origin: DVec3, n: DVec3, ray_o: DVec3, ray_d: DVec3) -> Option<DVec3> {
    let denom = ray_d.dot(n);
    if denom.abs() < 1e-12 {
        return None;
    }
    let t = (origin - ray_o).dot(n) / denom;
    (t > 0.0).then(|| ray_o + ray_d * t)
}

/// Right-hand angle of `p` about `axis` through `center` (radians, atan2 in
/// the [`itsjustcad_guided::rotation_basis`] of `axis`), so a positive delta
/// matches `Command::Rotate`'s positive angle.
pub fn plane_angle(center: DVec3, axis: DVec3, p: DVec3) -> f64 {
    let (u, v) = itsjustcad_guided::rotation_basis(axis);
    let d = p - center;
    d.dot(v).atan2(d.dot(u))
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
        let p = ray_plane(
            DVec3::new(0.0, 0.0, 1.0),
            DVec3::Z,
            DVec3::new(2.0, 2.0, 11.0),
            DVec3::new(0.0, 0.0, -1.0),
        )
        .unwrap();
        assert!((p - DVec3::new(2.0, 2.0, 1.0)).length() < 1e-12);
        let a = plane_angle(DVec3::new(1.0, 1.0, 1.0), DVec3::Z, p);
        assert!((a - std::f64::consts::FRAC_PI_4).abs() < 1e-12);
        // Side view onto the X ring: the ray hits the plane x = 3.
        let p = ray_plane(
            DVec3::new(3.0, 0.0, 0.0),
            DVec3::X,
            DVec3::new(10.0, 1.0, 1.0),
            -DVec3::X,
        )
        .unwrap();
        assert!((p - DVec3::new(3.0, 1.0, 1.0)).length() < 1e-12);
    }

    #[test]
    fn plane_parallel_or_behind_is_none() {
        assert!(ray_plane(DVec3::ZERO, DVec3::Z, DVec3::new(0.0, 0.0, 1.0), DVec3::X).is_none());
        assert!(ray_plane(DVec3::new(0.0, 0.0, 5.0), DVec3::Z, DVec3::ZERO, -DVec3::Z).is_none());
        assert!(ray_plane(DVec3::ZERO, DVec3::Y, DVec3::new(0.0, 1.0, 0.0), DVec3::X).is_none());
    }

    #[test]
    fn ring_angle_matches_rotate_direction_on_every_axis() {
        // Dragging the pointer from angle 0 to angle θ on a ring must preview
        // and commit the same turn `rotate … <θ> <axis>` applies.
        let c = DVec3::new(1.0, 2.0, 3.0);
        for (_, axis) in RINGS {
            let (u, _) = itsjustcad_guided::rotation_basis(axis);
            let theta = 0.6_f64;
            let moved = DMat4::from_axis_angle(axis, theta).transform_point3(u * 2.0) + c;
            assert!((plane_angle(c, axis, c + u * 2.0)).abs() < 1e-12);
            assert!(
                (plane_angle(c, axis, moved) - theta).abs() < 1e-12,
                "{axis}"
            );
            for p in ring_points(c, axis, 2.0, 12) {
                assert!((p - c).dot(axis).abs() < 1e-12, "ring lies in its plane");
                assert!(((p - c).length() - 2.0).abs() < 1e-12);
            }
        }
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
    fn commit_rotate_x_and_y_rings_use_their_axis() {
        for (handle, axis) in [(Handle::RotX, DVec3::X), (Handle::RotY, DVec3::Y)] {
            let d = drag(handle, 0.0, -std::f64::consts::FRAC_PI_6);
            match commit_command(&d).unwrap() {
                Command::Rotate {
                    angle_deg,
                    axis: a,
                    center,
                    ..
                } => {
                    assert!((angle_deg + 30.0).abs() < 1e-9);
                    assert_eq!(a, axis);
                    assert_eq!(center, Some(DVec3::new(1.0, 2.0, 0.0)));
                }
                other => panic!("expected rotate, got {other:?}"),
            }
            // The ghost turns the same way the command will.
            let m = pending_matrix(&d);
            let want = DMat4::from_translation(d.center)
                * DMat4::from_axis_angle(axis, -std::f64::consts::FRAC_PI_6)
                * DMat4::from_translation(-d.center);
            assert!(m.abs_diff_eq(want, 1e-12));
            assert!(
                drag_readout(&d, itsjustcad_doc::Units::default())
                    .starts_with(if handle == Handle::RotX { "X " } else { "Y " })
            );
        }
    }

    #[test]
    fn commit_rotate_x_logs_and_replays_as_rotate_x_line() {
        // The gumball's X-ring command and the typed `rotate … x about …` line
        // land the selection in the same place, in one undo step.
        let session = || {
            let mut s = itsjustcad_commands::Session::default();
            s.run(itsjustcad_commands::parse("box 0,0,0 2,1,3").unwrap())
                .unwrap();
            s
        };
        let (mut a, mut b) = (session(), session());
        let ids: Vec<ObjectId> = a.doc.objects().map(|o| o.id).collect();
        let undo_before = a.undo_len();
        let d = DragState {
            handle: Handle::RotX,
            center: DVec3::new(1.0, 0.5, 1.5),
            ids,
            start: 0.0,
            current: std::f64::consts::FRAC_PI_2,
        };
        a.run(commit_command(&d).unwrap()).unwrap();
        b.run(itsjustcad_commands::parse("rotate all 90 x about 1,0.5,1.5").unwrap())
            .unwrap();
        let bb = |s: &itsjustcad_commands::Session| {
            s.doc
                .geometry_aabb(&s.doc.objects().next().unwrap().geometry)
        };
        let (ba, bbx) = (bb(&a), bb(&b));
        // 90° about X swaps the 1 (y) by 3 (z) cross-section.
        assert!(((ba.max - ba.min) - DVec3::new(2.0, 3.0, 1.0)).length() < 1e-9);
        assert!(ba.min.abs_diff_eq(bbx.min, 1e-9) && ba.max.abs_diff_eq(bbx.max, 1e-9));
        assert_eq!(a.undo_len(), undo_before + 1, "one undo step");
        a.run(itsjustcad_commands::Command::Undo).unwrap();
        assert!(
            bb(&a).min.abs_diff_eq(DVec3::ZERO, 1e-9),
            "one undo step restores it"
        );
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
    fn no_gumball_on_generated_children_only_the_site_plan() {
        let mut s = itsjustcad_commands::Session::default();
        for line in ["rect 0,0,0 200 150", "siteplan last"] {
            s.run(itsjustcad_commands::parse(line).unwrap()).unwrap();
        }
        let on = |layer: &str| s.doc.objects().find(|o| o.layer == layer).unwrap().id;
        let (building, road) =
            (on(itsjustcad_commands::lot::BUILDINGS_LAYER), on(itsjustcad_commands::lot::ROADS_LAYER));
        let mut doc = s.doc.clone();
        doc.selection = [building].into();
        assert!(gumball_targets(&doc).is_empty(), "no gumball on a generated building");
        doc.selection = [building, road].into();
        assert!(gumball_targets(&doc).is_empty(), "nor on a mixed selection holding one");
        doc.selection = [road].into();
        assert!(gumball_targets(&doc).is_empty(), "nor on a generated road (#199)");
        let site = *doc.subdivision_links.keys().next().unwrap();
        doc.selection = [site].into();
        assert_eq!(gumball_targets(&doc), vec![site], "the site plan keeps it");
    }

    #[test]
    fn no_op_drags_emit_nothing() {
        assert!(commit_command(&drag(Handle::MoveX, 2.0, 2.0)).is_none());
        assert!(commit_command(&drag(Handle::RotZ, 1.0, 1.0)).is_none());
        assert!(commit_command(&drag(Handle::ScaleUniform, 50.0, 50.0)).is_none());
        assert!(commit_command(&drag(Handle::ScaleUniform, 1.0, 80.0)).is_none());
    }
}
