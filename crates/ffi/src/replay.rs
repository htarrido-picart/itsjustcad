// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Offline "prompt replay" eval of the FFI cloud deck (#105): no network, no
//! API credits. `dump` writes the exact request [`crate::ijc_deck_send`] would
//! send for each prompt of the iOS deck-eval manifest; someone (an agent
//! playing the model) writes the reply; `score` feeds each reply through the
//! same extractor, selector repair, `ijc_validate_command` check and run path,
//! then judges the scene the way the iOS cloud eval (`EvalJudge`) does.
//! Driven by `cargo run -p itsjustcad-ffi --example deck_replay`; see
//! `crates/ffi/tests/fixtures/REPLAY.md`.

use itsjustcad_commands::Session;
use itsjustcad_deck::{
    ChatMessage, ExtractEvent, Extractor, HereContext, Role, digest, normalize_selectors,
};
use serde_json::Value;

use crate::{PendingOp, check_line, deck_turn_context, history_from_json};

/// What the scene must look like after the turn (iOS `EvalPrompt.Check`).
#[derive(Debug, Clone, PartialEq)]
pub enum Check {
    Count(usize),
    Added(usize, Option<String>),
    Grew,
    Contains(String, Option<usize>),
    Changed(usize),
    HasMesh,
    Unchanged,
    Accepted,
    /// The reply asks a `QUESTION:` and emits no commands (#128).
    Question,
    /// Exactly one object added, its plan bbox (grown by [`NEAR_TOLERANCE`])
    /// containing this `x,y` point (#128).
    Near([f64; 2]),
    /// The reply's chat contains one of `any` and none of `not`, ignoring
    /// case (#133); commands are optional.
    Says { any: Vec<String>, not: Vec<String> },
}

/// How far (m) a [`Check::Near`] point may lie outside the new object's bbox.
pub const NEAR_TOLERANCE: f64 = 0.25;

/// One manifest prompt (iOS `EvalPrompt`), tagged `<group><index>`.
#[derive(Debug, Clone)]
pub struct Entry {
    pub id: String,
    pub group: String,
    pub prompt: String,
    pub setup: Vec<String>,
    pub verbs: Vec<String>,
    pub check: Check,
    /// Catalog sets also require the expected verbs (not tuning / heldout).
    pub require_verbs: bool,
    /// The host context `ijc_deck_set_context` would hold for this turn.
    pub here: Option<HereContext>,
    /// The chat thread `ijc_deck_set_history` would hold before this turn.
    pub history: Vec<ChatMessage>,
}

/// Parse `deck_eval_manifest.json` (from `scripts/eval_manifest_to_json.py`).
/// Returns the group order and the prompts in manifest order.
pub fn load_manifest(json: &str) -> Result<(Vec<String>, Vec<Entry>), String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("manifest: {e}"))?;
    let strs = |v: &Value| -> Vec<String> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    let groups = strs(&v["groups"]);
    let mut out = Vec::new();
    for p in v["prompts"].as_array().ok_or("manifest: no prompts")? {
        let s = |k: &str| {
            p[k].as_str()
                .map(String::from)
                .ok_or(format!("prompt lacks {k}"))
        };
        let c = &p["check"];
        let n = |k: &str| c[k].as_u64().map(|n| n as usize);
        let check = match c["kind"].as_str().unwrap_or("") {
            "count" => Check::Count(n("n").ok_or("count lacks n")?),
            "added" => Check::Added(
                n("n").ok_or("added lacks n")?,
                c["object_kind"].as_str().map(String::from),
            ),
            "grew" => Check::Grew,
            "contains" => Check::Contains(
                c["needle"]
                    .as_str()
                    .ok_or("contains lacks needle")?
                    .to_string(),
                n("count"),
            ),
            "changed" => Check::Changed(n("count").ok_or("changed lacks count")?),
            "hasMesh" => Check::HasMesh,
            "unchanged" => Check::Unchanged,
            "accepted" => Check::Accepted,
            "question" => Check::Question,
            "near" => Check::Near(
                serde_json::from_value(c["point"].clone()).map_err(|_| "near lacks point")?,
            ),
            "says" => Check::Says {
                any: strs(&c["any"]),
                not: strs(&c["not"]),
            },
            k => return Err(format!("unknown check kind {k:?}")),
        };
        out.push(Entry {
            id: s("id")?,
            group: s("group")?,
            prompt: s("prompt")?,
            setup: strs(&p["setup"]),
            verbs: strs(&p["verbs"]),
            check,
            require_verbs: p["require_verbs"].as_bool().unwrap_or(false),
            here: match &p["context"] {
                Value::Null => None,
                c => HereContext::from_json(&c.to_string())
                    .map_err(|e| format!("{}: context: {e}", p["id"]))?,
            },
            history: match &p["history"] {
                Value::Null => Vec::new(),
                h => history_from_json(&h.to_string())
                    .map_err(|e| format!("{}: history: {e}", p["id"]))?,
            },
        });
    }
    Ok((groups, out))
}

