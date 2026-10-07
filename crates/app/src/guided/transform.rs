// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Guided verb scripts — transform group. See `offset.rs` for the reference shape.

use super::assemble::{key_at, num_at, point_at, selector};
use super::{Input, Step, VerbScript, fmt, num};

// ── move ────────────────────────────────────────────────────────────────────
//
// Parser:  `move sel <delta>`
// Guided:  From point, To point → emit `move sel <to-from>`

static MOVE_STEPS: &[Step] = &[
    Step::PickPoint { prompt: "From point" },
    Step::PickPoint { prompt: "To point" },
];

fn assemble_move(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "move")?;
    let from = point_at(args, 1, "move")?;
    let to = point_at(args, 2, "move")?;
    Ok(format!("move {sel} {}", fmt(to - from)))
}

// ── copy ────────────────────────────────────────────────────────────────────
//
// Parser:  `copy sel <delta>`
// Guided:  From point, To point → emit `copy sel <to-from>`

static COPY_STEPS: &[Step] = &[
    Step::PickPoint { prompt: "From point" },
    Step::PickPoint { prompt: "To point" },
];

fn assemble_copy(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "copy")?;
    let from = point_at(args, 1, "copy")?;
    let to = point_at(args, 2, "copy")?;
    Ok(format!("copy {sel} {}", fmt(to - from)))
}

// ── rotate ──────────────────────────────────────────────────────────────────
//
// Parser:  `rotate sel <angle> [x|y|z] [about <center>]`
// Guided:  PickPoint "Center of rotation", Number "Rotation angle in degrees"
//          → emit `rotate sel <angle> about <center>`
// (Axis defaults to z in the parser when omitted.)

static ROTATE_STEPS: &[Step] = &[
    Step::PickPoint { prompt: "Center of rotation" },
    Step::Number { prompt: "Rotation angle in degrees", default: None },
];

fn assemble_rotate(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "rotate")?;
    let center = point_at(args, 1, "rotate")?;
    let angle = num_at(args, 2, "rotate")?;
    Ok(format!("rotate {sel} {} about {}", num(angle), fmt(center)))
}

// ── scale ───────────────────────────────────────────────────────────────────
//
// Parser:  `scale sel <factor> [about <center>]`
// Guided:  PickPoint "Base point", Number "Scale factor" default 1.0
//          → emit `scale sel <factor> about <base>`

static SCALE_STEPS: &[Step] = &[
    Step::PickPoint { prompt: "Base point" },
    Step::Number { prompt: "Scale factor", default: Some(1.0) },
];

fn assemble_scale(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "scale")?;
    let base = point_at(args, 1, "scale")?;
    let factor = num_at(args, 2, "scale")?;
    Ok(format!("scale {sel} {} about {}", num(factor), fmt(base)))
}

// ── mirror ──────────────────────────────────────────────────────────────────
//
// Parser:  `mirror sel <point> <normal>`  (PointNormal plane form)
// Guided:  PickPoint "Point on mirror plane", PickPoint "Normal direction"
//          The normal direction pick is interpreted as a vector from the
//          origin (i.e. fmt(normal_pt) is passed verbatim as the normal
//          argument — this is what the parser stores and exec uses).

static MIRROR_STEPS: &[Step] = &[
    Step::PickPoint { prompt: "Point on mirror plane" },
    Step::PickPoint { prompt: "Normal direction" },
];

fn assemble_mirror(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "mirror")?;
    let plane_pt = point_at(args, 1, "mirror")?;
    let normal = point_at(args, 2, "mirror")?;
    Ok(format!("mirror {sel} {} {}", fmt(plane_pt), fmt(normal)))
}

// ── align ───────────────────────────────────────────────────────────────────
//
// Parser:  `align sel <src1> <tgt1> <src2> <tgt2> [scale on|off]`
// Guided:  PickPoint ×4, Keyword "Scale" ["on","off"] default "off"
//          → emit `align sel s1 t1 s2 t2 scale <on|off>`

static ALIGN_STEPS: &[Step] = &[
    Step::PickPoint { prompt: "Source point 1" },
    Step::PickPoint { prompt: "Target point 1" },
    Step::PickPoint { prompt: "Source point 2" },
    Step::PickPoint { prompt: "Target point 2" },
    Step::Keyword { prompt: "Scale", options: &["on", "off"], default: "off" },
];

fn assemble_align(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "align")?;
    let src1 = point_at(args, 1, "align")?;
    let tgt1 = point_at(args, 2, "align")?;
    let src2 = point_at(args, 3, "align")?;
    let tgt2 = point_at(args, 4, "align")?;
    let scale = key_at(args, 5, "align")?;
    Ok(format!("align {sel} {} {} {} {} scale {scale}", fmt(src1), fmt(tgt1), fmt(src2), fmt(tgt2)))
}

