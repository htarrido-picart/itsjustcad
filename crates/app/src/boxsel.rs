//! Drag-box selection: pure screen-space geometry, Rhino convention.
//!
//! Left→right drag = window (only objects fully inside), right→left =
//! crossing (touching counts). The caller projects object AABBs to screen
//! rects; this module only compares rectangles, so it is unit-testable
//! without a camera or a document.

use mydrafter_doc::ObjectId;

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
}
