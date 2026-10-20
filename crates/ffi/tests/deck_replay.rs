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
