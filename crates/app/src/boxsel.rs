// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Drag-box selection: pure screen-space geometry, Rhino convention.
//!
//! Left→right drag = window (only objects fully inside), right→left =
//! crossing (touching counts). The caller projects an object's actual geometry
//! (a curve's tessellated polyline) to screen points; this module compares
//! points/segments against the drag rect, so it is unit-testable without a
//! camera or a document.

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

/// Does segment `p0→p1` properly intersect segment `p2→p3`? Orientation test
/// (with a collinear-on-segment fallback). Pure screen-space.
fn segs_cross(p0: egui::Pos2, p1: egui::Pos2, p2: egui::Pos2, p3: egui::Pos2) -> bool {
    fn orient(a: egui::Pos2, b: egui::Pos2, c: egui::Pos2) -> f32 {
        (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
    }
    // `c` lies within segment `a→b`'s bbox (used only when already collinear).
    fn on_seg(a: egui::Pos2, b: egui::Pos2, c: egui::Pos2) -> bool {
        c.x >= a.x.min(b.x) && c.x <= a.x.max(b.x) && c.y >= a.y.min(b.y) && c.y <= a.y.max(b.y)
    }
    let d0 = orient(p2, p3, p0);
    let d1 = orient(p2, p3, p1);
    let d2 = orient(p0, p1, p2);
    let d3 = orient(p0, p1, p3);
    // Strict straddle on BOTH segments → a genuine crossing.
    if (d0 > 0.0) != (d1 > 0.0)
        && (d2 > 0.0) != (d3 > 0.0)
        && (d0 != 0.0 || d1 != 0.0)
        && (d2 != 0.0 || d3 != 0.0)
    {
        return true;
    }
    // Collinear touch: an endpoint lies on the other segment.
    (d0 == 0.0 && on_seg(p2, p3, p0))
        || (d1 == 0.0 && on_seg(p2, p3, p1))
        || (d2 == 0.0 && on_seg(p0, p1, p2))
        || (d3 == 0.0 && on_seg(p0, p1, p3))
}

/// Does segment `a→b` touch axis-aligned `r`? True if either endpoint is inside,
/// or the segment crosses any of the rect's four edges (catches a segment that
/// passes straight THROUGH with both endpoints outside).
fn seg_hits_rect(a: egui::Pos2, b: egui::Pos2, r: egui::Rect) -> bool {
    if r.contains(a) || r.contains(b) {
        return true;
    }
    let tl = r.min;
    let tr = egui::pos2(r.max.x, r.min.y);
    let br = r.max;
    let bl = egui::pos2(r.min.x, r.max.y);
    segs_cross(a, b, tl, tr)
        || segs_cross(a, b, tr, br)
        || segs_cross(a, b, br, bl)
        || segs_cross(a, b, bl, tl)
}

/// Does a projected polyline match the drag box under `mode`? Tests the ACTUAL
/// geometry, not its AABB — so a diagonal line is NOT selected by a box that
/// merely overlaps the corner of its bounding rectangle. `pts` are the curve's
/// tessellated points projected to screen; `None` entries fall off-camera
/// (never inside, and break a window's full-enclosure requirement).
///
/// - Window: EVERY point must project on-screen AND lie inside `drag`.
/// - Crossing: ANY point inside `drag`, OR any on-screen segment touches it.
pub fn box_select_polyline(pts: &[Option<egui::Pos2>], drag: egui::Rect, mode: BoxMode) -> bool {
    if pts.is_empty() {
        return false;
    }
    match mode {
        BoxMode::Window => pts.iter().all(|p| p.is_some_and(|q| drag.contains(q))),
        BoxMode::Crossing => {
            if pts.iter().any(|p| p.is_some_and(|q| drag.contains(q))) {
                return true;
            }
            pts.windows(2).any(|w| match (w[0], w[1]) {
                (Some(a), Some(b)) => seg_hits_rect(a, b, drag),
                _ => false,
            })
        }
    }
}

/// Screen-space distance from `p` to segment `a→b` (0 if the foot of the
/// perpendicular lies on the segment, else the distance to the nearer endpoint).
fn dist_point_seg(p: egui::Pos2, a: egui::Pos2, b: egui::Pos2) -> f32 {
    let ab = b - a;
    let len_sq = ab.length_sq();
    let t = if len_sq <= f32::EPSILON {
        0.0
    } else {
        ((p - a).dot(ab) / len_sq).clamp(0.0, 1.0)
    };
    (p - (a + ab * t)).length()
}

/// Smallest screen-space distance from `pos` to a projected polyline — used by
/// the single-click PICK so a curve is hit only when the click lands within a
/// few pixels of the ACTUAL drawn line, not anywhere inside its bounding box.
/// `pts` are the curve's tessellated points projected to screen (`None` = a
/// point that fell off-camera, skipped). Returns `None` when no segment or point
/// is on-screen. A straight line tessellates to 2 points, so the segment test is
/// what makes a mid-line click register.
pub fn dist_to_polyline(pts: &[Option<egui::Pos2>], pos: egui::Pos2) -> Option<f32> {
    let mut best: Option<f32> = None;
    for w in pts.windows(2) {
        if let (Some(a), Some(b)) = (w[0], w[1]) {
            let d = dist_point_seg(pos, a, b);
            best = Some(best.map_or(d, |m| m.min(d)));
        }
    }
    if best.is_none() {
        // No on-screen segment (e.g. a single point): fall back to point distance.
        for p in pts.iter().flatten() {
            let d = (*p - pos).length();
            best = Some(best.map_or(d, |m| m.min(d)));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> egui::Rect {
        egui::Rect::from_min_max(egui::pos2(x0, y0), egui::pos2(x1, y1))
    }

    #[test]
    fn dist_to_polyline_measures_to_the_segment_not_the_bbox() {
        // A diagonal line from (0,0) to (100,100). A click at (90,10) is far from
        // the line (~56px) even though it's INSIDE the line's bounding box — this
        // is exactly the empty-click-near-a-line case that must NOT register.
        let line = [Some(egui::pos2(0.0, 0.0)), Some(egui::pos2(100.0, 100.0))];
        let d_far = dist_to_polyline(&line, egui::pos2(90.0, 10.0)).unwrap();
        assert!(d_far > 50.0, "click in the bbox corner is far from the line: {d_far}");
        // A click right on the middle of the line registers ~0.
        let d_on = dist_to_polyline(&line, egui::pos2(50.0, 50.0)).unwrap();
        assert!(d_on < 0.001, "click on the line is ~0: {d_on}");
        // A few px off the mid-line is a small distance (within a pick tolerance).
        let d_near = dist_to_polyline(&line, egui::pos2(52.0, 48.0)).unwrap();
        assert!(d_near < 3.0, "click 2px off the line: {d_near}");
        // Off-screen points are skipped.
        assert!(dist_to_polyline(&[None, None], egui::pos2(0.0, 0.0)).is_none());
    }

    #[test]
    fn direction_sets_mode() {
        assert_eq!(mode(egui::pos2(10.0, 10.0), egui::pos2(50.0, 40.0)), BoxMode::Window);
        assert_eq!(mode(egui::pos2(50.0, 10.0), egui::pos2(10.0, 40.0)), BoxMode::Crossing);
        // Pure vertical drag counts as window.
        assert_eq!(mode(egui::pos2(30.0, 10.0), egui::pos2(30.0, 40.0)), BoxMode::Window);
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

    fn p(x: f32, y: f32) -> Option<egui::Pos2> {
        Some(egui::pos2(x, y))
    }

    #[test]
    fn diagonal_line_in_empty_corner_is_not_selected() {
        // A diagonal line from (0,0) to (100,100). Its AABB is the whole
        // 0..100 square, but the line only passes through the main diagonal.
        let line = vec![p(0.0, 0.0), p(100.0, 100.0)];
        // A box in the TOP-RIGHT corner (near (80,10)) overlaps the AABB but the
        // line is nowhere near it → must NOT be selected (the reported bug).
        let corner = rect(70.0, 5.0, 95.0, 25.0);
        assert!(!box_select_polyline(&line, corner, BoxMode::Crossing), "empty corner");
        assert!(!box_select_polyline(&line, corner, BoxMode::Window), "empty corner window");
        // A box ON the diagonal (around (50,50)) → crossing hit.
        let on_line = rect(40.0, 40.0, 60.0, 60.0);
        assert!(box_select_polyline(&line, on_line, BoxMode::Crossing), "box on the line");
    }

    #[test]
    fn crossing_catches_line_passing_straight_through() {
        // Horizontal line crossing a box with BOTH endpoints outside it.
        let line = vec![p(0.0, 30.0), p(100.0, 30.0)];
        let drag = rect(40.0, 10.0, 60.0, 50.0);
        assert!(box_select_polyline(&line, drag, BoxMode::Crossing), "passes through");
        // Window requires full enclosure — endpoints are outside, so no.
        assert!(!box_select_polyline(&line, drag, BoxMode::Window), "not enclosed");
    }

    #[test]
    fn window_requires_whole_polyline_inside() {
        let line = vec![p(15.0, 15.0), p(25.0, 25.0), p(35.0, 20.0)];
        let encloses = rect(10.0, 10.0, 40.0, 40.0);
        assert!(box_select_polyline(&line, encloses, BoxMode::Window), "fully inside");
        let partial = rect(10.0, 10.0, 30.0, 30.0); // last vertex (35,20) outside
        assert!(!box_select_polyline(&line, partial, BoxMode::Window), "one vertex out");
        assert!(box_select_polyline(&line, partial, BoxMode::Crossing), "crossing still hits");
    }

    #[test]
    fn offscreen_point_breaks_window_but_crossing_uses_onscreen_segments() {
        // Middle vertex off-camera (None): window fails; crossing still tests the
        // on-screen segments (here the first point is inside the box).
        let line = vec![p(20.0, 20.0), None, p(90.0, 90.0)];
        let drag = rect(10.0, 10.0, 40.0, 40.0);
        assert!(!box_select_polyline(&line, drag, BoxMode::Window), "offscreen breaks window");
        assert!(box_select_polyline(&line, drag, BoxMode::Crossing), "inside point hits");
    }
}
