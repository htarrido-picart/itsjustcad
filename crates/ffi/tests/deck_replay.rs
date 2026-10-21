// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Self-test of the offline deck replay eval (`examples/deck_replay.rs`): the
//! hand-written fixture replies in `tests/fixtures/deck_replay/answers` must
//! get the verdicts listed in `tests/fixtures/deck_replay/expected.tsv`.

use itsjustcad_ffi::replay::{self, Entry};

const MANIFEST: &str = include_str!("fixtures/deck_eval_manifest.json");
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/deck_replay");

fn manifest() -> Vec<Entry> {
    replay::load_manifest(MANIFEST).expect("manifest parses").1
}

fn entry(id: &str) -> Entry {
    manifest()
        .into_iter()
        .find(|e| e.id == id)
        .unwrap_or_else(|| panic!("no {id}"))
}

#[test]
fn manifest_has_the_ios_tags() {
    let (groups, entries) = replay::load_manifest(MANIFEST).unwrap();
    assert_eq!(entries.len(), 106);
    assert_eq!(groups.first().map(String::as_str), Some("tuning"));
    for id in [
        "tuning0",
        "heldout9",
        "catalogTuning24",
        "catalogHeldout17",
        "followHeldout5",
    ] {
        assert!(entries.iter().any(|e| e.id == id), "{id}");
    }
}

#[test]
fn fixture_answers_get_the_expected_verdicts() {
    let expected = std::fs::read_to_string(format!("{FIXTURES}/expected.tsv")).unwrap();
    let mut seen = 0;
    for row in expected
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
    {
        let mut cols = row.split('\t');
        let (id, verdict) = (cols.next().unwrap(), cols.next().unwrap());
        let reply = std::fs::read_to_string(format!("{FIXTURES}/answers/{id}.txt")).unwrap();
        let v = replay::score(&entry(id), &reply, None);
        assert_eq!(v.pass, verdict == "PASS", "{id}: {v:?}");
        assert!(!v.split_mismatch, "{id}: chunked extraction differs");
        seen += 1;
    }
    assert_eq!(seen, 5);
}

#[test]
fn bad_answers_fail_for_the_right_reason() {
    let read = |id: &str| std::fs::read_to_string(format!("{FIXTURES}/answers/{id}.txt")).unwrap();
    let bare = replay::score(&entry("catalogTuning20"), &read("catalogTuning20"), None);
    assert_eq!(bare.rejected.len(), 1);
    assert!(
        bare.rejected[0].contains("is not a selector"),
        "{:?}",
        bare.rejected
    );

    let ze = replay::score(&entry("heldout1"), &read("heldout1"), None);
    assert!(
        ze.rejected.is_empty(),
        "#107 routes `ze`: {:?}",
        ze.rejected
    );
    assert_eq!(ze.problems, ["expected +1 object(s), got 0"]);

    // A desktop-only view verb is still an unknown command.
    let zs = replay::score(
        &entry("tuning0"),
        "Box.\n```draft\nbox 0,0,0 10,10,10\nzs\n```\n",
        None,
    );
    assert!(!zs.pass);
    assert!(
        zs.rejected[0].contains("unknown command 'zs'"),
        "{:?}",
        zs.rejected
    );
}

#[test]
fn every_prompt_dumps_the_ffi_request() {
    for e in manifest() {
        let t = replay::turn(&e);
        let md = replay::prompt_markdown(&e, &t);
        assert!(md.contains(replay::SYSTEM_HEADER) && md.contains(replay::USER_HEADER));
        assert!(md.trim_end().ends_with(&e.prompt), "{}", e.id);
        assert_eq!(
            t.ids.len(),
            replay::objects(&replay::setup_session(&e)).len(),
            "{}",
            e.id
        );
        assert!(t.system.contains("## Current scene"), "{}", e.id);
    }
}

const HERE_MANIFEST: &str = include_str!("fixtures/deck_here_manifest.json");

fn here(id: &str) -> Entry {
    replay::load_manifest(HERE_MANIFEST)
        .expect("here manifest parses")
        .1
        .into_iter()
        .find(|e| e.id == id)
        .unwrap_or_else(|| panic!("no {id}"))
}

fn draft(chat: &str, lines: &[&str]) -> String {
    format!("{chat}\n```draft\n{}\n```\n", lines.join("\n"))
}