/// Run one line as the host does (`ijc_run_command` → queue → next frame's
/// `apply_pending`): the same check, then `session.run`; camera verbs touch
/// no document. Returns the run error, if any (the FFI ignores it).
fn run_line(session: &mut Session, line: &str) -> Result<(), String> {
    match check_line(line).map_err(|(e, _)| e)? {
        PendingOp::Cmd(cmd) => session.run(cmd).map(|_| ()).map_err(|e| e.to_string()),
        PendingOp::Camera(_) => Ok(()),
    }
}

/// A fresh document (`ijc_new_document`) after the prompt's setup lines.
pub fn setup_session(entry: &Entry) -> Session {
    let mut session = Session::default();
    for line in &entry.setup {
        if let Err(e) = run_line(&mut session, line) {
            eprintln!("[replay] {}: setup `{line}` failed: {e}", entry.id);
        }
    }
    session
}

/// The digest's object lines (the iOS eval's `objects()`).
pub fn objects(session: &Session) -> Vec<String> {
    digest(&session.doc)
        .lines()
        .filter(|l| l.starts_with("- "))
        .map(String::from)
        .collect()
}

/// The request [`crate::ijc_deck_send`] sends for `entry` on a fresh handle:
/// the system prompt (the scene digest is its last section), the seeded
/// history (`ijc_deck_set_history`; empty after `ijc_new_document`) and the
/// user message.
pub struct Turn {
    pub system: String,
    pub history: Vec<ChatMessage>,
    pub user: String,
    /// The scene's full object ids, in document order (for `score`).
    pub ids: Vec<String>,
}

pub fn turn(entry: &Entry) -> Turn {
    let session = setup_session(entry);
    let (system, ids) = deck_turn_context(&session, entry.here.as_ref(), &entry.prompt);
    Turn {
        system,
        history: entry.history.clone(),
        user: entry.prompt.clone(),
        ids,
    }
}

pub const SYSTEM_HEADER: &str = "=== SYSTEM PROMPT ===";
pub const USER_HEADER: &str = "=== USER MESSAGE ===";
/// The headers of the earlier turns, before [`USER_HEADER`].
pub const EARLIER_USER_HEADER: &str = "=== EARLIER USER MESSAGE ===";
pub const EARLIER_ASSISTANT_HEADER: &str = "=== EARLIER ASSISTANT MESSAGE ===";

