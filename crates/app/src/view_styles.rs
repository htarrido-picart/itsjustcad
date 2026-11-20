// SPDX-License-Identifier: AGPL-3.0-only

//! The `style` verb (#362): named viewport styles, built-in and saved.
//!
//! `style <name> [all]` applies a style to the active pane (`all`: every
//! pane). The display and colour modes are per pane; the rest of the style
//! (lighting, edges, fog, overlays, background) is app-wide. `style save
//! <name>` stores the current settings in the document through the logged
//! `StyleSave` op (replayed identically, offline), `style update <name>`
//! overwrites a saved one, `style delete <name>` removes it and `style list`
//! names them all. Built-ins are read-only and their names are reserved.
//!
//! Shared by the app and the headless runner; the app supplies the live
//! settings ([`ViewStyle`]) and applies the resolved one.

use itsjustcad_commands::Command;
use itsjustcad_doc::Document;
use itsjustcad_render::{BuiltinStyle, ViewStyle};

/// One `style` verb.
#[derive(Clone, Debug, PartialEq)]
pub enum StyleVerb {
    /// `style` / `style list`.
    List,
    /// `style <name> [all]`: apply to the active pane, or every pane.
    Apply { name: String, all: bool },
    /// `style save <name>`: save the current settings as a new style (an
    /// existing saved one is overwritten, like `view save`).
    Save(String),
    /// `style update <name>`: overwrite an existing saved style.
    Update(String),
    /// `style delete <name>`.
    Delete(String),
}

/// Usage of the `style` verb.
pub const STYLE_USAGE: &str = "usage: style <name> [all] · style save|update|delete <name> · style list";

/// Words that cannot name a saved style.
const RESERVED: [&str; 5] = ["save", "update", "delete", "list", "all"];

/// The words after `style`.
pub fn parse<'a>(mut words: impl Iterator<Item = &'a str>) -> Option<StyleVerb> {
    let first = words.next();
    let second = words.next();
    if words.next().is_some() {
        return None;
    }
    let lower = first.map(str::to_ascii_lowercase);
    Some(match (lower.as_deref(), second) {
        (None | Some("list"), None) => StyleVerb::List,
        (Some("save"), Some(n)) => StyleVerb::Save(n.to_string()),
        (Some("update"), Some(n)) => StyleVerb::Update(n.to_string()),
        (Some("delete"), Some(n)) => StyleVerb::Delete(n.to_string()),
        (Some(w), all) if !RESERVED.contains(&w) => StyleVerb::Apply {
            name: first?.to_string(),
            all: match all {
                None => false,
                Some(a) if a.eq_ignore_ascii_case("all") => true,
                Some(_) => return None,
            },
        },
        _ => return None,
    })
}

/// The saved style `name` decoded, if the document has it.
fn saved(doc: &Document, name: &str) -> Option<Result<ViewStyle, String>> {
    doc.named_styles.get(name).map(|v| {
        serde_json::from_value(v.clone()).map_err(|e| format!("style '{name}' is unreadable: {e}"))
    })
}

/// The style called `name`: a built-in, else one saved in the document.
pub fn resolve(doc: &Document, name: &str) -> Result<ViewStyle, String> {
    if let Some(b) = BuiltinStyle::parse(name) {
        return Ok(b.style());
    }
    saved(doc, name).unwrap_or_else(|| {
        Err(format!("no style '{name}' (styles: {}; save one with: style save {name})", names(doc).join(", ")))
    })
}

/// Every style name: the built-ins, then the saved ones (sorted).
pub fn names(doc: &Document) -> Vec<String> {
    BuiltinStyle::ALL
        .iter()
        .map(|b| b.name().to_string())
        .chain(doc.named_styles.keys().cloned())
        .collect()
}

/// The `style list` report.
pub fn list_line(doc: &Document, active: Option<&str>) -> String {
    let mark = |n: &str| if Some(n) == active { format!("{n} (active)") } else { n.to_string() };
    let builtins: Vec<String> = BuiltinStyle::ALL.iter().map(|b| mark(b.name())).collect();
    let saved: Vec<String> = doc.named_styles.keys().map(|n| mark(n)).collect();
    format!(
        "styles: {} · saved: {}",
        builtins.join(", "),
        if saved.is_empty() { "none (save one with: style save <name>)".into() } else { saved.join(", ") }
    )
}

