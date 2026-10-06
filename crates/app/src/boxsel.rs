// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Drag-box selection: pure screen-space geometry, Rhino convention.
//!
//! Left→right drag = window (only objects fully inside), right→left =
//! crossing (touching counts). The caller projects object AABBs to screen
//! rects; this module only compares rectangles, so it is unit-testable
//! without a camera or a document.

use itsjustcad_doc::ObjectId;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BoxMode {
    /// Fully-enclosed objects only (drag started left of where it ended).
    Window,
    /// Anything the box touches (drag ended left of where it started).
    Crossing,
}

/// Drag direction → selection mode. A vertical drag (equal x) is a window.
pub fn mode(start: egui::Pos2, end: egui::Pos2) -> BoxMode {
    if end.x >= start.x {
        BoxMode::Window
    } else {
        BoxMode::Crossing
    }
}

/// Pick ONE interior point from a curve's tessellated samples when a drag box
/// catches it — the removal point for trim's "click parts to remove" phase.
/// Each sample is `(screen, world)`; `screen` is `None` when the point projects
/// off-camera (counted as outside). A curve passes when (window) EVERY sample
/// lies inside the box, or (crossing) ANY sample does; the MEDIAN inside sample
/// is returned so the point lands on the boxed portion of the curve. Returns
/// `None` when nothing qualifies. Pure screen-space geometry, like the rest of
/// this module, so it unit-tests without a camera or document.
pub fn box_pick_point(
    samples: &[(Option<egui::Pos2>, glam::DVec3)],
    drag: egui::Rect,
    mode: BoxMode,
) -> Option<glam::DVec3> {
    let inside: Vec<glam::DVec3> = samples
        .iter()
        .filter(|(s, _)| s.is_some_and(|p| drag.contains(p)))
        .map(|(_, w)| *w)
        .collect();
    if inside.is_empty() {
        return None;
    }
    let passes = match mode {
        BoxMode::Crossing => true,
        BoxMode::Window => inside.len() == samples.len(),
    };
    passes.then(|| inside[inside.len() / 2])
}

