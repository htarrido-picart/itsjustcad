// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Guided verb scripts — annotate group. See `offset.rs` for the reference shape.
//!
//! Implemented verbs
//! -----------------
//! - `dim`          — linear dimension, two free anchor points + optional offset
//! - `dimangular`   — angular dimension, vertex + two leg points + optional radius
//! - `autodim`      — batch-dimension a selection with an optional offset
//!
//! Deferred verbs
//! --------------
//! - `dimradius` / `dimdiameter` — zero-step verbs (selection only). The current
//!   engine has no `Confirm` step; `try_start` returns `Started` but `prompt()`
//!   returns `None`, so no prompt is shown and the tool silently waits. Needs
//!   either a `Step::Confirm` kind in mod.rs, or an app.rs change to call
//!   `commit_typed("")` when `prompt()` is `None` after `Started`. Defer until
//!   the engine gains that support. The assemblers are tested below.
//! - `field`  — the expression argument (`area <sel>`, `length <sel>`, `layer`,
//!   `units`, …) is free text that the guided engine cannot collect via
//!   PickPoint / Number / Keyword steps; defer until the engine gains a
//!   freetext step kind.

use super::assemble::{num_at, point_at, selector};
use super::{fmt, num, Input, Step, VerbScript};

pub static SCRIPTS: &[VerbScript] = &[
    // --- dim ---------------------------------------------------------------
    VerbScript {
        verb: "dim",
        needs_selection: false,
        steps: &[
            Step::PickPoint { prompt: "First extension point" },
            Step::PickPoint { prompt: "Second extension point" },
            Step::Number { prompt: "Dimension line offset", default: Some(0.5) },
        ],
        assemble: assemble_dim,
    },
    // --- dimangular --------------------------------------------------------
    VerbScript {
        verb: "dimangular",
        needs_selection: false,
        steps: &[
            Step::PickPoint { prompt: "Vertex point" },
            Step::PickPoint { prompt: "First leg point" },
            Step::PickPoint { prompt: "Second leg point" },
            Step::Number { prompt: "Arc radius", default: Some(1.0) },
        ],
        assemble: assemble_dimangular,
    },
    // --- autodim -----------------------------------------------------------
    VerbScript {
        verb: "autodim",
        needs_selection: true,
        steps: &[Step::Number { prompt: "Dimension line offset", default: Some(0.5) }],
        assemble: assemble_autodim,
    },
];

// ---------------------------------------------------------------------------
// Assemblers
// ---------------------------------------------------------------------------

/// `[Point(a), Point(b), Num(offset)] -> "dim <a> <b> <offset>"`
fn assemble_dim(args: &[Input]) -> Result<String, String> {
    let a = point_at(args, 0, "dim")?;
    let b = point_at(args, 1, "dim")?;
    let offset = num_at(args, 2, "dim")?;
    Ok(format!("dim {} {} {}", fmt(a), fmt(b), num(offset)))
}

/// `[Point(vertex), Point(p1), Point(p2), Num(radius)] -> "dimangular <v> <p1> <p2> <r>"`
fn assemble_dimangular(args: &[Input]) -> Result<String, String> {
    let vertex = point_at(args, 0, "dimangular")?;
    let p1 = point_at(args, 1, "dimangular")?;
    let p2 = point_at(args, 2, "dimangular")?;
    let radius = num_at(args, 3, "dimangular")?;
    Ok(format!(
        "dimangular {} {} {} {}",
        fmt(vertex),
        fmt(p1),
        fmt(p2),
        num(radius)
    ))
}

/// `[Objects(sel)] -> "dimradius <sel>"`
// Deferred from SCRIPTS (zero-step verb); tested in the test module below.
#[cfg_attr(not(test), allow(dead_code))]
fn assemble_dimradius(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "dimradius")?;
    Ok(format!("dimradius {sel}"))
}

/// `[Objects(sel)] -> "dimdiameter <sel>"`
// Deferred from SCRIPTS (zero-step verb); tested in the test module below.
#[cfg_attr(not(test), allow(dead_code))]
fn assemble_dimdiameter(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "dimdiameter")?;
    Ok(format!("dimdiameter {sel}"))
}

