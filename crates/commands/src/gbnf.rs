// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Grammar-constrained decoding support.
//!
//! Small local models drift: they invent verbs, forget the ```draft fence, or
//! wrap commands in prose. A GBNF grammar (llama.cpp / `llama-grammar` syntax)
//! lets a local endpoint *constrain* sampling so the model can only emit our
//! real output shape: free prose, then one or more ```draft fenced blocks whose
//! lines start with a real command verb.
//!
//! Scope is deliberately pragmatic. We constrain the two things that are cheap
//! to get right and high-value: the **verb set** (derived from [`registry`] so
//! it never drifts) and the **fence structure**. We do *not* encode each
//! command's argument grammar — that is brittle and the existing parse-error
//! retry loop already recovers from argument mistakes. The verb + fence
//! constraint is the 80%: it guarantees real verbs inside real fences.

use crate::registry;

/// Parser-level aliases that are valid verbs but are not distinct `registry`
/// entries (the registry lists the canonical name only). Kept in sync with the
/// match arms in `parse.rs` by `gbnf_aliases_are_parseable` / the parser tests.
/// If you add an alias in `parse.rs`, add it here so the grammar admits it.
const ALIASES: &[&str] = &[
    "pline",        // polyline
    "rectangle",    // rect
    "interp",       // interpcurve
    "diff",         // difference
    "subtract",     // difference
    "intersection", // intersect
    "parray",       // polararray
    "del",          // delete
    "opt",          // option
    "deselect",     // selectnone
    "dist",         // distance
    "vol",          // volume
];

/// Every verb the grammar admits: canonical registry names plus parser aliases,
/// sorted and de-duplicated for a stable grammar string.
pub fn verbs() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = registry()
        .iter()
        .map(|c| c.name)
        .chain(ALIASES.iter().copied())
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// A GBNF grammar (llama.cpp syntax) matching the deck's output shape:
///
/// ```text
/// <free prose> ( ```draft\n <command line>+ ``` <free prose> )+
/// ```
///
/// where each command line begins with a real verb followed by a permissive
/// argument tail. Derived from [`registry`] so the verb set never drifts.
pub fn command_grammar() -> String {
    let verb_alts = verbs()
        .iter()
        .map(|v| format!("\"{v}\""))
        .collect::<Vec<_>>()
        .join(" | ");

    // GBNF notes:
    // - `root` is the entry rule (required by llama.cpp).
    // - The model may lead with prose, then emit >=1 draft block, each
    //   optionally followed by more prose.
    // - A draft block is the literal fence `` ```draft `` on its own line, one
    //   or more command lines, then a closing `` ``` `` fence.
    // - A command line is a verb, then a permissive tail of any non-newline
    //   chars (argument shapes are left to the parser + retry loop).
    // - `prose` excludes backtick to avoid ambiguity with the fence markers.
    format!(
        r#"root      ::= prose ( draft prose )+
draft     ::= "```draft" nl command-line+ "```" nl
command-line ::= verb tail nl
verb      ::= {verb_alts}
tail      ::= [^\n]*
prose     ::= prose-char*
prose-char ::= [^`]
nl        ::= "\n"
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn grammar_contains_every_registry_verb() {
        let g = command_grammar();
        for spec in registry() {
            assert!(
                g.contains(&format!("\"{}\"", spec.name)),
                "grammar is missing registry verb {:?}",
                spec.name
            );
        }
    }

    #[test]
    fn grammar_contains_every_alias() {
        let g = command_grammar();
        for a in ALIASES {
            assert!(
                g.contains(&format!("\"{a}\"")),
                "grammar is missing alias {a:?}"
            );
        }
    }

    #[test]
    fn grammar_has_fence_rules() {
        let g = command_grammar();
        assert!(g.contains("```draft"), "grammar lacks opening draft fence");
        assert!(g.contains("draft     ::="), "grammar lacks draft rule");
        assert!(g.contains("root      ::="), "grammar lacks root rule");
    }

    /// Structural balance check: every rule referenced on a right-hand side is
    /// defined on some left-hand side, and there are no obviously empty rules.
    /// This is a lightweight stand-in for a full GBNF parser.
    #[test]
    fn grammar_is_structurally_balanced() {
        let g = command_grammar();

        // Collect defined rule names (LHS of `name ::= ...`).
        let mut defined: HashSet<&str> = HashSet::new();
        for line in g.lines() {
            if let Some((lhs, _rhs)) = line.split_once("::=") {
                let name = lhs.trim();
                assert!(!name.is_empty(), "empty rule name in line {line:?}");
                defined.insert(name);
            }
        }
        assert!(defined.contains("root"), "GBNF requires a `root` rule");

        // Collect referenced identifiers from every RHS: bare words that are not
        // inside a "string literal", not char classes, not GBNF operators.
        for line in g.lines() {
            let Some((_lhs, rhs)) = line.split_once("::=") else {
                continue;
            };
            for ident in rule_refs(rhs) {
                assert!(
                    defined.contains(ident),
                    "undefined rule reference {ident:?} in line {line:?}"
                );
            }
        }
    }

    /// Extract rule-name references from a GBNF right-hand side, skipping string
    /// literals ("..."), char classes ([...]) and operators.
    fn rule_refs(rhs: &str) -> Vec<&str> {
        let mut refs = Vec::new();
        let bytes = rhs.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'"' => {
                    // Skip to closing quote.
                    i += 1;
                    while i < bytes.len() && bytes[i] != b'"' {
                        i += 1;
                    }
                    i += 1;
                }
                b'[' => {
                    // Skip char class to closing bracket.
                    while i < bytes.len() && bytes[i] != b']' {
                        i += 1;
                    }
                    i += 1;
                }
                b'a'..=b'z' | b'A'..=b'Z' => {
                    let start = i;
                    while i < bytes.len()
                        && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-' || bytes[i] == b'_')
                    {
                        i += 1;
                    }
                    refs.push(&rhs[start..i]);
                }
                _ => i += 1,
            }
        }
        refs
    }

    #[test]
    fn verbs_are_sorted_and_unique() {
        let v = verbs();
        let mut sorted = v.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(v, sorted, "verbs() must be sorted and de-duplicated");
    }
}
