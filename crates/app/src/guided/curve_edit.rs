// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Guided verb scripts — curve_edit group.
//!
//! Implemented (emits existing canonical syntax):
//!   split     — sel + PickPoint            → `split sel <pt>`
//!   extend    — sel + Number               → `extend sel <dist>`
//!   powertrim — sel + PickPoint            → `powertrim sel <pt>`
//!   fillet    — pick curve A, curve B, radius → `fillet #a #b <r>`
//!   chamfer   — pick curve A, curve B, distance → `chamfer #a #b <d>`
//!   trim      — select cutters (click/window, Enter), then click OR window the
//!               parts to remove (Enter) → `trim #c1 #c2 … remove <p1> <p2> …`
//!   boundary  — no sel, PickPoint           → `boundary <pt>`
//!   divide    — sel + Integer               → `divide sel <count>`
//!   curvebool — sel + Keyword               → `curvebool <op> sel`
//!   join      — sel, zero steps (emit-on-start) → `join sel`
//!   explode   — sel, zero steps (emit-on-start) → `explode sel`

use super::assemble::{
    captured_points, int_at, key_at, num_at, obj_at, object_set_at, point_at, points_at, selector,
};
use super::{fmt, num, Input, ObjFilter, Step, VerbScript};

pub static SCRIPTS: &[VerbScript] = &[
    VerbScript {
        verb: "split",
        needs_selection: true,
        steps: &[Step::PickPoint { prompt: "Split point" }],
        assemble: assemble_split,
    },
    VerbScript {
        verb: "extend",
        needs_selection: true,
        steps: &[Step::Number { prompt: "Extension distance", default: Some(1.0) }],
        assemble: assemble_extend,
    },
    VerbScript {
        verb: "powertrim",
        needs_selection: true,
        steps: &[Step::PickPoint { prompt: "Pick point on segment to trim" }],
        assemble: assemble_powertrim,
    },
    VerbScript {
        verb: "fillet",
        needs_selection: false,
        steps: &[
            // Pick each curve NEAR THE END to round — the click LOCATION chooses
            // the corner (Rhino). Both picks capture their world point.
            Step::SelectObject {
                prompt: "Select first curve to fillet",
                filter: ObjFilter::Curve,
                capture_point: true,
            },
            Step::SelectObject {
                prompt: "Select second curve to fillet",
                filter: ObjFilter::Curve,
                capture_point: true,
            },
            Step::Number { prompt: "Fillet radius", default: Some(0.5) },
            Step::Keyword { prompt: "Trim", options: &["Yes", "No"], default: "Yes" },
            Step::Keyword { prompt: "Join", options: &["No", "Yes"], default: "No" },
        ],
        assemble: assemble_fillet,
    },
    VerbScript {
        verb: "chamfer",
        needs_selection: false,
        steps: &[
            Step::SelectObject {
                prompt: "Select first curve to chamfer",
                filter: ObjFilter::Curve,
                capture_point: false,
            },
            Step::SelectObject {
                prompt: "Select second curve to chamfer",
                filter: ObjFilter::Curve,
                capture_point: false,
            },
            Step::Number { prompt: "Chamfer distance", default: Some(0.5) },
        ],
        assemble: assemble_chamfer,
    },
    VerbScript {
        verb: "trim",
        needs_selection: false,
        steps: &[
            // Rhino's two-phase trim: pick the cutting objects as a SET (click or
            // window, Enter when done), then click OR window each piece to REMOVE
            // (Enter finishes).
            Step::SelectObjects {
                prompt: "Select cutting objects (Enter when done)",
                filter: ObjFilter::Any,
                min: 1,
            },
            // Pick markers, not vertices — `connect: false` so no rubber-band
            // line is drawn joining the clicked pieces (it's a trim, not a shape).
            Step::PointList { prompt: "Click the parts to remove (Enter to finish)", min: 1, connect: false },
        ],
        assemble: assemble_trim,
    },
    VerbScript {
        verb: "divide",
        needs_selection: true,
        steps: &[Step::Integer { prompt: "Number of segments", default: Some(10) }],
        assemble: assemble_divide,
    },
    VerbScript {
        verb: "boundary",
        needs_selection: false,
        steps: &[Step::PickPoint { prompt: "Seed point inside region" }],
        assemble: assemble_boundary,
    },
    VerbScript {
        verb: "curvebool",
        needs_selection: true,
        steps: &[Step::Keyword {
            prompt: "Boolean operation",
            options: &["Union", "Intersect", "Difference"],
            default: "Union",
        }],
        assemble: assemble_curvebool,
    },
    VerbScript {
        verb: "join",
        needs_selection: true,
        steps: &[],
        assemble: assemble_join,
    },
    VerbScript {
        verb: "explode",
        needs_selection: true,
        steps: &[],
        assemble: assemble_explode,
    },
];

