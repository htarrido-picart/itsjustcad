// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Guided creation + measure verbs: `extrude`, `revolve`, `pipe`, `box`, `arc`,
//! `ellipse`, `helix`, `area`, `volume`, `bbox`, `distance`, plus the multi-curve
//! builders that pick their operand curves interactively — `sweep`, `sweep2`,
//! `blend`, `railrevolve`, `loft`, `circletan`, `linetan`, `lineperp`. Every
//! assembler emits the canonical string the existing parser already accepts.

use super::assemble::{int_at, num_at, obj_at, point_at, points_at, selector};
use super::{Input, ObjFilter, Step, VerbScript, fmt, num};

pub static SCRIPTS: &[VerbScript] = &[
    VerbScript {
        verb: "interpcurve",
        needs_selection: false,
        // The parser requires at least 3 points for an interpolated curve, so
        // the guided flow collects at least 3 before Enter finishes.
        steps: &[Step::PointList { prompt: "Pick curve points (Enter to finish)", min: 3 }],
        assemble: assemble_interpcurve,
    },
    VerbScript {
        verb: "extrude",
        needs_selection: true,
        steps: &[Step::Number { prompt: "Extrusion height", default: Some(1.0) }],
        assemble: assemble_extrude,
    },
    VerbScript {
        verb: "revolve",
        needs_selection: true,
        steps: &[
            Step::PickPoint { prompt: "Axis start" },
            Step::PickPoint { prompt: "Axis end" },
            Step::Number { prompt: "Angle in degrees", default: Some(360.0) },
        ],
        assemble: assemble_revolve,
    },
    VerbScript {
        verb: "pipe",
        needs_selection: true,
        steps: &[Step::Number { prompt: "Pipe radius", default: Some(0.1) }],
        assemble: assemble_pipe,
    },
    VerbScript {
        verb: "box",
        needs_selection: false,
        steps: &[
            Step::PickPoint { prompt: "First base corner" },
            Step::PickPoint { prompt: "Opposite base corner" },
            Step::Number { prompt: "Height", default: Some(1.0) },
        ],
        assemble: assemble_box,
    },
    VerbScript {
        verb: "arc",
        needs_selection: false,
        // Center → start point → end point (Rhino's center-start-end arc). Each
        // point drives a live ghost (see GuidedTool::preview). Points can still
        // be typed as coordinates for precision.
        steps: &[
            Step::PickPoint { prompt: "Center of arc" },
            Step::PickPoint { prompt: "Start point (sets radius + start angle)" },
            Step::PickPoint { prompt: "End point (sets end angle)" },
        ],
        assemble: assemble_arc,
    },
    VerbScript {
        verb: "ellipse",
        needs_selection: false,
        steps: &[
            Step::PickPoint { prompt: "Center" },
            Step::Number { prompt: "X radius", default: Some(1.0) },
            Step::Number { prompt: "Y radius", default: Some(0.5) },
        ],
        assemble: assemble_ellipse,
    },
    VerbScript {
        verb: "helix",
        needs_selection: false,
        steps: &[
            Step::PickPoint { prompt: "Center" },
            Step::Number { prompt: "Radius", default: Some(1.0) },
            Step::Number { prompt: "Height", default: Some(1.0) },
            Step::Integer { prompt: "Turns", default: Some(3) },
        ],
        assemble: assemble_helix,
    },
    VerbScript {
        verb: "area",
        needs_selection: true,
        steps: &[],
        assemble: assemble_area,
    },
    VerbScript {
        verb: "volume",
        needs_selection: true,
        steps: &[],
        assemble: assemble_volume,
    },
    VerbScript {
        verb: "bbox",
        needs_selection: true,
        steps: &[],
        assemble: assemble_bbox,
    },
    VerbScript {
        verb: "distance",
        needs_selection: false,
        steps: &[
            Step::PickPoint { prompt: "First point" },
            Step::PickPoint { prompt: "Second point" },
        ],
        assemble: assemble_distance,
    },
    // ── multi-curve builders (interactive operand picking) ──────────────────
    VerbScript {
        verb: "sweep",
        needs_selection: false,
        steps: &[
            Step::SelectObject { prompt: "Select profile curve", filter: ObjFilter::Curve },
            Step::SelectObject { prompt: "Select rail curve", filter: ObjFilter::Curve },
        ],
        assemble: assemble_sweep,
    },
    VerbScript {
        verb: "sweep2",
        needs_selection: false,
        steps: &[
            Step::SelectObject { prompt: "Select profile curve", filter: ObjFilter::Curve },
            Step::SelectObject { prompt: "Select first rail", filter: ObjFilter::Curve },
            Step::SelectObject { prompt: "Select second rail", filter: ObjFilter::Curve },
        ],
        assemble: assemble_sweep2,
    },
    VerbScript {
        verb: "blend",
        needs_selection: false,
        steps: &[
            Step::SelectObject { prompt: "Select first curve", filter: ObjFilter::Curve },
            Step::SelectObject { prompt: "Select second curve", filter: ObjFilter::Curve },
            Step::Number { prompt: "Bulge", default: Some(1.0) },
        ],
        assemble: assemble_blend,
    },
    VerbScript {
        verb: "railrevolve",
        needs_selection: false,
        steps: &[
            Step::SelectObject { prompt: "Select profile curve", filter: ObjFilter::Curve },
            Step::SelectObject { prompt: "Select rail curve", filter: ObjFilter::Curve },
            Step::PickPoint { prompt: "Axis start" },
            Step::PickPoint { prompt: "Axis end" },
        ],
        assemble: assemble_railrevolve,
    },
    VerbScript {
        verb: "loft",
        needs_selection: true,
        steps: &[],
        assemble: assemble_loft,
    },
    VerbScript {
        verb: "circletan",
        needs_selection: false,
        steps: &[
            Step::SelectObject { prompt: "Select first tangent curve", filter: ObjFilter::Curve },
            Step::SelectObject { prompt: "Select second tangent curve", filter: ObjFilter::Curve },
            Step::Number { prompt: "Radius", default: Some(1.0) },
        ],
        assemble: assemble_circletan,
    },
    VerbScript {
        verb: "linetan",
        needs_selection: false,
        steps: &[
            Step::PickPoint { prompt: "Start point" },
            Step::SelectObject { prompt: "Select tangent curve", filter: ObjFilter::Curve },
        ],
        assemble: assemble_linetan,
    },
    VerbScript {
        verb: "lineperp",
        needs_selection: false,
        steps: &[
            Step::PickPoint { prompt: "Start point" },
            Step::SelectObject { prompt: "Select curve (perpendicular to)", filter: ObjFilter::Curve },
        ],
        assemble: assemble_lineperp,
    },
];

