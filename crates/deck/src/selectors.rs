// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Deterministic bare-id repair for deck command lines (#105). The scene
//! digest lists objects by short hex id, and models write that id bare
//! (`volume 59d76a88`) where the parser wants a selector. A bare token in a
//! selector position that names exactly one scene object becomes
//! `id:<token>` before the line is emitted.

use itsjustcad_commands::registry;

/// Shortest id prefix accepted as a reference (the digest prints 8).
const MIN_PREFIX: usize = 4;

/// One usage slot: whether it takes a selector, and its literal word if any.
#[derive(Debug)]
struct Slot {
    selector: bool,
    literal: Option<String>,
}

/// The first alternative of a usage string, split into slots after the verb.
/// `<..>` and `[..]` groups (nesting allowed) are one slot each.
fn slots(usage: &str) -> Vec<Slot> {
    let first = usage
        .split(" | ")
        .next()
        .unwrap_or(usage)
        .split(" · ")
        .next()
        .unwrap_or(usage);
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    let mut push = |cur: &mut String| {
        let t = std::mem::take(cur);
        if t.is_empty() {
            return;
        }
        let bracketed = t.starts_with('<') || t.starts_with('[');
        out.push(Slot {
            selector: t.to_ascii_lowercase().contains("selector"),
            literal: (!bracketed).then(|| t.clone()),
        });
    };
    for c in first.chars() {
        match c {
            '<' | '[' => {
                depth += 1;
                cur.push(c);
            }
            '>' | ']' => {
                depth -= 1;
                cur.push(c);
            }
            c if c.is_whitespace() && depth <= 0 => push(&mut cur),
            c => cur.push(c),
        }
    }
    push(&mut cur);
    if !out.is_empty() {
        out.remove(0); // the verb itself
    }
    out
}

fn is_hex(t: &str) -> bool {
    !t.is_empty() && t.chars().all(|c| c.is_ascii_hexdigit())
}

/// The ids (lowercase, no hyphens) that `token` names: an exact short or full
/// id, or a prefix of at least [`MIN_PREFIX`] chars.
fn id_hits<'a>(token: &str, ids: &'a [String]) -> Vec<&'a String> {
    let t = token.to_ascii_lowercase();
    if t.len() < MIN_PREFIX || !is_hex(&t) {
        return Vec::new();
    }
    ids.iter().filter(|id| id.starts_with(&t)).collect()
}