/// The prompt file an answering agent reads.
pub fn prompt_markdown(entry: &Entry, t: &Turn) -> String {
    let turns = match t.history.len() {
        0 => "single turn".to_string(),
        n => format!("{n} earlier message(s)"),
    };
    let earlier: String = t
        .history
        .iter()
        .map(|m| {
            let header = match m.role {
                Role::User => EARLIER_USER_HEADER,
                Role::Assistant => EARLIER_ASSISTANT_HEADER,
            };
            format!("{header}\n{}\n", m.content)
        })
        .collect();
    format!(
        "<!-- deck replay {id}: exact ijc_deck_send request (max_tokens 4096, {turns}). \
         The current scene is the last section of the system prompt. -->\n\
         {SYSTEM_HEADER}\n{system}\n{earlier}{USER_HEADER}\n{user}\n",
        id = entry.id,
        system = t.system.trim_end(),
        user = t.user,
    )
}

/// The chat text of `reply` (everything outside the draft block).
fn chat_text(reply: &str) -> String {
    let mut ex = Extractor::default();
    let mut events = ex.push(reply);
    events.extend(ex.finish());
    events
        .into_iter()
        .filter_map(|ev| match ev {
            ExtractEvent::Chat(c) => Some(c),
            ExtractEvent::Command(_) => None,
        })
        .collect()
}

/// Feed `reply` through the FFI stream path: the [`Extractor`] in chunks of
/// `chunk` chars (`None` = whole), then `normalize_selectors` against the
/// scene ids at turn start. Returns the lines `CB_COMMAND` would carry.
pub fn extract_commands(reply: &str, object_ids: &[String], chunk: Option<usize>) -> Vec<String> {
    let mut ex = Extractor::default();
    let mut events = Vec::new();
    match chunk {
        None => events.extend(ex.push(reply)),
        Some(n) => {
            let chars: Vec<char> = reply.chars().collect();
            for piece in chars.chunks(n.max(1)) {
                events.extend(ex.push(&piece.iter().collect::<String>()));
            }
        }
    }
    events.extend(ex.finish());
    events
        .into_iter()
        .filter_map(|ev| match ev {
            ExtractEvent::Command(line) => Some(normalize_selectors(&line, object_ids)),
            ExtractEvent::Chat(_) => None,
        })
        .collect()
}

/// The setup objects' full ids (32 hex chars), in document order.
pub fn object_ids(session: &Session) -> Vec<String> {
    session
        .doc
        .objects()
        .map(|o| o.id.0.simple().to_string())
        .collect()
}

/// Rewrite references to the ids `old` (full, or the digest's 8-char short
/// form) as the matching ids of `new`, pairwise in document order.
pub fn remap_ids(reply: &str, old: &[String], new: &[String]) -> String {
    let mut out = reply.to_string();
    for (o, n) in old.iter().zip(new) {
        if o.len() >= 8 && n.len() >= 8 {
            out = out.replace(o.as_str(), n).replace(&o[..8], &n[..8]);
        }
    }
    out
}

/// The transcript row iOS shows for a line `ijc_validate_command` refuses
/// (`CommandCheck.rejection(of:)`), or `None` when the line is valid.
pub fn rejection(line: &str) -> Option<String> {
    let (error, usage) = check_line(line).err()?;
    let reason = if error.is_empty() {
        String::new()
    } else {
        format!(" — {error}")
    };
    let usage = if usage.is_empty() {
        String::new()
    } else {
        format!("\nUsage: {usage}")
    };
    Some(format!("Command rejected: {line}{reason}{usage}"))
}