/// Ids whose projected screen rect matches the drag rect under `mode`.
pub fn box_select(
    items: &[(ObjectId, egui::Rect)],
    drag: egui::Rect,
    mode: BoxMode,
) -> Vec<ObjectId> {
    items
        .iter()
        .filter(|(_, r)| match mode {
            BoxMode::Window => drag.contains_rect(*r),
            BoxMode::Crossing => drag.intersects(*r),
        })
        .map(|(id, _)| *id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> ObjectId {
        ObjectId(uuid::Uuid::from_u128(n))
    }

    fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> egui::Rect {
        egui::Rect::from_min_max(egui::pos2(x0, y0), egui::pos2(x1, y1))
    }

    #[test]
    fn direction_sets_mode() {
        assert_eq!(mode(egui::pos2(10.0, 10.0), egui::pos2(50.0, 40.0)), BoxMode::Window);
        assert_eq!(mode(egui::pos2(50.0, 10.0), egui::pos2(10.0, 40.0)), BoxMode::Crossing);
        // Pure vertical drag counts as window.
        assert_eq!(mode(egui::pos2(30.0, 10.0), egui::pos2(30.0, 40.0)), BoxMode::Window);
    }

    #[test]
    fn window_requires_full_enclosure() {
        let items = vec![
            (id(1), rect(10.0, 10.0, 20.0, 20.0)),  // fully inside
            (id(2), rect(25.0, 25.0, 45.0, 45.0)),  // partially overlapping
            (id(3), rect(100.0, 100.0, 110.0, 110.0)), // outside
        ];
        let drag = rect(0.0, 0.0, 40.0, 40.0);
        assert_eq!(box_select(&items, drag, BoxMode::Window), vec![id(1)]);
    }

    #[test]
    fn crossing_counts_touching() {
        let items = vec![
            (id(1), rect(10.0, 10.0, 20.0, 20.0)),  // fully inside
            (id(2), rect(25.0, 25.0, 45.0, 45.0)),  // partially overlapping
            (id(3), rect(100.0, 100.0, 110.0, 110.0)), // outside
        ];
        let drag = rect(0.0, 0.0, 40.0, 40.0);
        assert_eq!(box_select(&items, drag, BoxMode::Crossing), vec![id(1), id(2)]);
    }

    #[test]
    fn crossing_edge_touch_counts() {
        // Shares only the drag rect's right edge — still a crossing hit.
        let items = vec![(id(1), rect(40.0, 10.0, 60.0, 20.0))];
        let drag = rect(0.0, 0.0, 40.0, 40.0);
        assert_eq!(box_select(&items, drag, BoxMode::Crossing), vec![id(1)]);
        assert!(box_select(&items, drag, BoxMode::Window).is_empty());
    }

    #[test]
    fn empty_drag_selects_nothing_in_window_mode() {
        let items = vec![(id(1), rect(10.0, 10.0, 20.0, 20.0))];
        let drag = rect(30.0, 30.0, 30.0, 30.0);
        assert!(box_select(&items, drag, BoxMode::Window).is_empty());
    }

    // Build (screen, world) samples with screen == world.xy for easy reasoning.
    fn sample(x: f32, y: f32) -> (Option<egui::Pos2>, glam::DVec3) {
        (Some(egui::pos2(x, y)), glam::DVec3::new(x as f64, y as f64, 0.0))
    }

    #[test]
    fn box_pick_point_crossing_returns_median_inside_sample() {
        // Three samples; the box covers the last two → median of {1,2} is index 0
        // of the inside list (len 2 → idx 1) = the second inside sample.
        let samples = vec![sample(5.0, 5.0), sample(20.0, 20.0), sample(30.0, 30.0)];
        let drag = rect(10.0, 10.0, 40.0, 40.0);
        let got = box_pick_point(&samples, drag, BoxMode::Crossing).unwrap();
        // inside = [(20,20),(30,30)]; median idx = 2/2 = 1 → (30,30).
        assert_eq!(got, glam::DVec3::new(30.0, 30.0, 0.0));
    }

    #[test]
    fn box_pick_point_window_requires_all_inside() {
        let samples = vec![sample(15.0, 15.0), sample(50.0, 50.0)];
        let drag = rect(10.0, 10.0, 40.0, 40.0);
        // One sample is outside → window mode rejects the whole curve.
        assert!(box_pick_point(&samples, drag, BoxMode::Window).is_none());
        // Crossing still catches it (one sample inside), returns that sample.
        assert_eq!(
            box_pick_point(&samples, drag, BoxMode::Crossing).unwrap(),
            glam::DVec3::new(15.0, 15.0, 0.0)
        );
    }

    #[test]
    fn box_pick_point_window_all_inside_returns_median() {
        let samples = vec![sample(12.0, 12.0), sample(20.0, 20.0), sample(30.0, 30.0)];
        let drag = rect(10.0, 10.0, 40.0, 40.0);
        // All inside → median idx 3/2 = 1 → (20,20).
        assert_eq!(
            box_pick_point(&samples, drag, BoxMode::Window).unwrap(),
            glam::DVec3::new(20.0, 20.0, 0.0)
        );
    }

    #[test]
    fn box_pick_point_no_sample_inside_is_none() {
        let samples = vec![sample(100.0, 100.0), sample(200.0, 200.0)];
        let drag = rect(10.0, 10.0, 40.0, 40.0);
        assert!(box_pick_point(&samples, drag, BoxMode::Crossing).is_none());
    }

    #[test]
    fn box_pick_point_offscreen_sample_counts_as_outside() {
        // An un-projected (None) sample disqualifies window mode.
        let samples = vec![sample(15.0, 15.0), (None, glam::DVec3::new(9.0, 9.0, 9.0))];
        let drag = rect(10.0, 10.0, 40.0, 40.0);
        assert!(box_pick_point(&samples, drag, BoxMode::Window).is_none());
    }
}
