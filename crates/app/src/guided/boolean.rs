// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Guided verb scripts — solid Boolean group.
//!
//! `union`/`intersect` fold a whole pre-selection (noun-verb, zero-step: emit on
//! start). `difference` has two roles — the body to keep and the cutter — so it
//! picks each interactively (Rhino's two-set boolean prompts).
//!
//!   union      — pre-select solids → `union sel`
//!   intersect  — pre-select solids → `intersect sel`
//!   difference — pick target, pick cutter → `difference #target #cutter`

use super::assemble::{obj_at, selector};
use super::{Input, ObjFilter, Step, VerbScript};

pub static SCRIPTS: &[VerbScript] = &[
    VerbScript {
        verb: "union",
        needs_selection: true,
        steps: &[],
        assemble: assemble_union,
    },
    VerbScript {
        verb: "intersect",
        needs_selection: true,
        steps: &[],
        assemble: assemble_intersect,
    },
    VerbScript {
        verb: "difference",
        needs_selection: false,
        steps: &[
            Step::SelectObject { prompt: "Select solid to subtract from", filter: ObjFilter::Solid, capture_point: false },
            Step::SelectObject { prompt: "Select cutting solid", filter: ObjFilter::Solid, capture_point: false },
        ],
        assemble: assemble_difference,
    },
];

/// `[Objects(sel)] -> "union sel"`
fn assemble_union(args: &[Input]) -> Result<String, String> {
    Ok(format!("union {}", selector(args, "union")?))
}

/// `[Objects(sel)] -> "intersect sel"`
fn assemble_intersect(args: &[Input]) -> Result<String, String> {
    Ok(format!("intersect {}", selector(args, "intersect")?))
}

/// `[Objects(#target), Objects(#cutter)] -> "difference #target #cutter"`
fn assemble_difference(args: &[Input]) -> Result<String, String> {
    let target = obj_at(args, 0, "difference")?;
    let cutter = obj_at(args, 1, "difference")?;
    Ok(format!("difference {target} {cutter}"))
}

#[cfg(test)]
mod tests {
    use super::super::{GuidedTool, StartResult, StepResult};
    use super::*;

    #[test]
    fn assemble_boolean_pure() {
        assert_eq!(
            assemble_union(&[Input::Objects("sel".into())]).unwrap(),
            "union sel"
        );
        assert_eq!(
            assemble_intersect(&[Input::Objects("sel".into())]).unwrap(),
            "intersect sel"
        );
        let d = [Input::Objects("#aaaa1111".into()), Input::Objects("#bbbb2222".into())];
        assert_eq!(assemble_difference(&d).unwrap(), "difference #aaaa1111 #bbbb2222");
        assert!(assemble_difference(&d[..1]).is_err());
    }

    #[test]
    fn union_emits_on_start_with_preselection() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("union", Some("sel")), StartResult::Started);
        assert_eq!(t.emit_if_ready(), Some(StepResult::Emit("union sel".into())));
    }

    #[test]
    fn union_without_selection_is_refused() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("union", None), StartResult::NeedSelection);
    }

    #[test]
    fn difference_picks_two_solids() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("difference", None), StartResult::Started);
        assert!(t.current_wants_object());
        assert_eq!(t.current_filter(), Some(ObjFilter::Solid));
        assert_eq!(t.commit_object("aaaa1111"), StepResult::NeedMore);
        assert_eq!(
            t.commit_object("bbbb2222"),
            StepResult::Emit("difference #aaaa1111 #bbbb2222".into())
        );
    }
}
