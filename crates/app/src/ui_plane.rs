// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! The UI/session tool plane: a SECOND tool group, distinct from the document
//! command substrate. Where document `Command`s are the drawing (op-logged,
//! replayed, undoable), these actions change only the *window layout* —
//! panel visibility, dock side, viewport split, workspace, theme. Layout is not
//! the drawing, so it must NEVER touch the op-log or be undoable; it persists to
//! `ui.json` instead.
//!
//! Keeping this a separate enum with its own dispatch (not a `Command`) is the
//! whole point: the deck can drive layout without any risk of a layout change
//! sneaking into the replayable document. The app reads the persisted values
//! from `ui.json` and reconciles its live widgets on the next frame.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Which side the docked panel lives on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DockSide {
    Left,
    Right,
}

/// Viewport split layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewportSplit {
    /// Single viewport.
    One,
    /// Two side-by-side viewports.
    Two,
    /// Four-up (plan/front/side/iso).
    Four,
}

impl ViewportSplit {
    pub fn count(self) -> u8 {
        match self {
            ViewportSplit::One => 1,
            ViewportSplit::Two => 2,
            ViewportSplit::Four => 4,
        }
    }
}

/// One UI-plane action. Distinct from any document `Command` — dispatched
/// through [`apply`] into `ui.json`, never through `Session::run`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "ui")]
pub enum UiAction {
    /// Show or hide the docked side panel.
    PanelVisible { visible: bool },
    /// Move the docked panel to a side.
    DockSide { side: DockSide },
    /// Set the viewport split (1 / 2 / 4).
    Split { split: ViewportSplit },
    /// Switch the active workspace by name (e.g. "model", "layout", "deck").
    Workspace { name: String },
    /// Set the theme ("dark" | "light").
    Theme { name: String },
}

impl UiAction {
    /// A one-line human summary for the chat/transcript.
    pub fn summary(&self) -> String {
        match self {
            UiAction::PanelVisible { visible } => {
                format!("panel {}", if *visible { "shown" } else { "hidden" })
            }
            UiAction::DockSide { side } => format!("dock {side:?}").to_lowercase(),
            UiAction::Split { split } => format!("viewport split {}", split.count()),
            UiAction::Workspace { name } => format!("workspace {name}"),
            UiAction::Theme { name } => format!("theme {name}"),
        }
    }
}

/// Parse a UI-plane action from a compact command line. Grammar (one per line):
///
/// - `panel show` / `panel hide`
/// - `dock left` / `dock right`
/// - `split 1` / `split 2` / `split 4`
/// - `workspace <name>`
/// - `theme dark` / `theme light`
///
/// Returns `Err(reason)` for anything unrecognized. Pure — no I/O.
pub fn parse_ui_action(line: &str) -> Result<UiAction, String> {
    let mut toks = line.split_whitespace();
    let verb = toks.next().ok_or_else(|| "empty ui action".to_string())?;
    let arg = toks.next();
    match verb {
        "panel" => match arg {
            Some("show") => Ok(UiAction::PanelVisible { visible: true }),
            Some("hide") => Ok(UiAction::PanelVisible { visible: false }),
            other => Err(format!("panel expects show|hide, got {other:?}")),
        },
        "dock" => match arg {
            Some("left") => Ok(UiAction::DockSide { side: DockSide::Left }),
            Some("right") => Ok(UiAction::DockSide { side: DockSide::Right }),
            other => Err(format!("dock expects left|right, got {other:?}")),
        },
        "split" => match arg {
            Some("1") => Ok(UiAction::Split { split: ViewportSplit::One }),
            Some("2") => Ok(UiAction::Split { split: ViewportSplit::Two }),
            Some("4") => Ok(UiAction::Split { split: ViewportSplit::Four }),
            other => Err(format!("split expects 1|2|4, got {other:?}")),
        },
        "workspace" => arg
            .map(|n| UiAction::Workspace { name: n.to_string() })
            .ok_or_else(|| "workspace expects a name".to_string()),
        "theme" => match arg {
            Some(n @ ("dark" | "light")) => Ok(UiAction::Theme { name: n.to_string() }),
            other => Err(format!("theme expects dark|light, got {other:?}")),
        },
        other => Err(format!("unknown ui action '{other}'")),
    }
}

