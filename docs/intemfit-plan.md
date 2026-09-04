# intemfit — Lot Subdivision for ItsJustCAD (Build Plan)

Working plan. Read first in every session touching this feature.

**What we're building:** a parametric site-planning capability inside ItsJustCAD
(Rust) that takes a site boundary and produces a road network, blocks, and lots —
in the vein of CityEngine block subdivision and TestFit yield studies. Verb name
prefix `lot*`; feature codename **intemfit** (phase `M-intemfit`).

**Target decision (2026-09-03):** native **ItsJustCAD Rust feature**, NOT a Rhino
`.rhp` plugin and NOT an ItsJustCAD JSON macro plugin. Algorithms are ported from
the Rhino build plan + the Python prototype, but land as a pure-Rust crate +
registry verbs, deck-callable, replay-stable — same shape as `M-landscape`.

**Requirements source:** a scoping questionnaire completed by Manuel (practicing
urbanist/architect), summarized in §1. Where this plan conflicts with an
assumption you'd otherwise make, his answers win.

---

## 0. Working agreement

- **Ask before inventing requirements.** If it isn't specified here, flag it rather
  than guess. The value of this plan is that it's grounded in one real user's answers.
- **Pure geometry stays out of egui AND out of the document layer.** The new
  `crates/subdivision` crate has ZERO `egui`/`itsjustcad-doc`/`itsjustcad-commands`
  deps — plain geometry in, plain geometry out, unit-tested headless in CI. Same rule
  the Rhino plan applied to RhinoCommon: dependency direction is Core ← commands, never
  the reverse.
- **Every algorithm ships with its §8 validation cases** before moving to the next phase.
- **Commit at each phase gate.** Phases are ordered so the tool is useful at the end of
  Phase 3, not only Phase 8.
- **Unit tests always** (project rule) — pure-math parts test hard (analytic assertions).
- **Don't build what §1 rules out.** Scope creep here is measured in months.
- **Deterministic for a fixed seed** — a hard requirement (replay/undo depend on it).

---

## 1. Confirmed requirements

### In scope — explicitly requested

| Area | Requirement |
|---|---|
| Lot patterns | Grid (recursive), Perimeter (offset), Street-following (skeleton). All three. |
| Lot sizes | Small through large — parameter range, no fixed target |
| Irregularity | Manuel: uniform to moderate, cap 0.4 (his default). Owner adds a `loose` mode unlocking to 1.0 — see owner-scope note below. |
| Automation | Tool generates roads AND lots from a raw boundary |
| Street patterns | Orthogonal grid, diagonal/skewed, organic/free-form, cul-de-sac clusters |
| Lot rules | Width **mix**, independent **depth**, front-loaded, **alley-loaded**, wider corner lots, flag/panhandle lots |
| Cleanup | Merge slivers into neighbors; measure frontage at setback line, not curb |
| Setbacks | Front, side, rear, build-to line, **and** buildable envelope per lot |
| Open space | Pocket parks, greenway/trail corridor, retention pond, tree-save — tool places them |
| Buildings | Footprints **yes**; roofs + 3D massing **yes** |

### Owner-requested — IN the tool, beyond Manuel's scope (Hector, 2026-09-04)

Manuel's questionnaire scopes *his* defaults; the product owner wants broader tool
capability. These were "ruled out" by Manuel but are **in scope as tool features**
(not necessarily Manuel's defaults — ship them, don't force them on his flow):

- **Radial / circular** street networks — a dedicated polar generator. Note: this
  **breaks the pure-rectilinear assumption**, so recursive-OBB is no longer the *sole*
  road core — it stays the core for ortho/skewed/organic blocks, while radial needs its
  own ring+spoke layout. Block subdivision still runs on whatever blocks it emits.