fn normalize_id(id: &str) -> String {
    id.chars()
        .filter(|c| *c != '-')
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Rewrite bare object ids in selector positions as `id:<token>`.
///
/// `object_ids` are the scene's object ids (full or short, hyphens ignored).
/// A token is rewritten when it is hex, at least four chars, names exactly one
/// object, and either sits in a usage slot that takes a selector or is that
/// object's whole short/full id (unambiguous anywhere in a verb that takes a
/// selector). Anything else — numbers, points, names, `last N`, existing
/// `id:`/`#` forms — is left untouched, as is a line whose verb is unknown.
pub fn normalize_selectors(line: &str, object_ids: &[String]) -> String {
    if object_ids.is_empty() {
        return line.to_string();
    }
    let toks: Vec<&str> = line.split_whitespace().collect();
    let Some((&verb, args)) = toks.split_first() else {
        return line.to_string();
    };
    let Some(spec) = registry()
        .iter()
        .find(|s| s.name.eq_ignore_ascii_case(verb))
    else {
        return line.to_string();
    };
    if !spec.usage.to_ascii_lowercase().contains("selector") {
        return line.to_string();
    }
    let ids: Vec<String> = object_ids.iter().map(|s| normalize_id(s)).collect();
    let slots = slots(spec.usage);

    let mut out: Vec<String> = vec![verb.to_string()];
    let mut slot = 0usize;
    let mut aligned = true;
    let mut i = 0usize;
    while i < args.len() {
        let arg = args[i];
        let in_selector_slot = aligned && slots.get(slot).is_some_and(|s| s.selector);
        if in_selector_slot
            && arg.eq_ignore_ascii_case("last")
            && args.get(i + 1).is_some_and(|n| n.parse::<usize>().is_ok())
        {
            out.push(arg.to_string());
            out.push(args[i + 1].to_string());
            i += 2;
            slot += 1;
            continue;
        }
        let hits = id_hits(arg, &ids);
        let whole_id = hits.len() == 1
            && (hits[0].len() == arg.len()
                || hits[0]
                    .get(..8)
                    .is_some_and(|s| s.eq_ignore_ascii_case(arg)));
        if hits.len() == 1 && (in_selector_slot || whole_id) {
            out.push(format!("id:{arg}"));
        } else {
            out.push(arg.to_string());
        }
        if aligned {
            match slots.get(slot) {
                Some(Slot {
                    literal: Some(l), ..
                }) if !l.eq_ignore_ascii_case(arg) => aligned = false,
                None => aligned = false,
                _ => {}
            }
        }
        slot += 1;
        i += 1;
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> Vec<String> {
        vec![
            "59d76a88-1111-4222-8333-444455556666".into(),
            "42038a51aaaabbbbccccddddeeeeffff".into(),
            "3ff7748f00000000000000000000000a".into(),
            "3ff7aaaa00000000000000000000000b".into(),
        ]
    }

    #[test]
    fn bare_ids_from_the_issue_become_id_selectors() {
        assert_eq!(
            normalize_selectors("volume 59d76a88", &ids()),
            "volume id:59d76a88"
        );
        assert_eq!(
            normalize_selectors("revolve 42038a51", &ids()),
            "revolve id:42038a51"
        );
        assert_eq!(
            normalize_selectors("rebuild 3ff7748f 20", &ids()),
            "rebuild id:3ff7748f 20"
        );
        assert_eq!(
            normalize_selectors("array 59d76a88 4,2,1 12,12,0", &ids()),
            "array id:59d76a88 4,2,1 12,12,0"
        );
        assert_eq!(
            normalize_selectors("hatch 59D76A88 concrete", &ids()),
            "hatch id:59D76A88 concrete"
        );
    }

    #[test]
    fn unique_prefix_in_a_selector_slot_is_rewritten() {
        assert_eq!(
            normalize_selectors("copy 4203 -12,0,0", &ids()),
            "copy id:4203 -12,0,0"
        );
        assert_eq!(
            normalize_selectors("difference 59d7 4203", &ids()),
            "difference id:59d7 id:4203"
        );
    }

    #[test]
    fn ambiguous_short_or_unknown_tokens_are_left_alone() {
        // `3ff7` prefixes two objects; `abc` is too short; `deadbeef` is no object.
        assert_eq!(normalize_selectors("volume 3ff7", &ids()), "volume 3ff7");
        assert_eq!(normalize_selectors("volume abc", &ids()), "volume abc");
        assert_eq!(
            normalize_selectors("volume deadbeef", &ids()),
            "volume deadbeef"
        );
    }

    #[test]
    fn real_selectors_numbers_and_points_are_untouched() {
        for line in [
            "volume last",
            "rebuild last 20",
            "move last 2 0,0,5",
            "copy all 1,0,0",
            "copy sel 1,0,0",
            "volume id:59d76a88",
            "volume #59d76a88",
            "name last tower",
            "box 0,0,0 5,5,3",
        ] {
            assert_eq!(normalize_selectors(line, &ids()), line);
        }
        // A count that merely prefixes an id stays a number outside a selector slot.
        assert_eq!(
            normalize_selectors("polararray last 4203 0,0,0", &ids()),
            "polararray last 4203 0,0,0"
        );
    }

    #[test]
    fn unknown_verbs_and_empty_scenes_pass_through() {
        assert_eq!(
            normalize_selectors("frobnicate 59d76a88", &ids()),
            "frobnicate 59d76a88"
        );
        assert_eq!(
            normalize_selectors("volume 59d76a88", &[]),
            "volume 59d76a88"
        );
    }

    #[test]
    fn rewritten_lines_parse() {
        for line in [
            "volume 59d76a88",
            "rebuild 3ff7748f 20",
            "copy 42038a51 -12,0,0",
            "polararray 59d76a88 6",
        ] {
            let fixed = normalize_selectors(line, &ids());
            assert!(
                itsjustcad_commands::parse(&fixed).is_ok(),
                "{fixed} must parse"
            );
            assert!(
                itsjustcad_commands::parse(line).is_err(),
                "{line} must not parse bare"
            );
        }
    }
}
