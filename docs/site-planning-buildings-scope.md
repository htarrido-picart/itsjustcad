# Site planning buildings — re-scope answers (owner, 2026-09-05)

The "re-scope with the client before buildings" gate was resolved by the
**product owner (Hector)**, unblocking buildings without waiting on the client.
These answers are the buildings spec — fold into `docs/site-planning-plan.md`
§7/§9 when the buildings cart launches. Buildings generate INSIDE the buildable
envelope from setbacks.

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
   polygon AND the extruded 3D mass, divided into floors so yield reporting can
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
- FAR/GFA (floor areas net of step-backs) flow into the yield report (`lotreport`) — report
  gross site, net developable (minus open space), and built GFA / FAR.
- euro_latam placeholder floor_height ~3 m, coverage defaults, etc. — flagged
  "confirm with the client" like the lot-rule and setback numbers.

## Train position

Owner directive: buildings unblocked → the site-planning train now runs
setbacks → open space → buildings → yield reporting → polish (no halt at
buildings). **M-dwg-bridge after the last train cart (polish) lands.** The client
still gets the build through open space for feedback in parallel; their answers
refine the euro_latam placeholders (lot-rule, setback and building numbers).