/// `[Points(p1..pn)] -> "interpcurve <p1> <p2> …"` (space-joined). The optional
/// `closed` suffix is deferred (v1 collects an open point list only).
fn assemble_interpcurve(args: &[Input]) -> Result<String, String> {
    let pts = points_at(args, 0, "interpcurve")?;
    let joined = pts.iter().map(|p| fmt(*p)).collect::<Vec<_>>().join(" ");
    Ok(format!("interpcurve {joined}"))
}

/// `[Objects(sel), Num(h)] -> "extrude sel <h>"`
fn assemble_extrude(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "extrude")?;
    let h = num_at(args, 1, "extrude")?;
    Ok(format!("extrude {sel} {}", num(h)))
}

/// `[Objects(sel), Point(start), Point(end), Num(angle)] -> "revolve sel <start> <dir> <angle>"`
/// axis_dir = end - start, formatted with fmt.
fn assemble_revolve(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "revolve")?;
    let axis_start = point_at(args, 1, "revolve")?;
    let axis_end = point_at(args, 2, "revolve")?;
    let angle = num_at(args, 3, "revolve")?;
    let axis_dir = axis_end - axis_start;
    Ok(format!("revolve {sel} {} {} {}", fmt(axis_start), fmt(axis_dir), num(angle)))
}

/// `[Objects(sel), Num(r)] -> "pipe sel <r>"`
fn assemble_pipe(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "pipe")?;
    let r = num_at(args, 1, "pipe")?;
    Ok(format!("pipe {sel} {}", num(r)))
}

/// `[Point(a), Point(b), Num(h)] -> "box <corner> <sx,sy,sz>"`
/// corner = componentwise min of a and b; size = (|dx|, |dy|, height).
fn assemble_box(args: &[Input]) -> Result<String, String> {
    let a = point_at(args, 0, "box")?;
    let b = point_at(args, 1, "box")?;
    let h = num_at(args, 2, "box")?;
    let corner = a.min(b);
    let sx = (a.x - b.x).abs();
    let sy = (a.y - b.y).abs();
    let sz = num(h);
    let size = glam::DVec3::new(sx, sy, sz);
    Ok(format!("box {} {}", fmt(corner), fmt(size)))
}

