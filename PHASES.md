# ItsJustCAD — Phases Remaining

Status as of 2026-09-01. Shipped milestones (M2–M5 core, layers, gumball,
undo, dims/sheets/PDF, DXF/glTF/OBJ/STL, local-LLM onboarding, per-doc chat
persistence, native menus, clipboard) are omitted — this tracks what's LEFT.

## Design system (report batches A–F) — COMPLETE ✅

HIG-grounded design revision, run as sequential batches. All six landed.

- [x] **A — Tokens + a11y foundation** — `surface_elevated`/`on_surface_tertiary`/`destructive` roles, WCAG contrast helper + passing ≥4.5:1 body-role tests, type scale (caption/title/weight), spacing guards. *(already implemented; verified)*
- [x] **B — Iconography** — Lucide clean, de-dup guard, `Line→pen-line`; added `Paperclip` icon replacing 📎. *(shipped ce0d1e1)*
- [x] **C — Menus + keyboard** — Window menu, shortcuts shown, disable-don't-hide, stateful View menu (Panel Hide/Show ⌘\, display/lighting radios). *(shipped f470de7)*
- [x] **D — Component correctness** — button roles (destructive/pressed/"…"), segmented controls + toolbar grouping, unsaved-changes + neutral-title alerts. *(already implemented; verified)*
- [x] **E — Ergonomics + states** — empty-doc viewport hint + add-layer toolbar to top (rest already present: empty chat, hit-target floor, focus ring, middle-truncate, striped rows). *(shipped 36e6418)*
- [x] **F — Polish** — killed dead `HISTORY_H` (history height already token-relative), wired `reduce_motion` to spinners, corrected download-cancel warning (partial `.part` is kept for resume; autosuggest accent fill already present). *(shipped)*

## Queued — small, high-impact

- [x] **Pending deck usability** — download-that-can't-run (disk-space gate + unknown-RAM warning; RAM gate was already in), download wired to active deck even with Model Setup closed (per-frame terminal poll), model-mismatch surfaced for Anthropic too + suffix-tolerant matching (`qwen3` ↔ `qwen3:latest`), active model always visible in the chat header, download failures announced in the deck transcript. *(shipped; builds on 5d16875)*
- [ ] **Local model finetune** — 0.6B too weak; default 4B Qwen; free synthetic training data from the command registry.
- [ ] **Chat encryption** *(deferred by user)* — encrypt chat-session files at rest, OS-keychain-keyed, transparent auto-unlock. Design drafted (AES-256-GCM, `keyring` crate, plaintext fallback when keyring absent).

## Core UX debt

- [x] **M-layerspanel** — Rhino-style 7-column layers table + lock/linetype. *(verified shipped)*
- [x] **M-uilayout** — Rhino docking layout. *(mostly landed; verified)*
- [ ] **M-legacyskin / M-uipolish** — onboarding-driven skins (AutoCAD/Rhino/Revit theme + fonts + aliases), app icon (all platform sizes), toolbar iconography, ergonomics pass. Largely == design batches A–F.

## Big domain adds

