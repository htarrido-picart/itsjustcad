// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart
//
// Comprehensive, data-driven coverage of the command-line command surface: for
// EVERY entry in `registry()`, take the documented `Example:` from its summary
// and run it through the real `parse` → `Session::run` pipeline, one command at
// a time. This complements the ~215 hand-written per-command execution tests in
// `exec.rs`: it auto-covers new commands (a fresh `CommandSpec` is exercised the
// moment it lands) and guarantees no documented example can ever panic.
//
// It deliberately does NOT assert `Ok` for every command: many examples
// reference per-command context (named objects like `prof`/`rail`, a defined
// `material`/`section`, a live viewport for `view save`, or the opt-in
// `kernel-occt` feature for STEP). Seeding all of that generically is not
// possible, so the universal contract enforced here is *panic-freedom* — a
// crash is a bug, a graceful `Err` is not. Commands whose example runs against
// an empty document are additionally asserted to succeed.

use itsjustcad_commands::{parse, registry, Session};
use std::panic::{self, AssertUnwindSafe};

/// Extract the first alternative of the documented example from a spec summary.
/// Summaries embed `… Example: <a> · <b> · <c>`; we take `<a>`. Returns `None`
/// for commands that document no example (e.g. `undo`, `selectnone`).
fn first_example(summary: &str) -> Option<String> {
    // Summaries use either "Example:" or "Examples:"; find the label, then skip
    // to the text after the following ":".
    let label = summary.find("Example")?;
    let colon = summary[label..].find(':')? + label;
    let tail = summary[colon + 1..].trim();
    let first = tail.split(" · ").next().unwrap_or(tail).trim();
    (!first.is_empty()).then(|| first.to_string())
}

/// An example may be a sequence joined by " then " (e.g. `box … then array …`).
/// Split it into individual command lines, in order.
fn clauses(example: &str) -> Vec<String> {
    example
        .split(" then ")
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect()
}

/// Run every clause of `example` on a fresh session. Returns `Ok(())` if every
/// clause that PARSES as a registry `Command` also executes without error;
/// parse failures are treated as "not a registry command line" (app-verb-only
/// examples such as `camera 2point` parse to `Err`) and skipped, not failed.
fn try_run(example: &str) -> Result<(), String> {
    let mut s = Session::default();
    for clause in clauses(example) {
        match parse(&clause) {
            Ok(cmd) => s
                .run(cmd)
                .map_err(|e| format!("`{clause}` → {e}"))?,
            // Not a substrate command (app-verb / view verb) — nothing to run.
            Err(_) => return Ok(()),
        };
    }
    Ok(())
}

#[test]
fn every_registry_example_is_panic_free() {
    // Silence panic backtraces during the sweep; we report the offenders
    // ourselves via the collected list.
    let prev = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    let mut panicked = Vec::new();
    for spec in registry() {
        let Some(example) = first_example(spec.summary) else {
            continue;
        };
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
            // Ignore the Result here — a graceful Err is acceptable; we are
            // only proving nothing panics.
            let _ = try_run(&example);
        }));
        if outcome.is_err() {
            panicked.push(format!("{}: `{}`", spec.name, example));
        }
    }

    panic::set_hook(prev);
    assert!(
        panicked.is_empty(),
        "these commands PANICKED on their documented example (must be graceful \
         Ok/Err, never a crash):\n  {}",
        panicked.join("\n  ")
    );
}

#[test]
fn every_registry_command_has_a_documented_example() {
    // Advertisement/quality gate: a registry command with no `Example:` in its
    // summary is undiscoverable to the deck LLM and to users. A short allowlist
    // covers argument-free verbs whose name IS the whole invocation.
    const NO_EXAMPLE_OK: &[&str] = &[
        "undo", "redo", "selectnone", "sunoff", "underlayoff", "blocks",
        "blocklib",
    ];
    let mut missing = Vec::new();
    for spec in registry() {
        if first_example(spec.summary).is_none() && !NO_EXAMPLE_OK.contains(&spec.name) {
            missing.push(spec.name);
        }
    }
    assert!(
        missing.is_empty(),
        "registry commands missing a documented `Example:` (add one, or allowlist \
         if argument-free): {missing:?}"
    );
}

#[test]
fn context_free_examples_execute_cleanly() {
    // The subset of examples that need NO prior scene, named object, material,
    // feature flag, or live viewport MUST execute successfully against an empty
    // document. This is where a broken doc example (like the former `pblock`
    // `rect 0,0,0 {w},0.05` arity bug) is caught. Each entry is a command whose
    // first documented example is fully self-contained.
    const SELF_CONTAINED: &[&str] = &[
        "box", "line", "polyline", "rect", "circle", "arc", "ellipse", "polygon",
        "curve", "interpcurve", "helix", "geodesic", "spaceframe", "hypar",
        "gaussvault", "funicular", "tensegrity", "cablenet", "distance",
        "material", "story", "pblock",
    ];
    let mut failures = Vec::new();
    for spec in registry() {
        if !SELF_CONTAINED.contains(&spec.name) {
            continue;
        }
        let Some(example) = first_example(spec.summary) else {
            failures.push(format!("{}: no example", spec.name));
            continue;
        };
        if let Err(e) = try_run(&example) {
            failures.push(format!("{}: {e}", spec.name));
        }
    }
    assert!(
        failures.is_empty(),
        "self-contained examples must execute against an empty document:\n  {}",
        failures.join("\n  ")
    );
}
