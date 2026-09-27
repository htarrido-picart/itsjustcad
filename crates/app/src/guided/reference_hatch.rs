// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Guided verb scripts — reference/hatch group.
//!
//! Implemented:
//!   - `hatch`  — fill a closed curve with a hatch pattern (simple patterns only)
//!   - `ncopy`  — copy a nested sub-object out of a block/xref by index
//!   - `xclip`  — clip a block/xref instance to a rectangular boundary
//!   - `block`  — capture a pre-selection as a named block definition
//!   - `insert` — place an instance of a named block at a point

use super::assemble::{int_at, key_at, num_at, point_at, selector, text_at};
use super::{fmt, num, Input, Step, VerbScript};

pub static SCRIPTS: &[VerbScript] = &[
    VerbScript {
        verb: "hatch",
        needs_selection: true,
        steps: &[
            Step::Keyword {
                prompt: "Pattern",
                options: &["solid", "brick", "concrete", "insulation", "earth"],
                default: "solid",
            },
            Step::Number {
                prompt: "Spacing",
                default: Some(0.25),
            },
        ],
        assemble: assemble_hatch,
    },
    VerbScript {
        verb: "ncopy",
        needs_selection: true,
        steps: &[Step::Integer {
            prompt: "Sub-object index",
            default: Some(0),
        }],
        assemble: assemble_ncopy,
    },
    VerbScript {
        verb: "xclip",
        needs_selection: true,
        steps: &[
            Step::PickPoint {
                prompt: "Clip rectangle — first corner",
            },
            Step::PickPoint {
                prompt: "Clip rectangle — opposite corner",
            },
        ],
        assemble: assemble_xclip,
    },
    VerbScript {
        verb: "block",
        needs_selection: true,
        steps: &[Step::Text { prompt: "Block name" }],
        assemble: assemble_block,
    },
    VerbScript {
        verb: "insert",
        needs_selection: false,
        steps: &[
            Step::Text { prompt: "Block name" },
            Step::PickPoint { prompt: "Insertion point" },
        ],
        assemble: assemble_insert,
    },
];

/// `[Objects(sel), Key(pattern), Num(spacing)] -> "hatch sel solid" | "hatch sel <pattern> <spacing>"`.
/// `solid` takes no spacing argument.
fn assemble_hatch(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "hatch")?;
    let pat = key_at(args, 1, "hatch")?;
    if pat == "solid" {
        return Ok(format!("hatch {sel} solid"));
    }
    let spacing = num_at(args, 2, "hatch")?;
    Ok(format!("hatch {sel} {pat} {}", num(spacing)))
}

/// `[Objects(sel), Int(index)] -> "ncopy sel <index>"`.
fn assemble_ncopy(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "ncopy")?;
    let idx = int_at(args, 1, "ncopy")?;
    Ok(format!("ncopy {sel} {idx}"))
}

/// `[Objects(sel), Point(min), Point(max)] -> "xclip sel <min> <max>"`.
fn assemble_xclip(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "xclip")?;
    let min = point_at(args, 1, "xclip")?;
    let max = point_at(args, 2, "xclip")?;
    Ok(format!("xclip {sel} {} {}", fmt(min), fmt(max)))
}

/// `[Objects(sel), Text(name)] -> "block sel <name>"`.
fn assemble_block(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "block")?;
    let name = text_at(args, 1, "block")?;
    Ok(format!("block {sel} {name}"))
}

