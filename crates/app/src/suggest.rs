//! Command-line autosuggestion engine.
//!
//! All public functions are pure and fully unit-tested.
//! The UI layer in `command_line.rs` calls these each frame.
//!
//! # Alias list
//! Aliases come from `parse.rs` match arms. Keep in sync when aliases change.
//! See: `crates/commands/src/parse.rs` match arms for polyline/pline,
//! rect/rectangle, difference/diff/subtract, intersect/intersection,
//! delete/del, polararray/parray.
//!
//! Legacy-CAD preset aliases (from `preset.rs`) are injected at call time
//! via the `active_preset_aliases` parameter added to the main entry point.

use mydrafter_commands::registry;

/// Canonical → one or more aliases recognised by the parser.
/// Update this list whenever parse.rs match arms change.
const ALIASES: &[(&str, &[&str])] = &[
    ("polyline", &["pline"]),
    ("rect", &["rectangle"]),
    ("difference", &["diff", "subtract"]),
    ("intersect", &["intersection"]),
    ("delete", &["del"]),
    ("polararray", &["parray"]),
];

/// App-level verbs handled by `execute_line` in `app.rs` (not in the
/// commands registry, so they need a separate entry point).
/// See: `crates/app/src/app.rs` — `execute_line` match arms.
pub const APP_VERBS: &[&str] = &[
    "help",
    "save",
    "open",
    "recover",
    "ze",
    "zoomextents",
    "display",
    "viewports",
    "vp",
    "top",
    "bottom",
    "front",
    "back",
    "left",
    "right",
    "persp",
    "perspective",
    "camera",
    "view",
    "template",
    "critique",
    "copyselection",
    "pasteselection",
];

/// A single autocomplete candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// The verb (or full first token) that should replace the current word.
    pub completion: String,
    /// Short usage hint, shown greyed out after the verb is fully typed.
    pub usage: Option<String>,
}

/// Returns whether the first argument position in `usage` expects a selector.
/// Detects strings that contain `<selector>` or `<sel>` (case-insensitive).
pub fn usage_needs_selector(usage: &str) -> bool {
    let lower = usage.to_lowercase();
    lower.contains("<selector>") || lower.contains("<sel>")
}

/// True when `input` has one complete token (the verb is done) and optionally
/// more — i.e. the user has already typed the verb and at least one space.
pub fn verb_is_complete(input: &str) -> bool {
    // At least one space exists after the first non-space character.
    let trimmed = input.trim_start();
    trimmed.contains(' ')
}

/// Return the verb part of `input` (first whitespace-delimited token).
pub fn verb_of(input: &str) -> &str {
    input.split_whitespace().next().unwrap_or("")
}

/// Standard selector words always available.
const SELECTOR_TOKENS: &[&str] = &["last", "all", "sel"];

/// Produce up to `limit` verb completions for `prefix`.
///
/// `preset_aliases` is the active legacy-CAD alias map from `preset::preset_for`.
/// Pass `&[]` when no preset is active.
///
/// Ordering: exact match before prefix match. Registry canonical names come
/// first, then parser aliases, then preset aliases, then app-only verbs.
/// All comparisons are case-insensitive.
pub fn verb_completions(
    prefix: &str,
    limit: usize,
    preset_aliases: &'static [(&'static str, &'static str)],
) -> Vec<Suggestion> {
    let prefix_lower = prefix.to_lowercase();

    // Build the full candidate list: registry names, their aliases, then app verbs.
    let mut candidates: Vec<Suggestion> = Vec::new();

    for spec in registry() {
        let name = spec.name;
        if name.starts_with(prefix_lower.as_str()) || prefix_lower.is_empty() {
            candidates.push(Suggestion {
                completion: name.to_string(),
                usage: Some(spec.usage.to_string()),
            });
        }
        // Parser-level aliases for this canonical name
        for alias_list in ALIASES.iter().filter(|(canon, _)| *canon == name) {
            for alias in alias_list.1.iter() {
                if alias.starts_with(prefix_lower.as_str()) {
                    candidates.push(Suggestion {
                        completion: alias.to_string(),
                        usage: Some(spec.usage.to_string()),
                    });
                }
            }
        }
    }

    // Preset (legacy-CAD) aliases — shown with a hint pointing at the canonical.
    for &(alias, canonical) in preset_aliases {
        if alias.starts_with(prefix_lower.as_str()) || prefix_lower.is_empty() {
            // Find usage for the canonical command if it's in the registry.
            let usage = registry()
                .iter()
                .find(|s| s.name == canonical)
                .map(|s| s.usage.to_string());
            candidates.push(Suggestion {
                completion: alias.to_string(),
                usage,
            });
        }
    }

    // App-only verbs (no usage hint available)
    for &verb in APP_VERBS {
        if verb.starts_with(prefix_lower.as_str()) {
            candidates.push(Suggestion {
                completion: verb.to_string(),
                usage: None,
            });
        }
    }

    // Deduplicate while preserving order.
    let mut seen = std::collections::HashSet::new();
    candidates.retain(|s| seen.insert(s.completion.clone()));

    // Exact matches first, then prefix matches.
    candidates.sort_by_key(|s| u8::from(s.completion.to_lowercase() != prefix_lower));

    candidates.truncate(limit);
    candidates
}

