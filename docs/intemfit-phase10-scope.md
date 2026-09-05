# intemfit Phase 10 — Buildings: re-scope answers (owner, 2026-09-05)

The Phase-10 "re-scope with Manuel before buildings" gate was resolved by the
**product owner (Hector)**, unblocking Phase 10 without waiting on Manuel. These
answers are the Phase-10 spec — fold into `docs/intemfit-plan.md` §7/§9 when the
Phase-10 cart launches. Buildings generate INSIDE the Phase-8 buildable envelope.

## Answers

1. **Footprint rule — ALL.** Support every fill mode as options: full-envelope,
   coverage-ratio (lot-coverage %), inset-from-envelope, and typology-driven.
   Default sensibly (typology-driven); expose the others via args/settings.
2. **Typology — ALL.** Detached, row/terrace (party-wall — matches euro_latam
   side=0), courtyard, apartment slab. Each fills the lot differently; typology
   drives the footprint shape.
3. **Height — fixed, by floors × floor-height, with upper-floor step-backs YES.**
   Height = floor_count × floor_height (fixed floor height). Upper floors
   step back (setback per floor above a threshold) — a stepped massing.
4. **Roof — ALL.** flat / gable / hip / shed, per-typology or per-arg.
5. **Detail — footprint AND mass, with floors for FAR.** Emit the footprint
   polygon AND the extruded 3D mass, divided into floors so Phase 11 yield can
   compute GFA / FAR (floor_count × floor_area, net of step-backs).

## Implications for the build

- New `crates/subdivision/src/buildings/`: `footprint.rs` (typology → footprint
  in envelope, 4 fill modes), `massing.rs` (extrude by floors, per-floor
  step-back, floor slabs for FAR), `roof.rs` (flat/gable/hip/shed).
- New settings (SubdivisionSettings, serde-default, euro_latam-flagged where a
  number is a placeholder): typology, footprint_mode, coverage_frac,
  floor_height, floor_count (or target height), stepback_start_floor,
  stepback_depth, roof_type, roof_pitch.
- Verb `lotbuilding <sel> [typology= footprint= floors= floorheight= roof= …]` →
  bakes footprints + mass onto `buildings` layer (mass on a 3D layer), logged,
  undo, replay-stable, deterministic.
- FAR/GFA (floor areas net of step-backs) flow into Phase 11 `lotreport` — report
  gross site, net developable (minus open space), and built GFA / FAR.
- euro_latam placeholder floor_height ~3 m, coverage defaults, etc. — flagged
  "confirm with Manuel" like Phase 6/8.

## Train position

Owner directive: Phase 10 unblocked → the intemfit train now runs
8 → 9 → 10 → 11 → 12 (no halt at 10). **M-dwg-bridge after the last train cart
(Phase 12) lands.** Manuel still gets the Phase 1–9 build for feedback in
parallel; his answers refine the euro_latam placeholders (Phase 6/8/10 numbers).
