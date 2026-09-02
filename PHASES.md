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

- [ ] **Pending deck usability** — download-that-can't-run, download not wired to active deck, silent model-mismatch, active-deck clarity, surface connection errors.
- [ ] **Local model finetune** — 0.6B too weak; default 4B Qwen; free synthetic training data from the command registry.
- [ ] **Chat encryption** *(deferred by user)* — encrypt chat-session files at rest, OS-keychain-keyed, transparent auto-unlock. Design drafted (AES-256-GCM, `keyring` crate, plaintext fallback when keyring absent).

## Core UX debt

- [x] **M-layerspanel** — Rhino-style 7-column layers table + lock/linetype. *(verified shipped)*
- [x] **M-uilayout** — Rhino docking layout. *(mostly landed; verified)*
- [ ] **M-legacyskin / M-uipolish** — onboarding-driven skins (AutoCAD/Rhino/Revit theme + fonts + aliases), app icon (all platform sizes), toolbar iconography, ergonomics pass. Largely == design batches A–F.

## Big domain adds

- [ ] **Camera projections** — perspective controls, 2-point, panorama, fisheye; simulate iPhone/Android camera focal lengths + sensor crops.
- [ ] **Render & colors** — pencil/NPR display mode, SketchUp preset (sun-shading + profile edges + sky/ground gradient), lighting-mode rework (hemispheric-diffuse default, specular opt-in), per-object color + color modes.
- [ ] **M-enviro** — Ladybug-style: sun-path diagram, shadow studies, sunlight-hours heatmaps, radiation/insolation, EPW import, analysis meshes the deck can critique.
- [ ] **Diffusion render** — cassette-style SD backends (ComfyUI/A1111/Draw Things local; Replicate/fal/Stability keyed). Ships with NO backend. Control-image export (depth/edge/mask) buildable independently first.
- [ ] **M-structural** — BIM structural members (grid/story/beam/column/slab/wall/section/material/load/support), then **IFC-structural / SAF** open-format handoff to ETABS/SAP2000/Robot. Interop only — never claim to analyze. Then **M-bim-import** (typed members from IFC).
- [ ] **M-expressive** — generative/form-finding structures (Candela hypar, Dieste gauss-vault, geodesic domes, tensegrity, Frei Otto minimal surfaces, Gaudí funicular). One shared form-finding engine (dynamic relaxation) unlocks the whole family.

## Interop (M7 remainder)

- [ ] IFC import/export, 3DM (openNURBS opt-in download), SVG/CSV export, point-cloud LAS/LAZ/E57, mesh import (OBJ/STL/glTF/Collada), STEP AP242 (needs kernel-brep).
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