/// Suggest selector tokens + object names from the document.
///
/// `prefix` is the current incomplete token after the verb.
/// `object_names` is the deduplicated list of named objects in the document.
pub fn selector_completions(prefix: &str, object_names: &[String], limit: usize) -> Vec<Suggestion> {
    let prefix_lower = prefix.to_lowercase();
    let mut candidates: Vec<Suggestion> = Vec::new();

    for &tok in SELECTOR_TOKENS {
        if tok.starts_with(prefix_lower.as_str()) {
            candidates.push(Suggestion { completion: tok.to_string(), usage: None });
        }
    }
    for name in object_names {
        if name.to_lowercase().starts_with(prefix_lower.as_str()) {
            candidates.push(Suggestion { completion: name.clone(), usage: None });
        }
    }

    // Deduplicate
    let mut seen = std::collections::HashSet::new();
    candidates.retain(|s| seen.insert(s.completion.clone()));
    candidates.truncate(limit);
    candidates
}

/// Top-level entry: given the current `input` text and a list of named objects,
/// return suggestions appropriate for the cursor position.
///
/// `preset_aliases` — active legacy-CAD alias map (pass `&[]` for default).
///
/// - Empty / single incomplete token: verb completions (including preset aliases).
/// - Verb is complete AND usage expects a selector: selector completions for
///   the last token.
/// - Verb is complete but usage does NOT expect a selector: empty (no noise).
pub fn suggestions(
    input: &str,
    object_names: &[String],
    limit: usize,
    preset_aliases: &'static [(&'static str, &'static str)],
) -> Vec<Suggestion> {
    let trimmed = input.trim_start();
    if trimmed.is_empty() {
        return vec![];
    }

    if !verb_is_complete(input) {
        // Still typing the verb.
        return verb_completions(trimmed, limit, preset_aliases);
    }

    // Verb is done — find its usage string.
    let verb = verb_of(trimmed).to_lowercase();
    if let Some(spec) = registry().iter().find(|s| s.name == verb.as_str())
        && usage_needs_selector(spec.usage)
    {
        // Suggest selectors + object names for the last typed token.
        let last_token = trimmed.split_whitespace().last().unwrap_or("");
        // If the last char is a space, a new token has started — empty prefix.
        let prefix = if input.ends_with(' ') { "" } else { last_token };
        return selector_completions(prefix, object_names, limit);
    }

    vec![]
}