/// The name of the style the settings `current` match: a saved one first
/// (the user's name for it), else a built-in. `None` when nothing matches.
pub fn active_name(doc: &Document, current: &ViewStyle) -> Option<String> {
    doc.named_styles
        .keys()
        .find(|n| matches!(saved(doc, n), Some(Ok(s)) if s == *current))
        .cloned()
        .or_else(|| BuiltinStyle::matching(current).map(|b| b.name().to_string()))
}

/// The logged op that saves `current` as `name` (`update`: only over an
/// existing saved style). Built-in and reserved names are refused.
pub fn save_command(doc: &Document, name: &str, update: bool, current: &ViewStyle) -> Result<Command, String> {
    if BuiltinStyle::parse(name).is_some() {
        return Err(format!("style '{name}' is built in and read-only; save under another name"));
    }
    if RESERVED.contains(&name.to_ascii_lowercase().as_str()) {
        return Err(format!("'{name}' is reserved; pick another style name"));
    }
    if update && !doc.named_styles.contains_key(name) {
        return Err(format!("style update: no saved style '{name}' (save it with: style save {name})"));
    }
    let style = serde_json::to_value(current).map_err(|e| e.to_string())?;
    Ok(Command::StyleSave { name: name.to_string(), style: Some(style) })
}

/// The logged op that deletes the saved style `name`.
pub fn delete_command(doc: &Document, name: &str) -> Result<Command, String> {
    if BuiltinStyle::parse(name).is_some() {
        return Err(format!("style '{name}' is built in and cannot be deleted"));
    }
    if !doc.named_styles.contains_key(name) {
        return Err(format!("style delete: no saved style '{name}'"));
    }
    Ok(Command::StyleDelete { name: name.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use itsjustcad_commands::Session;
    use itsjustcad_render::DisplayMode;

    #[test]
    fn parses_every_form() {
        let p = |s: &str| parse(s.split_whitespace());
        assert_eq!(p(""), Some(StyleVerb::List));
        assert_eq!(p("list"), Some(StyleVerb::List));
        assert_eq!(p("presentation"), Some(StyleVerb::Apply { name: "presentation".into(), all: false }));
        assert_eq!(p("Mine all"), Some(StyleVerb::Apply { name: "Mine".into(), all: true }));
        assert_eq!(p("save mine"), Some(StyleVerb::Save("mine".into())));
        assert_eq!(p("update mine"), Some(StyleVerb::Update("mine".into())));
        assert_eq!(p("delete mine"), Some(StyleVerb::Delete("mine".into())));
        for bad in ["save", "update", "delete", "all", "list x", "mine some", "save a b"] {
            assert_eq!(p(bad), None, "{bad}");
        }
    }

    #[test]
    fn save_apply_update_delete_round_trip() {
        let mut s = Session::default();
        let mine = ViewStyle { display: DisplayMode::Wireframe, grid: false, ..Default::default() };
        // Built-in and reserved names are refused; update needs a saved one.
        assert!(save_command(&s.doc, "sketch", false, &mine).unwrap_err().contains("built in"));
        assert!(save_command(&s.doc, "all", false, &mine).unwrap_err().contains("reserved"));
        assert!(save_command(&s.doc, "mine", true, &mine).unwrap_err().contains("no saved style"));
        s.run(save_command(&s.doc, "mine", false, &mine).unwrap()).unwrap();
        assert_eq!(resolve(&s.doc, "mine"), Ok(mine));
        assert_eq!(active_name(&s.doc, &mine).as_deref(), Some("mine"));
        assert!(list_line(&s.doc, Some("mine")).contains("mine (active)"));
        // Built-ins resolve without the document and win the active mark
        // only when no saved style matches.
        assert_eq!(resolve(&s.doc, "xray").unwrap().display, DisplayMode::XRay);
        assert_eq!(active_name(&s.doc, &ViewStyle::default()).as_deref(), Some("modelling"));
        let mine2 = ViewStyle { axes: false, ..mine };
        s.run(save_command(&s.doc, "mine", true, &mine2).unwrap()).unwrap();
        assert_eq!(resolve(&s.doc, "mine"), Ok(mine2));
        assert!(delete_command(&s.doc, "massing").is_err());
        s.run(delete_command(&s.doc, "mine").unwrap()).unwrap();
        assert!(resolve(&s.doc, "mine").unwrap_err().contains("no style 'mine'"));
        assert!(delete_command(&s.doc, "mine").is_err());
    }
}