- [x] **Camera projections** — COMPLETE (audited 2026-09-02; core had already shipped). Perspective FOV via `camera <mm>` full-frame lens math, 2-point (`camera 2point`, projection shear — verticals stay vertical), equirect panorama + equidistant fisheye (cubemap capture + remap), phone-lens sims (iPhone/Pixel/Galaxy ultrawide/main/tele as 35mm equivalents). Gap closed this session: stateful **View ▸ Camera radios** (Perspective / Two-Point / Panorama 360° / Fisheye; checked from the live camera, fire the same `camera` verbs the deck uses).
- [x] **Render & colors** — COMPLETE (audited 2026-09-02; had already shipped). Pencil display mode + sketchy NPR edges, `sketchup` preset verb (sun shading + profile edges + sky/ground gradient), lighting rework (Working = hemispheric-diffuse default, Sun = SPA key light, Presentation = specular opt-in), per-object `color` + ColorMode (by-layer/by-object/by-type/random).
- [x] **M-enviro** — COMPLETE (closed 2026-09-02): `shadowstudy`, `sunhours` grid heatmap, `facesunhours` per-face insolation, EPW import (sets location + reports stats), `sunpath` yearly dome, `radiation` annual kWh/m²·yr per face (EPW DNI/DHI month×hour bins, occlusion-tested, bins embedded in the op-log). **Deck critique hooks closed this session**: every analysis now stores a compact structured `AnalysisReport` on the document (min/avg/max, 6-bin distribution, worst/best sample locations with compass facings — token-frugal, regenerated on replay); new read-only `report [analysis]` registry command serves it to the deck; system prompt gained an "Environmental critique" section teaching analysis → report → design feedback (sun-hour thresholds, radiation hot/cold faces, shadow coverage) so the LLM grounds critique in sampled locations ("north face at (0,10,2) gets 0.5 h — don't put the terrace there").
- [ ] **Diffusion render** — control-image export DONE (`controlimages <prefix>` → depth/edge/mask PNGs from the current view). Remaining: cassette-style SD backends (ComfyUI/A1111/Draw Things local; Replicate/fal/Stability keyed). Ships with NO backend.
- [x] **M-structural** — COMPLETE (audited + closed 2026-09-02). Audit: nearly all had ALREADY shipped — verbs (grid/story/beam/column/slab/wall/load/support), **IFC-structural export** (typed IfcBeam/IfcColumn/IfcSlab/IfcWall with swept-solid bodies + full IfcStructuralAnalysisModel: IfcStructuralCurveMember/SurfaceMember, IfcStructuralPointConnection with boundary conditions, IfcStructuralLoadGroup with point/linear/planar actions), **SAF export**, and **M-bim-import** (IFC import reconstructs typed Frame/Area members with stories/materials via `import_semantic`; mesh fallback for the rest). Closed this session: SAF upgraded from a nonstandard ZIP-of-CSV to a **genuine SAF 2.2.0 .xlsx workbook** (hand-rolled OOXML on the existing store-mode ZIP writer — no new deps; 9 SAF sheets, numeric cells native, `.saf`/`.xlsx` both accepted, export dialog fixed from a dead "xml" filter), plus the **never-analyze disclaimer** embedded in both formats (xlsx docProps description + IFC FILE_NAME header note: "geometry+topology handoff — no analysis results"). Interop only — the app never claims to analyze.
- [x] **M-expressive** — COMPLETE (audited + closed 2026-09-02). Audit: the shared engine ALREADY existed (`kernel-mesh/src/formfind.rs`) and funicular/tensegrity/cablenet already routed through the one `dynamic_relaxation`; nothing needed re-unifying. Closed this session: engine hardening (per-node point loads, true Barnes/Day kinetic damping option, Schek force-density solver — deterministic Gauss–Seidel) + hard tests (settled sag vs analytic catenary ≤2%, kinetic == viscous equilibria, pinned-bar stability, perturbed-start uniqueness, net symmetry), and **Frei Otto minimal surfaces shipped** — `minsurf <closed-curve-sel> [n]` (aliases soapfilm/minimalsurface) stretches a soap film (zero-rest-length force-density harmonic net) across any closed boundary curve; undo/replay-stable, deck-callable. Deliberately left analytic (closed-form is exact; relaxation adds nothing): hypar, geodesic, spaceframe, gaussvault (exact catenary section), gridshell (rides analytic surfaces).

## New phases (added 2026-09-02)