/// `[Point(center), Num(r), Num(s), Num(e)] -> "arc <center> <r> <s> <e>"`
/// `[Point(center), Point(start), Point(end)] -> "arc <center> <r> <start°> <end°>"`
/// Radius = |start-center|; angles = CCW from +X of the start/end points.
fn assemble_arc(args: &[Input]) -> Result<String, String> {
    let center = point_at(args, 0, "arc")?;
    let start = point_at(args, 1, "arc")?;
    let end = point_at(args, 2, "arc")?;
    let r = center.distance(start);
    let start_deg = (start.y - center.y).atan2(start.x - center.x).to_degrees();
    let end_deg = (end.y - center.y).atan2(end.x - center.x).to_degrees();
    Ok(format!("arc {} {} {} {}", fmt(center), num(r), num(start_deg), num(end_deg)))
}

/// `[Point(center), Num(rx), Num(ry)] -> "ellipse <center> <rx> <ry>"`
fn assemble_ellipse(args: &[Input]) -> Result<String, String> {
    let center = point_at(args, 0, "ellipse")?;
    let rx = num_at(args, 1, "ellipse")?;
    let ry = num_at(args, 2, "ellipse")?;
    Ok(format!("ellipse {} {} {}", fmt(center), num(rx), num(ry)))
}

/// `[Point(center), Num(r), Num(h), Int(turns)] -> "helix <center> <r> <h> <turns>"`
fn assemble_helix(args: &[Input]) -> Result<String, String> {
    let center = point_at(args, 0, "helix")?;
    let r = num_at(args, 1, "helix")?;
    let h = num_at(args, 2, "helix")?;
    let turns = int_at(args, 3, "helix")?;
    Ok(format!("helix {} {} {} {}", fmt(center), num(r), num(h), turns))
}

/// `[Objects(sel)] -> "area sel"`
fn assemble_area(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "area")?;
    Ok(format!("area {sel}"))
}

/// `[Objects(sel)] -> "volume sel"`
fn assemble_volume(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "volume")?;
    Ok(format!("volume {sel}"))
}

/// `[Objects(sel)] -> "bbox sel"`
fn assemble_bbox(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "bbox")?;
    Ok(format!("bbox {sel}"))
}

/// `[Point(a), Point(b)] -> "distance <a> <b>"`
fn assemble_distance(args: &[Input]) -> Result<String, String> {
    let a = point_at(args, 0, "distance")?;
    let b = point_at(args, 1, "distance")?;
    Ok(format!("distance {} {}", fmt(a), fmt(b)))
}

// ── multi-curve builders ────────────────────────────────────────────────────

/// `[#profile, #rail] -> "sweep #profile #rail"`
fn assemble_sweep(args: &[Input]) -> Result<String, String> {
    let profile = obj_at(args, 0, "sweep")?;
    let rail = obj_at(args, 1, "sweep")?;
    Ok(format!("sweep {profile} {rail}"))
}

/// `[#profile, #railA, #railB] -> "sweep2 #profile #railA #railB"`
fn assemble_sweep2(args: &[Input]) -> Result<String, String> {
    let profile = obj_at(args, 0, "sweep2")?;
    let rail_a = obj_at(args, 1, "sweep2")?;
    let rail_b = obj_at(args, 2, "sweep2")?;
    Ok(format!("sweep2 {profile} {rail_a} {rail_b}"))
}

/// `[#a, #b, Num(bulge)] -> "blend #a #b <bulge>"`
fn assemble_blend(args: &[Input]) -> Result<String, String> {
    let a = obj_at(args, 0, "blend")?;
    let b = obj_at(args, 1, "blend")?;
    let bulge = num_at(args, 2, "blend")?;
    Ok(format!("blend {a} {b} {}", num(bulge)))
}

/// `[#profile, #rail, Point(start), Point(end)] -> "railrevolve #profile #rail <start> <dir>"`
/// axis_dir = end - start.
fn assemble_railrevolve(args: &[Input]) -> Result<String, String> {
    let profile = obj_at(args, 0, "railrevolve")?;
    let rail = obj_at(args, 1, "railrevolve")?;
    let axis_start = point_at(args, 2, "railrevolve")?;
    let axis_end = point_at(args, 3, "railrevolve")?;
    Ok(format!("railrevolve {profile} {rail} {} {}", fmt(axis_start), fmt(axis_end - axis_start)))
}

