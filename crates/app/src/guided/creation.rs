// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Guided creation + measure verbs: `extrude`, `revolve`, `pipe`, `box`, `arc`,
//! `ellipse`, `helix`, `area`, `volume`, `bbox`, `distance`. Every assembler
//! emits the canonical string the existing parser already accepts, unchanged.

use super::assemble::{int_at, num_at, point_at, selector};
use super::{Input, Step, VerbScript, fmt, num};

pub static SCRIPTS: &[VerbScript] = &[
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
        steps: &[
            Step::PickPoint { prompt: "Center" },
            Step::Number { prompt: "Radius", default: Some(1.0) },
            Step::Number { prompt: "Start angle", default: Some(0.0) },
            Step::Number { prompt: "End angle", default: Some(90.0) },
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
];

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
fn assemble_arc(args: &[Input]) -> Result<String, String> {
    let center = point_at(args, 0, "arc")?;
    let r = num_at(args, 1, "arc")?;
    let s = num_at(args, 2, "arc")?;
    let e = num_at(args, 3, "arc")?;
    Ok(format!("arc {} {} {} {}", fmt(center), num(r), num(s), num(e)))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guided::{GuidedTool, StartResult, StepResult};
    use glam::DVec3;

    // --- pure assembler round-trips ---

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
        let args = [
            Input::Point(DVec3::new(0.0, 0.0, 0.0)),
            Input::Num(1.0),
            Input::Num(0.0),
            Input::Num(90.0),
        ];
        assert_eq!(assemble_arc(&args).unwrap(), "arc 0,0 1 0 90");
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
        // Step 1: center
        assert!(t.current_is_point());
        assert_eq!(t.on_click(DVec3::new(0.0, 0.0, 0.0)), StepResult::NeedMore);
        // Step 2: radius
        assert!(!t.current_is_point());
        assert_eq!(t.commit_typed("2"), StepResult::NeedMore);
        // Step 3: start angle (default 0.0, bare Enter)
        assert_eq!(t.commit_typed(""), StepResult::NeedMore);
        // Step 4: end angle
        assert_eq!(
            t.commit_typed("180"),
            StepResult::Emit("arc 0,0 2 0 180".into())
        );
        assert!(!t.active());
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