// ── assemblers ────────────────────────────────────────────────────────────────

/// `[Objects(sel), Point(pt)] -> "split sel <pt>"`
fn assemble_split(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "split")?;
    let pt = point_at(args, 1, "split")?;
    Ok(format!("split {sel} {}", fmt(pt)))
}

/// `[Objects(sel), Num(dist)] -> "extend sel <dist>"`
fn assemble_extend(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "extend")?;
    let dist = num_at(args, 1, "extend")?;
    Ok(format!("extend {sel} {}", num(dist)))
}

/// `[Objects(sel), Point(pt)] -> "powertrim sel <pt>"`
fn assemble_powertrim(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "powertrim")?;
    let pt = point_at(args, 1, "powertrim")?;
    Ok(format!("powertrim {sel} {}", fmt(pt)))
}

/// `[Objects(#a), Objects(#b), Num(r), Key(trim), Key(join), <Point(ptA), Point(ptB)>]`
/// → `"fillet #a #b <r> [at <ptA> <ptB>] trim <yes|no> join <yes|no>"`.
///
/// Rhino parity: each curve is picked NEAR THE END to round, so the captured
/// pick points (the `at` clause) choose the corner; Trim/Join mirror Rhino's
/// options. Emits the bare `at`-less form when no points were captured (unit
/// tests / non-capturing callers).
fn assemble_fillet(args: &[Input]) -> Result<String, String> {
    let a = obj_at(args, 0, "fillet")?;
    let b = obj_at(args, 1, "fillet")?;
    let r = num_at(args, 2, "fillet")?;
    let trim = key_at(args, 3, "fillet")?;
    let join = key_at(args, 4, "fillet")?;
    let mut cmd = format!("fillet {a} {b} {}", num(r));
    if let Some(pts) = captured_points(args, 2) {
        cmd.push_str(&format!(" at {} {}", fmt(pts[0]), fmt(pts[1])));
    }
    let yn = |k: &str| if k.eq_ignore_ascii_case("yes") { "yes" } else { "no" };
    cmd.push_str(&format!(" trim {} join {}", yn(trim), yn(join)));
    Ok(cmd)
}

/// `[Objects(#a), Objects(#b), Num(d)] -> "chamfer #a #b <d>"` — two curves
/// picked interactively (mirrors the fillet flow, straight bevel instead).
fn assemble_chamfer(args: &[Input]) -> Result<String, String> {
    let a = obj_at(args, 0, "chamfer")?;
    let b = obj_at(args, 1, "chamfer")?;
    let d = num_at(args, 2, "chamfer")?;
    Ok(format!("chamfer {a} {b} {}", num(d)))
}

/// `[ObjectSet([#c1,#c2,…]), Points([p1,p2,…])] -> "trim #c1 #c2 … remove <p1> <p2> …"`
///
/// Rhino two-phase trim: the cutter SET, the literal `remove`, then the click
/// points identifying the pieces to delete. (The `extend` variant is reachable
/// via the typed command; the guided flow always emits `remove`.)
fn assemble_trim(args: &[Input]) -> Result<String, String> {
    let cutters = object_set_at(args, 0, "trim")?;
    let removes = points_at(args, 1, "trim")?;
    let pts = removes.iter().map(|p| fmt(*p)).collect::<Vec<_>>().join(" ");
    Ok(format!("trim {cutters} remove {pts}"))
}

/// `[Objects(sel), Int(count)] -> "divide sel <count>"`
fn assemble_divide(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "divide")?;
    let count = int_at(args, 1, "divide")?;
    Ok(format!("divide {sel} {count}"))
}

/// `[Point(seed)] -> "boundary <seed>"`
///
/// `needs_selection: false` — there is no Objects seed; the first collected
/// input is the seed point at index 0.
fn assemble_boundary(args: &[Input]) -> Result<String, String> {
    let pt = point_at(args, 0, "boundary")?;
    Ok(format!("boundary {}", fmt(pt)))
}

/// `[Objects(sel), Key(op)] -> "curvebool <op> sel"`
fn assemble_curvebool(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "curvebool")?;
    let op = key_at(args, 1, "curvebool")?;
    Ok(format!("curvebool {op} {sel}"))
}

/// `[Objects(sel)] -> "join sel"` — zero-step verb (emit-on-start).
fn assemble_join(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "join")?;
    Ok(format!("join {sel}"))
}

