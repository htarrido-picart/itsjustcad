// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Offline prompt-replay eval of the FFI cloud deck (#105). No network.
//!
//!   cargo run -p itsjustcad-ffi --example deck_replay -- dump  --out <dir> [--only id,…] [--manifest <json>]
//!   cargo run -p itsjustcad-ffi --example deck_replay -- score --answers <dir> [--scenes <json>] [--only id,…] [--manifest <json>]
//!
//! `dump` writes `<dir>/prompts/<id>.md` (the exact `ijc_deck_send` request),
//! `<dir>/index.tsv`, `<dir>/scenes.json` (each prompt's object ids) and
//! `<dir>/REPLAY.md`. `score` reads `<answers>/<id>.txt` (the raw reply text),
//! maps the dumped ids to this run's (default `<answers>/../scenes.json`), and
//! prints `EVAL REPLAY` lines like the iOS cloud eval.
//! See `crates/ffi/tests/fixtures/REPLAY.md`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use itsjustcad_deck::estimate_tokens;
use itsjustcad_ffi::replay::{self, Entry};

const DEFAULT_MANIFEST: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/deck_eval_manifest.json"
);
const GUIDE: &str = include_str!("../tests/fixtures/REPLAY.md");

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1).cloned())
    };
    let Some(cmd) = args.first() else {
        eprintln!(
            "usage: deck_replay dump --out <dir> | score --answers <dir>  [--only id,…] [--manifest <json>]"
        );
        return ExitCode::from(2);
    };
    let manifest = flag("--manifest").unwrap_or_else(|| DEFAULT_MANIFEST.to_string());
    let (groups, entries) = match std::fs::read_to_string(&manifest)
        .map_err(|e| format!("{manifest}: {e}"))
        .and_then(|s| replay::load_manifest(&s))
    {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let only: Option<Vec<String>> =
        flag("--only").map(|s| s.split(',').map(|t| t.trim().to_string()).collect());
    let entries: Vec<Entry> = entries
        .into_iter()
        .filter(|e| only.as_ref().is_none_or(|o| o.contains(&e.id)))
        .collect();

    match cmd.as_str() {
        "dump" => {
            let Some(out) = flag("--out").map(PathBuf::from) else {
                eprintln!("dump needs --out <dir>");
                return ExitCode::from(2);
            };
            dump(&out, &entries)
        }
        "score" => {
            let Some(dir) = flag("--answers").map(PathBuf::from) else {
                eprintln!("score needs --answers <dir>");
                return ExitCode::from(2);
            };
            let scenes = flag("--scenes")
                .map(PathBuf::from)
                .unwrap_or_else(|| dir.join("../scenes.json"));
            score(&dir, &scenes, &groups, &entries)
        }
        other => {
            eprintln!("unknown subcommand {other:?} (dump | score)");
            ExitCode::from(2)
        }
    }
}

fn dump(out: &std::path::Path, entries: &[Entry]) -> ExitCode {
    let prompts = out.join("prompts");
    if let Err(e) = std::fs::create_dir_all(&prompts) {
        eprintln!("{}: {e}", prompts.display());
        return ExitCode::FAILURE;
    }
    let mut index = String::from("id\tgroup\tbytes\tapprox_tokens\tfile\tprompt\n");
    let (mut total, mut max) = (0usize, 0usize);
    let mut scenes = serde_json::Map::new();
    for e in entries {
        let t = replay::turn(e);
        scenes.insert(e.id.clone(), serde_json::json!(t.ids));
        let md = replay::prompt_markdown(e, &t);
        let file = prompts.join(format!("{}.md", e.id));
        if let Err(err) = std::fs::write(&file, &md) {
            eprintln!("{}: {err}", file.display());
            return ExitCode::FAILURE;
        }
        let tok = estimate_tokens(&t.system) + estimate_tokens(&t.user);
        total += tok;
        max = max.max(tok);
        index.push_str(&format!(
            "{}\t{}\t{}\t{}\tprompts/{}.md\t{}\n",
            e.id,
            e.group,
            md.len(),
            tok,
            e.id,
            e.prompt
        ));
    }
    let _ = std::fs::write(out.join("index.tsv"), index);
    let _ = std::fs::write(
        out.join("scenes.json"),
        serde_json::to_string_pretty(&serde_json::Value::Object(scenes)).unwrap_or_default(),
    );
    let _ = std::fs::write(out.join("REPLAY.md"), GUIDE);
    let _ = std::fs::create_dir_all(out.join("answers"));
    println!(
        "dumped {} prompt(s) to {} (approx tokens: avg {}, max {}, total {})",
        entries.len(),
        prompts.display(),
        total / entries.len().max(1),
        max,
        total
    );
    ExitCode::SUCCESS
}

