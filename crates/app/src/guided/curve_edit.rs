// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Guided verb scripts — curve_edit group.
//!
//! Implemented (pre-selection model, emits existing canonical syntax):
//!   split     — sel + PickPoint  → `split sel <pt>`
//!   extend    — sel + Number     → `extend sel <dist>`
//!   powertrim — sel + PickPoint  → `powertrim sel <pt>`
//!   fillet    — sel + Number     → `fillet sel <r>`  (same-selector form)
//!   boundary  — no sel, PickPoint → `boundary <pt>`
//!   curvebool — sel + Keyword    → `curvebool <op> sel`
//!
//! Deferred (engine or parser limitation):
//!   join      — zero steps; the engine has no "emit on start" path in v1
//!               (try_start arms the tool but prompt() returns None for an
//!               empty step list, leaving it permanently armed). Already trivial
//!               to type directly.
//!   trim      — requires target selector + independent cutter selector + keep pt;
//!               interactive second-object picking unsupported in v1
//!   explode   — no parser arm in commands/src/parse.rs
//!   chamfer   — no parser arm in commands/src/parse.rs

use super::assemble::{key_at, num_at, point_at, selector};
use super::{fmt, num, Input, Step, VerbScript};

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
        needs_selection: true,
        steps: &[Step::Number { prompt: "Fillet radius", default: Some(0.5) }],
        assemble: assemble_fillet,
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

/// `[Objects(sel), Num(r)] -> "fillet sel <r>"`
///
/// Uses the single-selector form accepted by the parser's fillet arm
/// (`fillet <sel> <r>`), which resolves to `a = b = sel`.
fn assemble_fillet(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "fillet")?;
    let r = num_at(args, 1, "fillet")?;
    Ok(format!("fillet {sel} {}", num(r)))
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
        let args = [Input::Objects("sel".into()), Input::Num(0.5)];
        assert_eq!(assemble_fillet(&args).unwrap(), "fillet sel 0.5");
        assert!(assemble_fillet(&args[..1]).is_err());
    }

    #[test]
    fn assemble_boundary_pure() {
        let args = [Input::Point(DVec3::new(3.0, 4.0, 0.0))];
        assert_eq!(assemble_boundary(&args).unwrap(), "boundary 3,4");
        assert!(assemble_boundary(&[]).is_err());
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
    fn fillet_walk_default() {
        let mut t = GuidedTool::default();
        t.try_start("fillet", Some("sel"));
        assert_eq!(t.prompt().unwrap(), "Fillet radius <0.5>:");
        assert_eq!(t.commit_typed(""), StepResult::Emit("fillet sel 0.5".into()));
    }

    #[test]
    fn fillet_walk_typed_radius() {
        let mut t = GuidedTool::default();
        t.try_start("fillet", Some("sel"));
        assert_eq!(t.commit_typed("1.25"), StepResult::Emit("fillet sel 1.25".into()));
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
}
