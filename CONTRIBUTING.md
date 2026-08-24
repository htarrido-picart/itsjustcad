# Contributing

## Tests — required, not optional

Every PR must ship tests. The minimum set per change:

- **parse test** — if the command has a new syntax, a round-trip through `parse`
- **exec/undo/redo test** — if the command mutates the document
- **replay-stability test** — `to_json` → `from_json` → `to_json` must be
  byte-identical, and object ids must match (`pre_*` companion for old-file
  compat if a new field is added)

`cargo test --workspace` must be green before a PR is opened. No exceptions.

No new clippy warnings. Run `cargo clippy --workspace -- -D warnings`.

## The substrate rule

**All user-facing operations go through `Command`.**

The command line, the LLM deck, scripted automation, and replay all emit the
same `Command` enum. There is no side channel. If it mutates the document, it
is a `Command` variant in `crates/commands/src/command.rs`, parsed by
`parse.rs`, executed by `exec.rs`, and logged by the `Session`.

The corollary: do not add imperative mutation methods to `Document` that bypass
the substrate. Add a command instead.

## Minimal-deps ethos

This project has a small, audited dependency tree. Before adding a crate:

1. Can the stdlib or an existing dep do it?
2. Is it pure Rust with no build scripts or `unsafe` outside of
   well-understood FFI?
3. Does `cargo deny check licenses` still pass?

Convenience crates (e.g. `itertools` for a one-liner) are usually a no. A
carefully chosen domain crate with real leverage (e.g. `image` for PNG I/O in
the headless renderer) is fine.

## File format stability

See `FORMAT.md`. New Command fields must use `#[serde(default)]`. The version
number in `io.rs::FORMAT_VERSION` is bumped only for breaking changes and
requires a migration path and a `pre_*` test.

## Code style

- Swift 6 / Rust 2024 edition: safe, `Send`-clean, no implicit `unsafe`.
- Comments only for non-obvious constraints. Self-documenting names preferred.
- `tracing::info!` / `tracing::warn!` for runtime events, not `println!`.
- Match the surrounding style in the file you're editing.

## Commit messages

One subject line, imperative mood. Body optional. End with:

```
Co-Authored-By: <name> <email>
```

## License & Contributor License Agreement (CLA)

The desktop app is AGPLv3-or-later, but the copyright is held solely by the
author, who also ships a paid mobile edition and offers commercial licenses.

**By submitting a contribution you agree to a Contributor License Agreement:**
you license your contribution to the author under terms that let the author
(a) release it in the AGPLv3 desktop app, and (b) include it in the proprietary
commercial and mobile (iOS/iPadOS/Android tablet) editions. You keep copyright
to your own contribution; you grant the author the right to dual-license it.
This is what keeps the free open desktop build and the paid mobile build
possible from one codebase.

The agreement is codified in git: see [`CLA.md`](CLA.md). On your first PR a bot
asks you to sign by commenting one line; your signature (GitHub id + timestamp +
CLA version) is recorded in `.github/cla-signatures.json` and the PR is blocked
until signed. No paper, no email — the git record is the proof.

If you cannot agree to this, open an issue to discuss before sending a PR.

Do not paste code from incompatibly-licensed sources. Contributors are credited
in `AUTHORS`. The author-attribution notices (AGPLv3 §7b) must not be removed.