/// `[Objects(sel)] -> "explode sel"` — zero-step verb (emit-on-start).
fn assemble_explode(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "explode")?;
    Ok(format!("explode {sel}"))
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::{GuidedTool, StartResult, StepResult};
    use super::*;
    use glam::DVec3;

    // ---- assembler unit tests -----------------------------------------------

    #[test]
    fn assemble_split_pure() {
        let args = [
            Input::Objects("sel".into()),
            Input::Point(DVec3::new(5.0, 3.0, 0.0)),
        ];
        assert_eq!(assemble_split(&args).unwrap(), "split sel 5,3");
        assert!(assemble_split(&args[..1]).is_err());
        assert!(assemble_split(&[]).is_err());
    }

    #[test]
    fn assemble_extend_pure() {
        let args = [Input::Objects("sel".into()), Input::Num(2.5)];
        assert_eq!(assemble_extend(&args).unwrap(), "extend sel 2.5");
        assert!(assemble_extend(&args[..1]).is_err());
    }

    #[test]
    fn assemble_powertrim_pure() {
        let args = [
            Input::Objects("sel".into()),
            Input::Point(DVec3::new(1.0, 2.0, 0.0)),
        ];
        assert_eq!(assemble_powertrim(&args).unwrap(), "powertrim sel 1,2");
        assert!(assemble_powertrim(&args[..1]).is_err());
    }

    #[test]
    fn assemble_fillet_pure() {
        // Bare form (no captured points): default Trim/Join keywords.
        let args = [
            Input::Objects("#a1b2c3d4".into()),
            Input::Objects("#00ffee11".into()),
            Input::Num(0.5),
            Input::Key("Yes".into()),
            Input::Key("No".into()),
        ];
        assert_eq!(
            assemble_fillet(&args).unwrap(),
            "fillet #a1b2c3d4 #00ffee11 0.5 trim yes join no"
        );
        assert!(assemble_fillet(&args[..1]).is_err());
    }

    #[test]
    fn assemble_fillet_with_captured_points_and_options() {
        // Two captured pick points ride at the tail → `at <pA> <pB>`; Trim=No,
        // Join=Yes flip the option flags.
        let args = [
            Input::Objects("#a1b2c3d4".into()),
            Input::Objects("#00ffee11".into()),
            Input::Num(0.5),
            Input::Key("No".into()),
            Input::Key("Yes".into()),
            Input::Point(DVec3::new(0.0, 0.0, 0.0)),
            Input::Point(DVec3::new(5.0, 5.0, 0.0)),
        ];
        assert_eq!(
            assemble_fillet(&args).unwrap(),
            "fillet #a1b2c3d4 #00ffee11 0.5 at 0,0 5,5 trim no join yes"
        );
    }

    #[test]
    fn assemble_chamfer_pure() {
        let args = [
            Input::Objects("#a1b2c3d4".into()),
            Input::Objects("#00ffee11".into()),
            Input::Num(0.5),
        ];
        assert_eq!(assemble_chamfer(&args).unwrap(), "chamfer #a1b2c3d4 #00ffee11 0.5");
        assert!(assemble_chamfer(&args[..1]).is_err());
    }

    #[test]
    fn assemble_trim_pure() {
        // Cutter SET + a run of removal points → `trim #c1 #c2 remove <p1> <p2>`.
        let args = [
            Input::ObjectSet(vec!["#aaaa1111".into(), "#bbbb2222".into()]),
            Input::Points(vec![DVec3::new(2.0, 1.0, 0.0), DVec3::new(8.0, 0.0, 0.0)]),
        ];
        assert_eq!(
            assemble_trim(&args).unwrap(),
            "trim #aaaa1111 #bbbb2222 remove 2,1 8,0"
        );
        // Missing the removal points is an error.
        assert!(assemble_trim(&args[..1]).is_err());
    }

    #[test]
    fn fillet_walk_picks_two_curves_then_radius() {
        let mut t = GuidedTool::default();
        // Verb-first: no pre-selection needed; picks each curve interactively.
        assert_eq!(t.try_start("fillet", None), StartResult::Started);
        assert!(t.current_wants_object());
        // Both picks capture the click location (Rhino corner selection).
        assert!(t.current_wants_object_point());
        assert_eq!(
            t.commit_object_at("a1b2c3d4", Some(DVec3::new(1.0, 0.0, 0.0))),
            StepResult::NeedMore
        );
        assert!(t.current_wants_object());
        assert!(t.current_wants_object_point());
        assert_eq!(
            t.commit_object_at("00ffee11", Some(DVec3::new(0.0, 1.0, 0.0))),
            StepResult::NeedMore
        );
        // Radius, then Trim/Join keywords.
        assert!(!t.current_wants_object());
        assert_eq!(t.commit_typed("0.5"), StepResult::NeedMore); // radius
        assert_eq!(t.commit_typed(""), StepResult::NeedMore); // Trim default Yes
        assert_eq!(
            t.commit_typed(""), // Join default No → emit
            StepResult::Emit(
                "fillet #a1b2c3d4 #00ffee11 0.5 at 1,0 0,1 trim yes join no".into()
            )
        );
    }

    #[test]
    fn assemble_boundary_pure() {
        let args = [Input::Point(DVec3::new(3.0, 4.0, 0.0))];
        assert_eq!(assemble_boundary(&args).unwrap(), "boundary 3,4");
        assert!(assemble_boundary(&[]).is_err());
    }

    #[test]
    fn assemble_divide_pure() {
        let args = [Input::Objects("sel".into()), Input::Int(8)];
        assert_eq!(assemble_divide(&args).unwrap(), "divide sel 8");
        assert!(assemble_divide(&args[..1]).is_err());
        assert!(assemble_divide(&[]).is_err());
    }

    #[test]
    fn assemble_curvebool_pure() {
        let args = [Input::Objects("sel".into()), Input::Key("Union".into())];
        assert_eq!(assemble_curvebool(&args).unwrap(), "curvebool Union sel");
        assert!(assemble_curvebool(&args[..1]).is_err());
    }

    // ---- GuidedTool walk tests ----------------------------------------------

    #[test]
    fn split_guided_walk_start_to_emit() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("split", Some("sel")), StartResult::Started);
        assert!(t.active());
        assert!(t.current_is_point());
        assert!(t.prompt().unwrap().starts_with("Split point"));
        assert_eq!(
            t.on_click(DVec3::new(5.0, 3.0, 0.0)),
            StepResult::Emit("split sel 5,3".into())
        );
        assert!(!t.active());
    }

    #[test]
    fn split_needs_selection() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("split", None), StartResult::NeedSelection);
    }

    #[test]
    fn extend_walk_default() {
        let mut t = GuidedTool::default();
        t.try_start("extend", Some("sel"));
        assert_eq!(t.prompt().unwrap(), "Extension distance <1>:");
        // Bare Enter accepts default (1.0).
        assert_eq!(t.commit_typed(""), StepResult::Emit("extend sel 1".into()));
    }

    #[test]
    fn extend_walk_typed() {
        let mut t = GuidedTool::default();
        t.try_start("extend", Some("sel"));
        assert_eq!(t.commit_typed("3.5"), StepResult::Emit("extend sel 3.5".into()));
    }

    #[test]
    fn powertrim_guided_walk() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("powertrim", Some("sel")), StartResult::Started);
        assert!(t.current_is_point());
        assert_eq!(
            t.on_click(DVec3::new(1.0, 2.0, 0.0)),
            StepResult::Emit("powertrim sel 1,2".into())
        );
    }

    #[test]
    fn fillet_walk_default_radius_and_options_after_two_picks() {
        let mut t = GuidedTool::default();
        t.try_start("fillet", None);
        t.commit_object_at("a1b2c3d4", Some(DVec3::new(2.0, 0.0, 0.0)));
        t.commit_object_at("00ffee11", Some(DVec3::new(0.0, 2.0, 0.0)));
        assert_eq!(t.prompt().unwrap(), "Fillet radius <0.5>:");
        // Bare Enter takes the default radius, then the Trim/Join defaults.
        assert_eq!(t.commit_typed(""), StepResult::NeedMore); // radius 0.5
        assert_eq!(t.prompt().unwrap(), "Trim <Yes>:");
        assert_eq!(t.commit_typed(""), StepResult::NeedMore); // Trim Yes
        assert_eq!(t.prompt().unwrap(), "Join <No>:");
        assert_eq!(
            t.commit_typed(""),
            StepResult::Emit(
                "fillet #a1b2c3d4 #00ffee11 0.5 at 2,0 0,2 trim yes join no".into()
            )
        );
    }

    #[test]
    fn trim_walk_selects_cutters_then_removal_points() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("trim", None), StartResult::Started);
        // Phase 1: multi-select the cutting objects, Enter (finish_objects).
        assert!(t.current_wants_objects());
        assert_eq!(t.push_selected_object("aaaa1111"), StepResult::NeedMore);
        assert_eq!(t.push_selected_object("bbbb2222"), StepResult::NeedMore);
        assert_eq!(t.finish_objects(), StepResult::NeedMore);
        // Phase 2: click the parts to remove, Enter (finish_list).
        assert!(t.current_wants_point_list());
        assert_eq!(t.push_list_point(DVec3::new(2.0, 1.0, 0.0)), StepResult::NeedMore);
        assert_eq!(t.push_list_point(DVec3::new(8.0, 0.0, 0.0)), StepResult::NeedMore);
        assert_eq!(
            t.finish_list(),
            StepResult::Emit("trim #aaaa1111 #bbbb2222 remove 2,1 8,0".into())
        );
        assert!(!t.active());
    }

    #[test]
    fn boundary_no_selection_needed() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("boundary", None), StartResult::Started);
        assert!(t.current_is_point());
        assert_eq!(
            t.on_click(DVec3::new(3.0, 4.0, 0.0)),
            StepResult::Emit("boundary 3,4".into())
        );
    }

    #[test]
    fn divide_guided_walk_typed_count() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("divide", Some("sel")), StartResult::Started);
        assert!(t.active());
        assert_eq!(t.commit_typed("8"), StepResult::Emit("divide sel 8".into()));
        assert!(!t.active());
    }

    #[test]
    fn divide_walk_default_count() {
        let mut t = GuidedTool::default();
        t.try_start("divide", Some("sel"));
        assert_eq!(t.prompt().unwrap(), "Number of segments <10>:");
        // Bare Enter accepts the default (10).
        assert_eq!(t.commit_typed(""), StepResult::Emit("divide sel 10".into()));
    }

    #[test]
    fn divide_needs_selection() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("divide", None), StartResult::NeedSelection);
    }

    #[test]
    fn curvebool_walk_default_union() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("curvebool", Some("sel")), StartResult::Started);
        assert!(t.prompt().unwrap().contains("Union"));
        assert_eq!(
            t.commit_typed(""),
            StepResult::Emit("curvebool Union sel".into())
        );
    }

    #[test]
    fn curvebool_walk_difference_prefix() {
        let mut t = GuidedTool::default();
        t.try_start("curvebool", Some("sel"));
        assert_eq!(
            t.commit_typed("Di"),
            StepResult::Emit("curvebool Difference sel".into())
        );
    }

    #[test]
    fn curvebool_walk_intersect() {
        let mut t = GuidedTool::default();
        t.try_start("curvebool", Some("sel"));
        assert_eq!(
            t.commit_typed("In"),
            StepResult::Emit("curvebool Intersect sel".into())
        );
    }

    #[test]
    fn chamfer_walk_default_distance_after_two_picks() {
        let mut t = GuidedTool::default();
        t.try_start("chamfer", None);
        t.commit_object("a1b2c3d4");
        t.commit_object("00ffee11");
        assert_eq!(t.prompt().unwrap(), "Chamfer distance <0.5>:");
        assert_eq!(
            t.commit_typed(""),
            StepResult::Emit("chamfer #a1b2c3d4 #00ffee11 0.5".into())
        );
    }

    #[test]
    fn chamfer_walk_typed_distance() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("chamfer", None), StartResult::Started);
        assert!(t.current_wants_object());
        t.commit_object("aaaa1111");
        t.commit_object("bbbb2222");
        assert!(!t.current_wants_object());
        assert_eq!(
            t.commit_typed("1.25"),
            StepResult::Emit("chamfer #aaaa1111 #bbbb2222 1.25".into())
        );
    }

    #[test]
    fn assemble_explode_pure() {
        let args = [Input::Objects("sel".into())];
        assert_eq!(assemble_explode(&args).unwrap(), "explode sel");
        assert!(assemble_explode(&[]).is_err());
    }

    #[test]
    fn explode_emits_on_start() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("explode", Some("sel")), StartResult::Started);
        assert_eq!(t.emit_if_ready(), Some(StepResult::Emit("explode sel".into())));
        assert!(!t.active());
    }

    #[test]
    fn explode_needs_selection() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("explode", None), StartResult::NeedSelection);
    }

    #[test]
    fn assemble_join_pure() {
        let args = [Input::Objects("sel".into())];
        assert_eq!(assemble_join(&args).unwrap(), "join sel");
        assert!(assemble_join(&[]).is_err());
    }

    #[test]
    fn join_emits_on_start() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("join", Some("sel")), StartResult::Started);
        assert_eq!(t.emit_if_ready(), Some(StepResult::Emit("join sel".into())));
        assert!(!t.active());
    }

    #[test]
    fn join_needs_selection() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("join", None), StartResult::NeedSelection);
    }
}
