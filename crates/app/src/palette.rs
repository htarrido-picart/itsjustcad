// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Command palette (⌘K): a fuzzy searcher over EVERY command the app knows —
//! all registry [`CommandSpec`] verbs plus the app-level verbs (camera / display
//! / lighting / view / zoom-extents) that `execute_line` owns but the substrate
//! parser does not.
//!
//! The heavy lifting in ItsJustCAD is the LLM deck and the command line; the
//! menu bar is deliberately minimal. The palette is the discoverability surface:
//! one keystroke, type a few characters, run any verb. It is registry-driven so
//! it never drifts from the actual command set.
//!
//! Everything here is pure and unit-tested: [`entries`] builds the candidate set,
//! [`search`] ranks a query against it (fuzzy subsequence + prefix bonus), and
//! [`enter_action`] decides whether Enter executes a no-arg verb outright or
//! prefills the command line for a verb that still needs arguments. The egui
//! overlay in `app.rs` is a thin renderer over these.

use itsjustcad_commands::registry;

use crate::menu::MenuAction;

/// One searchable command in the palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteEntry {
    /// The verb typed on the command line (also the primary match key).
    pub name: String,
    /// Group label shown weakly beside the name (registry category, or a synthetic
    /// group like "Camera" / "View" for app verbs).
    pub category: String,
    /// Usage / syntax hint shown in mono (empty when the verb takes no arguments).
    pub usage: String,
    /// One-line description, folded into the fuzzy match text.
    pub summary: String,
}

/// App-level verbs (owned by `execute_line`, not the substrate parser) surfaced
/// in the palette. Mirrors the set advertised to the deck (`app_verbs::classify`
/// / `suggest::APP_VERBS`) so the palette, the command line, and the LLM all see
/// the same vocabulary. `(name, group, usage)` — an empty usage means the verb
/// runs with no arguments (Enter executes it directly).
const APP_VERB_ENTRIES: &[(&str, &str, &str)] = &[
    // Zoom / framing.
    ("ze", "View", ""),
    ("zoomextents", "View", ""),
    // Standard views.
    ("top", "View", ""),
    ("bottom", "View", ""),
    ("front", "View", ""),
    ("back", "View", ""),
    ("left", "View", ""),
    ("right", "View", ""),
    ("persp", "View", ""),
    ("perspective", "View", ""),
    // Viewport layout.
    ("viewports", "View", "viewports 1|2|4"),
    // Display + lighting.
    ("display", "Display", "display shaded|wireframe|xray|ghosted|pencil"),
    ("lightmode", "Display", "lightmode working|sun|presentation"),
    ("sketchup", "Display", ""),
    ("sketchy", "Display", "sketchy [on|off]"),
    ("profileedges", "Display", "profileedges [on|off]"),
    ("shadededges", "Display", "shadededges [on|off]"),
    ("edgefx", "Display", "edgefx jitter=.. extension=.."),
    // Camera / lens.
    ("camera", "Camera", "camera 2point|persp|pano|fisheye [fov]|<n>mm|phone <lens>"),
    ("basemap", "View", "basemap [osm|sat] [span_m] [opacity] | off"),
    // App / session.
    ("help", "Help", "help [verb]"),
    ("save", "File", "save [path]"),
    ("open", "File", ""),
    ("template", "File", ""),
    ("critique", "Tools", "critique [note]"),
    ("reducemotion", "Tools", "reducemotion [on|off]"),
];

/// Build the full palette candidate set: every registry verb (name / category /
/// usage / summary) followed by the app-level verbs. Deduplicated by name — a
/// registry verb wins over an app-verb of the same name.
pub fn entries() -> Vec<PaletteEntry> {
    let mut out: Vec<PaletteEntry> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for spec in registry() {
        if seen.insert(spec.name.to_string()) {
            out.push(PaletteEntry {
                name: spec.name.to_string(),
                category: spec.category.key().to_string(),
                usage: spec.usage.to_string(),
                summary: spec.summary.to_string(),
            });
        }
    }
    for &(name, group, usage) in APP_VERB_ENTRIES {
        if seen.insert(name.to_string()) {
            out.push(PaletteEntry {
                name: name.to_string(),
                category: group.to_string(),
                usage: usage.to_string(),
                summary: String::new(),
            });
        }
    }
    out
}

/// Score `query` against a single haystack string using a case-insensitive
/// subsequence match. Returns `None` when the query is not a subsequence. Higher
/// is better. A run of consecutive matched characters and a match at the very
/// start (prefix) are rewarded, so `bo` ranks `box` above `bbox`.
fn fuzzy_score(query: &str, hay: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let hay_l = hay.to_lowercase();
    let hay_b = hay_l.as_bytes();
    let q = query.to_lowercase();
    let q_b = q.as_bytes();

    let mut qi = 0usize;
    let mut score = 0i32;
    let mut run = 0i32;
    let mut first_match: Option<usize> = None;
    for (hi, &hc) in hay_b.iter().enumerate() {
        if qi < q_b.len() && hc == q_b[qi] {
            if first_match.is_none() {
                first_match = Some(hi);
            }
            run += 1;
            // Reward consecutive matches (a contiguous substring scores highest).
            score += 2 + run;
            qi += 1;
        } else {
            run = 0;
        }
    }
    if qi != q_b.len() {
        return None; // not a subsequence
    }
    // Prefix bonus: matched from index 0.
    if first_match == Some(0) {
        score += 10;
    }
    // Whole-string exact match is best.
    if hay_l == q {
        score += 25;
    }
    // Shorter haystacks (tighter match) rank slightly higher.
    score -= (hay_b.len() as i32) / 8;
    Some(score)
}