/// `[Objects(sel)] -> "loft sel"` — profiles come from the pre-selection.
fn assemble_loft(args: &[Input]) -> Result<String, String> {
    Ok(format!("loft {}", selector(args, "loft")?))
}

/// `[#a, #b, Num(r)] -> "circletan #a #b <r>"`
fn assemble_circletan(args: &[Input]) -> Result<String, String> {
    let a = obj_at(args, 0, "circletan")?;
    let b = obj_at(args, 1, "circletan")?;
    let r = num_at(args, 2, "circletan")?;
    Ok(format!("circletan {a} {b} {}", num(r)))
}

/// `[Point(from), #curve] -> "linetan <from> #curve"`
fn assemble_linetan(args: &[Input]) -> Result<String, String> {
    let from = point_at(args, 0, "linetan")?;
    let curve = obj_at(args, 1, "linetan")?;
    Ok(format!("linetan {} {curve}", fmt(from)))
}

/// `[Point(from), #curve] -> "lineperp <from> #curve"`
fn assemble_lineperp(args: &[Input]) -> Result<String, String> {
    let from = point_at(args, 0, "lineperp")?;
    let curve = obj_at(args, 1, "lineperp")?;
    Ok(format!("lineperp {} {curve}", fmt(from)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guided::{GuidedTool, StartResult, StepResult};
    use glam::DVec3;

    // --- pure assembler round-trips ---

    #[test]
    fn assemble_interpcurve_pure() {
        let args = [Input::Points(vec![
            DVec3::new(0.0, 0.0, 0.0),
            DVec3::new(1.0, 0.0, 0.0),
            DVec3::new(1.0, 1.0, 0.0),
        ])];
        assert_eq!(assemble_interpcurve(&args).unwrap(), "interpcurve 0,0 1,0 1,1");
        assert!(assemble_interpcurve(&[]).is_err());
    }

    #[test]
    fn guided_interpcurve_walk() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("interpcurve", None), StartResult::Started);
        assert!(t.current_wants_point_list());
        assert_eq!(t.push_list_point(DVec3::new(0.0, 0.0, 0.0)), StepResult::NeedMore);
        assert_eq!(t.push_list_point(DVec3::new(1.0, 0.0, 0.0)), StepResult::NeedMore);
        // Below min(3): Enter refuses.
        assert!(matches!(t.finish_list(), StepResult::Error(_)));
        assert_eq!(t.push_list_point(DVec3::new(1.0, 1.0, 0.0)), StepResult::NeedMore);
        assert_eq!(t.finish_list(), StepResult::Emit("interpcurve 0,0 1,0 1,1".into()));
        assert!(!t.active());
    }

    #[test]
    fn assemble_extrude_pure() {
        let args = [Input::Objects("sel".into()), Input::Num(3.0)];
        assert_eq!(assemble_extrude(&args).unwrap(), "extrude sel 3");
        assert!(assemble_extrude(&args[..1]).is_err());
    }

    #[test]
    fn assemble_revolve_pure() {
        let args = [
            Input::Objects("sel".into()),
            Input::Point(DVec3::new(0.0, 0.0, 0.0)),
            Input::Point(DVec3::new(0.0, 0.0, 1.0)),
            Input::Num(360.0),
        ];
        assert_eq!(assemble_revolve(&args).unwrap(), "revolve sel 0,0 0,0,1 360");
        assert!(assemble_revolve(&args[..2]).is_err());
    }

    #[test]
    fn assemble_pipe_pure() {
        let args = [Input::Objects("sel".into()), Input::Num(0.1)];
        assert_eq!(assemble_pipe(&args).unwrap(), "pipe sel 0.1");
        assert!(assemble_pipe(&args[..1]).is_err());
    }

    #[test]
    fn assemble_box_pure() {
        // corner = min(1,1,0) and (4,5,0) = (1,1,0); fmt drops z=0 → "1,1"
        // size = (3, 4, 2); z != 0 → "3,4,2"
        let args = [
            Input::Point(DVec3::new(1.0, 1.0, 0.0)),
            Input::Point(DVec3::new(4.0, 5.0, 0.0)),
            Input::Num(2.0),
        ];
        assert_eq!(assemble_box(&args).unwrap(), "box 1,1 3,4,2");
        assert!(assemble_box(&args[..1]).is_err());
    }

    #[test]
    fn assemble_arc_pure() {
        // center, start point (2,0 → r=2, 0°), end point (0,2 → 90°).
        let args = [
            Input::Point(DVec3::new(0.0, 0.0, 0.0)),
            Input::Point(DVec3::new(2.0, 0.0, 0.0)),
            Input::Point(DVec3::new(0.0, 2.0, 0.0)),
        ];
        assert_eq!(assemble_arc(&args).unwrap(), "arc 0,0 2 0 90");
        assert!(assemble_arc(&args[..2]).is_err());
    }

    #[test]
    fn assemble_ellipse_pure() {
        let args = [
            Input::Point(DVec3::new(0.0, 0.0, 0.0)),
            Input::Num(2.0),
            Input::Num(1.0),
        ];
        assert_eq!(assemble_ellipse(&args).unwrap(), "ellipse 0,0 2 1");
        assert!(assemble_ellipse(&args[..1]).is_err());
    }

    #[test]
    fn assemble_helix_pure() {
        let args = [
            Input::Point(DVec3::new(0.0, 0.0, 0.0)),
            Input::Num(1.0),
            Input::Num(3.0),
            Input::Int(4),
        ];
        assert_eq!(assemble_helix(&args).unwrap(), "helix 0,0 1 3 4");
        assert!(assemble_helix(&args[..1]).is_err());
    }

    #[test]
    fn assemble_area_pure() {
        let args = [Input::Objects("sel".into())];
        assert_eq!(assemble_area(&args).unwrap(), "area sel");
        assert!(assemble_area(&[]).is_err());
    }

    #[test]
    fn assemble_volume_pure() {
        let args = [Input::Objects("sel".into())];
        assert_eq!(assemble_volume(&args).unwrap(), "volume sel");
        assert!(assemble_volume(&[]).is_err());
    }

    #[test]
    fn assemble_bbox_pure() {
        let args = [Input::Objects("sel".into())];
        assert_eq!(assemble_bbox(&args).unwrap(), "bbox sel");
        assert!(assemble_bbox(&[]).is_err());
    }

    #[test]
    fn assemble_distance_pure() {
        let args = [
            Input::Point(DVec3::new(0.0, 0.0, 0.0)),
            Input::Point(DVec3::new(3.0, 4.0, 0.0)),
        ];
        assert_eq!(assemble_distance(&args).unwrap(), "distance 0,0 3,4");
        assert!(assemble_distance(&args[..1]).is_err());
    }

    #[test]
    fn assemble_multi_curve_builders_pure() {
        let a = Input::Objects("#aaaa1111".into());
        let b = Input::Objects("#bbbb2222".into());
        let c = Input::Objects("#cccc3333".into());
        assert_eq!(
            assemble_sweep(&[a.clone(), b.clone()]).unwrap(),
            "sweep #aaaa1111 #bbbb2222"
        );
        assert_eq!(
            assemble_sweep2(&[a.clone(), b.clone(), c.clone()]).unwrap(),
            "sweep2 #aaaa1111 #bbbb2222 #cccc3333"
        );
        assert_eq!(
            assemble_blend(&[a.clone(), b.clone(), Input::Num(1.0)]).unwrap(),
            "blend #aaaa1111 #bbbb2222 1"
        );
        assert_eq!(
            assemble_circletan(&[a.clone(), b.clone(), Input::Num(2.0)]).unwrap(),
            "circletan #aaaa1111 #bbbb2222 2"
        );
        assert_eq!(assemble_loft(&[Input::Objects("sel".into())]).unwrap(), "loft sel");
        // railrevolve: axis dir = end - start.
        let rr = [
            a.clone(),
            b.clone(),
            Input::Point(DVec3::new(0.0, 0.0, 0.0)),
            Input::Point(DVec3::new(0.0, 0.0, 5.0)),
        ];
        assert_eq!(
            assemble_railrevolve(&rr).unwrap(),
            "railrevolve #aaaa1111 #bbbb2222 0,0 0,0,5"
        );
        // linetan/lineperp: from point then a picked curve.
        let lt = [Input::Point(DVec3::new(1.0, 2.0, 0.0)), a.clone()];
        assert_eq!(assemble_linetan(&lt).unwrap(), "linetan 1,2 #aaaa1111");
        assert_eq!(assemble_lineperp(&lt).unwrap(), "lineperp 1,2 #aaaa1111");
    }

    #[test]
    fn sweep_walk_picks_profile_then_rail() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("sweep", None), StartResult::Started);
        assert!(t.current_wants_object());
        assert_eq!(t.commit_object("aaaa1111"), StepResult::NeedMore);
        assert_eq!(
            t.commit_object("bbbb2222"),
            StepResult::Emit("sweep #aaaa1111 #bbbb2222".into())
        );
    }

    // --- full GuidedTool walk tests ---

    #[test]
    fn guided_box_walk() {
        let mut t = GuidedTool::default();
        // box is needs_selection:false, so no selection needed
        assert_eq!(t.try_start("box", None), StartResult::Started);
        assert!(t.active());
        // Step 1: first base corner
        assert!(t.current_is_point());
        assert_eq!(t.on_click(DVec3::new(1.0, 1.0, 0.0)), StepResult::NeedMore);
        // Step 2: opposite base corner
        assert!(t.current_is_point());
        assert_eq!(t.on_click(DVec3::new(4.0, 5.0, 0.0)), StepResult::NeedMore);
        // Step 3: height (type "2")
        assert!(!t.current_is_point());
        assert_eq!(
            t.commit_typed("2"),
            StepResult::Emit("box 1,1 3,4,2".into())
        );
        assert!(!t.active());
    }

    #[test]
    fn guided_arc_walk() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("arc", None), StartResult::Started);
        // Center → start point → end point, all interactive (live ghost).
        assert!(t.current_is_point());
        assert_eq!(t.on_click(DVec3::new(0.0, 0.0, 0.0)), StepResult::NeedMore);
        assert!(t.current_is_point());
        assert_eq!(t.on_click(DVec3::new(2.0, 0.0, 0.0)), StepResult::NeedMore);
        assert!(t.current_is_point());
        assert_eq!(
            t.on_click(DVec3::new(0.0, 2.0, 0.0)),
            StepResult::Emit("arc 0,0 2 0 90".into())
        );
        assert!(!t.active());
    }

    #[test]
    fn arc_preview_shows_ring_then_arc() {
        let mut t = GuidedTool::default();
        t.try_start("arc", None);
        // Before the center is picked, nothing to preview.
        assert!(t.preview(Some(DVec3::new(1.0, 0.0, 0.0))).is_empty());
        t.on_click(DVec3::ZERO); // center
        // On the start-point step: a radius ring + a radius line to the cursor.
        let g = t.preview(Some(DVec3::new(2.0, 0.0, 0.0)));
        assert_eq!(g.len(), 2, "ring + radius line");
        t.on_click(DVec3::new(2.0, 0.0, 0.0)); // start point
        // On the end-point step: an arc ghost + two radius lines.
        let g = t.preview(Some(DVec3::new(0.0, 2.0, 0.0)));
        assert!(g.len() >= 1, "arc ghost present");
    }

    #[test]
    fn guided_distance_walk() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("distance", None), StartResult::Started);
        assert!(t.current_is_point());
        assert_eq!(t.on_click(DVec3::new(0.0, 0.0, 0.0)), StepResult::NeedMore);
        assert!(t.current_is_point());
        assert_eq!(
            t.on_click(DVec3::new(3.0, 4.0, 0.0)),
            StepResult::Emit("distance 0,0 3,4".into())
        );
        assert!(!t.active());
    }

    #[test]
    fn guided_area_emits_on_start() {
        // area/volume/bbox are zero-step: on a pre-selection, `emit_if_ready`
        // (called by the app right after `try_start`) fires the command at once.
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("area", Some("sel")), StartResult::Started);
        assert_eq!(t.emit_if_ready(), Some(StepResult::Emit("area sel".into())));
        assert!(!t.active(), "emitting resets the tool");
    }

    #[test]
    fn stepful_verb_does_not_emit_on_start() {
        // A verb with steps must NOT auto-emit; it waits for its picks.
        let mut t = GuidedTool::default();
        t.try_start("box", None);
        assert_eq!(t.emit_if_ready(), None);
        assert!(t.active());
    }

    #[test]
    fn guided_volume_assembler_emits() {
        let args = [Input::Objects("sel".into())];
        assert_eq!(assemble_volume(&args).unwrap(), "volume sel");
    }

    #[test]
    fn guided_bbox_assembler_emits() {
        let args = [Input::Objects("sel".into())];
        assert_eq!(assemble_bbox(&args).unwrap(), "bbox sel");
    }
}
