# Plan — Guided Command Engine (#4)

**Branch:** `feat/guided-command-engine` (based on `d236393`).
**Scope (user-chosen):** the reusable engine **+ every applicable verb** (anything that takes object and/or point input). Rhino behavior: type a bare verb → it walks you through each step in the command line + viewport, then runs.

This pairs with the shipped **empty-Enter/Space re-runs the last verb alone** — a bare verb entry should now start its guided flow.

---

## 1. Goal

Type a verb (or pre-select objects, then type the verb) and get prompted through its steps — pick objects, pick points, type distances/counts, choose keyword options — instead of having to know the full one-line syntax. Every guided run must emit the **same canonical command string** the parser already accepts, so it flows through the identical substrate (op-logged, undoable, replay-stable).

## 2. Current state to reuse (do NOT reinvent)

- **`crates/app/src/draw_tool.rs`** — the existing step machine for *creation* verbs (`line`, `polyline`, `rect`, `circle`, `polygon`). Study its API; the engine generalizes it:
  - `try_start(line)`, `on_click(world)`, `on_enter()`, `push_input(c)`/`pop_input()`/`take_input()`, `prompt()`, `preview(cursor)`, `last_point()`, `cancel()`.
  - It collects picks/typed input and returns a finished **command string**.
- **`crates/app/src/app.rs` ~4960–5048** — the per-frame snap-resolution + draw wiring: osnap (`crate::osnap::resolve`), grid, **persistent Ortho + Shift** (`precise::ortho_lock`), **SmartTrack** guide snap (`crate::smarttrack`), precise typed input (`crate::precise::resolve_input`), ghost preview painting, prompt overlay, click/enter dispatch to `execute_line`. The engine hooks in here exactly where `draw_tool` does.
- **`crates/app/src/command_line.rs`** — `last_verb` tracking, `is_destructive_verb`, submit path → `execute_line`.
- **`crates/app/src/smarttrack.rs`**, **`osnap.rs`**, **`precise.rs`** — snapping/ortho/tracking + typed-point resolution, all already usable per-pick.
- **Selection system** — pre-selected objects live in the session/doc; box-select + `selregion`/`selwindow`/`selcrossing` exist. The engine's "select objects" step must consume the current selection when present (noun-verb) and otherwise let the user pick.
- **`crates/commands/src/parse.rs` + `exec.rs`** — every verb's textual syntax + executor. The engine assembles the canonical string; where a verb's parser can't express a needed guided input (e.g. offset side as a point), **extend the parser** (this is the bulk of per-verb work).

## 3. Architecture — the step engine (`crates/app/src/guided.rs`, new)

Model it on `draw_tool` but generalize the step to a heterogeneous list. Keep pure logic in `guided.rs` (unit-tested); timing/egui/rendering stay in `app.rs`.

```
enum Step {
    SelectObjects { prompt, min, max, filter: ObjFilter },   // consumes current selection if present
    PickPoint     { prompt, base_relative: bool },            // world point (osnap/ortho/smarttrack aware)
    Number        { prompt, default: Option<f64>, from_pick: bool }, // typed, or distance from a picked point
    Integer       { prompt, default: Option<i64> },
    Keyword       { prompt, options: &[&str], default: &str },// command-line option switch
    // optional/repeat handled by the verb script (see below)
}

enum Input { Objects(Selection), Point(DVec3), Num(f64), Int(i64), Key(String) }

struct GuidedTool {
    verb: Option<&'static VerbScript>,
    steps_done: Vec<Input>,
    cursor_step: usize,
    input_buf: String,     // typed buffer for current Number/Integer/Keyword step
    // ... mirror draw_tool's buffer/prompt handling
}
```

**Public API (mirror draw_tool so app.rs wiring is minimal):**
`try_start(line, current_selection) -> bool`, `on_click(world) -> StepResult`, `on_enter() -> StepResult`, `push_input(c)`, `pop_input()`, `prompt() -> Option<String>`, `preview(cursor) -> Vec<Vec<DVec3>>`, `active()`, `cancel()`. `StepResult` = `NeedMore | Emit(String) | Error(String)`.

## 4. Verb-script model + assembler (data-driven)

A registry mapping each verb → its ordered `Step`s + a pure **assembler** that turns collected `Input`s into the canonical command string.

```
struct VerbScript {
    verb: &'static str,
    steps: &'static [Step],
    assemble: fn(&[Input]) -> Result<String, String>,   // -> "offset last 5 @3,0"
    allow_preselect: bool,                                // noun-verb: skip SelectObjects if selection present
}
```

- Keep assemblers **pure and unit-tested** (given a fixed `Vec<Input>` → exact command string).
- Distance-by-pick: a `Number{from_pick:true}` step accepts EITHER a typed number OR a picked point whose distance from the previous point becomes the value (Rhino offset/fillet feel).
- Keyword steps map to the verb's option tokens in the emitted string (e.g. array `Polar`, offset `BothSides`, fillet `Radius`).

## 5. Pre-selection (noun-verb)

If `allow_preselect` and the current selection is non-empty when the verb starts, seed the first `SelectObjects` step from it and skip straight to the next step. Otherwise prompt: "Select objects to <verb>". Support pick + box-select + Enter-to-finish-selection (reuse existing selection input).

## 6. Integration (app.rs / command_line)