/// Rank `query` against `entries`, best first. An empty query returns all entries
/// in their natural order (registry then app verbs). The match text is the verb
/// name (weighted highest), then category and summary, so typing part of a
/// description or group still finds the command.
pub fn search<'a>(query: &str, entries: &'a [PaletteEntry], limit: usize) -> Vec<&'a PaletteEntry> {
    let q = query.trim();
    if q.is_empty() {
        return entries.iter().take(limit).collect();
    }
    let mut scored: Vec<(i32, usize, &PaletteEntry)> = Vec::new();
    for (idx, e) in entries.iter().enumerate() {
        // Name match dominates; category/summary add fallback reach at a discount.
        let name = fuzzy_score(q, &e.name);
        let cat = fuzzy_score(q, &e.category).map(|s| s - 6);
        let sum = if e.summary.is_empty() {
            None
        } else {
            fuzzy_score(q, &e.summary).map(|s| s - 10)
        };
        let best = [name, cat, sum].into_iter().flatten().max();
        if let Some(score) = best {
            scored.push((score, idx, e));
        }
    }
    // Higher score first; stable by original index on ties.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().take(limit).map(|(_, _, e)| e).collect()
}

/// The [`MenuAction`] Enter dispatches for a chosen palette entry. A verb with a
/// required argument (its usage contains a `<placeholder>`) prefills the command
/// line as `"<verb> "` for the user to complete; a verb that needs nothing runs
/// immediately. Draw verbs start their interactive tool (via [`MenuAction`]),
/// import/export pop their native dialogs — reusing [`crate::menu::menu_action`]
/// for any registry verb keeps the palette and menu behaviour identical.
pub fn enter_action(entry: &PaletteEntry) -> MenuAction {
    // Registry verbs defer to the shared menu classification so draw tools,
    // import/export dialogs, and arg-vs-no-arg all behave exactly as the menu.
    let is_registry = registry().iter().any(|s| s.name == entry.name);
    if is_registry {
        return crate::menu::menu_action(&entry.name);
    }
    // App verbs: a usage with an angle-bracket placeholder needs typed args, so
    // prefill; otherwise execute the bare verb.
    if entry.usage.contains('<') {
        MenuAction::Insert(format!("{} ", entry.name))
    } else {
        MenuAction::Execute(entry.name.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_include_registry_and_app_verbs() {
        let es = entries();
        assert!(es.iter().any(|e| e.name == "box"), "registry verb box missing");
        assert!(es.iter().any(|e| e.name == "extrude"), "registry verb extrude missing");
        // App verbs advertised to the deck are present.
        assert!(es.iter().any(|e| e.name == "ze"), "app verb ze missing");
        assert!(es.iter().any(|e| e.name == "camera"), "app verb camera missing");
        assert!(es.iter().any(|e| e.name == "display"), "app verb display missing");
        assert!(es.iter().any(|e| e.name == "top"), "app verb top missing");
    }

    #[test]
    fn entries_have_no_duplicate_names() {
        let es = entries();
        let mut seen = std::collections::HashSet::new();
        for e in &es {
            assert!(seen.insert(e.name.clone()), "duplicate palette entry: {}", e.name);
        }
    }

    #[test]
    fn empty_query_returns_everything_capped() {
        let es = entries();
        let hits = search("", &es, 5);
        assert_eq!(hits.len(), 5);
        // Order preserved: first registry verb comes first.
        assert_eq!(hits[0].name, es[0].name);
    }

    #[test]
    fn prefix_query_ranks_exact_prefix_first() {
        let es = entries();
        let hits = search("box", &es, 8);
        assert_eq!(hits[0].name, "box", "'box' should rank first for query box");
    }

    #[test]
    fn fuzzy_subsequence_matches_noncontiguous() {
        // "exd" is a subsequence of "extrude" (e..x..d) — fuzzy should find it.
        let es = entries();
        let hits = search("exd", &es, 20);
        assert!(hits.iter().any(|e| e.name == "extrude"), "fuzzy exd → extrude: {hits:?}");
    }

    #[test]
    fn non_subsequence_query_excluded() {
        // A query whose letters are not a subsequence of a name/category/summary
        // must not match that entry.
        assert_eq!(fuzzy_score("zzq", "box"), None);
    }

    #[test]
    fn search_matches_by_category() {
        // Typing a group name surfaces verbs of that category even when the query
        // is not a subsequence of the verb name.
        let es = entries();
        let hits = search("camera", &es, 20);
        assert!(hits.iter().any(|e| e.name == "camera"));
    }

    #[test]
    fn enter_executes_no_arg_app_verb() {
        let ze = PaletteEntry {
            name: "ze".into(),
            category: "View".into(),
            usage: String::new(),
            summary: String::new(),
        };
        assert_eq!(enter_action(&ze), MenuAction::Execute("ze".into()));
        let top = PaletteEntry {
            name: "top".into(),
            category: "View".into(),
            usage: String::new(),
            summary: String::new(),
        };
        assert_eq!(enter_action(&top), MenuAction::Execute("top".into()));
    }

    #[test]
    fn enter_prefills_app_verb_needing_args() {
        // `camera` has a `<lens>` placeholder ⇒ prefill for the user to complete.
        let cam = entries().into_iter().find(|e| e.name == "camera").unwrap();
        assert_eq!(enter_action(&cam), MenuAction::Insert("camera ".into()));
    }

    #[test]
    fn enter_defers_registry_verbs_to_menu_action() {
        // A registry draw verb starts the draw tool; an arg registry verb prefills.
        let line = entries().into_iter().find(|e| e.name == "line").unwrap();
        assert_eq!(enter_action(&line), MenuAction::StartDraw("line".into()));
        let boxe = entries().into_iter().find(|e| e.name == "box").unwrap();
        assert_eq!(enter_action(&boxe), MenuAction::Insert("box ".into()));
    }
}
