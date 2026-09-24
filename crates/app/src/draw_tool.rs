// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

use glam::DVec3;

/// Rhino-style interactive drawing: type a bare verb ("rect"), pick points on
/// the canvas, the tool emits the equivalent command string — so click-drawn
/// geometry goes through the exact same substrate as typed or LLM commands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Verb {
    Line,
    Polyline,
    Rect,
    Circle,
    Polygon,
}

/// Default polygon side count; overridable by typing an integer before the
/// center click.
const POLYGON_DEFAULT_SIDES: usize = 6;

#[derive(Default)]
pub struct DrawTool {
    state: Option<(Verb, Vec<DVec3>)>,
    /// Typed numeric buffer for precise input ("5.2,3", "@2,3", "5"); shown
    /// in the prompt overlay, resolved by the app layer on Enter.
    input: String,
    /// Live side count for the polygon tool; set from the typed buffer before
    /// the center is picked (see `sync_polygon_sides`).
    sides: usize,
}

/// World-space radius around the polyline's first point that snap-closes the
/// loop on click. Rhino's close is forgiving; keep it generous and paired with
/// the on-screen highlight ([`DrawTool::close_target`]) so the affordance is
/// visible, not guessed.
const CLOSE_SNAP: f64 = 0.5;

/// Millimeter rounding for emitted command strings — points arrive already
/// resolved (osnap hit or grid snap), this only strips float noise.
fn num(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

fn fmt(p: DVec3) -> String {
    if p.z.abs() < 1e-9 {
        format!("{},{}", num(p.x), num(p.y))
    } else {
        format!("{},{},{}", num(p.x), num(p.y), num(p.z))
    }
}

impl DrawTool {
    pub fn active(&self) -> bool {
        self.state.is_some()
    }

    /// Start picking if `line` is a bare drawing verb. Returns true when consumed.
    pub fn try_start(&mut self, line: &str) -> bool {
        let verb = match line.trim() {
            "line" => Verb::Line,
            "polyline" | "pline" => Verb::Polyline,
            "rect" | "rectangle" => Verb::Rect,
            "circle" => Verb::Circle,
            "polygon" => Verb::Polygon,
            _ => return false,
        };
        if verb == Verb::Polygon {
            self.sides = POLYGON_DEFAULT_SIDES;
        }
        self.state = Some((verb, Vec::new()));
        self.input.clear();
        true
    }

    /// Before the polygon's center is picked, a typed integer sets the side
    /// count (clamped ≥3) and clears the buffer, so "8 <click center>" draws an
    /// octagon. No-op for a non-integer buffer.
    fn sync_polygon_sides(&mut self) {
        if let Ok(n) = self.input.trim().parse::<usize>() {
            self.sides = n.max(3);
            self.input.clear();
        }
    }

    pub fn cancel(&mut self) {
        self.state = None;
        self.input.clear();
    }

    /// Last picked point — anchor for relative/distance/ortho input.
    pub fn last_point(&self) -> Option<DVec3> {
        self.state.as_ref()?.1.last().copied()
    }

    /// Feed one typed character to the numeric buffer. Returns true when
    /// consumed (tool active and the char is numeric-input material).
    pub fn push_input(&mut self, c: char) -> bool {
        if self.state.is_some() && crate::precise::accepts_char(c) {
            self.input.push(c);
            true
        } else {
            false
        }
    }

    /// Backspace: returns true when it ate a buffered char.
    pub fn pop_input(&mut self) -> bool {
        self.input.pop().is_some()
    }

    /// Take (and clear) the typed buffer; empty when nothing was typed.
    pub fn take_input(&mut self) -> String {
        std::mem::take(&mut self.input)
    }

    pub fn prompt(&self) -> Option<String> {
        let (verb, points) = self.state.as_ref()?;
        let base = match (verb, points.len()) {
            (Verb::Line, 0) => "line: pick start point (Esc cancels)".into(),
            (Verb::Line, _) => "line: pick end point".into(),
            (Verb::Rect, 0) => "rect: pick first corner (Esc cancels)".into(),
            (Verb::Rect, _) => "rect: pick opposite corner".into(),
            (Verb::Circle, 0) => "circle: pick center (Esc cancels)".into(),
            (Verb::Circle, _) => "circle: pick a point on the circle".into(),
            (Verb::Polygon, 0) => format!(
                "polygon: {} sides — pick center (type N for sides, Esc cancels)",
                self.sides
            ),
            (Verb::Polygon, _) => "polygon: pick a point on the circumradius".into(),
            (Verb::Polyline, 0) => "polyline: pick first point (Esc cancels)".into(),
            (Verb::Polyline, n) => format!(
                "polyline: pick next point ({n} so far — Enter finishes, C or click start closes)"
            ),
        };
        Some(if self.input.is_empty() {
            base
        } else {
            format!("{base}  |  typed: {}_", self.input)
        })
    }

    /// Register a canvas pick. `world` must already be snap-resolved by the
    /// caller (osnap hit or grid). Returns the finished command string when
    /// the shape is complete.
    pub fn on_click(&mut self, world: DVec3) -> Option<String> {
        let (verb, mut points) = self.state.take()?;
        match verb {
            Verb::Line => {
                if points.is_empty() {
                    points.push(world);
                    self.state = Some((verb, points));
                    None
                } else {
                    Some(format!("line {} {}", fmt(points[0]), fmt(world)))
                }
            }
            Verb::Rect => {
                if points.is_empty() {
                    points.push(world);
                    self.state = Some((verb, points));
                    None
                } else {
                    let a = points[0];
                    let corner = a.min(world);
                    let size = (world - a).abs();
                    if size.x < 1e-9 || size.y < 1e-9 {
                        // zero-area drag; keep waiting
                        self.state = Some((verb, points));
                        return None;
                    }
                    Some(format!("rect {} {} {}", fmt(corner), num(size.x), num(size.y)))
                }
            }
            Verb::Circle => {
                if points.is_empty() {
                    points.push(world);
                    self.state = Some((verb, points));
                    None
                } else {
                    let r = num(points[0].distance(world));
                    if r < 1e-9 {
                        self.state = Some((verb, points));
                        return None;
                    }
                    Some(format!("circle {} {r}", fmt(points[0])))
                }
            }
            Verb::Polygon => {
                if points.is_empty() {
                    // A number typed before the center sets the side count.
                    self.sync_polygon_sides();
                    points.push(world);
                    self.state = Some((verb, points));
                    None
                } else {
                    let r = num(points[0].distance(world));
                    if r < 1e-9 {
                        self.state = Some((verb, points));
                        return None;
                    }
                    Some(format!("polygon {} {r} {}", fmt(points[0]), self.sides))
                }
            }
            Verb::Polyline => {
                // Clicking near the first point closes the loop.
                if points.len() >= 3 && points[0].distance(world) < CLOSE_SNAP {
                    let pts: Vec<String> = points.iter().map(|p| fmt(*p)).collect();
                    return Some(format!("polyline {} closed", pts.join(" ")));
                }
                points.push(world);
                self.state = Some((verb, points));
                None
            }
        }
    }

    /// Enter finishes an open polyline (needs at least 2 points).
    pub fn on_enter(&mut self) -> Option<String> {
        if let Some((Verb::Polyline, points)) = &self.state
            && points.len() >= 2
        {
            let pts: Vec<String> = points.iter().map(|p| fmt(*p)).collect();
            self.state = None;
            return Some(format!("polyline {}", pts.join(" ")));
        }
        None
    }

    /// `C` closes an open polyline into a loop (Rhino's Close). Needs ≥3 points;
    /// a no-op otherwise so the keypress falls through harmlessly.
    pub fn on_close(&mut self) -> Option<String> {
        if let Some((Verb::Polyline, points)) = &self.state
            && points.len() >= 3
        {
            let pts: Vec<String> = points.iter().map(|p| fmt(*p)).collect();
            self.state = None;
            return Some(format!("polyline {} closed", pts.join(" ")));
        }
        None
    }

    /// The polyline's first point once closing is possible (≥3 picked), so the
    /// canvas can highlight the snap-close target. `None` when not a polyline or
    /// too few points to close.
    pub fn close_target(&self) -> Option<DVec3> {
        match &self.state {
            Some((Verb::Polyline, points)) if points.len() >= 3 => points.first().copied(),
            _ => None,
        }
    }

    /// Ghost geometry to overlay: polylines in world space, given the current
    /// (already snap-resolved) cursor position.
    pub fn preview(&self, cursor: Option<DVec3>) -> Vec<Vec<DVec3>> {
        let Some((verb, points)) = &self.state else {
            return Vec::new();
        };
        match verb {
            Verb::Line | Verb::Polyline => {
                let mut strip = points.clone();
                if let Some(c) = cursor {
                    strip.push(c);
                }
                if strip.len() >= 2 { vec![strip] } else { Vec::new() }
            }
            Verb::Rect => match (points.first(), cursor) {
                (Some(&a), Some(b)) => vec![vec![
                    a,
                    DVec3::new(b.x, a.y, a.z),
                    b,
                    DVec3::new(a.x, b.y, a.z),
                    a,
                ]],
                _ => Vec::new(),
            },
            Verb::Circle => match (points.first(), cursor) {
                (Some(&c), Some(edge)) => {
                    let r = c.distance(edge);
                    if r < 1e-9 {
                        return Vec::new();
                    }
                    let n = 48;
                    let mut strip: Vec<DVec3> = (0..=n)
                        .map(|i| {
                            let t = std::f64::consts::TAU * (i as f64) / (n as f64);
                            c + DVec3::new(r * t.cos(), r * t.sin(), 0.0)
                        })
                        .collect();
                    strip.push(strip[0]);
                    // Rhino draws the center→cursor radius line while dragging so
                    // the center stays visible for context; overlay it too.
                    vec![strip, vec![c, edge]]
                }
                _ => Vec::new(),
            },
            Verb::Polygon => match (points.first(), cursor) {
                (Some(&c), Some(edge)) => {
                    let r = c.distance(edge);
                    if r < 1e-9 {
                        return Vec::new();
                    }
                    // The circumradius vertex sits under the cursor; step by the
                    // side count and close the loop by repeating the first point.
                    let n = self.sides.max(3);
                    let a0 = (edge.y - c.y).atan2(edge.x - c.x);
                    let mut strip: Vec<DVec3> = (0..n)
                        .map(|i| {
                            let t = a0 + std::f64::consts::TAU * (i as f64) / (n as f64);
                            c + DVec3::new(r * t.cos(), r * t.sin(), 0.0)
                        })
                        .collect();
                    strip.push(strip[0]);
                    vec![strip]
                }
                _ => Vec::new(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_two_clicks() {
        let mut t = DrawTool::default();
        assert!(t.try_start("rect"));
        // Caller resolves snapping; tool only strips float noise (mm rounding).
        assert!(t.on_click(DVec3::new(6.0000004, 4.0, 0.0)).is_none());
        let cmd = t.on_click(DVec3::new(0.0, 0.0, 0.0)).unwrap();
        assert_eq!(cmd, "rect 0,0 6 4"); // normalized to min corner
        assert!(!t.active());
    }

    #[test]
    fn osnap_precision_survives_mm() {
        // An osnap hit on a midpoint like 2.05 must NOT get grid-rounded away.
        let mut t = DrawTool::default();
        t.try_start("line");
        t.on_click(DVec3::new(2.05, 3.15, 0.0));
        let cmd = t.on_click(DVec3::new(7.125, 0.0, 0.0)).unwrap();
        assert_eq!(cmd, "line 2.05,3.15 7.125,0");
    }

    #[test]
    fn circle_center_edge() {
        let mut t = DrawTool::default();
        t.try_start("circle");
        t.on_click(DVec3::new(2.0, 2.0, 0.0));
        let cmd = t.on_click(DVec3::new(5.0, 2.0, 0.0)).unwrap();
        assert_eq!(cmd, "circle 2,2 3");
    }

    #[test]
    fn circle_preview_includes_radius_line() {
        // While dragging, the ghost is the ring PLUS a center→cursor radius
        // line (Rhino behavior) so the center stays visible for context.
        let mut t = DrawTool::default();
        t.try_start("circle");
        let center = DVec3::new(2.0, 2.0, 0.0);
        t.on_click(center);
        let edge = DVec3::new(5.0, 2.0, 0.0);
        let ghost = t.preview(Some(edge));
        assert_eq!(ghost.len(), 2, "ring + radius line");
        // The radius line is the 2-point segment from center to the cursor.
        let radius = ghost.iter().find(|p| p.len() == 2).expect("radius segment");
        assert_eq!(radius[0], center);
        assert_eq!(radius[1], edge);
    }

    #[test]
    fn polyline_click_near_start_closes() {
        let mut t = DrawTool::default();
        t.try_start("polyline");
        for p in [(0.0, 0.0), (5.0, 0.0), (5.0, 5.0)] {
            assert!(t.on_click(DVec3::new(p.0, p.1, 0.0)).is_none());
        }
        let cmd = t.on_click(DVec3::new(0.05, 0.05, 0.0)).unwrap();
        assert_eq!(cmd, "polyline 0,0 5,0 5,5 closed");
    }

    #[test]
    fn polygon_two_clicks_emits_verb() {
        let mut t = DrawTool::default();
        assert!(t.try_start("polygon"));
        assert!(t.on_click(DVec3::new(0.0, 0.0, 0.0)).is_none());
        let cmd = t.on_click(DVec3::new(5.0, 0.0, 0.0)).unwrap();
        assert_eq!(cmd, "polygon 0,0 5 6"); // default 6 sides
        assert!(!t.active());
    }

    #[test]
    fn polygon_sides_override_and_clamp() {
        let mut t = DrawTool::default();
        t.try_start("polygon");
        // Type "8" before the center → octagon.
        for c in "8".chars() {
            assert!(t.push_input(c));
        }
        t.on_click(DVec3::new(0.0, 0.0, 0.0));
        assert!(t.on_click(DVec3::new(5.0, 0.0, 0.0)).unwrap().ends_with(" 8"));
        // A count below 3 clamps up to 3.
        t.try_start("polygon");
        t.push_input('2');
        t.on_click(DVec3::new(0.0, 0.0, 0.0));
        assert!(t.on_click(DVec3::new(5.0, 0.0, 0.0)).unwrap().ends_with(" 3"));
    }

    #[test]
    fn polygon_preview_closes_with_sides_plus_one() {
        let mut t = DrawTool::default();
        t.try_start("polygon"); // 6 sides
        t.on_click(DVec3::new(0.0, 0.0, 0.0));
        let ghost = t.preview(Some(DVec3::new(5.0, 0.0, 0.0)));
        assert_eq!(ghost.len(), 1);
        let ring = &ghost[0];
        assert_eq!(ring.len(), 7); // 6 vertices + closing repeat
        assert_eq!(ring.first(), ring.last());
    }

    #[test]
    fn polygon_with_args_not_consumed() {
        let mut t = DrawTool::default();
        assert!(!t.try_start("polygon 0,0 5 6"));
    }

    #[test]
    fn polyline_c_key_closes_loop() {
        let mut t = DrawTool::default();
        t.try_start("polyline");
        assert!(t.close_target().is_none(), "no close target before 3 points");
        for p in [(0.0, 0.0), (5.0, 0.0), (5.0, 5.0)] {
            t.on_click(DVec3::new(p.0, p.1, 0.0));
        }
        // Close target is the first point; C emits the closed polyline.
        assert_eq!(t.close_target(), Some(DVec3::new(0.0, 0.0, 0.0)));
        assert_eq!(t.on_close().unwrap(), "polyline 0,0 5,0 5,5 closed");
        assert!(!t.active(), "closing finishes the tool");
    }

    #[test]
    fn polyline_c_key_is_noop_under_three_points() {
        let mut t = DrawTool::default();
        t.try_start("polyline");
        t.on_click(DVec3::new(0.0, 0.0, 0.0));
        t.on_click(DVec3::new(3.0, 0.0, 0.0));
        assert!(t.on_close().is_none(), "need ≥3 points to close");
        assert!(t.active(), "a no-op close leaves the tool running");
    }

    #[test]
    fn polyline_enter_finishes_open() {
        let mut t = DrawTool::default();
        t.try_start("line"); // wrong verb first: line has no enter-finish
        assert!(t.on_enter().is_none());
        t.cancel();
        t.try_start("polyline");
        t.on_click(DVec3::new(0.0, 0.0, 0.0));
        t.on_click(DVec3::new(3.0, 0.0, 0.0));
        assert_eq!(t.on_enter().unwrap(), "polyline 0,0 3,0");
    }

    #[test]
    fn input_buffer_filters_edits_and_clears() {
        let mut t = DrawTool::default();
        assert!(!t.push_input('5'), "inactive tool consumes nothing");
        t.try_start("line");
        for c in "5.2,3".chars() {
            assert!(t.push_input(c));
        }
        assert!(!t.push_input('x'), "letters are not numeric input");
        assert!(t.prompt().unwrap().contains("typed: 5.2,3_"));
        assert!(t.pop_input());
        assert_eq!(t.take_input(), "5.2,");
        assert!(!t.pop_input(), "buffer already empty");
        // buffer never leaks across tool runs
        t.push_input('7');
        t.cancel();
        t.try_start("rect");
        assert_eq!(t.take_input(), "");
    }

    #[test]
    fn last_point_tracks_picks() {
        let mut t = DrawTool::default();
        assert_eq!(t.last_point(), None);
        t.try_start("polyline");
        assert_eq!(t.last_point(), None);
        t.on_click(DVec3::new(1.0, 2.0, 0.0));
        t.on_click(DVec3::new(4.0, 2.0, 0.0));
        assert_eq!(t.last_point(), Some(DVec3::new(4.0, 2.0, 0.0)));
    }

    #[test]
    fn typed_commands_with_args_not_consumed() {
        let mut t = DrawTool::default();
        assert!(!t.try_start("rect 0,0,0 4 6"));
        assert!(!t.try_start("box"));
    }
}