- **Hexagonal** street networks — a hex-lattice generator (also non-rectilinear).
- **Loose / highly irregular** lots — the `irregularity` clamp becomes a *soft default*
  at 0.4 (Manuel's preference) with a "loose" mode unlocking up to 1.0; loose likely
  needs the organic subdivider (heavy jitter / non-orthogonal splits), not plain OBB.

### Explicitly ruled out — do not build

- **"No subdivision"** mode (block stays one parcel) — not requested by anyone.
- **Automatic "reserve N% open space."** Manuel wants *named features placed*, not a blind percentage. Do NOT implement percentage-reservation.
- **Voronoi** street/lot networks — RESOLVED from the actual form (2026-09-03): Q6 asks "any pattern you'd never use?" and Manuel wrote **"Voronoi"**; the owner has not re-requested it. Do not build it unless Hector asks.

### Form verified against the real questionnaire (2026-09-03)

The 4-page `subdivision-visual-picker.pdf` (Manuel's marks) confirms every §1 row
above. Notable exacts: Q2 circled **all three** sizes (small/medium/large → full
parameter range, no fixed target); Q7 marked **all six** lot rules incl. flag/panhandle;
Q10 marked front/side/rear/build-to/buildable-envelope (not "lot lines are enough");
Q11 marked park/greenway/pond/tree-save (NOT %-reserve, NOT "I place them"). Q9
("anything wrong about lots") and Q14 ("missing on day one") were left blank.

### Open questions — resolve with Manuel before Phase 5/6

1. **"What's missing on day one"** (Q14) — left blank on the form. Follow up separately.
2. **Lot width mix** — the form confirms he wants a width *mix* but not the product list.
   Need his actual products (e.g. 40/50/60 ft) + proportions, and whether the mix is a
   hard ratio or soft preference. Algorithm shape depends on it. (Not asked on this form.)
3. **Alley dimensions** — ROW width + whether alleys are required on every block or only
   some. (Not asked on this form.)
4. **Flag-lot area accounting** — confirm the pole area is excluded from countable lot
   area (§7.4).

---

## 2. Phase order

Each phase has a definition of done (§9). Don't start a phase until the previous passes.

| Phase | Deliverable | Why here |
|---|---|---|
| 1 | `crates/subdivision` foundation: `Polygon2d`, `i_overlay` bridge (offset/boolean/cleanup), `OrientedBox` (min-area rect), `PolylineTools`. **i_overlay AGPLv3-compat + offset support confirmed.** | Everything depends on robust 2D ops |
| 2 | Verb + settings plumbing: `lotsubdivide` in registry (deck-callable, GBNF), sticky `SubdivisionSettings` in the doc, `Preview` viewport draw | Makes every later phase testable by hand |
| 3 | **Recursive OBB subdivision** (`lotsubdivide method=grid`) | First genuinely useful tool. Ship-able alone. |
| 4 | Offset / perimeter subdivision (`method=perimeter`) | Small delta on Phase 3 |
| 5 | Road network generation + block extraction (`lotgeneratesite`) | Unlocks "tool does it all" |
| 6 | Lot rules: width mix, depth, corner, flag, front/alley loading | Bulk of Manuel's asks |
| 7 | Skeleton (street-following) subdivision (`method=streetfollowing`) | Hardest; needs Phase 5 street tagging |
| 8 | Setbacks + buildable envelopes (`lotsetbacks`) | Cheap once lots are correct |
| 9 | Open-space feature placement (`lotopenspace`) | Pocket park, greenway, pond, tree-save |
| 10 | Building footprints + roof massing | Second product; do not start early |
| 11 | Yield reporting + option comparison (via `report` plane) | Makes it a decision tool |
| 12 | Consistent indexing, true straight skeleton, optimization | Polish |

**Phase 3 is the first shippable milestone.** Get it to Manuel before starting Phase 5.

---

## 3. Repository layout

```
crates/
  subdivision/                 # NEW pure-Rust crate — no egui/doc/commands deps
    src/
      geometry/
        polygon2d.rs           # polygon + i_overlay interop
        clip_bridge.rs         # offset, boolean, cleanup (i_overlay), int-scale once
        oriented_box.rs        # min-area rect, long/short axis
        polyline.rs            # resample, simplify, perpendiculars, arc-length
      streets/
        street_graph.rs        # centerlines, widths, hierarchy, adjacency
        generators/            # orthogonal, skewed, organic, culdesac
        block_extractor.rs     # ROW offset -> boolean -> tagged blocks
      blocks/
        block.rs               # polygon + BlockEdge[] with street tags
        block_edge.rs          # is_street, street_id, width, length, is_alley
      subdivision/
        recursive_obb.rs       # Phase 3
        offset_sub.rs          # Phase 4
        skeleton_sub.rs        # Phase 7
        lot_rules/             # width_mix, depth, corner, flag, loading, sliver_merge
        setbacks.rs            # Phase 8 incl. buildable envelope
      open_space/              # Phase 9
      buildings/               # Phase 10
      straight_skeleton/
        offset_approx.rs       # Phase 7 — build first
        felkel.rs              # Phase 12 — swap in later, same trait
      reporting.rs             # YieldReport -> ItsJustCAD AnalysisReport plane
      settings.rs              # SubdivisionSettings
    tests/                     # §8 validation cases
    samples/blocks/            # the §8 test polygons as JSON

crates/commands/src/
  lot.rs                       # verb exec: lotsubdivide/lotgeneratesite/lotsetbacks/...
                               # bridges doc curves <-> subdivision::Polygon2d,
                               # bakes results as logged ops (replay-stable),
                               # registers verbs (deck-callable + GBNF)

prototype/python/             # the existing Shapely prototype — PORT from it, don't reinvent
```

Dependency direction: `subdivision` (leaf) ← `commands` ← `app`. `subdivision` never
references doc/egui — the bridge in `commands/src/lot.rs` does all conversion.

---

## 4. Existing prototype — read before Phase 3/4

A working **Python prototype** (`/prototype/python`, Shapely) already implements
`recursive_obb()`, `offset_subdivision()`, `street_following()`, `clip_corners()` and
produced the questionnaire diagrams. **Port the algorithms; don't reinvent** — the
edge-case handling was earned. Two known limits to fix in the port:

1. `offset_subdivision` builds lots by outward ray-cast quads — robust but approximate
   at sharp corners. Replace with proper ring splitting once `i_overlay` offset is in.
2. `street_following` slices perpendicular to a supplied frontage curve — fine for a
   single-frontage block, wrong for multi-frontage. Phase 7 replaces it with a skeleton.

---

## 5. Key technical decisions (ItsJustCAD-adapted)

### 2D ops: `i_overlay` (pure Rust) — replaces Clipper2

Clipper2 was the Rhino plan's choice; the Rust equivalent is **`i_overlay`** (MIT, pure
Rust — no C/FFI, AGPLv3-compatible) for every offset and boolean. **Phase-1 blocker:
confirm `i_overlay` ships polygon *offset* (outward/inward buffering), not just boolean
overlay — if offset is weak, evaluate `geo` + a buffer impl, or vendor a Clipper2 port.
Resolve in Phase 1, not Phase 5.**
- **Do not use naive per-edge offset for block insets** — fails on non-convex /
  near-self-intersecting contours, which is what real parcels look like.
- **Work in integer coords.** Pick scale `1000`, define it once in `clip_bridge.rs`,
  never let a raw f64 reach the clipper.
- We already have `kernel-mesh`: Delaunay (`triangulate`), `signed_area`, earcut, 3D
  BSP CSG — reuse for triangulation/area, but the 2D offset/boolean gap is real and is
  what `i_overlay` fills.

### Straight skeleton — build/port

No production straight skeleton exists ready-made. Same strategy as the Rhino plan:
1. **Phase 7:** `offset_approx.rs` — iterated small `i_overlay` insets with topology
   tracking. Approximate medial axis, robust, fast. Good at survey tolerances.
2. **Phase 12:** port Felkel & Obdržálek (1998) priority-queue event algorithm behind
   the same `StraightSkeleton` trait.

### No Rhino infra — ItsJustCAD equivalents

Everything Rhino-specific in the source plan is **dropped and replaced**:
- multi-targeting `net48;net7.0-windows` → N/A (single Rust workspace).
- `.rui` toolbar / flyouts / icons / Eto panel / DisplayConduit / RhinoCommon command
  option-loops → **ItsJustCAD registry verbs** (deck-callable + GBNF), sticky settings
  on the document, optional viewport **preview** via the existing scene overlay, and
  optionally a dynamic **"Site" tab** (reuse `M-dyntabs`) later. No toolbar work.
- Command-line option flow (`_LotSubdivide _Method=Offset`) → ItsJustCAD verb args
  (`lotsubdivide grid area=6500 width=50 ...`), same as every existing verb.

### Verbs, deck, replay

- All verbs `lot`-prefixed so they group in the palette/autocomplete (like `layer*`).
- Registry-registered → deck can drive them; GBNF grammar picks them up automatically.
- Every op **logged + replay-stable**; any randomness seeded from op data (region hash
  + salt), so undo/redo and file reload reproduce byte-identical output.
- Results bake onto dedicated layers (`lots`, `roads`, `blocks`, `openspace`,
  `setbacks`) like the landscape/analysis verbs create their layers.
- Yield reporting rides the existing `AnalysisReport` + `report` plane (§M-enviro), so
  the deck can critique a layout.

### Street edge tagging is a hard dependency

Every `BlockEdge` carries `{ is_street, street_id, street_width, street_length,
is_alley }`. Skeleton subdivision, corner-lot assignment, front/alley loading, and
frontage measurement are all unimplementable without it. Build correctly in Phase 5;
do not defer.

### Frontage measured at the setback line (default, not an option)

Manuel explicitly requested this. Differs enormously from curb-line on cul-de-sac bulbs
and tight curves, and is the most common reason automated layouts get rejected by
reviewers.

---

## 6. Parameter object (Rust)

Mirror CityEngine attribute names where they exist (makes their docs usable as
reference). serde-defaulted so old docs load; stored on the document.

```rust
pub enum SubdivisionMethod { Recursive, Offset, Skeleton }
pub enum LoadingType       { FrontLoaded, AlleyLoaded, Mixed }
pub enum CornerAlignment   { StreetWidth, StreetLength }
pub enum StreetPattern     { Orthogonal, Skewed, Organic, CulDeSac, Radial, Hexagonal } // Radial/Hexagonal = owner scope (Phase 5b)

pub struct SubdivisionSettings {
    pub method: SubdivisionMethod, // = Recursive
    pub seed: u64,                 // deterministic output

    // Recursive
    pub force_street_access: f64,   // 1.0 = mandatory
    pub lot_area_min: f64,         // = 5000
    pub lot_area_max: f64,         // = 9000
    pub lot_width_min: f64,        // = 50
    pub irregularity: f64,         // soft default clamp [0.0, 0.4] (Manuel); `loose` unlocks up to 1.0
    pub loose: bool,               // owner scope: unlock irregularity >0.4, route to organic subdivider
    pub corner_angle_max: f64,     // = 45
    pub corner_width: f64,

    // Offset
    pub offset_width: f64,         // = 120
    pub subdivide_core: bool,      // = true

    // Skeleton
    pub shallow_lot_frac: f64,
    pub corner_align: CornerAlignment,
    pub simplify: f64,

    // Lot rules (Phase 6)
    pub width_mix: Option<LotWidthMix>, // None = single target width
    pub lot_depth_target: f64,          // independent of area
    pub lot_depth_tolerance: f64,
    pub loading: LoadingType,           // = FrontLoaded
    pub alley_width: f64,               // = 20
    pub corner_lot_width_bonus: f64,
    pub allow_flag_lots: bool,          // = false
    pub flag_pole_width_min: f64,       // = 20
    pub merge_slivers: bool,            // = true
    pub sliver_area_frac: f64,          // = 0.5 (× lot_area_min)
    pub frontage_at_setback: bool,      // = true

    // Setbacks (Phase 8)
    pub setback_front: f64,  // 25
    pub setback_side: f64,   // 5
    pub setback_rear: f64,   // 20
    pub build_to_line: f64,  // 0 = disabled
    pub draw_buildable_envelope: bool, // true
}

pub struct LotWidthMix {
    pub products: Vec<(f64 /*width*/, f64 /*proportion*/)>, // e.g. [(40,0.3),(50,0.5),(60,0.2)]
    pub strict_proportions: bool,
}
```

### Semantic trap — document in code

`lot_area_min`, `lot_width_min`, `irregularity` mean **different things** per method:

| Param | Recursive | Skeleton |
|---|---|---|
| `lot_area_min` | recursion stop condition | post-process merge threshold |
| `lot_width_min` | min length of *any* lot side | *ideal* street frontage per lot |
| `irregularity` | split-pivot deviation from OBB midpoint | jitter on width + edge direction |

Keep separate internal fields; expose the shared names only in the verb args.

---

## 7. Algorithm notes (portable — port from Python/Rhino plan)

### 7.1 Recursive OBB (Phase 3)
Min-area OBB. Split line along the OBB **short** direction, pivoted on the **long** axis
midpoint. Recurse while area > `lot_area_min`. Four modifiers:
- **Street access** — if a child would lose its street edge, use the orthogonal
  direction; `force_street_access = 1.0` makes it mandatory.
- **Snap to contour vertices** — if a split lands near an original vertex, move the pivot
  onto it (stops lot lines landing inches off a bend).
- **Edge alignment** — use one of the lot's own edges as the angular reference.
- **Seeding** — compute child seeds *before* the recursive call.
Terminate when `area < lot_area_min` or any child side < `lot_width_min`. A high
`lot_width_min` can force lots *larger* than `lot_area_max` — correct, don't "fix" it.

### 7.2 Offset / perimeter (Phase 4)
Inward-offset the block by `offset_width`. Sample the ring at spacing from target
area/depth, jittered by `irregularity`. Split the strip with lines orthogonal to the
offset curve. If `subdivide_core`, run recursive OBB on the interior. Fall back to
recursive OBB when `offset_width ≈ 0` or the offset polygon collapses.

### 7.3 Road network (Phase 5, + radial/hex in Phase 5b — owner scope)
Generators → a `StreetGraph` of centerlines with widths. **Four rectilinear (Manuel):**
- **Orthogonal** — recursive OBB of the *site* to ~2× lot depth; spine roads along splits.
- **Skewed** — same with a global rotation on split directions.
- **Organic** — spline spines fitted to the site long axis with controlled sinusoidal
  deviation, then secondary connectors.
- **Cul-de-sac** — a spine plus perpendicular stubs ending in bulbs, spaced by block depth.

**Two non-rectilinear (owner-requested, Phase 5b — do after the four above work):**
- **Radial / circular** — a center (or centers) with ring roads at block-depth spacing +
  radial spokes; blocks are annular-sector polygons. Does NOT use OBB site-splitting;
  its own polar layout. Feed the sector blocks into offset/skeleton subdivision (not
  recursive-OBB, which assumes rectilinear).
- **Hexagonal** — a hex lattice sized to block depth; blocks are the hex cells (or
  6-way street intersections). Non-rectilinear; same "subdivide the emitted blocks" flow.
These raise the §8 validation bar: add annular-sector and hex-cell blocks as cases.
Then offset centerlines by ROW/2 (`i_overlay`), boolean-subtract from the site, tag
every resulting block edge with its generating street. Snap to existing boundary access
points. If `AlleyLoaded`, insert a second tier of narrower rear lanes bisecting each
block along its long axis, tagged `is_alley`.

### 7.4 Lot rules (Phase 6)
- **Width mix is the hard one** — turns division into *packing*: given a frontage length
  and a product list with proportions, choose a *sequence* of widths that fits and
  respects the ratios. Greedy fill weighted by running proportion deficit, then a local
  swap pass to absorb the remainder. Do NOT spread slack evenly across all lots — that
  defeats fixed products.
- **Alley-loaded** — depth becomes a two-frontage problem; requires the alley in the
  street graph (hence after Phase 5).
- **Corner lots** — interior angle < `corner_angle_max` → widen by
  `corner_lot_width_bonus`; auto-clamp width to avoid self-intersection.
- **Flag lots** — only if `allow_flag_lots`; pole width ≥ `flag_pole_width_min`; pole
  area excluded from countable area (**confirm with Manuel**).
- **Sliver merging** — repeatedly merge any lot below `sliver_area_frac × lot_area_min`
  into its largest-shared-edge neighbor until none remain. Single biggest difference
  between professional-looking and generated-looking output.

### 7.5 Skeleton subdivision (Phase 7)
1. Straight skeleton of the block → faces, one per contour edge.
2. Group adjacent faces whose street edges have **similar curvature** (a run of lots
   along a curved street reads as one band, not a per-segment fan).
3. Assign corner regions by `corner_align` (widest street wins; tie-break on length).
4. Slice each face group perpendicular to its street edges at `lot_width_min` spacing.
5. Merge lots below `lot_area_min`.
6. Merge shallow/triangular lots per `shallow_lot_frac`.
7. Apply `simplify` vertex reduction.
Produces the perpendicular-to-curve lot lines around cul-de-sac bulbs. ~60% of total
subdivision effort — why it's late despite being visually important.

---

## 8. Validation cases (Rust unit tests; JSON in `crates/subdivision/samples/blocks`)

Every subdivision algorithm must pass these block shapes:
1. Long thin rectangle → one double-loaded row
2. L-shaped block
3. Block with a re-entrant notch
4. Cul-de-sac bulb (near-circular, single street edge)
5. Curved-street block, varying radius
6. Block with an interior hole (retention pond)
7. One very short street edge, three long non-street edges
8. Near-degenerate sliver block
9. Block where `lot_width_min` forces lots above `lot_area_max`
10. 15° acute corner (corner-lot clamping)
11. Annular-sector block (radial generator output — owner scope)
12. Hex-cell block (hexagonal generator output — owner scope)

**Assertions (all cases):**
- Σ lot area == block area within tolerance
- No overlapping lots; no gaps
- Every lot has a street edge when `force_street_access == 1.0`
- All lot widths ≥ `lot_width_min` where the method guarantees it
- No lot below `sliver_area_frac × lot_area_min` when `merge_slivers` on
- **Deterministic for a fixed seed** (also gives the ItsJustCAD replay invariant)

---

## 9. Phase definitions of done

- **Phase 1** — `i_overlay` round-trips a polygon with no coordinate drift; offset +
  boolean pass on all 10 blocks; `OrientedBox` returns correct long/short axes for
  rotated inputs. **`i_overlay` offset support + AGPLv3 compatibility confirmed — if
  offset is inadequate, resolve here, not later.** Crate builds with no doc/egui deps.
- **Phase 2** — `lotsubdivide` runs from command line + deck; settings sticky across
  save/reload (serde on the doc); preview draws through the viewport overlay and bakes
  nothing until commit; verb registered (GBNF + palette). Runs headless (no GUI needed).
- **Phase 3** — recursive OBB passes all §8 assertions on all 10 blocks. **Send a build
  to Manuel here.**
- **Phase 4** — offset subdivision passes; degenerate fallbacks verified.
- **Phase 5** — all four street patterns generate valid non-overlapping blocks on a real
  parcel; every block edge correctly tagged; alley tier generates when requested.
- **Phase 6** — width mix hits requested proportions within 5% on a 500 ft frontage;
  alley-loaded blocks have correct two-sided depth; no slivers remain.
- **Phase 7** — skeleton output on the cul-de-sac + curved-street blocks visually matches
  the questionnaire reference diagrams.
- **Phase 8** — setbacks + buildable envelopes render per lot; frontage at setback line.
- **Phases 9–12** — as specified; re-scope with Manuel before Phase 10.

---

## 10. Command / UX surface (ItsJustCAD-native)

Replaces the Rhino toolbar/command-flow sections. All verbs `lot`-prefixed,
registry-registered, deck-callable, GBNF-grammared, logged + undoable.

| Verb | Selects | Key args |
|---|---|---|
| `lotgeneratesite` | boundary curve | pattern, blockdepth, roadwidth, alleys, preview |
| `lotsubdivide` | block curve(s) | method=grid\|perimeter\|streetfollowing, area, width, irregularity, preview |
| `lotrelot` | lots | (re-runs stored settings, keeps indices) |
| `lotloading` | block(s) | type=front\|alley |
| `lotsetbacks` | lots | front, side, rear, buildto, envelope |
| `lotfrontage` | lots | at=setback\|curb (default setback) |
| `lotmergeslivers` | lots | threshold |
| `lotopenspace` | region | type=park\|greenway\|pond\|treesave, area |
| `lotreport` | lots/site | → AnalysisReport (yield: lot count, avg area, frontage) |
| `lotsettings` | none | show/set the sticky SubdivisionSettings |

- **Preview**: `preview=yes` draws the result through the viewport overlay as args
  change; bakes only on commit. Keep it cheap (subset of blocks / reduced detail for
  large sites). Biggest perceived-quality win — same lesson as the Rhino plan.
- **Deck flow**: "subdivide this block into 50-ft lots, alley-loaded" → deck emits
  `lotsubdivide grid width=50 loading=alley`. Yield critique via `lotreport` + the
  `report` plane, grounded like the M-enviro critique hooks.
- **Later (optional)**: a dynamic **Site tab** (reuse `M-dyntabs`) listing blocks/lots
  with counts + re-lot buttons. Not required for Phase 3.

---

## 11. Scope warning

Manuel marked ~90% of the questionnaire — full road generation, building footprints,
roof massing. Literally that's CityEngine + TestFit + a massing generator. **Do not
attempt Phases 10–12 in the first release.** Phases 1–8 are a real, useful, defensible
tool. Get Phase 3 into his hands early and let his reaction reorder everything after it.

---

## 12. Open blockers to clear before coding

1. **Confirm `i_overlay` polygon offset** (inward/outward buffer) quality — Phase-1 gate.
2. **Locate the Python prototype** — the plan references `/prototype/python`; it is not
   yet in this repo. Get it from Manuel/source before Phase 3 (port target).
3. **Manuel's §1 open questions** (width-mix products, alley dims, day-one, flag-lot
   area accounting). Voronoi is now resolved — ruled out.