/// iOS `EvalJudge.judge`: `None` = ok, else why the scene doesn't match.
pub fn judge_scene(check: &Check, before: &[String], after: &[String]) -> Option<String> {
    match check {
        Check::Count(n) => {
            (after.len() != *n).then(|| format!("expected {n} object(s), got {}", after.len()))
        }
        Check::Added(n, kind) => {
            if after.len() != before.len() + n {
                return Some(format!(
                    "expected +{n} object(s), got {}",
                    after.len() as i64 - before.len() as i64
                ));
            }
            match kind {
                Some(k) if !after.iter().any(|l| l.contains(&format!("- {k} "))) => {
                    Some(format!("no {k} in scene"))
                }
                _ => None,
            }
        }
        Check::Grew => (after.len() <= before.len()).then(|| "nothing was added".into()),
        Check::Contains(needle, n) => {
            if let Some(n) = n
                && after.len() != *n
            {
                return Some(format!("expected {n} object(s), got {}", after.len()));
            }
            (!after.join("\n").contains(needle.as_str())).then(|| format!("scene lacks {needle}"))
        }
        Check::Changed(n) => {
            if after.len() != *n {
                return Some(format!("expected {n} object(s), got {}", after.len()));
            }
            (before == after).then(|| "scene unchanged".into())
        }
        Check::HasMesh => {
            (!after.iter().any(|l| l.contains("- mesh "))).then(|| "no mesh produced".into())
        }
        Check::Unchanged => (before != after).then(|| "a query changed the scene".into()),
        Check::Accepted | Check::Question | Check::Says { .. } => None,
        Check::Near(p) => {
            let id = |l: &String| l.split_whitespace().nth(2).map(String::from);
            let old: Vec<_> = before.iter().filter_map(id).collect();
            let added: Vec<&String> = after
                .iter()
                .filter(|l| id(l).is_some_and(|i| !old.contains(&i)))
                .collect();
            if added.len() != 1 || after.len() != before.len() + 1 {
                return Some(format!("expected +1 object, got {}", added.len()));
            }
            let [lo, hi] = bbox(added[0])?;
            let inside = (0..2).all(|i| {
                p[i] >= lo[i] - NEAR_TOLERANCE && p[i] <= hi[i] + NEAR_TOLERANCE
            });
            (!inside).then(|| format!("{:?} is not at {p:?}", added[0]))
        }
    }
}

/// The `bbox [x,y,z]..[x,y,z]` of a digest object line.
fn bbox(line: &str) -> Option<[[f64; 3]; 2]> {
    let (lo, hi) = line.split_once("bbox ")?.1.split_once("..")?;
    let point = |s: &str| -> Option<[f64; 3]> {
        let v: Vec<f64> = s
            .trim()
            .trim_matches(['[', ']'])
            .split(',')
            .map(|n| n.trim().parse().ok())
            .collect::<Option<_>>()?;
        v.try_into().ok()
    };
    Some([point(lo)?, point(hi.split_whitespace().next()?)?])
}

/// The verdict on one reply.
#[derive(Debug, Clone)]
pub struct Verdict {
    pub id: String,
    pub pass: bool,
    /// Lines that passed the check and ran, in order (iOS command rows).
    pub commands: Vec<String>,
    /// iOS `Command rejected: …` rows.
    pub rejected: Vec<String>,
    pub problems: Vec<String>,
    /// Valid lines whose run failed (iOS shows them as no-op rows; not a failure).
    pub run_errors: Vec<String>,
    /// The streaming self-check: chunked extraction disagreed with whole.
    pub split_mismatch: bool,
}

/// Chunk sizes the extractor is replayed with; every one must give the same
/// lines as the whole reply.
pub const CHUNKS: &[usize] = &[1, 2, 3, 7, 16, 64];

