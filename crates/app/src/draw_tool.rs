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
}

pub struct DrawTool {
    state: Option<(Verb, Vec<DVec3>)>,
}

/// Snap picks to 10cm so emitted commands stay clean.
fn snap(p: DVec3) -> DVec3 {
    (p * 10.0).round() / 10.0
}

fn fmt(p: DVec3) -> String {
    if p.z.abs() < 1e-9 {
        format!("{},{}", p.x, p.y)
    } else {
        format!("{},{},{}", p.x, p.y, p.z)
    }
}

impl Default for DrawTool {
    fn default() -> Self {
        Self { state: None }
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
            _ => return false,
        };
        self.state = Some((verb, Vec::new()));
        true
    }

    pub fn cancel(&mut self) {
        self.state = None;
    }

    pub fn prompt(&self) -> Option<String> {
        let (verb, points) = self.state.as_ref()?;
        Some(match (verb, points.len()) {
            (Verb::Line, 0) => "line: pick start point (Esc cancels)".into(),
            (Verb::Line, _) => "line: pick end point".into(),
            (Verb::Rect, 0) => "rect: pick first corner (Esc cancels)".into(),
            (Verb::Rect, _) => "rect: pick opposite corner".into(),
            (Verb::Circle, 0) => "circle: pick center (Esc cancels)".into(),
            (Verb::Circle, _) => "circle: pick a point on the circle".into(),
            (Verb::Polyline, 0) => "polyline: pick first point (Esc cancels)".into(),
            (Verb::Polyline, n) => format!(
                "polyline: pick next point ({n} so far — Enter finishes, click near start closes)"
            ),
        })
    }

    /// Register a canvas pick. Returns the finished command string when the
    /// shape is complete.
    pub fn on_click(&mut self, world: DVec3) -> Option<String> {
        let world = snap(world);
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
                    Some(format!("rect {} {} {}", fmt(corner), size.x, size.y))
                }
            }
            Verb::Circle => {
                if points.is_empty() {
                    points.push(world);
                    self.state = Some((verb, points));
                    None
                } else {
                    let r = points[0].distance(world);
                    if r < 1e-9 {
                        self.state = Some((verb, points));
                        return None;
                    }
                    Some(format!("circle {} {r}", fmt(points[0])))
                }
            }
            Verb::Polyline => {
                // Clicking near the first point closes the loop.
                if points.len() >= 3 && points[0].distance(world) < 0.3 {
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

    /// Ghost geometry to overlay: polylines in world space, given the current
    /// cursor position on the ground plane.
    pub fn preview(&self, cursor: Option<DVec3>) -> Vec<Vec<DVec3>> {
        let Some((verb, points)) = &self.state else {
            return Vec::new();
        };
        let cursor = cursor.map(snap);
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
        assert!(t.on_click(DVec3::new(6.02, 4.01, 0.0)).is_none());
        let cmd = t.on_click(DVec3::new(0.0, 0.0, 0.0)).unwrap();
        assert_eq!(cmd, "rect 0,0 6 4"); // normalized to min corner, snapped
        assert!(!t.active());
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
    fn typed_commands_with_args_not_consumed() {
        let mut t = DrawTool::default();
        assert!(!t.try_start("rect 0,0,0 4 6"));
        assert!(!t.try_start("box"));
    }
}