/// `[Text(name), Point(pos)] -> "insert <name> <pos>"`. Optional rotation/scale
/// are deferred (v1 places at the point with defaults).
fn assemble_insert(args: &[Input]) -> Result<String, String> {
    let name = text_at(args, 0, "insert")?;
    let pos = point_at(args, 1, "insert")?;
    Ok(format!("insert {name} {}", fmt(pos)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::DVec3;

    // ── hatch ────────────────────────────────────────────────────────────────

    #[test]
    fn assemble_hatch_solid_omits_spacing() {
        let args = [
            Input::Objects("sel".into()),
            Input::Key("solid".into()),
            Input::Num(0.25),
        ];
        assert_eq!(assemble_hatch(&args).unwrap(), "hatch sel solid");
    }

    #[test]
    fn assemble_hatch_brick_includes_spacing() {
        let args = [
            Input::Objects("sel".into()),
            Input::Key("brick".into()),
            Input::Num(0.3),
        ];
        assert_eq!(assemble_hatch(&args).unwrap(), "hatch sel brick 0.3");
    }

    #[test]
    fn assemble_hatch_missing_pattern_is_err() {
        let args = [Input::Objects("sel".into())];
        assert!(assemble_hatch(&args).is_err());
    }

    // ── ncopy ─────────────────────────────────────────────────────────────────

    #[test]
    fn assemble_ncopy_emits_correct_string() {
        let args = [Input::Objects("sel".into()), Input::Int(2)];
        assert_eq!(assemble_ncopy(&args).unwrap(), "ncopy sel 2");
    }

    #[test]
    fn assemble_ncopy_missing_index_is_err() {
        let args = [Input::Objects("sel".into())];
        assert!(assemble_ncopy(&args).is_err());
    }

    // ── xclip ────────────────────────────────────────────────────────────────

    #[test]
    fn assemble_xclip_emits_correct_string() {
        let args = [
            Input::Objects("sel".into()),
            Input::Point(DVec3::new(0.0, 0.0, 0.0)),
            Input::Point(DVec3::new(10.0, 10.0, 0.0)),
        ];
        assert_eq!(assemble_xclip(&args).unwrap(), "xclip sel 0,0 10,10");
    }

    #[test]
    fn assemble_xclip_missing_point_is_err() {
        let args = [
            Input::Objects("sel".into()),
            Input::Point(DVec3::new(0.0, 0.0, 0.0)),
        ];
        assert!(assemble_xclip(&args).is_err());
    }

    // ── GuidedTool walk (ncopy) ───────────────────────────────────────────────

    #[test]
    fn guided_ncopy_walk_emits_command() {
        use super::super::{GuidedTool, StartResult, StepResult};
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("ncopy", Some("sel")), StartResult::Started);
        assert!(t.prompt().unwrap().contains("Sub-object index"));
        assert_eq!(t.commit_typed("3"), StepResult::Emit("ncopy sel 3".into()));
        assert!(!t.active());
    }

    #[test]
    fn guided_ncopy_default_index_zero() {
        use super::super::{GuidedTool, StartResult, StepResult};
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("ncopy", Some("sel")), StartResult::Started);
        assert_eq!(t.commit_typed(""), StepResult::Emit("ncopy sel 0".into()));
    }

    // ── block ────────────────────────────────────────────────────────────────

    #[test]
    fn assemble_block_pure() {
        let args = [Input::Objects("sel".into()), Input::Text("door".into())];
        assert_eq!(assemble_block(&args).unwrap(), "block sel door");
        assert!(assemble_block(&args[..1]).is_err());
    }

    #[test]
    fn guided_block_walk() {
        use super::super::{GuidedTool, StartResult, StepResult};
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("block", Some("sel")), StartResult::Started);
        assert!(t.current_wants_text());
        for c in "door".chars() {
            assert!(t.push_input(c));
        }
        assert_eq!(t.commit_text(), StepResult::Emit("block sel door".into()));
    }

    #[test]
    fn block_without_selection_is_refused() {
        use super::super::{GuidedTool, StartResult};
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("block", None), StartResult::NeedSelection);
    }

    // ── insert ───────────────────────────────────────────────────────────────

    #[test]
    fn assemble_insert_pure() {
        let args = [Input::Text("door".into()), Input::Point(DVec3::new(3.0, 3.0, 0.0))];
        assert_eq!(assemble_insert(&args).unwrap(), "insert door 3,3");
        assert!(assemble_insert(&args[..1]).is_err());
    }

    #[test]
    fn guided_insert_walk() {
        use super::super::{GuidedTool, StartResult, StepResult};
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("insert", None), StartResult::Started);
        assert!(t.current_wants_text());
        for c in "door".chars() {
            assert!(t.push_input(c));
        }
        assert_eq!(t.commit_text(), StepResult::NeedMore);
        assert!(t.current_is_point());
        assert_eq!(
            t.on_click(DVec3::new(3.0, 3.0, 0.0)),
            StepResult::Emit("insert door 3,3".into())
        );
        assert!(!t.active());
    }
}
