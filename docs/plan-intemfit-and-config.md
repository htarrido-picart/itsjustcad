# Plan — IntemFit associativity, ChatGPT support, hotkeys

Branch: `feat/intemfit-and-config`

## STATUS (2026-10-09) — W1 + W2 + W4 SHIPPED on this branch (PR #16)
All three workstreams are implemented, tested, and manually verified; bundled
into one PR (#16) off `feat/intemfit-and-config`. The sections below are kept as
the original design record. See "Queued follow-ups" at the bottom for what's next.

What shipped:
- **W1 IntemFit associativity** — `SubdivLink` (kind Lots|Site) + a per-frame
  geometry-signature scan → `LotRefresh`; `lotsubdivide` (per block) and
  `lotgeneratesite` record source→output links and auto-recompute on boundary
  edit. Parameters-tab inspector (17 curated `SubdivisionSettings` fields,
  kind-filtered) with debounced `LotSetParams`, `LotFreeze`, Refresh.
  Undo-symmetric + replay-stable. Deleting a source cascades to its children +
  drops the link. Lots are clipped to the block (no spill on non-convex sites).
  Viewport cue: source curves cyan (live) / slate (frozen). Verified by unit +
  GPU-journey tests and a manual click-through.
- **W2 cloud keys** — LLM ▸ API Keys dialog (Anthropic + OpenAI), onboarding
  OpenAI path, endpoint-based vision, curated OpenAI model list (default
  `gpt-6-sol`), `max_completion_tokens`/no-`temperature` fix for gpt-5/6/o-series.
- **W4 hotkeys** — keybindings.json, keymap overlay, modeless editor
  (autocomplete + chord capture), `bind_hotkey` gated at the deck plane.
- Plus: onboarding window-size fix, system symbol-font fallback for ←/→,
  i18n parity across 8 catalogs.

Original decisions (all honored):
- **IntemFit (W1):** associative + live params, auto-update on polygon edit,
  with a freeze toggle.
- **Cloud (W2):** first-class in onboarding AND a menu-bar **LLM ▸ API Keys…**
  dialog covering both Claude (Anthropic) and ChatGPT (OpenAI).
- **Hotkeys (W4):** both a UI editor and an LLM `bind_hotkey` tool.

---

Four workstreams. W1 and W3 share the parametric/associative machinery, so
build W3's pattern knowledge first, then W1 reuses it. W2 and W4 are
independent and can land in any order.

---

## W1 — IntemFit: polygon redraw updates street division + live params

### What
Turn the `M-intemfit` subdivision (`lotgeneratesite` / `lotsubdivide`, in
`crates/subdivision`) from baked static geometry into an **associative,
parametric** result: it remembers its source polygon + settings, exposes live
params like expressive structures, and recomputes when the source polygon is
edited. A per-result **freeze** toggle bakes it to plain curves.

### Why
Today results are independent curves on the `lots`/`roads`/`blocks` layers with
no back-link; editing the source block leaves stale lots and forces a manual
re-run. The user wants redraw → division updates, and the interaction exposed
as parameters.

### How
Reuse the expressive-structures pattern (ParamSchema → ParamMap → derive →
Command::ParamSet, debounced inspector):

1. **Associative object / dependency link** (`crates/doc`)
   - Add a subdivision result record tying `source_block: ObjectId` +
     `SubdivisionSettings` (as a `ParamMap`) to produced child ids. Two viable
     shapes:
     - a new `Geometry::Subdivision { source, params, derived_cache }` object, or
     - a document-level `subdivision_links: BTreeMap<ObjectId, SubdivLink>`
       (block id → {settings, produced ids}).
   - Prefer mirroring `Geometry::Parametric` so undo/replay invariants reuse the
     proven path. Serde-default it so pre-existing `.ijc` files load unchanged.
2. **Pure derive** (`crates/subdivision` + `crates/commands/src/lot.rs`)
   - Already pure: `subdivide(block, settings)`. Wrap as
     `derive_subdivision(source_polygon, params) -> LotBake` so it matches the
     `derive_mesh`/`derive_segments` contract (deterministic, replay-stable).
3. **Params schema** (`crates/doc/src/param_schema.rs` style)
   - Define a `SubdivisionParams` schema: method (enum grid/perimeter/
     streetfollowing), lot_area_min, lot_width_min, road_width, block_depth,
     setbacks, seed, etc. — bounds + widget hints so the inspector is generated.
4. **Recompute on edit** (`crates/commands/src/exec.rs`)
   - After mutating ops (Move/Stretch/Flatten/edit of a curve), check if the
     touched id is a `source_block`; if so, re-run `derive_subdivision` and
     replace the produced children. Bump `doc.generation`.
   - Gate behind a `freeze` flag per link so the user can lock a result.
   - Add `Command::LotRefresh` (manual fallback / bulk) + `Command::LotFreeze`.
5. **Inspector UI** (`crates/app/src/param_editor.rs` + `app.rs` params tab)
   - When a subdivision object is selected, render its schema with the same
     debounced commit path (`commit_paramset`-analogue → `Command::ParamSet`-
     analogue for subdivision params).

### Risks / notes
- Edit-detection is the hard part: need a reliable "this op touched block X"
  signal. Simplest correct approach = after each op, diff affected ids against
  the `subdivision_links` keys.
- Keep derive deterministic (fixed seed) so replay/undo stays byte-stable.
- Recompute cost on every drag frame — debounce + only on op-commit, not
  per-frame, same as the param inspector.

---

## W2 — Claude + ChatGPT API keys (menu-bar settings) + model selection

### What
First-class cloud setup for **both** Claude (Anthropic) and ChatGPT (OpenAI):
- An **LLM ▸ API Keys…** menu-bar entry to set/update either provider's key at
  any time (not just onboarding).
- Onboarding path to pick a cloud provider and paste a key.
- Probe + pick a model per provider (claude-sonnet/opus…, gpt-4o…).

### Why
OpenAI already works via `DeckKind::OpenaiCompat` and Anthropic via
`DeckKind::Anthropic`, but there's no guided way to set either key or pick the
model — the user must hand-edit `decks.json`. Keys should be settable from the
menu bar on demand.

### How
Backend is ~done; this is mostly UI/config wiring:
1. **LLM ▸ API Keys… settings dialog** (new modeless panel, pattern =
   Model Setup) — `MenuAction::ShowApiKeys` under the LLM menu. Two fields:
   - **Anthropic (Claude)** key → updates/creates the `Anthropic` cassette
     (`api_key`, base_url `https://api.anthropic.com`).
   - **OpenAI (ChatGPT)** key → updates/creates the `OpenaiCompat` cassette
     (base_url `https://api.openai.com/v1`).
   - Each row: key field (masked), "Probe models" button, model `ComboBox`,
     "Save". Show whether a key is currently set and its source (env vs stored).
2. **Onboarding** (`crates/app/src/app.rs`, `DeckBrain`) — add an
   `DeckBrain::OpenAi` radio ("OpenAI / ChatGPT — paste an API key") alongside
   the existing cloud (Claude) path; on Start write the matching cassette.
3. **Model selection** (`crates/app/src/deck_pane.rs` + `probe.rs`) — reuse the
   existing probe (`GET /models`; `x-api-key` for Anthropic, Bearer for OpenAI)
   to populate the model `ComboBox`.
4. **Vision/model rules** (`crates/deck/src/config.rs`) — extend the
   vision-capable model check for gpt-4o-class models.
5. **Key storage** — follow existing pattern: `env:ANTHROPIC_API_KEY` /
   `env:OPENAI_API_KEY` preferred; a pasted literal is written to `decks.json`
   at 0600 via `write_private`. (System keyring is a possible later upgrade;
   out of scope here.)

### Risks / notes
- Masked key display; never log keys. Prefer env-var indirection where present.
- Streaming: OpenAI SSE uses `choices[0].delta.content` (handled in
  `openai_compat.rs`); Anthropic uses `content_block_delta` (handled in
  `anthropic.rs`) — verify both against the live endpoints.
- Don't send tools/web_search unless enabled (keep the offline stance).
- `local_only` mode must still gate both cloud cassettes.

---

## W3 — Expressive-structures-style params for IntemFit

This is not a separate deliverable — it's the **pattern W1 adopts**. Reference
blueprint (already in-tree):
- `crates/doc/src/param_schema.rs` — `ParamSchema`, `ParamField`, `ParamValue`,
  `ParamMap`, `GeneratorKind::schema()`, `sanitize()`.
- `crates/app/src/param_editor.rs` — schema→control mapping, `render_field`.
- `crates/app/src/app.rs` ~7176–7255 — params tab + debounce + `commit_paramset`.
- `crates/commands/src/exec.rs` ~13125–13196 — `Command::ParamSet` execute +
  re-derive + inverse for undo.

W1's "SubdivisionParams" schema and inspector mirror these one-for-one.

---

## W4 — User-definable hotkeys (UI + LLM)

### What
Let users bind their own hotkeys both through a UI editor and by asking the
assistant.

### Why
`crates/app/src/keymap.rs` is a hardcoded pure function; no user overrides, no
persistence.

### How
1. **Persistence** (`crates/app/src/keybindings.rs`, new) — `KeybindingsFile {
   bindings: BTreeMap<KeyCombo, String> }` at
   `~/.config/itsjustcad/keybindings.json`, load/save mirroring
   `deck::config::DecksFile` (+ `write_private`). `KeyCombo` serializes
   stably (cmd/shift/alt + key name).
2. **Keymap integration** — `keymap_with_user(key, mods, ctx, &bindings)`:
   user overrides first, fall back to built-in `keymap()`. Keep both pure for
   unit tests. Validate targets against the command registry + app verbs.
3. **UI editor** (`crates/app/src/keybindings_editor.rs`, new) — modeless
   window (same pattern as Model Setup/Plugins): table of bindings, "press a
   key" capture, conflict detection, validate verb, delete/reset-to-default.
   New `MenuAction::ShowKeybindings` under Tools/View; menu accelerators keep
   reading the single source so they don't drift.
4. **LLM tool** (`crates/deck/src/tool_loop.rs` dispatch) — a `bind_hotkey`
   tool (`"<combo>=<verb>"`) handled by the app's `ToolDispatch` (NOT the
   Claude CLI — it can't touch our config). Validate combo + verb, persist,
   return confirmation. Gate behind an opt-in ("let the assistant set hotkeys").

### Risks / notes
- Conflicts with OS/AppKit-reserved combos (⌘X/⌘C/⌘V intentionally not menu
  accelerators) — warn and refuse to bind those.
- Reserve a safe "reset to defaults" so a bad binding can't lock the user out.

---

## Sequencing

**W2 (quick win) → W4 (self-contained) → W1+W3 (largest, split into
derive → associative-link → inspector → freeze). Each ships as its own PR off
this branch.**

1. **W2** — Claude + ChatGPT API keys (menu-bar dialog) + model selection.
   Smallest, mostly UI; backend already exists. Unblocks the user fastest.
2. **W4** — user-definable hotkeys. Self-contained: persistence + keymap
   overlay + editor panel + `bind_hotkey` LLM tool.
3. **W1 + W3** — IntemFit associative + live params. Largest; reuses the
   expressive-structures pattern. Split into:
   - (a) **derive** — wrap `subdivide()` as a pure `derive_subdivision` + define
     the `SubdivisionParams` schema.
   - (b) **associative-link** — store source-block → settings → produced ids,
     and recompute-on-edit.
   - (c) **inspector** — schema-driven param panel with debounced commit.
   - (d) **freeze** — freeze/refresh toggles (`lotfreeze` / `lotrefresh`).

---

## Queued follow-ups (next session)

Captured on-branch (GitHub issue creation was permission-blocked). Priority order.

### F1 — CityEngine-style meta site-plan (BIG, the headline next step)
Match CityEngine: ONE component + ONE attribute panel driving the whole site
plan (streets → blocks → lots → setbacks → buildings), fully associative. The
individual `lot*` verbs stay as composable primitives; this adds the composed,
discoverable path.

Foundation already in place:
- `SubdivisionSettings` is ALREADY one struct spanning every phase (streets,
  lots, irregularity, setbacks, typology/floors/roof/coverage, open space). The
  17-field inspector is a curated slice of it.
- W1 associativity machinery (`SubdivLink` + per-frame signature scan →
  `LotRefresh`, kind-generic) is reusable.

Proposed:
1. **Meta-command** (`siteplan` / `urbanize` — name TBD): boundary + unified
   settings → runs the full chain (roads+blocks → lots → setbacks → building
   masses) as ONE associative result.
2. **One grouped inspector**: collapsible sections (Streets / Lots / Setbacks /
   Buildings / Open space), all editing the single `SubdivisionSettings`.
3. **End-to-end associativity via one meta-object** that owns ALL produced ids
   and regenerates them together on boundary/param edit. This avoids the
   multi-level DAG id-re-keying problem of extending associativity verb-by-verb
   (a single deterministic re-derive, streets→…→buildings, in one pass).

Open questions: regen coalescing/perf (a full chain regen per param nudge — ties
into op-log churn; debounce + maybe "heavy phases off during drag"); partial
freeze (freeze buildings while still editing lots); naming.

NOTE: this SUPERSEDES the narrower "extend associativity to lotsetbacks +
lotbuilding verb-by-verb" idea — the meta-object owning the whole output is the
cleaner model.

### F2 — Op-log churn from auto-recompute
Every source edit fires a logged `LotRefresh`, so a drag = one undo step per
commit and the op-log bloats under heavy editing. Add op-level coalescing /
debounce (one refresh per gesture, not per commit).

### F3 — Expose more of the ~50 `SubdivisionSettings` in the inspector
Currently 17 curated fields; unexposed ones are preserved through re-derive but
not editable. (Largely subsumed by F1's grouped inspector.)

### F4 — Cross-platform verification
Everything verified on macOS. Font fallback, keybindings paths, and the bundle
are macOS-centric; the app also targets Linux — verify there.

### F5 — Deferred to a SEPARATE session (already discussed)
- Ticket-worthy: assistant produces malformed ramps/stairs — no generator / no
  design rules (only after-the-fact `codecheck` ibc2021/ada2010 validation).
  Proposal: a parametric `ramp`/`stair` command reusing those rule packs.
- Ticket-worthy: macOS app ships without a visible icon — the `.icns` IS in the
  bundle; the blank icon is from the app being unsigned + quarantined (Launch
  Services won't cache it). Fix is distribution-side (ad-hoc codesign + strip
  quarantine + asset catalog + icon-cache reset).

### F6 — Process
PR #16 bundles W1+W2+W4 (the plan wanted one PR per workstream). Decide whether
to split before merge or accept the combined PR.
