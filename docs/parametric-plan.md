# M-parametric — Live Parametric Structures + Parameters tab (plan)

Planned 2026-09-08. NOT started. Owner request: when the LLM (or user) builds a
generative structure — catenary/funicular, geodesic/Fuller dome, hypar, gauss
vault, spaceframe, tensegrity, minimal surface, gridshell — it should stay
**parametric**: a dynamic **Parameters tab** appears when the model has ≥1
parametric object, showing a **card per parametric object**; clicking a card
opens a rich param editor (sliders / numeric inputs / dropdowns — whatever fits
each param) and the geometry **re-derives live** on change.

## Why (current gap)
The expressive/form-finding generators (`geodesic`, `hypar`, `gaussvault`,
`gridshell`, `funicular`, `tensegrity`, `cablenet`, `spaceframe`, `minsurf`)
currently bake to STATIC mesh geometry. Changing "frequency" or "radius" means
re-typing the whole verb — the params are lost the moment the mesh is created.
Meanwhile `pblock`/`param_blocks` (dynamic blocks) DO carry params + re-derive
(`param <sel> key=value`). This phase brings that live-parametric model to the
built-in generators and gives it a proper editing UI.

## Design

### 1. Parametric object model
Store the GENERATOR + its params on the object, not just the baked mesh. Two
viable shapes — pick during Phase 1:
- **(a) Reuse `param_blocks`/Instance machinery**: each generator becomes a
  parametric definition; the object is an Instance carrying param overrides;
  `param` re-derives it. Least new code — the re-derive + undo/replay path
  already exists.
- **(b) New `ParametricObject { generator: GeneratorKind, params: ParamMap }`**
  on the object, with the mesh as a derived cache. Cleaner model for built-in
  generators that aren't user-authored blocks.
Prefer (a) if the generators can be expressed through the block-template path;
else (b). Either way: the object holds `{generator id, param values}`; the mesh
is derived and re-baked on any param change. Deterministic + replay-stable
(param change = one logged op; re-derive is pure).

### 2. Param SCHEMA per generator (the UI driver)
Each generator declares a schema so the UI is generated, not hand-built:
`{ name, kind: Float|Int|Enum|Bool, min, max, step, default, widget: Slider|Numeric|Dropdown|Toggle, unit, label_key }`.
Examples:
- **geodesic**: frequency (Int 1..6, Slider), radius (Float>0, Numeric, m), mode (Enum dome|full, Dropdown).
- **hypar**: a,b,c (Float, Numeric/Slider), nu,nv (Int, Slider).
- **funicular**: support a,b (points — Numeric xyz), segments (Int, Slider), load, slack (Float, Slider), invert (Bool, Toggle).
- **gaussvault**: span,length,rise (Float, Slider), undulate (Bool, Toggle).
- **spaceframe/tensegrity/gridshell/minsurf/cablenet**: their existing args mapped to typed params.
Schema lives next to each generator (single source; the verb parser + the UI +
the deck catalog all read it, so they can't drift). A completeness test asserts
every generator has a schema and every schema field round-trips.

### 3. Parameters dynamic tab
- New `PanelTab::Parameters` (or "Parametric") in the right dock, same dynamic
  pattern as Blocks/Sheets (appears iff the doc has ≥1 parametric object;
  explicitly openable via `panel tab parameters` + a menu item; deck/UI-plane
  reachable). Pure `dyntabs::parametric_rows(doc)` derivation.
- **Cards**: one per parametric object — name, generator kind, small thumbnail
  (reuse a mini mesh preview), current key param summary. Clicking a card opens
  the editor for that object (inline expand or a side panel).

### 4. Param editor UI (schema-driven)
For the selected parametric object, render one control per schema field:
- **Slider** for bounded Float/Int (min..max, step) — the primary, most tactile.
- **Numeric input** (drag-value + type) for unbounded or precise Float/Int.
- **Dropdown** for Enum (dome|full, etc.).
- **Toggle** for Bool (invert, undulate).
- Point/vector params: 3 numeric fields (x,y,z).
Live re-derive on change, **debounced** (don't rebuild the mesh every slider
pixel — coalesce to ~30–60ms or on release for heavy generators); show a cheap
preview during drag if the generator is expensive, full re-bake on commit.
Every committed change is a logged op → undo/redo + replay-stable. Reset-to-
default per param. The editor is generated from the schema so a new generator
gets a UI for free.

### 5. LLM / deck integration
- The generator verbs create PARAMETRIC objects (not static mesh) by default (or
  a flag; default parametric so "build a geodesic dome" stays editable).
- The deck can set params via the existing `param <sel> key=value` path (extend
  it to built-in generators) — so "make the dome frequency 4" works, and the
  Parameters tab reflects it. The compact catalog already lists the generators;
  add a one-liner that they're parametric + editable.

## Phases (within M-parametric)
1. **Parametric object model + schema** — pick (a)/(b); store generator+params;
   re-derive on change; schema per generator; `param` works on built-in
   generators; logged/undo/replay-stable. Tests: round-trip, re-derive
   determinism, every generator has a schema.
2. **Parameters dynamic tab** — `PanelTab::Parameters`, `parametric_rows`, cards
   + thumbnails, appears-when + openable. Tests mirror Blocks/Sheets.
3. **Schema-driven editor UI** — slider/numeric/dropdown/toggle per field, live
   debounced re-derive, reset-to-default, undo. Pure schema→control-list fn is
   unit-tested; painting is GUI.
4. **Deck integration + catalog note** — verbs emit parametric objects; deck
   `param` on generators; catalog mentions editability.

## Notes / invariants
- Determinism/replay: a param change is a single logged op; re-derivation is a
  pure function of (generator, params) + seed where relevant. No RNG drift.
- Don't break existing static output: users who just want a mesh can still
  `explode`/bake a parametric object to plain mesh (a `bake`/`freeze` verb).
- Reuse, don't duplicate: the `param_blocks` re-derive + `param` verb + the
  Blocks/Sheets dynamic-tab pattern are the templates.