- [ ] **M-landscape** — landscape design proper (terrain/OSM/sun exist; design tools don't):
  - *Grading & earthwork*: `pad <sel> <elev>` building pads with side slopes, `grade` regions to target slope, **cut/fill volumes** vs original terrain (report m³), retaining edge where slope exceeds max.
  - *Contours OUT*: `contours <interval>` — generate contour polylines FROM terrain mesh (we only ingest contours today); major/minor lines, labels, feed sheets/PDF.
  - *Planting*: plant catalog (species, mature height/canopy Ø/growth rate — data-driven JSON like models.json), `plant <species> <pt>` / `plantrow` / `plantmass <region> <spacing>`, canopy meshes participate in `shadowstudy`/`sunhours`/`radiation` (deciduous transparency by season), **plant schedule CSV export** (species/count/size — landscape BOM).
  - *Water & drainage*: steepest-descent **flow arrows** on terrain, watershed/ponding detection, `swale` along path. Analysis-viz only, never claims hydrology engineering.
  - *Hardscape*: `path <curve> <width>` draped on terrain, stairs/ramp on slope w/ max-slope check (ADA 1:12 style warning), site sections through terrain.
  - Deck-callable throughout; heatmaps/analysis meshes reuse M-enviro plumbing. Pure-math parts (cut/fill, flow, contour extraction) test hard.

- [ ] **M-deckagent** — deck LLM interactivity + agent harness (ties to deck-harness-FOSS goal):
  - *Terse mode*: caveman-style response budget — system-prompt style rules + hard max-token cap per turn; substance stays, fluff dies. Toggle in settings; default ON for local models (saves tokens = faster local inference).
  - *Clarify-before-act*: prompt teaches model to ask ONE short clarifying question when the request is ambiguous (missing dimensions, ambiguous selector, unclear target) instead of guessing; UI renders question + user reply continues turn. Test: "make it bigger" with 3 objects selected → question, not random scale.
  - *Plan-execute harness* (Claude-Code/DeepSeek-style loop for prolonged tasks): model first emits a PLAN (numbered steps, shown as checklist in transcript) → executes step-by-step, each step = commands + reads results/errors → self-corrects on error (retry/replan, bounded) → verifies end state (query objects, counts) → summarizes. Op-log stays the substrate; plan state persists in chat session so an interrupted plan resumes. Bounded iterations + user cancel.
  - Architecture: extend the existing tool_loop in crates/deck (already threads error results back) into plan/step/verify states; grammar (GBNF) gains plan/question/step message types so small local models emit them reliably.

## Compliance plugins (planned 2026-09-02 — NOT started; plan only)

Shared foundation first, then two rule packs riding it. Advisory only — every
report carries "advisory pre-check, not a code review; verify with a licensed
professional / AHJ". Rule packs are data (JSON), never hardcoded, so editions
update without code and the deck LLM can read/extend them.

- [ ] **M-checkengine** — declarative compliance-check engine (prereq for both packs):
  - Check-plugin plane alongside the existing JSON macro plugins: a rule = JSON (id, code ref, severity, query over typed model, geometric predicate, threshold, message template). Engine evaluates rules against the document (typed members, stories, blocks, terrain, paths) and emits a structured `ComplianceReport` (pass/fail/warn + object ids + locations + measured vs required) — same shape/plumbing as the M-enviro `AnalysisReport` + `report` verb.
  - Geometry probes library: clear-width along path, slope of ramp/path, riser/tread extraction from stair geometry, door clear opening, headroom, turning-circle fit (60" circle), travel-distance along route, count-by-type per story.
  - Verbs: `codecheck <pack> [story|sel]`, `report codecheck`; failures optionally highlighted on an 'compliance' layer (red markers). Deck-callable + prompt section so the LLM runs checks and critiques grounded in rule ids ("stair S2 riser 8.1in > IBC 1011.5.2 max 7in").
  - Rule packs user-loadable/LLM-authorable like plugins (`plugin` infra precedent); unit-test engine + each probe hard (known geometry → known verdict).
- [ ] **M-ibc** — International Building Code pack (IBC 2021 edition, data-driven so 2024 swaps in):
  - Stairs (riser 4–7in, tread ≥11in, headroom, handrail height/continuity, guard ≥42in), corridor/exit widths, ceiling heights, door widths, occupant-load calc (area ÷ table 1004.5 factors per space type), exit count vs occupant load, common-path/travel-distance approximations, story height triggers.
  - Needs space/room semantics for occupancy — minimal `room <poly> <occupancy-type>` tagging verb ships with this pack.
- [ ] **M-ada** — ADA pack (2010 ADA Standards / ICC A117.1):
  - Ramps ≤1:12 + landing intervals/size, accessible route ≥36in clear (32in at points), door clear width ≥32in + maneuvering clearances, 60in turning space, thresholds ≤½in, reach ranges, accessible parking count vs total, restroom fixture clearances, stair handrail extensions.
  - Reuses M-landscape ramp/slope math (ADA 1:12 warning already planned there) and check-engine probes; site + building both.

## Interop (M7 remainder) — COMPLETE ✅ (audited + closed 2026-09-02)

Audit found nearly all of M7 had already shipped; the two real gaps (LAZ,
.3dm export) closed this session.

- [x] **IFC** — import (IFC4/IFC2x3, meshes → 'ifc' layer, semantic variant for M-bim-import) + export (IFC4). *(already shipped)*
- [x] **3DM** — pure-Rust openNURBS reader (meshes/lines/polylines/NURBS curves with names + layers; breps skipped) — no opt-in download needed. Export shipped 25d004a: the spec-conformant V5 writer promoted from test-only to `export .3dm`.
- [x] **SVG / CSV export** — shipped earlier (svg.rs, csv.rs, wired to dialog + registry).
- [x] **Point clouds** — LAS 1.2–1.4 (hand-rolled) + E57 (e57 crate) already in; **LAZ decompression added b5223c4** via laz-rs (Apache-2.0), batch-decompressed so decimation to ≤200k never materializes the cloud.
- [x] **Mesh import** — OBJ/STL/glTF/GLB/Collada .dae all shipped (mesh_import.rs, hand-rolled parsers with size caps).
- [x] STEP AP242 import + faceted export — implemented via OCCT, but **gated behind the opt-in 'kernel-occt' feature** (M-kernel); default builds report a clear "needs the exact-BREP tier" error.
- DWG / SKP / RVT: bridge via open exchange only (no proprietary readers — license clean).

## Long-term / research (audited 2026-09-02 — most had ALREADY shipped)

- [x] **M-kernel** — DONE as designed: opt-in OCCT tier exists (`kernel-occt` crate behind the `kernel-occt` feature; STEP + exact booleans route through it, default builds report the clear "needs the exact-BREP tier" error). Mesh kernel stays default. No further work planned.
- [x] **M-plugins** — DONE (was already shipped; verified): declarative JSON macros (params + command-template body, NO native code), persisted to `~/.config/itsjustcad/plugins/`, loaded at startup, `plugin list|reload|save|define|delete` verbs, menu integration, path-traversal + prompt-injection hardening, and the deck system prompt teaches the LLM to author macros mid-chat via `plugin define {json}` (17 tests).
- [x] **M-nurbs** — COMPLETE this session: interpolation (`interpcurve`) + control-point editing (`setpoint` + draggable UI) were already in; **added knot insertion** (`insertknot`, Boehm on homogeneous coords, shape-exact for rational curves) and **curvature graph** (`curvature` — comb hairs + tip curve on 'analysis', Menger curvature, reports max κ / min radius). Not done (niche): degree elevation; NURBS-output rebuild (rebuild emits polylines).
- [x] **M-solids2** — COMPLETE this session: sweep2, rail revolve and variable-radius pipe were already in; **added loft with guide curves** (`loft <profiles> guides <sel>` — skin bows through open guides, cosine falloff) and **blend surface** (`blend <a> <b> [bulge]` — Hermite-eased sheet, bulge 1 = ruled).
- [x] **M-constraints** — CORE SHIPPED 2026-09-02 (audit: was truly not started). New pure-std `crates/constraints`: 2D parametric sketch solver, Newton–Raphson with Levenberg-Marquardt damping (SolveSpace approach), numeric Jacobian, rank diagnostics (DOF count, redundant-constraint blame, per-constraint conflict list); 19 constraint kinds; 32 tests incl. 25 scrambled-start convergence runs. Command layer: `constrain <kind> <sel> [sel] [value]` (14 kinds — coincident/horizontal/vertical/distance/length/angle/parallel/perpendicular/equal/radius/fixed/tangent/midpoint/on; polymorphic by target type, nearest-endpoint resolution latched into the stored constraint, solves immediately), `solveconstraints`, `constraints list|delete <n>|clear`. Undo/redo-able, op-log replay-stable, deck-callable, solver status (solved / N DOF / redundant / conflicts) in the command output. Works on lines + circles/arcs. Remaining (niche until a sketch-mode UI exists): constraints on polyline vertices / NURBS control points, `symmetric` via command line (solver supports it), constraint glyphs in the viewport, drag-with-constraints.
- [x] **M-site / M-basemap** — mostly DONE (was already shipped; verified): `terrain <csv|geojson>` from contours, GeoJSON/OSM import, OSM + keyless-satellite slippy-tile basemaps (offline-first, transient, never op-logged), `location` georeferencing.
- [x] **M-assets** — COMPLETE this session: block library (`blocklib/blockload/blocksave` + 6 seeded starters), 7 drafting hatches and Hershey vector drafting font were already in; **added the ANSI standard hatch set** (`hatch <sel> ansi31..ansi38 [spacing]` — iron/steel/bronze/plastic/fire-brick/marble/lead/aluminum; renders in viewport + PDF, scales with the object, replay-stable). Not done (niche): TrueType drafting fonts, user-defined hatch rules.
- [x] **M-dynblocks** — DONE (was already shipped; verified): `pblock <name> [param=default ...] : templates` parametric blocks, `insert ... param=value` overrides, `param <sel> key=value` re-derives geometry. No constraint-driven grips (see M-constraints).
- [x] **M-cloudfiles** — COMPLETE this session: atomic saves (write-temp + fsync + rename) and crash journal were already in; **added external-change detection** (mtime/size watch, throttled 2 s poll, Reload-vs-keep-mine modal, deleted-file notice) and **conflict-copy awareness** (Dropbox "conflicted copy" + numbered-duplicate siblings warned at open/save).
- [x] **M-histedit / M-options** — DONE (was already shipped; verified): `amend <step> <command>` op-log rewriting with full replay, `option save/list/switch/delete` design-option branches persisted in the file format. Not done: arbitrary rebase-style history surgery (amend-only by design).
- [ ] **M-i18n / M-docs** — NOT STARTED (audited: no i18n infra, all UI strings hardcoded). Localization (Spanish first), tutorials + sample files + website. Real remaining work.

## Pre-release

- [ ] **PRE-PUSH PASS** — README end-user Download & Install section; 6 value-prop screenshots (massing, pencil section, sun study, dimensioned sheet→PDF, deck-from-prompt, Rhino-skinned UI); `git rm` the security-review doc + add SECURITY.md; fix repo URL owner; push main + tag v0.1.
- [ ] **M-secreview / M-codereview** — untrusted-parser hardening (DXF/IFC/LAS/OBJ/GLB/GeoJSON/EPW/OSM/plugin JSON), prompt-injection via imported names, panic/unwrap audit, replay-invariant checks. Rerun before each release.
- [ ] **M-sign** *(deprioritized)* — Apple + Windows code-signing certs to kill first-launch warnings. Only when marketing to non-technical architects at scale.

## Out of scope (this repo)

- iOS/iPadOS/mobile app — separate private/paid repo consuming the shared egui-free Rust core.
- Web/WASM viewer — dropped.