// ── stretch ─────────────────────────────────────────────────────────────────
//
// Parser:  `stretch sel <min> <max> <delta>`
// Guided:  PickPoint "Box min corner", PickPoint "Box max corner",
//          PickPoint "From point", PickPoint "To point"
//          → delta = To - From; emit `stretch sel <min> <max> <delta>`

static STRETCH_STEPS: &[Step] = &[
    Step::PickPoint { prompt: "Box min corner" },
    Step::PickPoint { prompt: "Box max corner" },
    Step::PickPoint { prompt: "From point (stretch base)" },
    Step::PickPoint { prompt: "To point (stretch target)" },
];

fn assemble_stretch(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "stretch")?;
    let min = point_at(args, 1, "stretch")?;
    let max = point_at(args, 2, "stretch")?;
    let from = point_at(args, 3, "stretch")?;
    let to = point_at(args, 4, "stretch")?;
    Ok(format!("stretch {sel} {} {} {}", fmt(min), fmt(max), fmt(to - from)))
}

// ── tozero / flatten ──────────────────────────────────────────────────────────
//
// Parser:  `tozero sel` / `flatten sel`
// Guided:  zero-step verbs on a pre-selection (emit-on-start), like `area`.

/// `[Objects(sel)] -> "tozero sel"`.
fn assemble_tozero(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "tozero")?;
    Ok(format!("tozero {sel}"))
}

/// `[Objects(sel)] -> "flatten sel"`.
fn assemble_flatten(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "flatten")?;
    Ok(format!("flatten {sel}"))
}

/// `[Objects(sel)] -> "freeze sel"`
fn assemble_freeze(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "freeze")?;
    Ok(format!("freeze {sel}"))
}

// ── SCRIPTS ─────────────────────────────────────────────────────────────────

