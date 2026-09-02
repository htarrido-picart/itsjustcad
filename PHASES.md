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
- [ ] **M-enviro** — MOSTLY DONE (audited 2026-09-02): `shadowstudy`, `sunhours` grid heatmap, `facesunhours` per-face insolation, EPW import (sets location + reports stats), and **`sunpath` shipped this session** — yearly sun-path dome (7 date arcs Dec→Jun + hour analemmas + horizon ring, golden 'sunpath' layer, undo/replay-stable, deck-callable). Remaining: EPW-weighted radiation (kWh/m² — needs the 8760 radiation columns retained from EPW), deck critique hooks for analysis meshes.
- [ ] **Diffusion render** — control-image export DONE (`controlimages <prefix>` → depth/edge/mask PNGs from the current view). Remaining: cassette-style SD backends (ComfyUI/A1111/Draw Things local; Replicate/fal/Stability keyed). Ships with NO backend.
- [ ] **M-structural** — verbs exist (grid/story/beam/column/slab/wall/load/support); remaining: **IFC-structural / SAF** open-format handoff to ETABS/SAP2000/Robot. Interop only — never claim to analyze. Then **M-bim-import** (typed members from IFC).
- [ ] **M-expressive** — generator verbs exist (geodesic/spaceframe/hypar/gaussvault/gridshell/funicular/tensegrity/cablenet); remaining: unify on one shared dynamic-relaxation form-finding engine + Frei Otto minimal surfaces.

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

## Long-term / research

- [ ] **M-kernel** — opt-in OCCT exact-BREP tier (mesh kernel stays default; ~50MB download).
- [ ] **M-plugins** — LLM-authored plugins (DeepSeek-harness model: everything pluggable, deck writes new commands/generators mid-chat, persist to disk).
- [ ] **M-nurbs** — interpolated curves, control-point editing, knot insert, curvature graph.
- [ ] **M-solids2** — sweep2, rail revolve, loft w/ guides, blend, variable pipe.
- [ ] **M-constraints** — dimensional constraint solver (SolveSpace-style). Heavy.
- [ ] **M-site / M-basemap** — terrain from contours, GeoJSON/OSM import, satellite basemaps (Esri/OSM no-key default).
- [ ] **M-assets** — block library, hatch patterns, drafting fonts.
- [ ] **M-dynblocks** — parametric/dynamic blocks (params + grips), LLM-authored.
- [ ] **M-cloudfiles** — atomic saves, external-change detection, Dropbox conflict awareness.
- [ ] **M-histedit / M-options** — op-log history editing; design-option branches (git-like).
- [ ] **M-i18n / M-docs** — localization (Spanish first), tutorials + sample files + website.

## Pre-release

- [ ] **PRE-PUSH PASS** — README end-user Download & Install section; 6 value-prop screenshots (massing, pencil section, sun study, dimensioned sheet→PDF, deck-from-prompt, Rhino-skinned UI); `git rm` the security-review doc + add SECURITY.md; fix repo URL owner; push main + tag v0.1.
- [ ] **M-secreview / M-codereview** — untrusted-parser hardening (DXF/IFC/LAS/OBJ/GLB/GeoJSON/EPW/OSM/plugin JSON), prompt-injection via imported names, panic/unwrap audit, replay-invariant checks. Rerun before each release.
- [ ] **M-sign** *(deprioritized)* — Apple + Windows code-signing certs to kill first-launch warnings. Only when marketing to non-technical architects at scale.

## Out of scope (this repo)

- iOS/iPadOS/mobile app — separate private/paid repo consuming the shared egui-free Rust core.
- Web/WASM viewer — dropped.