/// Lookup the usage string for a fully-typed verb (used for the hint line).
pub fn usage_for_verb(verb: &str) -> Option<&'static str> {
    let verb_lower = verb.to_lowercase();
    registry().iter().find(|s| s.name == verb_lower.as_str()).map(|s| s.usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── verb_completions ────────────────────────────────────────────────────

    #[test]
    fn prefix_box_returns_box() {
        let results = verb_completions("bo", 8, &[]);
        assert!(results.iter().any(|s| s.completion == "box"), "{results:?}");
    }

    #[test]
    fn prefix_b_returns_box_and_bbox() {
        let results = verb_completions("b", 8, &[]);
        let names: Vec<_> = results.iter().map(|s| s.completion.as_str()).collect();
        assert!(names.contains(&"box"), "{names:?}");
        assert!(names.contains(&"bbox"), "{names:?}");
    }

    #[test]
    fn exact_match_sorts_first() {
        let results = verb_completions("box", 8, &[]);
        assert_eq!(results[0].completion, "box", "{results:?}");
    }

    #[test]
    fn alias_pline_appears_for_pl_prefix() {
        let results = verb_completions("pl", 8, &[]);
        assert!(results.iter().any(|s| s.completion == "pline"), "{results:?}");
    }

    #[test]
    fn alias_diff_appears_for_di_prefix() {
        let results = verb_completions("di", 16, &[]);
        assert!(results.iter().any(|s| s.completion == "diff"), "{results:?}");
    }

    #[test]
    fn alias_del_appears_for_de_prefix() {
        let results = verb_completions("de", 16, &[]);
        assert!(results.iter().any(|s| s.completion == "del"), "{results:?}");
    }

    #[test]
    fn app_verb_ze_appears_for_z_prefix() {
        let results = verb_completions("z", 8, &[]);
        assert!(results.iter().any(|s| s.completion == "ze"), "{results:?}");
    }

    #[test]
    fn app_verb_help_appears_for_he_prefix() {
        let results = verb_completions("he", 8, &[]);
        assert!(results.iter().any(|s| s.completion == "help"), "{results:?}");
    }

    #[test]
    fn registry_verbs_carry_usage_hint() {
        let results = verb_completions("box", 4, &[]);
        let box_sugg = results.iter().find(|s| s.completion == "box").unwrap();
        assert!(box_sugg.usage.is_some(), "box should have a usage hint");
    }

    #[test]
    fn app_verb_has_no_usage_hint() {
        let results = verb_completions("ze", 4, &[]);
        let ze_sugg = results.iter().find(|s| s.completion == "ze").unwrap();
        assert!(ze_sugg.usage.is_none(), "app verb ze should have no usage hint");
    }

    #[test]
    fn empty_prefix_returns_empty() {
        // top-level suggestions fn returns empty for empty input
        let results = suggestions("", &[], 8, &[]);
        assert!(results.is_empty(), "{results:?}");
    }

    #[test]
    fn no_duplicate_completions() {
        let results = verb_completions("", 500, &[]);
        let mut seen = std::collections::HashSet::new();
        for s in &results {
            assert!(seen.insert(s.completion.clone()), "duplicate: {}", s.completion);
        }
    }

    // ── usage_needs_selector ────────────────────────────────────────────────

    #[test]
    fn usage_with_selector_tag_detected() {
        assert!(usage_needs_selector("extrude <selector> <height>"));
        assert!(usage_needs_selector("delete <selector>"));
        assert!(usage_needs_selector("move <selector> <delta x,y,z>"));
    }

    #[test]
    fn usage_without_selector_tag_not_detected() {
        assert!(!usage_needs_selector("box <corner x,y,z> <size x,y,z>"));
        assert!(!usage_needs_selector("line <a x,y,z> <b x,y,z>"));
        assert!(!usage_needs_selector("undo"));
    }

    #[test]
    fn selector_tag_case_insensitive() {
        assert!(usage_needs_selector("cmd <SELECTOR> value"));
        assert!(usage_needs_selector("cmd <Sel> value"));
    }

    // ── verb_is_complete ────────────────────────────────────────────────────

    #[test]
    fn verb_is_complete_when_space_after_verb() {
        assert!(verb_is_complete("extrude "));
        assert!(verb_is_complete("extrude last"));
        assert!(verb_is_complete("box 0,0,0 5,5,3"));
    }

    #[test]
    fn verb_is_not_complete_when_single_token() {
        assert!(!verb_is_complete("extrude"));
        assert!(!verb_is_complete("bo"));
    }

    // ── selector_completions ────────────────────────────────────────────────

    #[test]
    fn selector_completions_includes_last_all_sel() {
        let results = selector_completions("", &[], 8);
        let names: Vec<_> = results.iter().map(|s| s.completion.as_str()).collect();
        assert!(names.contains(&"last"), "{names:?}");
        assert!(names.contains(&"all"), "{names:?}");
        assert!(names.contains(&"sel"), "{names:?}");
    }

    #[test]
    fn selector_completions_includes_object_names() {
        let names: Vec<String> =
            ["tower-a", "slab", "core"].iter().map(|s| s.to_string()).collect();
        let results = selector_completions("", &names, 16);
        let completions: Vec<_> = results.iter().map(|s| s.completion.as_str()).collect();
        assert!(completions.contains(&"tower-a"), "{completions:?}");
        assert!(completions.contains(&"slab"), "{completions:?}");
    }

    #[test]
    fn selector_completions_prefix_filters() {
        let names: Vec<String> = ["tower-a", "slab"].iter().map(|s| s.to_string()).collect();
        let results = selector_completions("to", &names, 16);
        let completions: Vec<_> = results.iter().map(|s| s.completion.as_str()).collect();
        assert!(completions.contains(&"tower-a"), "{completions:?}");
        assert!(!completions.contains(&"slab"), "{completions:?}");
    }

    #[test]
    fn selector_prefix_l_returns_last() {
        let results = selector_completions("l", &[], 8);
        assert!(results.iter().any(|s| s.completion == "last"), "{results:?}");
    }

    // ── object-name suggestions via top-level fn ────────────────────────────

    #[test]
    fn suggestions_after_selector_verb_returns_object_names() {
        // "extrude " — verb complete, expects selector
        let object_names = vec!["tower".to_string(), "slab".to_string()];
        let results = suggestions("extrude ", &object_names, 8, &[]);
        let completions: Vec<_> = results.iter().map(|s| s.completion.as_str()).collect();
        assert!(completions.contains(&"last"), "{completions:?}");
        assert!(completions.contains(&"tower"), "{completions:?}");
    }

    #[test]
    fn suggestions_for_non_selector_verb_returns_empty() {
        // "box " — verb complete, no selector expected
        let results = suggestions("box ", &[], 8, &[]);
        assert!(results.is_empty(), "{results:?}");
    }

    #[test]
    fn suggestions_typing_verb_returns_verb_completions() {
        let results = suggestions("ext", &[], 8, &[]);
        assert!(results.iter().any(|s| s.completion == "extrude"), "{results:?}");
    }

    // ── preset alias suggestions ──────────────────────────────────────────────

    #[test]
    fn autocad_alias_l_appears_in_suggestions() {
        use crate::preset::AUTOCAD_ALIASES;
        let results = verb_completions("l", 20, AUTOCAD_ALIASES);
        let completions: Vec<_> = results.iter().map(|s| s.completion.as_str()).collect();
        assert!(completions.contains(&"l"), "AutoCAD alias 'l' should appear: {completions:?}");
    }

    #[test]
    fn autocad_alias_e_appears_in_suggestions() {
        use crate::preset::AUTOCAD_ALIASES;
        let results = verb_completions("e", 20, AUTOCAD_ALIASES);
        let completions: Vec<_> = results.iter().map(|s| s.completion.as_str()).collect();
        assert!(completions.contains(&"e"), "AutoCAD alias 'e' should appear: {completions:?}");
    }

    #[test]
    fn no_duplicates_with_preset_aliases() {
        use crate::preset::AUTOCAD_ALIASES;
        let results = verb_completions("", 500, AUTOCAD_ALIASES);
        let mut seen = std::collections::HashSet::new();
        for s in &results {
            assert!(seen.insert(s.completion.clone()), "duplicate: {}", s.completion);
        }
    }

    // ── usage_for_verb ──────────────────────────────────────────────────────

    #[test]
    fn usage_for_known_verb_returns_some() {
        let usage = usage_for_verb("box");
        assert!(usage.is_some(), "expected usage for 'box'");
        assert!(usage.unwrap().contains("box"), "{:?}", usage);
    }

    #[test]
    fn usage_for_unknown_verb_returns_none() {
        assert!(usage_for_verb("frobnicate").is_none());
    }

    #[test]
    fn usage_for_verb_case_insensitive() {
        assert_eq!(usage_for_verb("BOX"), usage_for_verb("box"));
    }
}
