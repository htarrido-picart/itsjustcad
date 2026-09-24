// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Guided verb scripts — array group. See `offset.rs` for the reference shape.
//!
//! Implemented:
//! - `array`      — rectangular/grid array (nx,ny counts + dx,dy spacing)
//! - `polararray` — polar/rotational array (count, center point, total angle)
//!
//! Deferred (engine gap):
//! - `arraycurve`/`patharray` — needs a second interactive object pick (path
//!   curve selector). The guided engine v1 has no step for picking a second
//!   object interactively; all selectors are seeded from a pre-selection.
//!   Re-enable once the engine gains an `ObjectPick` step type.

use super::assemble::{int_at, num_at, point_at, selector};
use super::{Input, Step, VerbScript, fmt, num};

pub static SCRIPTS: &[VerbScript] = &[
    VerbScript {
        verb: "array",
        needs_selection: true,
        steps: &[
            Step::Integer { prompt: "Number in X", default: Some(2) },
            Step::Integer { prompt: "Number in Y", default: Some(2) },
            Step::Number { prompt: "X spacing", default: None },
            Step::Number { prompt: "Y spacing", default: None },
        ],
        assemble: assemble_array,
    },
    VerbScript {
        verb: "polararray",
        needs_selection: true,
        steps: &[
            Step::Integer { prompt: "Number of items", default: Some(6) },
            Step::PickPoint { prompt: "Center of array" },
            Step::Number { prompt: "Total angle in degrees", default: Some(360.0) },
        ],
        assemble: assemble_polararray,
    },
];

/// `[Objects(sel), Int(nx), Int(ny), Num(dx), Num(dy)] -> "array sel nx,ny,1 dx,dy,0"`.
fn assemble_array(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "array")?;
    let nx = int_at(args, 1, "array")?;
    let ny = int_at(args, 2, "array")?;
    let dx = num_at(args, 3, "array")?;
    let dy = num_at(args, 4, "array")?;
    Ok(format!("array {sel} {nx},{ny},1 {},{},0", num(dx), num(dy)))
}

/// `[Objects(sel), Int(count), Point(center), Num(angle)] -> "polararray sel count cx,cy angle"`.
fn assemble_polararray(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "polararray")?;
    let count = int_at(args, 1, "polararray")?;
    let center = point_at(args, 2, "polararray")?;
    let angle = num_at(args, 3, "polararray")?;
    Ok(format!("polararray {sel} {count} {} {}", fmt(center), num(angle)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guided::{GuidedTool, StartResult, StepResult};
    use glam::DVec3;

    // --- pure assembler tests ---

    #[test]
    fn assemble_array_is_pure() {
        let args = [
            Input::Objects("sel".into()),
            Input::Int(3),
            Input::Int(2),
            Input::Num(1.5),
            Input::Num(2.0),
        ];
        assert_eq!(assemble_array(&args).unwrap(), "array sel 3,2,1 1.5,2,0");
    }

    #[test]
    fn assemble_array_missing_args_is_err() {
        let args = [Input::Objects("sel".into()), Input::Int(3)];
        assert!(assemble_array(&args).is_err());
    }

    #[test]
    fn assemble_polararray_is_pure() {
        let args = [
            Input::Objects("sel".into()),
            Input::Int(6),
            Input::Point(DVec3::new(0.0, 0.0, 0.0)),
            Input::Num(360.0),
        ];
        assert_eq!(assemble_polararray(&args).unwrap(), "polararray sel 6 0,0 360");
    }

    #[test]
    fn assemble_polararray_nonzero_center() {
        let args = [
            Input::Objects("sel".into()),
            Input::Int(4),
            Input::Point(DVec3::new(5.0, 3.0, 0.0)),
            Input::Num(180.0),
        ];
        assert_eq!(assemble_polararray(&args).unwrap(), "polararray sel 4 5,3 180");
    }

    #[test]
    fn assemble_polararray_missing_args_is_err() {
        let args = [Input::Objects("sel".into()), Input::Int(6)];
        assert!(assemble_polararray(&args).is_err());
    }

    // --- full GuidedTool walk tests ---

    #[test]
    fn array_guided_walk_emits_correct_command() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("array", Some("sel")), StartResult::Started);
        assert!(t.active());
        // Step 1: Number in X (default 2)
        assert!(t.prompt().unwrap().contains("Number in X"));
        assert_eq!(t.commit_typed("3"), StepResult::NeedMore);
        // Step 2: Number in Y (default 2)
        assert!(t.prompt().unwrap().contains("Number in Y"));
        assert_eq!(t.commit_typed("2"), StepResult::NeedMore);
        // Step 3: X spacing
        assert!(t.prompt().unwrap().contains("X spacing"));
        assert_eq!(t.commit_typed("1.5"), StepResult::NeedMore);
        // Step 4: Y spacing
        assert!(t.prompt().unwrap().contains("Y spacing"));
        assert_eq!(
            t.commit_typed("2"),
            StepResult::Emit("array sel 3,2,1 1.5,2,0".into())
        );
        assert!(!t.active());
    }

    #[test]
    fn array_guided_walk_defaults() {
        let mut t = GuidedTool::default();
        t.try_start("array", Some("sel"));
        // Accept all defaults (nx=2, ny=2) then provide spacings
        assert_eq!(t.commit_typed(""), StepResult::NeedMore); // nx default 2
        assert_eq!(t.commit_typed(""), StepResult::NeedMore); // ny default 2
        assert_eq!(t.commit_typed("5"), StepResult::NeedMore);
        assert_eq!(
            t.commit_typed("5"),
            StepResult::Emit("array sel 2,2,1 5,5,0".into())
        );
    }

    #[test]
    fn polararray_guided_walk_emits_correct_command() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("polararray", Some("sel")), StartResult::Started);
        assert!(t.active());
        // Step 1: Number of items (default 6)
        assert!(t.prompt().unwrap().contains("Number of items"));
        assert_eq!(t.commit_typed("6"), StepResult::NeedMore);
        // Step 2: PickPoint - center
        assert!(t.current_is_point());
        assert!(t.prompt().unwrap().contains("Center of array"));
        assert_eq!(t.on_click(DVec3::new(0.0, 0.0, 0.0)), StepResult::NeedMore);
        // Step 3: Total angle (default 360)
        assert!(t.prompt().unwrap().contains("Total angle"));
        assert_eq!(
            t.commit_typed("360"),
            StepResult::Emit("polararray sel 6 0,0 360".into())
        );
        assert!(!t.active());
    }

    #[test]
    fn polararray_guided_walk_defaults() {
        let mut t = GuidedTool::default();
        t.try_start("polararray", Some("sel"));
        assert_eq!(t.commit_typed(""), StepResult::NeedMore); // count default 6
        assert_eq!(t.on_click(DVec3::new(1.0, 2.0, 0.0)), StepResult::NeedMore); // center
        assert_eq!(
            t.commit_typed(""),
            StepResult::Emit("polararray sel 6 1,2 360".into()) // angle default 360
        );
    }

    #[test]
    fn array_needs_selection() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("array", None), StartResult::NeedSelection);
        assert!(!t.active());
    }

    #[test]
    fn polararray_needs_selection() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("polararray", None), StartResult::NeedSelection);
        assert!(!t.active());
    }
}