- On submit of a bare verb (or verb with insufficient args) that is a guided verb: `GuidedTool::try_start(line, selection)` instead of erroring. If `false`, fall through to the parser as today.
- Per frame (same block as draw_tool): feed the snap-resolved `cursor_world` to `preview`; on click → `on_click`; on Enter → `on_enter` (also accepts typed buffer via `precise::resolve_input` for point steps and parse for number/keyword steps); Esc → `cancel` (respect SmartTrack's "Esc clears acquired first" rule already in place).
- On `Emit(cmd)` → `self.execute_line(cmd)` (same substrate). The emitted verb also updates `last_verb` so empty-Enter repeats it.
- Prompt overlay + status snap already render from `prompt()`/`status_snap`.
- Draw-tool and guided-tool are mutually exclusive; consider unifying later (creation verbs are just a `VerbScript` with PickPoint steps) — **optional refactor, not required for v1**.

## 7. Interaction details

- Command line shows the current step prompt with default in brackets: `Offset distance <5>:`.
- Keyword options shown inline: `Corner ( Sharp / Round ) <Sharp>:` — type the option or its first letter.
- Enter accepts the default; typing overrides; Esc cancels the step machine.
- Ghost preview per verb where cheap (offset ghost curve, mirror axis line, array footprint, rotate rubber-band) via `preview()`.
- Reuse osnap / persistent Ortho / SmartTrack / grid for every PickPoint — free, since it's the same resolution path.

## 8. Command assembly & parser audit (the real per-verb work)

For each verb: confirm the canonical textual form can express every guided input. Where it can't, extend `parse.rs` (+ `exec.rs` if needed) and add parser tests. Examples likely to need work:
- **offset** — needs a side indicator; support a side point (`offset <sel> <dist> <point>`) if not already.
- **fillet/chamfer** — radius/distances + the two picked curves + pick locations.
- **trim/extend/powertrim** — cutting/boundary set + picked segments.
- **array** (rect/polar/path) — counts, spacing, angle, center/path.
- **rotate/scale** — base point + angle/factor (reference option).
- **mirror** — two-point axis.
Keep each parser change small, replay-stable, and covered by a unit test.

## 9. Verb inventory (group + status)

Grep `parse.rs`/`registry.rs` to finalize; expected groups (mark ✅ already-interactive, ✎ typed-only today):
- **Create (✅ draw_tool):** line, polyline, rect, circle, polygon. (Optionally fold into engine later.)
- **Transform:** move, copy, rotate, scale, mirror, align/orient, stretch, array/arraycurve/patharray.
- **Curve edit:** offset, fillet, chamfer, trim, extend, powertrim, split, join, explode, curvebool, boundary.
- **Reference/instance:** ncopy, xclip, block/insert.
- **Annotate:** dimlinear, dimradius, dimdiameter, dimangular, autodim, leader, field.
- **Hatch:** hatch (pick boundary + pattern/scale).
Produce the authoritative list in the first session by grepping the registry; log any verb deliberately excluded.

## 10. Testing

- Pure engine: step advancement, buffer handling, default acceptance, keyword parse, distance-by-pick, cancel, pre-select skip.
- Per-verb assembler: fixed `Vec<Input>` → exact command string.
- Parser: any new/extended syntax.
- Optional GPU-gated journey tests for a couple of flows (offset, fillet) behind `#[ignore]` like existing ones.
- Gate: `cargo clippy --workspace -- -D warnings` clean; full suite green; rebuild dist for manual verification each milestone.

## 11. Rollout (agent fan-out)

1. **Session 1 — engine + offset (lock the pattern).** Build `guided.rs`, `VerbScript` registry, app.rs wiring, pre-select, prompt/preview, and wire **offset** end-to-end (incl. any parser change). Manual-verify in dist. Commit.
2. **Then fan out per group**, one agent per group on this branch (sequence to avoid app.rs collisions, or worktree-isolate + merge):
   - transform (move/copy/rotate/scale/mirror/align/stretch)
   - array family
   - curve-edit (fillet/chamfer/trim/extend/powertrim/split/join/explode/curvebool/boundary)
   - annotate (dims/leader/field)
   - reference (ncopy/xclip/insert) + hatch
   Each agent: steps + assembler + parser extension + tests; green build/clippy/test; no regressions; commit per group; report a manual checklist.
3. Update dist after each group; keep PR-per-milestone or one PR for the branch.

## 12. Non-negotiables

- Keep the command panel `exact_size` — never nest a resizable panel (dropped-keystroke bug c2cfcff).
- Emit canonical command strings → op-logged, undoable, replay-stable. No side-channel mutation.
- Reuse osnap/ortho/smarttrack/precise; don't duplicate snapping.
- Don't touch `local_runtime.rs`; preserve DWG-drop.
- Every change unit-tested (project rule).

## 13. Risks / open questions

- **Selection-step UX** while a guided flow is active (click-to-select vs click-to-pick-point disambiguation) — define per verb via the step type.
- **Draw-tool vs guided-tool unification** — deferred; keep both for v1, share the snap path.
- **Parser gaps** — some verbs may need new option tokens; keep additive + tested.
- **Preview cost** for heavy verbs (array) — keep ghost cheap or skip.

## 14. First-session concrete checklist

- [ ] Grep `registry.rs`/`parse.rs`; write the authoritative verb list + per-verb step sketch.
- [ ] Create `crates/app/src/guided.rs` with `Step`/`Input`/`VerbScript`/`GuidedTool` + `mod guided;`.
- [ ] Implement the engine (pure) with unit tests.
- [ ] Wire into `app.rs` (start on bare/insufficient guided verb; per-frame click/enter/esc; emit → `execute_line`; prompt/preview) and `command_line` start path.
- [ ] Implement **offset** VerbScript + assembler; extend `parse.rs` for a side point if needed (+ test).
- [ ] `cargo build -p itsjustcad --profile quick` + `cargo clippy --workspace -- -D warnings` + `cargo test -p itsjustcad --bin itsjustcad` green.
- [ ] Rebuild + swap `dist/ItsJustCAD.app`; manual: `offset` with pre-selected curve → prompted for distance/side → runs.
- [ ] Commit; then fan out per group.
