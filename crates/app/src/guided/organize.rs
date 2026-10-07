// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Guided verb scripts — organize group (naming + layers).
//!
//!   name  — pre-select objects, type a name → `name sel <name>`
//!   layer — type a layer name (created/switched) → `layer <name>`
//!
//! Both use the free-text [`super::Step::Text`] step: the value is a single
//! whitespace-free token so it round-trips through the parser.

use super::assemble::{selector, text_at};
use super::{Input, Step, VerbScript};

pub static SCRIPTS: &[VerbScript] = &[
    VerbScript {
        verb: "name",
        needs_selection: true,
        steps: &[Step::Text { prompt: "Name" }],
        assemble: assemble_name,
    },
    VerbScript {
        verb: "layer",
        needs_selection: false,
        steps: &[Step::Text { prompt: "Layer name" }],
        assemble: assemble_layer,
    },
];

/// `[Objects(sel), Text(name)] -> "name sel <name>"`.
fn assemble_name(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "name")?;
    let name = text_at(args, 1, "name")?;
    Ok(format!("name {sel} {name}"))
}

/// `[Text(name)] -> "layer <name>"`.
fn assemble_layer(args: &[Input]) -> Result<String, String> {
    let name = text_at(args, 0, "layer")?;
    Ok(format!("layer {name}"))
}

#[cfg(test)]
mod tests {
    use super::super::{GuidedTool, StartResult, StepResult};
    use super::*;

    #[test]
    fn assemble_name_pure() {
        let args = [Input::Objects("sel".into()), Input::Text("widget".into())];
        assert_eq!(assemble_name(&args).unwrap(), "name sel widget");
        assert!(assemble_name(&args[..1]).is_err());
    }

    #[test]
    fn assemble_layer_pure() {
        let args = [Input::Text("walls".into())];
        assert_eq!(assemble_layer(&args).unwrap(), "layer walls");
        assert!(assemble_layer(&[]).is_err());
    }

    #[test]
    fn guided_name_walk() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("name", Some("sel")), StartResult::Started);
        assert!(t.current_wants_text());
        for c in "widget".chars() {
            assert!(t.push_input(c));
        }
        assert_eq!(t.commit_text(), StepResult::Emit("name sel widget".into()));
        assert!(!t.active());
    }

    #[test]
    fn name_without_selection_is_refused() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("name", None), StartResult::NeedSelection);
    }

    #[test]
    fn guided_layer_walk_no_selection_needed() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("layer", None), StartResult::Started);
        assert!(t.current_wants_text());
        for c in "walls".chars() {
            assert!(t.push_input(c));
        }
        assert_eq!(t.commit_text(), StepResult::Emit("layer walls".into()));
    }
}