pub static SCRIPTS: &[VerbScript] = &[
    VerbScript {
        verb: "move",
        needs_selection: true,
        steps: MOVE_STEPS,
        assemble: assemble_move,
    },
    VerbScript {
        verb: "copy",
        needs_selection: true,
        steps: COPY_STEPS,
        assemble: assemble_copy,
    },
    VerbScript {
        verb: "rotate",
        needs_selection: true,
        steps: ROTATE_STEPS,
        assemble: assemble_rotate,
    },
    VerbScript {
        verb: "scale",
        needs_selection: true,
        steps: SCALE_STEPS,
        assemble: assemble_scale,
    },
    VerbScript {
        verb: "mirror",
        needs_selection: true,
        steps: MIRROR_STEPS,
        assemble: assemble_mirror,
    },
    VerbScript {
        verb: "align",
        needs_selection: true,
        steps: ALIGN_STEPS,
        assemble: assemble_align,
    },
    VerbScript {
        verb: "stretch",
        needs_selection: true,
        steps: STRETCH_STEPS,
        assemble: assemble_stretch,
    },
    VerbScript {
        verb: "tozero",
        needs_selection: true,
        steps: &[],
        assemble: assemble_tozero,
    },
    VerbScript {
        verb: "flatten",
        needs_selection: true,
        steps: &[],
        assemble: assemble_flatten,
    },
    // Bake a parametric object to an editable mesh; consumes the selection so a
    // bare `freeze` with something selected just works (was: "expects a selector").
    VerbScript {
        verb: "freeze",
        needs_selection: true,
        steps: &[],
        assemble: assemble_freeze,
    },
];

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guided::{GuidedTool, Input, StartResult, StepResult};
    use glam::DVec3;

    // ── assembler unit tests ─────────────────────────────────────────────────

    #[test]
    fn assemble_move_delta() {
        let args = [
            Input::Objects("sel".into()),
            Input::Point(DVec3::new(1.0, 2.0, 0.0)),
            Input::Point(DVec3::new(4.0, 5.0, 0.0)),
        ];
        // delta = to - from = (3,3)
        assert_eq!(assemble_move(&args).unwrap(), "move sel 3,3");
        assert!(assemble_move(&args[..1]).is_err());
    }

    #[test]
    fn assemble_copy_delta() {
        let args = [
            Input::Objects("sel".into()),
            Input::Point(DVec3::new(0.0, 0.0, 0.0)),
            Input::Point(DVec3::new(5.0, 0.0, 0.0)),
        ];
        assert_eq!(assemble_copy(&args).unwrap(), "copy sel 5,0");
        assert!(assemble_copy(&args[..2]).is_err());
    }

    #[test]
    fn assemble_rotate_center_angle() {
        let args = [
            Input::Objects("sel".into()),
            Input::Point(DVec3::new(0.0, 0.0, 0.0)),
            Input::Num(90.0),
        ];
        assert_eq!(assemble_rotate(&args).unwrap(), "rotate sel 90 about 0,0");
        assert!(assemble_rotate(&args[..2]).is_err());
    }

    #[test]
    fn assemble_scale_base_factor() {
        let args = [
            Input::Objects("sel".into()),
            Input::Point(DVec3::new(1.0, 1.0, 0.0)),
            Input::Num(2.5),
        ];
        assert_eq!(assemble_scale(&args).unwrap(), "scale sel 2.5 about 1,1");
        assert!(assemble_scale(&args[..1]).is_err());
    }

    #[test]
    fn assemble_mirror_point_normal() {
        let args = [
            Input::Objects("sel".into()),
            Input::Point(DVec3::new(0.0, 5.0, 0.0)),
            Input::Point(DVec3::new(1.0, 0.0, 0.0)),
        ];
        assert_eq!(assemble_mirror(&args).unwrap(), "mirror sel 0,5 1,0");
        assert!(assemble_mirror(&args[..2]).is_err());
    }

    #[test]
    fn assemble_align_four_points_scale_off() {
        let args = [
            Input::Objects("sel".into()),
            Input::Point(DVec3::new(0.0, 0.0, 0.0)),
            Input::Point(DVec3::new(5.0, 5.0, 0.0)),
            Input::Point(DVec3::new(1.0, 0.0, 0.0)),
            Input::Point(DVec3::new(6.0, 5.0, 0.0)),
            Input::Key("off".into()),
        ];
        assert_eq!(
            assemble_align(&args).unwrap(),
            "align sel 0,0 5,5 1,0 6,5 scale off"
        );
        assert!(assemble_align(&args[..4]).is_err());
    }

    #[test]
    fn assemble_align_scale_on() {
        let args = [
            Input::Objects("sel".into()),
            Input::Point(DVec3::new(0.0, 0.0, 0.0)),
            Input::Point(DVec3::new(0.0, 0.0, 0.0)),
            Input::Point(DVec3::new(1.0, 0.0, 0.0)),
            Input::Point(DVec3::new(2.0, 0.0, 0.0)),
            Input::Key("on".into()),
        ];
        assert_eq!(
            assemble_align(&args).unwrap(),
            "align sel 0,0 0,0 1,0 2,0 scale on"
        );
    }

    #[test]
    fn assemble_stretch_from_to_delta() {
        let args = [
            Input::Objects("sel".into()),
            Input::Point(DVec3::new(0.0, 0.0, 0.0)),
            Input::Point(DVec3::new(5.0, 5.0, 0.0)),
            Input::Point(DVec3::new(2.0, 0.0, 0.0)),
            Input::Point(DVec3::new(3.0, 0.0, 0.0)),
        ];
        // delta = to - from = (1,0)
        assert_eq!(
            assemble_stretch(&args).unwrap(),
            "stretch sel 0,0 5,5 1,0"
        );
        assert!(assemble_stretch(&args[..3]).is_err());
    }

    // ── start-to-emit walk tests ─────────────────────────────────────────────

    #[test]
    fn move_guided_walk_start_to_emit() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("move", Some("sel")), StartResult::Started);
        assert!(t.active());
        assert!(t.prompt().unwrap().starts_with("From point"));
        assert!(t.current_is_point());
        // Step 1: from point.
        assert_eq!(t.on_click(DVec3::new(1.0, 2.0, 0.0)), StepResult::NeedMore);
        // Step 2: to point.
        assert!(t.current_is_point());
        assert!(t.prompt().unwrap().starts_with("To point"));
        let result = t.on_click(DVec3::new(4.0, 5.0, 0.0));
        assert_eq!(result, StepResult::Emit("move sel 3,3".into()));
        assert!(!t.active());
    }

    #[test]
    fn rotate_guided_walk_start_to_emit() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("rotate", Some("sel")), StartResult::Started);
        assert!(t.active());
        assert!(t.current_is_point());
        assert!(t.prompt().unwrap().starts_with("Center of rotation"));
        // Step 1: center.
        assert_eq!(t.on_click(DVec3::new(0.0, 0.0, 0.0)), StepResult::NeedMore);
        // Step 2: angle.
        assert!(!t.current_is_point());
        assert!(t.prompt().unwrap().starts_with("Rotation angle in degrees"));
        let result = t.commit_typed("45");
        assert_eq!(result, StepResult::Emit("rotate sel 45 about 0,0".into()));
        assert!(!t.active());
    }

    #[test]
    fn move_needs_selection() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("move", None), StartResult::NeedSelection);
        assert!(!t.active());
    }

    #[test]
    fn scale_default_factor_on_bare_enter() {
        let mut t = GuidedTool::default();
        t.try_start("scale", Some("sel"));
        // Step 1: base point.
        t.on_click(DVec3::new(0.0, 0.0, 0.0));
        // Step 2: factor — bare Enter accepts default 1.0.
        let result = t.commit_typed("");
        assert_eq!(result, StepResult::Emit("scale sel 1 about 0,0".into()));
    }

    #[test]
    fn rotate_requires_angle() {
        let mut t = GuidedTool::default();
        t.try_start("rotate", Some("sel"));
        t.on_click(DVec3::new(0.0, 0.0, 0.0));
        // No default → empty Enter is an error.
        assert!(matches!(t.commit_typed(""), StepResult::Error(_)));
        // Garbage is also an error.
        assert!(matches!(t.commit_typed("abc"), StepResult::Error(_)));
        // A valid number emits.
        assert_eq!(
            t.commit_typed("90"),
            StepResult::Emit("rotate sel 90 about 0,0".into())
        );
    }

    #[test]
    fn align_walks_four_points_then_keyword() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("align", Some("sel")), StartResult::Started);
        t.on_click(DVec3::new(0.0, 0.0, 0.0));
        t.on_click(DVec3::new(5.0, 5.0, 0.0));
        t.on_click(DVec3::new(1.0, 0.0, 0.0));
        t.on_click(DVec3::new(6.0, 5.0, 0.0));
        // Keyword step: default "off".
        assert!(t.prompt().unwrap().starts_with("Scale"));
        let result = t.commit_typed("");
        assert_eq!(result, StepResult::Emit("align sel 0,0 5,5 1,0 6,5 scale off".into()));
    }

    #[test]
    fn copy_3d_delta_preserves_z() {
        let mut t = GuidedTool::default();
        t.try_start("copy", Some("sel"));
        t.on_click(DVec3::new(0.0, 0.0, 0.0));
        let result = t.on_click(DVec3::new(1.0, 2.0, 3.0));
        assert_eq!(result, StepResult::Emit("copy sel 1,2,3".into()));
    }

    #[test]
    fn stretch_walk_start_to_emit() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("stretch", Some("sel")), StartResult::Started);
        t.on_click(DVec3::new(0.0, 0.0, 0.0)); // min corner
        t.on_click(DVec3::new(5.0, 5.0, 0.0)); // max corner
        t.on_click(DVec3::new(2.0, 0.0, 0.0)); // from
        let result = t.on_click(DVec3::new(4.0, 0.0, 0.0)); // to → delta (2,0)
        assert_eq!(result, StepResult::Emit("stretch sel 0,0 5,5 2,0".into()));
    }

    // ── tozero / flatten (zero-step, emit-on-start) ──────────────────────────

    #[test]
    fn assemble_tozero_and_flatten_pure() {
        let args = [Input::Objects("sel".into())];
        assert_eq!(assemble_tozero(&args).unwrap(), "tozero sel");
        assert_eq!(assemble_flatten(&args).unwrap(), "flatten sel");
        assert_eq!(assemble_freeze(&args).unwrap(), "freeze sel");
        assert!(assemble_tozero(&[]).is_err());
        assert!(assemble_flatten(&[]).is_err());
        assert!(assemble_freeze(&[]).is_err());
    }

    #[test]
    fn freeze_emits_on_start_with_selection() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("freeze", Some("sel")), StartResult::Started);
        assert_eq!(t.emit_if_ready(), Some(StepResult::Emit("freeze sel".into())));
        // Bare freeze with nothing selected asks for a selection instead of erroring.
        assert_eq!(t.try_start("freeze", None), StartResult::NeedSelection);
    }

    #[test]
    fn tozero_emits_on_start() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("tozero", Some("sel")), StartResult::Started);
        assert_eq!(t.emit_if_ready(), Some(StepResult::Emit("tozero sel".into())));
        assert!(!t.active());
    }

    #[test]
    fn flatten_emits_on_start() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("flatten", Some("sel")), StartResult::Started);
        assert_eq!(t.emit_if_ready(), Some(StepResult::Emit("flatten sel".into())));
        assert!(!t.active());
    }

    #[test]
    fn tozero_and_flatten_need_selection() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("tozero", None), StartResult::NeedSelection);
        assert_eq!(t.try_start("flatten", None), StartResult::NeedSelection);
    }
}