#[test]
fn here_cases_carry_their_context_line() {
    let (groups, entries) = replay::load_manifest(HERE_MANIFEST).unwrap();
    assert_eq!(groups, ["here"]);
    assert_eq!(entries.len(), 5);
    for (id, line) in [
        ("here0", "here: tap 4.0,2.0,0.0\n"),
        ("here1", "here: selection 10.0,3.0,1.5\n"),
        ("here2", "here: camera -6.0,8.0,0.0\n"),
        (
            "here3",
            "here: tap 4.0,2.0,0.0 | selection 10.0,3.0,1.5 | camera 0.0,0.0,0.0\n",
        ),
    ] {
        let s = replay::turn(&here(id)).system;
        assert!(s.contains(&format!("selectors)\n{line}")), "{id}");
        assert!(s.contains("more than 2 m apart"), "{id}");
    }
    // The iOS manifest has no context: no `here` line or rules.
    let s = replay::turn(&entry("tuning0")).system;
    assert!(!s.contains("here:") && !s.contains("more than 2 m apart"));
}

#[test]
fn here_cases_score_placement_and_the_question() {
    let at = |x: f64, y: f64| format!("box {},{},0 1,1,1", x - 0.5, y - 0.5);
    let pass = |id: &str, reply: &str| replay::score(&here(id), reply, None);
    for (id, x, y) in [
        ("here0", 4.0, 2.0),
        ("here1", 10.0, 3.0),
        ("here2", -6.0, 8.0),
        ("here4", 4.0, 2.0),
    ] {
        let v = pass(id, &draft("A box there.", &[&at(x, y)]));
        assert!(v.pass, "{id}: {v:?}");
    }
    // Away from the tap.
    let v = pass("here4", &draft("A box.", &[&at(8.0, 6.0)]));
    assert!(!v.pass && v.problems[0].contains("is not at"), "{v:?}");
    // At the origin instead of the tap.
    assert!(!pass("here0", &draft("A box.", &["box 0,0,0 1,1,1"])).pass);

    // Far candidates: a question, and no command.
    let q = "QUESTION: At the point you pressed, or at the selected column?\n";
    assert!(pass("here3", q).pass);
    let v = pass("here3", &draft("A box.", &[&at(4.0, 2.0)]));
    assert_eq!(v.problems, ["expected a question, got commands"]);
    assert_eq!(pass("here3", "Where?\n").problems, ["no QUESTION"]);
}

const HISTORY_MANIFEST: &str = include_str!("fixtures/deck_history_manifest.json");

fn history(id: &str) -> Entry {
    replay::load_manifest(HISTORY_MANIFEST)
        .expect("history manifest parses")
        .1
        .into_iter()
        .find(|e| e.id == id)
        .unwrap_or_else(|| panic!("no {id}"))
}

#[test]
fn history_cases_dump_the_stale_thread_and_the_live_scene() {
    let (groups, entries) = replay::load_manifest(HISTORY_MANIFEST).unwrap();
    assert_eq!(groups, ["history"]);
    assert_eq!(entries.len(), 2);
    for e in &entries {
        let t = replay::turn(e);
        let md = replay::prompt_markdown(e, &t);
        let stale = format!(
            "{}\nNo — the scene is empty and nothing is selected.\n{}\n",
            replay::EARLIER_ASSISTANT_HEADER,
            replay::USER_HEADER
        );
        assert!(md.contains(&stale), "{}: {md}", e.id);
        assert!(md.contains("2 earlier message(s)"), "{}", e.id);
        assert!(t.system.contains("selection: 1 object(s)"), "{}", e.id);
        assert!(t.system.contains("[SELECTED]"), "{}", e.id);
        assert!(t.system.contains("It overrides anything earlier"), "{}", e.id);
    }
    // The iOS cases have no history.
    let md = replay::prompt_markdown(&entry("tuning0"), &replay::turn(&entry("tuning0")));
    assert!(!md.contains("EARLIER") && md.contains("single turn"));
}

#[test]
fn history_cases_score_the_live_selection() {
    let says = |reply: &str| replay::score(&history("history0"), reply, None);
    assert!(says("Yes — one box is selected.\n").pass);
    let v = says("No — the scene is empty and nothing is selected.\n");
    assert!(!v.pass, "{v:?}");
    assert!(says("I see nothing.\n").problems[0].contains("says none of"));

    let up = |reply: &str| replay::score(&history("history1"), reply, None);
    assert!(up(&draft("Moving it up.", &["move sel 0,0,1"])).pass);
    let v = up("The scene is empty; there is nothing to move.\n");
    assert_eq!(v.problems, ["no commands"]);
}