fn one_line(s: &str) -> String {
    s.replace('\n', " ")
}

/// The dumped prompts' object ids, by prompt id (`scenes.json` from `dump`).
fn load_scenes(path: &std::path::Path) -> BTreeMap<String, Vec<String>> {
    let Ok(text) = std::fs::read_to_string(path) else {
        eprintln!(
            "[replay] no {} — ids in replies are not remapped (fine for replies that use last/all/names)",
            path.display()
        );
        return BTreeMap::new();
    };
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    v.as_object()
        .map(|m| {
            m.iter()
                .map(|(k, ids)| {
                    let ids = ids
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|s| s.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    (k.clone(), ids)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn score(
    dir: &std::path::Path,
    scenes: &std::path::Path,
    groups: &[String],
    entries: &[Entry],
) -> ExitCode {
    let scenes = load_scenes(scenes);
    // group → (passed, total, rejected)
    let mut per: BTreeMap<&str, (usize, usize, usize)> = BTreeMap::new();
    let (mut missing, mut unknown, mut unknown_ze, mut mismatches) = (0, 0, 0, 0);
    for e in entries {
        let path = dir.join(format!("{}.txt", e.id));
        let Ok(reply) = std::fs::read_to_string(&path) else {
            missing += 1;
            continue;
        };
        let v = replay::score(e, &reply, scenes.get(&e.id).map(Vec::as_slice));
        let slot = per.entry(e.group.as_str()).or_default();
        slot.1 += 1;
        slot.2 += v.rejected.len();
        if v.pass {
            slot.0 += 1;
        }
        for r in &v.rejected {
            if r.contains("unknown command") {
                unknown += 1;
                let line = r.trim_start_matches("Command rejected: ");
                if line.split_whitespace().next() == Some("ze") {
                    unknown_ze += 1;
                }
            }
        }
        if v.split_mismatch {
            mismatches += 1;
        }
        println!(
            "EVAL REPLAY {} {} prompt={}\nEVAL REPLAY   cmds={}\nEVAL REPLAY   problems={}",
            e.id,
            if v.pass { "PASS" } else { "FAIL" },
            e.prompt,
            v.commands.join(" ; "),
            one_line(&v.problems.join("; "))
        );
        if !v.run_errors.is_empty() {
            println!(
                "EVAL REPLAY   run_errors={}",
                one_line(&v.run_errors.join("; "))
            );
        }
    }
    let (mut passed, mut total, mut rejected) = (0, 0, 0);
    for g in groups {
        if let Some(&(p, t, r)) = per.get(g.as_str()) {
            println!("EVAL REPLAY SUMMARY {g} passed={p}/{t} rejected={r}");
            passed += p;
            total += t;
            rejected += r;
        }
    }
    println!(
        "EVAL REPLAY SUMMARY overall passed={passed}/{total} rejected={rejected} missing={missing}"
    );
    println!("EVAL REPLAY unknown-command rejections={unknown} (ze={unknown_ze})");
    if mismatches > 0 {
        println!(
            "EVAL REPLAY WARNING chunked extraction differed from whole in {mismatches} answer(s)"
        );
    }
    ExitCode::SUCCESS
}