/// Score one reply exactly like `CloudDeckEvalTests.score`: setup, stream the
/// reply, reject lines `ijc_validate_command` refuses, run the rest in order
/// (approval mode: held, approved at turn end), then the verb and scene checks.
///
/// `dumped_ids` are the setup objects' ids in the dumped prompt (object ids
/// are random per document): the reply's references to them are mapped to
/// this run's ids, as if the model had seen this run's scene.
pub fn score(entry: &Entry, reply: &str, dumped_ids: Option<&[String]>) -> Verdict {
    let mut session = setup_session(entry);
    let before = objects(&session);
    let (_, ids) = deck_turn_context(&session, entry.here.as_ref(), &entry.prompt);
    let reply = match dumped_ids {
        Some(old) => remap_ids(reply, old, &ids),
        None => reply.to_string(),
    };
    let reply = reply.as_str();

    let lines = extract_commands(reply, &ids, None);
    let split_mismatch = CHUNKS
        .iter()
        .any(|&n| extract_commands(reply, &ids, Some(n)) != lines);

    let mut commands = Vec::new();
    let mut rejected = Vec::new();
    let mut run_errors = Vec::new();
    for line in &lines {
        match rejection(line) {
            Some(why) => rejected.push(why),
            None => commands.push(line.clone()),
        }
    }
    for line in &commands {
        if let Err(e) = run_line(&mut session, line) {
            run_errors.push(format!("{line}: {e}"));
        }
    }
    let after = objects(&session);

    let mut problems: Vec<String> = rejected.clone();
    if split_mismatch {
        problems.push("chunked extraction differs from whole".into());
    }
    let asks = reply
        .lines()
        .any(|l| l.trim_start().starts_with("QUESTION:"));
    if entry.check == Check::Question {
        if !lines.is_empty() {
            problems.push("expected a question, got commands".into());
        } else if !asks {
            problems.push("no QUESTION".into());
        }
    } else if let Check::Says { any, not } = &entry.check {
        let chat = chat_text(reply).to_lowercase();
        let has = |n: &String| chat.contains(&n.to_lowercase());
        if !any.iter().any(has) {
            problems.push(format!("chat says none of {any:?}"));
        }
        if let Some(n) = not.iter().find(|n| has(n)) {
            problems.push(format!("chat says {n:?}"));
        }
    } else if commands.is_empty() && rejected.is_empty() {
        problems.push("no commands".into());
    }
    let emitted: std::collections::BTreeSet<&str> = commands
        .iter()
        .filter_map(|c| c.split(' ').next())
        .collect();
    if entry.require_verbs
        && problems.is_empty()
        && !entry.verbs.iter().all(|v| emitted.contains(v.as_str()))
    {
        problems.push(format!(
            "expected {}, emitted {:?}",
            entry.verbs.join("+"),
            emitted
        ));
    }
    if problems.is_empty()
        && let Some(reason) = judge_scene(&entry.check, &before, &after)
    {
        problems.push(reason);
    }
    Verdict {
        id: entry.id.clone(),
        pass: problems.is_empty(),
        commands,
        rejected,
        problems,
        run_errors,
        split_mismatch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(setup: &[&str], verbs: &[&str], check: Check, require: bool) -> Entry {
        Entry {
            id: "t0".into(),
            group: "t".into(),
            prompt: "p".into(),
            setup: setup.iter().map(|s| s.to_string()).collect(),
            verbs: verbs.iter().map(|s| s.to_string()).collect(),
            check,
            require_verbs: require,
            here: None,
            history: Vec::new(),
        }
    }

    #[test]
    fn bare_id_is_repaired_before_validation() {
        let e = entry(&["box 0,0,0 10,10,10"], &["move"], Check::Changed(1), true);
        let old = turn(&e).ids;
        let reply = format!("Moving it.\n```draft\nmove {} 0,0,5\n```\n", &old[0][..8]);
        let v = score(&e, &reply, Some(&old));
        assert!(v.pass, "{v:?}");
        assert!(v.commands[0].contains("id:"), "{:?}", v.commands);
    }

    #[test]
    fn dumped_ids_map_to_this_run() {
        let e = entry(&["box 0,0,0 10,10,10"], &["move"], Check::Changed(1), true);
        let old = turn(&e).ids;
        let reply = format!("Up.\n```draft\nmove id:{} 0,0,5\n```\n", &old[0][..8]);
        assert!(score(&e, &reply, Some(&old)).pass);
        assert!(!score(&e, &reply, None).pass, "stale id must not resolve");
    }

    #[test]
    fn rejection_text_matches_ios() {
        let r = rejection("ze2").unwrap();
        assert!(
            r.starts_with("Command rejected: ze2 — unknown command"),
            "{r}"
        );
        assert!(rejection("box 0,0,0 1,1,1").is_none());
    }
}