/// Apply a UI-plane action into the `ui.json` object `value`, mutating only the
/// layout keys. This is the ENTIRE side effect of the UI plane — it writes
/// `ui.json` state and nothing else. The document/op-log is never touched.
///
/// Keys written mirror the ones the app already reads on `ui.json`:
/// `panel_visible`, `dock_side`, `viewport_split`, `workspace`, `theme`.
pub fn apply(value: &mut Value, action: &UiAction) {
    if !value.is_object() {
        *value = Value::Object(Default::default());
    }
    let obj = value.as_object_mut().expect("just ensured object");
    match action {
        UiAction::PanelVisible { visible } => {
            obj.insert("panel_visible".into(), Value::Bool(*visible));
        }
        UiAction::DockSide { side } => {
            let s = match side {
                DockSide::Left => "left",
                DockSide::Right => "right",
            };
            obj.insert("dock_side".into(), Value::String(s.into()));
        }
        UiAction::Split { split } => {
            obj.insert("viewport_split".into(), Value::from(split.count()));
        }
        UiAction::Workspace { name } => {
            obj.insert("workspace".into(), Value::String(name.clone()));
        }
        UiAction::Theme { name } => {
            obj.insert("theme".into(), Value::String(name.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use itsjustcad_commands::{parse, Session};

    #[test]
    fn parses_every_ui_verb() {
        assert_eq!(
            parse_ui_action("panel hide").unwrap(),
            UiAction::PanelVisible { visible: false }
        );
        assert_eq!(
            parse_ui_action("dock left").unwrap(),
            UiAction::DockSide { side: DockSide::Left }
        );
        assert_eq!(
            parse_ui_action("split 4").unwrap(),
            UiAction::Split { split: ViewportSplit::Four }
        );
        assert_eq!(
            parse_ui_action("workspace layout").unwrap(),
            UiAction::Workspace { name: "layout".into() }
        );
        assert_eq!(
            parse_ui_action("theme dark").unwrap(),
            UiAction::Theme { name: "dark".into() }
        );
    }

    #[test]
    fn rejects_unknown_or_malformed_actions() {
        assert!(parse_ui_action("").is_err());
        assert!(parse_ui_action("frobnicate").is_err());
        assert!(parse_ui_action("split 3").is_err());
        assert!(parse_ui_action("theme neon").is_err());
        assert!(parse_ui_action("panel").is_err());
    }

    #[test]
    fn ui_action_is_not_a_document_command() {
        // A UI-plane line must NOT parse as a document command: the two tool
        // groups are disjoint. `split 4` / `dock left` / `theme dark` are not
        // substrate verbs.
        for line in ["panel hide", "dock left", "split 4", "workspace layout", "theme dark"] {
            assert!(
                parse(line).is_err(),
                "'{line}' must not be a document command"
            );
        }
    }

    #[test]
    fn apply_writes_only_ui_json_keys() {
        let mut v = serde_json::json!({ "zoom": 1.3 });
        apply(&mut v, &UiAction::Split { split: ViewportSplit::Two });
        apply(&mut v, &UiAction::PanelVisible { visible: false });
        apply(&mut v, &UiAction::DockSide { side: DockSide::Left });
        apply(&mut v, &UiAction::Theme { name: "dark".into() });
        assert_eq!(v["viewport_split"], serde_json::json!(2));
        assert_eq!(v["panel_visible"], serde_json::json!(false));
        assert_eq!(v["dock_side"], serde_json::json!("left"));
        assert_eq!(v["theme"], serde_json::json!("dark"));
        // Pre-existing, unrelated keys are preserved.
        assert_eq!(v["zoom"], serde_json::json!(1.3));
    }

    #[test]
    fn ui_plane_action_leaves_the_document_and_op_log_unchanged() {
        // THE invariant: a UI-plane action changes ui.json, NOT the drawing.
        // Build a session, snapshot its op-log + document, run several UI
        // actions through the UI plane, and assert the op-log and document are
        // byte-for-byte unchanged (layout must never replay or undo).
        let mut session = Session::default();
        session.run(parse("box 0,0,0 5,5,3").unwrap()).unwrap();
        session.run(parse("circle 10,0,0 2").unwrap()).unwrap();
        let log_before = serde_json::to_string(&session.save_log()).unwrap();
        let doc_before = format!("{:?}", session.doc);
        let gen_before = session.doc.generation;

        // Drive the UI plane — the only side effect is on this ui.json Value.
        let mut ui = serde_json::json!({});
        for line in ["panel hide", "dock left", "split 4", "theme dark"] {
            let action = parse_ui_action(line).unwrap();
            apply(&mut ui, &action);
        }
        assert_eq!(ui["viewport_split"], serde_json::json!(4));

        // The document substrate is untouched: same op-log, same document,
        // same generation counter (no mutation happened at all).
        assert_eq!(
            serde_json::to_string(&session.save_log()).unwrap(),
            log_before,
            "op-log must be unchanged by a UI-plane action"
        );
        assert_eq!(format!("{:?}", session.doc), doc_before, "document unchanged");
        assert_eq!(session.doc.generation, gen_before, "no generation bump");
    }

    #[test]
    fn prompt_advertises_every_ui_action() {
        // COMPLETENESS (UI/session plane): every UiAction variant's verb must be
        // advertised in the deck system prompt, and every advertised token must
        // actually parse via `parse_ui_action` (never advertise dead syntax).
        // This is the third of the three plane-completeness tests; see the
        // 3-plane / 3-test invariant documented in `itsjustcad_deck::prompt`.
        let prompt =
            itsjustcad_deck::system_prompt("", &itsjustcad_commands::PluginRegistry::new());

        // The whole dedicated UI section is embedded verbatim.
        assert!(
            prompt.contains(itsjustcad_deck::UI_VERB_HELP),
            "UI_VERB_HELP not injected into the system prompt"
        );
        assert!(prompt.contains("## UI/session commands"));

        // Each variant's grammar line is present with the EXACT tokens
        // `parse_ui_action` accepts.
        for line in [
            "panel show|hide",
            "dock left|right",
            "split 1|2|4",
            "workspace <name>",
            "theme dark|light",
        ] {
            assert!(prompt.contains(line), "prompt missing UI grammar '{line}'");
        }

        // Every advertised example must actually parse — the model is never told
        // syntax the dispatcher rejects. This exercises all five UiAction verbs.
        for example in [
            "panel hide",
            "dock right",
            "split 4",
            "workspace layout",
            "theme dark",
        ] {
            assert!(
                parse_ui_action(example).is_ok(),
                "prompt advertises '{example}' but parse_ui_action rejects it"
            );
        }
    }

    #[test]
    fn action_json_round_trips() {
        let a = UiAction::Workspace { name: "deck".into() };
        let s = serde_json::to_string(&a).unwrap();
        let back: UiAction = serde_json::from_str(&s).unwrap();
        assert_eq!(a, back);
    }
}