/// `[Objects(sel), Num(offset)] -> "autodim <sel> offset <offset>"`
fn assemble_autodim(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "autodim")?;
    let offset = num_at(args, 1, "autodim")?;
    Ok(format!("autodim {sel} offset {}", num(offset)))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guided::{GuidedTool, StartResult, StepResult};
    use glam::DVec3;

    // -- assembler unit tests -----------------------------------------------

    #[test]
    fn assemble_dim_emits_canonical_string() {
        let args = [
            Input::Point(DVec3::new(0.0, 0.0, 0.0)),
            Input::Point(DVec3::new(10.0, 0.0, 0.0)),
            Input::Num(0.5),
        ];
        assert_eq!(assemble_dim(&args).unwrap(), "dim 0,0 10,0 0.5");
        // custom offset
        let args2 = [
            Input::Point(DVec3::new(1.0, 2.0, 0.0)),
            Input::Point(DVec3::new(5.0, 2.0, 0.0)),
            Input::Num(0.8),
        ];
        assert_eq!(assemble_dim(&args2).unwrap(), "dim 1,2 5,2 0.8");
        // missing second point reports an error
        assert!(assemble_dim(&args[..1]).is_err());
    }

    #[test]
    fn assemble_dimangular_emits_canonical_string() {
        let args = [
            Input::Point(DVec3::new(0.0, 0.0, 0.0)),
            Input::Point(DVec3::new(1.0, 0.0, 0.0)),
            Input::Point(DVec3::new(0.0, 1.0, 0.0)),
            Input::Num(1.0),
        ];
        assert_eq!(assemble_dimangular(&args).unwrap(), "dimangular 0,0 1,0 0,1 1");
        // non-default radius
        let args2 = [
            Input::Point(DVec3::new(0.0, 0.0, 0.0)),
            Input::Point(DVec3::new(2.0, 0.0, 0.0)),
            Input::Point(DVec3::new(0.0, 2.0, 0.0)),
            Input::Num(2.5),
        ];
        assert_eq!(assemble_dimangular(&args2).unwrap(), "dimangular 0,0 2,0 0,2 2.5");
        assert!(assemble_dimangular(&args[..2]).is_err());
    }

    // assembler tests for deferred dimradius/dimdiameter — the assemblers are
    // correct and exercised here even though the verbs aren't in SCRIPTS yet.
    #[test]
    fn assemble_dimradius_and_dimdiameter_emit_canonical_strings() {
        let args = [Input::Objects("sel".into())];
        assert_eq!(assemble_dimradius(&args).unwrap(), "dimradius sel");
        assert_eq!(assemble_dimdiameter(&args).unwrap(), "dimdiameter sel");
        // no selection → error
        assert!(assemble_dimradius(&[]).is_err());
        assert!(assemble_dimdiameter(&[]).is_err());
    }

    #[test]
    fn assemble_autodim_emits_canonical_string() {
        let args = [Input::Objects("sel".into()), Input::Num(0.5)];
        assert_eq!(assemble_autodim(&args).unwrap(), "autodim sel offset 0.5");
        let args2 = [Input::Objects("last".into()), Input::Num(0.8)];
        assert_eq!(assemble_autodim(&args2).unwrap(), "autodim last offset 0.8");
        assert!(assemble_autodim(&[]).is_err());
    }

    // -- full GuidedTool walk for `dim` ------------------------------------

    #[test]
    fn guided_dim_walk_emits_canonical_string() {
        let mut t = GuidedTool::default();
        // dim needs no pre-selection
        assert_eq!(t.try_start("dim", None), StartResult::Started);
        assert!(t.active());

        // Step 0: first extension point
        assert!(t.current_is_point());
        assert_eq!(t.prompt().unwrap(), "First extension point (Esc cancels):");
        assert_eq!(t.on_click(DVec3::new(0.0, 0.0, 0.0)), StepResult::NeedMore);

        // Step 1: second extension point
        assert!(t.current_is_point());
        assert!(t.prompt().unwrap().starts_with("Second extension point"));
        assert_eq!(t.on_click(DVec3::new(10.0, 0.0, 0.0)), StepResult::NeedMore);

        // Step 2: offset — Number step
        assert!(!t.current_is_point());
        assert_eq!(t.prompt().unwrap(), "Dimension line offset <0.5>:"); // default shown
        // Accept the default by submitting empty
        assert_eq!(
            t.commit_typed(""),
            StepResult::Emit("dim 0,0 10,0 0.5".into())
        );
        assert!(!t.active());
    }

    #[test]
    fn guided_dim_custom_offset() {
        let mut t = GuidedTool::default();
        t.try_start("dim", None);
        t.on_click(DVec3::new(0.0, 0.0, 0.0));
        t.on_click(DVec3::new(5.0, 0.0, 0.0));
        assert_eq!(
            t.commit_typed("0.8"),
            StepResult::Emit("dim 0,0 5,0 0.8".into())
        );
    }

    #[test]
    fn guided_autodim_default_offset() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("autodim", Some("sel")), StartResult::Started);
        assert_eq!(t.prompt().unwrap(), "Dimension line offset <0.5>:");
        assert_eq!(
            t.commit_typed(""),
            StepResult::Emit("autodim sel offset 0.5".into())
        );
    }

    #[test]
    fn guided_autodim_custom_offset() {
        let mut t = GuidedTool::default();
        t.try_start("autodim", Some("all"));
        assert_eq!(
            t.commit_typed("0.8"),
            StepResult::Emit("autodim all offset 0.8".into())
        );
    }

    #[test]
    fn guided_dimangular_walk() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("dimangular", None), StartResult::Started);
        t.on_click(DVec3::new(0.0, 0.0, 0.0)); // vertex
        t.on_click(DVec3::new(1.0, 0.0, 0.0)); // p1
        t.on_click(DVec3::new(0.0, 1.0, 0.0)); // p2
        // default radius 1.0
        assert_eq!(
            t.commit_typed(""),
            StepResult::Emit("dimangular 0,0 1,0 0,1 1".into())
        );
    }
}
