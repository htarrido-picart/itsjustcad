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
- **Voronoi** street networks — owner-requested 2026-09-04 (Manuel had it on "never use";
  Hector overrides for tool capability). A Voronoi generator: seed points (jittered grid
  or Poisson-disk) → Voronoi diagram → cell edges become streets, cells become blocks.
  Non-rectilinear like radial/hex. **We can derive it as the dual of our existing
  Bowyer-Watson Delaunay** (`kernel-mesh::triangulate`) — circumcenters of adjacent
  triangles are the Voronoi vertices — so no new geometry dep. Phase 5b with radial/hex.
- **Blind %-reserve open space** — owner-requested 2026-09-04 as an **opt-in keyword**,
  NOT the default. `lotopenspace reserve=<pct>` pulls whole blocks out of subdivision
  until ~pct of the *site* is open, biggest-and-most-central first (CityEngine's model:
  open space = a block you chose not to subdivide). Default stays feature-placement
  (`type=park|greenway|pond|treesave`, Manuel's preference); reserve is a separate mode
  a user asks for by keyword. Reserved blocks are tagged so `lotreport` nets them out
  (§11). `reserve=0` (default) = off.

### Explicitly ruled out — do not build

- **"No subdivision"** mode (whole site stays one parcel) — not requested by anyone.
  (Distinct from reserve, which excludes *selected blocks*, not the entire site.)

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
pub enum StreetPattern     { Orthogonal, Skewed, Organic, CulDeSac, Radial, Hexagonal, Voronoi } // Radial/Hexagonal/Voronoi = owner scope (Phase 5b)

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

    // Open space (Phase 9)
    pub open_space_reserve_frac: f64,  // 0.0 = off (feature-placement default); >0 = blind %-reserve mode (owner opt-in)
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

### 6b. Default profile — metric, European / Latin-American (placeholder until Manuel confirms)

**Units are metres / m².** Manuel works in Cali + Guayaquil; defaults follow
European + LatAm urban form (party-wall `medianería` lots, deep narrow `solares`,
continuous street-wall / build-to tradition, the Law-of-the-Indies gridiron), NOT US
suburban feet. **Every number below is a placeholder** sourced from typical Euro/LatAm
practice — flag it in output and swap when Manuel answers the §1 open questions.

| Setting | Default (m / m² / %) | Rationale |
|---|---|---|
| `lot_width_min` | 6 m | European terraced / narrow LatAm frontage |
| width mix (soft) | 6 / 8 / 10 m at 25 / 50 / 25 % | narrow-row → LatAm-standard blend; soft, not hard ratio |
| `lot_depth_target` / tol | 25 m / ±5 m | deep LatAm solar; European-dense variant 18 m |
| `lot_area_min` / target | 120 / 200 m² | row/terraced through standard urban lot |
| `loading` | FrontLoaded | continuous street facade is the tradition |
| `alley_width` | 5 m | `callejón` / mews (US would be ~6 m) |
| `setback_front` | 3 m | suburban; urban cores use `build_to_line = 0` |
| `setback_side` | **0 m** | party-wall / `medianería` — attached housing is the norm; detached overrides |
| `setback_rear` | 3 m | patio/courtyard behind |
| `corner_lot_width_bonus` | +15 % | `esquina` premium (corner lots larger in the grid) |
| `corner_angle_max` | 45° | as US |
| `allow_flag_lots` | false | uncommon in the formal grid; pole 3 m + excluded area if enabled |
| `sliver_area_frac` | 0.5 | as US |
| road ROW (Phase 5) | 12 m | residential `calle` |
| block depth (Phase 5) | ~2× lot depth (~50 m); LatAm `cuadra` option ~84–100 m | double-loaded block; the classic 100-vara ≈ 84 m block |
| `irregularity` | 0.0 (formal grid); cap 0.4 (Manuel); `loose` → 1.0 (owner) | grid regularity by default |

Ship this as a named **`region` profile** (`euro_latam` default; a `us_suburban` profile
with feet-derived numbers can be added later). The §6 struct defaults should carry the
metric `euro_latam` values, not the imperial examples shown in the field comments.

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
- **Voronoi** — seed points (jittered grid / Poisson-disk, seeded from op data for
  replay), Voronoi diagram via the dual of `kernel-mesh::triangulate` (circumcenters of
  adjacent Delaunay triangles = Voronoi vertices), clipped to the site; cell edges →
  streets, cells → blocks. Irregular by nature — pairs naturally with `loose`.
These raise the §8 validation bar: add annular-sector, hex-cell, and Voronoi-cell blocks.
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
13. Voronoi-cell block (irregular convex polygon — owner scope)

**Assertions (all cases):**
- Σ lot area == block area within tolerance
- No overlapping lots; no gaps
- Every lot has a street edge when `force_street_access == 1.0`
- All lot widths ≥ `lot_width_min` where the method guarantees it
- No lot below `sliver_area_frac × lot_area_min` when `merge_slivers` on
- **Deterministic for a fixed seed** (also gives the ItsJustCAD replay invariant)

---

## 9. Phase definitions of done

- **Phase 1** — ✅ **DONE (2026-09-04).** `crates/subdivision` pure-Rust leaf crate
  (no egui/doc/commands deps): `Polygon2d` (CCW, shoelace, point-in-poly, centroid),
  `OrientedBox` (min-area rect via rotating calipers on the monotone-chain hull —
  rotated-rectangle tests recover correct long/short axes), `PolylineTools` (resample,
  Douglas–Peucker simplify, perpendicular-at-param, arc-length), `split_by_line`
  (Sutherland–Hodgman half-plane clip — all Phase 3 needs), and `clip_bridge` (the sole
  `i_overlay` touch-point, int-scale ×1000 = `CLIP_SCALE` defined once). **i_overlay
  offset support + AGPLv3 compatibility CONFIRMED** (see §12.1). Offset round-trips with
  no drift; boolean union/intersection/difference pass. 10 sample blocks in
  `samples/blocks/*.json` (§8 cases 1–10; #11/#12/#13 deferred to Phase 5b). 31 unit +
  7 §8 integration tests green; clippy clean.
- **Phase 2** — ✅ **DONE (2026-09-04).** `lotsubdivide` + `lotsettings` run from the
  command line + deck (registry-registered → GBNF auto-derives + palette). Settings are
  sticky across save/reload (`Document::subdivision_settings`, serde-default so
  pre-intemfit files load; logged so replay reproduces them). The bridge
  (`commands/src/lot.rs`) converts closed doc curves ↔ `Polygon2d`; `perimeter`/
  `streetfollowing` return a clear "not yet implemented (Phase 4/7)" error, never a
  panic. Runs fully headless. **Preview overlay: SKIPPED for this milestone** — the verb
  bakes on run (acceptable per the plan's "bake-on-run is acceptable"); results bake as a
  logged op with written-back ids (contours precedent) so undo is one `CreatedOnLayer`
  inverse and replay is byte-identical. A viewport-overlay preview is a cheap later
  follow-up (draw the same lot polygons before commit).
- **Phase 3** — ✅ **DONE (2026-09-04).** Recursive OBB (`method=grid`) in
  `subdivision/src/subdivision/recursive_obb.rs`: min-area OBB → cut along the short axis
  pivoted on the long-axis midpoint → recurse while area > `lot_area_min`; terminates when
  area < min OR any child side < `lot_width_min` (a high `lot_width_min` forcing lots
  above `lot_area_max` is left correct, not "fixed"). Four modifiers: street-access
  fallback (orthogonal split; every original block edge counts as frontage in Phase 3 —
  no street graph yet), snap-to-contour-vertex, edge-alignment (OBB hull-edge reference),
  seed-before-recurse. **Deterministic for a fixed seed** (splitmix64 seeded from a
  quantized block hash + `settings.seed`) — replay byte-identical (verified by a Session
  replay test). All §8 assertions pass on all 10 base blocks (`tests/blocks.rs`): area
  conserved, no overlaps, every lot has a street edge under `force_street_access=1.0`,
  widths ≥ `lot_width_min` where guaranteed, determinism. **Send a build to Manuel here.**
- **Phase 4** — ✅ **DONE (2026-09-04).** Offset / perimeter subdivision
  (`method=perimeter`) in `subdivision/src/subdivision/offset_sub.rs`: inward-offset
  the block by `offset_width` via `clip_bridge::offset` (i_overlay) to get the
  interior **core**; walk the boundary sampling at a spacing derived from
  `lot_area_min / offset_width` (≥ `lot_width_min`), jittered by `irregularity`
  (splitmix64 seeded from a quantized block hash + `settings.seed`, salted apart
  from recursive_obb → replay byte-identical); cut the block with lines **orthogonal
  to the boundary** at each sample and keep each wedge minus the core as a perimeter
  lot; if `subdivide_core`, run `recursive_obb` on the core, else keep it as one
  hollow-ring lot. **Degenerate fallbacks verified** (`tests/offset_blocks.rs`):
  `offset_width ≈ 0` → falls back to recursive OBB (byte-identical); huge
  `offset_width` collapsing the interior → clean fallback, no panic; thin rectangle
  (#1) whose inset self-collapses → fallback; plus a covered-area safety net that
  falls back rather than emit an under-covered block. All §8 assertions pass on the
  10 base blocks: Σ lot area == block area within tol, no overlaps, every lot is a
  real (positive-extent) ring, deterministic for a fixed seed. `subdivide_core`
  on/off proven to give core lots vs a hollow ring. `lot.rs` wires `method=perimeter`
  (replaces the deferral) with the same bake path as grid: logged op, written-back
  ids on the `lots` layer, undo, replay byte-identical (verified by a Session replay
  test).
- **Phase 5** — ✅ **DONE (2026-09-04).** Road-network generation + block
  extraction + street tagging via `lotgeneratesite`. `crates/subdivision/src/streets/`:
  `street_graph.rs` (`StreetGraph` = centerlines + widths + hierarchy tier
  {Spine/Connector/Stub/Alley} + endpoint adjacency) and the FOUR rectilinear
  generators in `generators/` — **orthogonal** (recursive min-area-OBB split of the
  site to ~2× block depth, a spine centerline per split), **skewed** (same, split
  directions globally rotated 22.5°), **organic** (a sinusoidally-deviated spine down
  the site long axis + perpendicular connectors spaced by block depth), **culdesac**
  (a long-axis spine + alternating perpendicular stubs ending in turnaround bulb
  loops). `block_extractor.rs` offsets each centerline by ROW/2 into a ribbon and
  boolean-subtracts each ribbon from the site (`clip_bridge`), subtracting ribbons
  one-by-one (not a pre-union) so disjoint ribbons never merge and swallow interior
  blocks. `blocks/block.rs` + `block_edge.rs`: **every `BlockEdge` carries
  `{is_street, street_id, street_width, street_length, is_alley}`** (the §5 hard
  dependency) — an edge is `is_street` when its midpoint sits ½-width from a street
  centerline AND is not on the untouched site boundary, tagged with that street's
  id/width/length; original-boundary non-street edges stay `is_street=false`. **Alley
  tier**: with `loading=AlleyLoaded`, each block is bisected along its long axis into
  two sub-blocks whose shared edge is tagged `is_alley` (width `alley_width`); without,
  none. Verb `lotgeneratesite [selector] <pattern> roadwidth= blockdepth= alleys=on|off
  seed=` (registry-registered → GBNF/deck; parse/dispatch/exec) bakes roads onto a
  `roads` layer + blocks onto a `blocks` layer as one logged op (road ids then block
  ids written back; undo `CreatedOnLayer` removes both; **seeded-deterministic →
  replay byte-identical**). `StreetPattern` + `road_width` + `block_depth` added to
  `SubdivisionSettings` (serde-default). radial/hex/Voronoi return a clean
  "Phase 5b" error, never a panic. **Confirmed the tagged block geometry survives
  into the bake** so a downstream `lotsubdivide` on a generated block honours
  `force_street_access` (test `generated_block_subdivides_with_street_access`). Tests
  (all green): `tests/streets.rs` — the 4 generators on a rectangular + L-shaped site
  (valid graph, non-overlapping blocks, coverage = site − ROW within tol, no gaps),
  **street-tagging correctness on a known 2×2 orthogonal grid** (each corner block
  fronts exactly 2 roads with the right id/width; outer edges `is_street=false`),
  alley tier present iff `AlleyLoaded`, byte-identical for a fixed seed; plus
  street_graph/block_extractor unit tests + `lotgeneratesite` exec (bake/undo/replay)
  + `lot.rs` bridge tests. Phase 5b (radial/hex/Voronoi) and Phase 6 (lot rules) pick
  up from here.
- **Phase 5b** — ✅ **DONE (2026-09-04).** The three NON-rectilinear street
  generators (owner scope), each emitting a `StreetGraph` fed into the SAME
  `block_extractor` + street-tagging path as Phase 5:
  - **Radial / circular** (`streets/generators/radial.rs`): its own polar layout —
    a center (site centroid; N centers spread along the long axis for large sites)
    → concentric ring roads (Connector tier) at block-depth spacing + radial spokes
    (Spine tier), each clipped to the site boundary into inside runs. Blocks are
    annular-sector polygons. Does NOT use OBB site-splitting.
  - **Hexagonal** (`hexagonal.rs`): a pointy-top hex lattice sized so each cell is
    ≈ one block deep (circumradius = depth/√3), tiled over the padded site bbox;
    every distinct hex-cell edge (deduped) is one street centerline. Blocks are the
    hex cells; boundary cells clipped by the extractor boolean.
  - **Voronoi** (`voronoi.rs`): jittered-grid seeds (splitmix64 seeded from a
    quantized site hash + `settings.seed`, salted apart → replay-stable) → the
    Voronoi diagram built as the **dual of `kernel_mesh::triangulate`** (Bowyer-
    Watson Delaunay; circumcenters of the two triangles sharing a Delaunay edge =
    a Voronoi edge; hull-edge rays dropped, site clips the rest). `kernel-mesh`
    added as a subdivision dep — it depends only on glam+serde (no subdivision), so
    no cycle, and it is a workspace crate so **no new external dependency**. Cell
    edges → streets, cells → blocks.
  All three wired into `generate()` dispatch + `lotgeneratesite` (replacing the
  clean-error stubs in `lot.rs` `parse_pattern`/`generate_site`). Same bake path:
  roads→`roads` layer, blocks→`blocks` layer, one logged op, undo, seeded-
  deterministic → byte-identical replay. Street tagging via the shared
  `block_extractor` so every emitted block still carries `{is_street, street_id,
  street_width, street_length, is_alley}`; a generated non-rectilinear block feeds
  `lotsubdivide` and honours `force_street_access`. The extractor gained a
  block-depth-derived crumb floor (`0.03·block_depth²`) so the many overlapping
  curved ribbons the polar/hex/Voronoi paths subtract cannot leave sub-cell slivers
  that pairwise-overlap (rectilinear blocks are ~block_depth², untouched). Sample
  blocks #11 annular-sector / #12 hex-cell / #13 Voronoi-cell added to
  `samples/blocks/` and run through the shared §8 subdivision assertions.
  **Tests:** `tests/streets.rs` grew 6 Phase-5b tests — the three generators on a
  rectangular + L-shaped site (valid graph, non-overlapping blocks inside the site),
  street-tagged blocks, byte-identical fixed-seed replay, radial ring/spoke
  geometry, hexagonal interior-cell size, Voronoi cell-count-tracks-seeds + the
  **Delaunay-dual correctness on a known square seed set** (dual vertex = center),
  and downstream `lotsubdivide` honouring force_street_access; plus per-generator
  unit tests + updated `lot.rs`/`exec.rs` bridge tests. Phase 6 (lot rules: width
  mix, depth, corner, flag, front/alley loading) picks up here.
- **Phase 6** — ✅ **DONE (2026-09-04, on euro_latam placeholder defaults).** Lot
  rules in `crates/subdivision/src/subdivision/lot_rules/`, applied as a post-pass /
  mode on subdivision. **WidthMixSolver** (`width_mix.rs`) — the packing problem:
  greedy fill weighted by running proportion deficit (pick the product whose count
  share is furthest below target and still fits; ties → narrower first,
  deterministic), then a swap pass widening the LAST lot to absorb the remainder at
  the end (slack concentrated, never smeared). **Hits the requested proportions
  within 5% on a 500 m frontage** (metric, §9) — verified analytically. **DepthController**
  (`depth.rs`) — independent depth band (euro_latam 25 m ±5), clamp + in-band, separate
  from area. **CornerLots** (`corner.rs`) — interior angle < `corner_angle_max` (45°) →
  widen the corner lot by `corner_lot_width_bonus` (euro_latam +15%), width auto-clamped
  to available slack (no self-intersection; verified on the 15° acute block #10).
  **FlagLots** (`flag.rs`) — only if `allow_flag_lots` (euro_latam false); pole width ≥
  `flag_pole_width_min` (3 m); **pole area excluded from countable lot area** (§7.4 open —
  implemented as the exclusion; ASSUMPTION to confirm with Manuel, surfaced in the note).
  **LoadingStrategy** (`loading.rs`) — FrontLoaded (full depth) vs AlleyLoaded (two-
  frontage street→alley half depth; requires an `is_alley` block edge from Phase 5's
  alley tier; degrades to front-loaded when absent). **SliverMerger** (`sliver.rs`) —
  repeatedly merge any lot below `sliver_area_frac × lot_area_min` (0.5) into its
  largest-shared-edge neighbour until none remain (biggest professional-vs-generated
  difference). A named **`region` profile** (`euro_latam` default carrying the §6b metric
  placeholders; `us_suburban` stub) + `LotWidthMix` added to `SubdivisionSettings`
  (serde-default); `effective_*` accessors return `(value, used_placeholder)` so any run
  resolving a rule from the profile prints **"using euro_latam defaults (placeholder —
  confirm with Manuel)"**. Wired into `lotsubdivide` (width-mix is opt-in frontage packing;
  `apply_lot_rules` = corner + sliver runs on every subdivide) + new `lotsettings` keys
  (region/loading/widthmix/depth/corner/flag/mergeslivers/…) + a `lotloading [sel]
  front|alley` verb (sticky, logged, undoable, replay-stable). All Σ-area-conserved,
  seeded-deterministic (byte-identical replay). Tests: 30 unit + 9 §8/§9 integration
  (`tests/lot_rules.rs`) + 6 commands exec. **Placeholders IN USE — swap when Manuel
  answers §1:** width mix 6/8/10 m @ 25/50/25%, depth 25±5, corner +15%, alley 5 m, flag
  pole 3 m + pole-area exclusion. Phase 7 (skeleton) picks up from here.
- **Phase 7** — ✅ **DONE (2026-09-04).** Skeleton / street-following subdivision
  (`method=streetfollowing`). New `crates/subdivision/src/straight_skeleton/`: a
  `StraightSkeleton` trait (interface) + `offset_approx.rs` = **OffsetApproxSkeleton**,
  the Phase-7 approximate straight skeleton. It partitions the block into one
  **face per contour edge** as the *nearest-edge* region (each face = the block
  clipped by the perpendicular bisector between that edge's supporting line and
  every other edge's — exactly the seam an inward offset wavefront carves, i.e.
  the offset-approximate skeleton per §5, computed in closed form per edge so it
  is robust and never panics on non-convex/notched blocks; coverage-checked with a
  centroid-fan fallback). `medial_axis_ridge` (iterated `clip_bridge::offset`
  insets) gives the ridge the unit tests assert on (square → meets near centre;
  rectangle → medial ridge). Felkel (`felkel.rs`) stays **Phase 12** — the trait is
  left so it slots in later. `subdivision/skeleton_sub.rs` (SkeletonSubdivision)
  runs the §7.5 steps: (1) straight skeleton → faces; (2) group adjacent faces
  along runs of similar-curvature street edges into bands (curvature turn < 40° →
  one band, so a curved street reads as one run, not a per-segment fan; uses the
  block's `is_street` tags, every edge counts as frontage on an untagged block);
  (3) corner regions assigned by `CornerAlignment` (widest street wins, tie-break
  on length); (4) slice each band **perpendicular to its street edge(s)** at
  `lot_width_min` spacing — a closed-loop band (cul-de-sac bulb) is **pie-sliced
  from the centroid to the perimeter** (perpendicular-to-curve wedges, exact-
  tiling), an open curved band slices each convex face by the shared global
  perpendicular cut lines so lot lines line up across faces; (5) merge lots below
  `lot_area_min`; (6) merge shallow/triangular lots per `shallow_lot_frac`; (7)
  `simplify` vertex reduction. All merges are **area-conserving** (single-ring
  unions only — never drop the smaller piece), and a `force_street_access` pass
  folds any streetless wedge into a street neighbour. **Deterministic** (pure
  geometry, no RNG) → op-log replay byte-identical. Wired into `lot.rs`
  (`method=streetfollowing` replaces the deferral; same bake path as grid/perimeter
  — logged op, written-back ids on the `lots` layer, undo, replay-stable) via a
  shared `subdivide_by_method` dispatch; width-mix packing no longer overrides the
  skeleton's own slicing. **Tests (all green):** `straight_skeleton` unit tests
  (square faces meet near centre + tile to 4 equal faces; rectangle medial ridge;
  notched block no-panic + covers; triangle tiles); `tests/skeleton_blocks.rs` — the
  §8 blocks that matter (#1 long-thin, #4 cul-de-sac bulb, #5 curved-street varying
  radius, #7 one short street edge) asserting Σ lot area == block area within tol,
  no overlaps, no gaps (>95% sampled coverage), every lot has a street edge under
  `force_street_access`, **lots roughly perpendicular to the street on the bulb +
  curved cases** (each lot's inward side aligns with the local street normal within
  30°, ≥60% of lots — the §9 visual claim as a structural proxy), lot count in a
  sane range, deterministic byte-identical; plus `lot.rs`/`exec.rs` bridge tests
  (method runs, bakes, undo + byte-identical replay). Phase 8 (setbacks +
  buildable envelopes) picks up from here.
- **Phase 8** — setbacks + buildable envelopes render per lot; frontage at setback line.
- **Phase 9** — feature placement (`type=park|greenway|pond|treesave`) works; the
  opt-in `reserve=<pct>` mode excludes whole blocks until ~pct of the site is open,
  central-and-large first, and tags them as open space. Default (`reserve=0`) unchanged.
- **Phase 11** — `lotreport` reports yield on **net developable area** (site minus
  open-space features AND reserved blocks), not gross — a gross number lies once open
  space exists. Report both gross and net so the ratio is visible. Option comparison
  diffs two settings runs.
- **Phases 10, 12** — as specified; re-scope with Manuel before Phase 10.

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
| `lotopenspace` | region | type=park\|greenway\|pond\|treesave, area · OR reserve=<pct> (owner opt-in blind %) |
| `lotreport` | lots/site | → AnalysisReport (yield: lot count, avg area, frontage, **net-of-open-space**) |
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

1. ✅ **RESOLVED (2026-09-04) — `i_overlay` polygon offset works.** `i_overlay` 8.1.0
   (crates.io, **MIT OR Apache-2.0 → AGPLv3-clean**, pure Rust, no C/FFI) ships BOTH
   boolean overlay (`SingleFloatOverlay::overlay` — union/intersection/difference/xor)
   AND polygon OFFSET via the `OutlineOffset` trait (`OutlineStyle` with independent
   outer/inner offset + Miter/Round/Bevel joins; `outline_fixed_scale` pins the
   float→int scale we control). Verified in `clip_bridge.rs` tests: inward offset shrinks
   area, outward grows it, a deep inset collapses to empty, and boolean ops give the
   expected areas. **No fallback to `geo` or a vendored Clipper2 needed.** Phase 3
   (recursive OBB) does not use offset at all — it uses only the half-plane clip — so the
   bridge is wired + smoke-tested now for Phase 4+ but is not on the Phase-3 critical path.
2. **Locate the Python prototype** — the plan references `/prototype/python`; it is not
   yet in this repo. Get it from Manuel/source before Phase 3 (port target).
3. **Manuel's §1 open questions** (width-mix products, alley dims, day-one, flag-lot
   area accounting). Voronoi is now an owner-requested Phase-5b generator (dual of our
   Delaunay), not ruled out. ✅ **Phase 5b SHIPPED 2026-09-04** — radial/hexagonal/
   Voronoi all built; Voronoi reuses `kernel_mesh::triangulate` (no new external dep,
   no cycle). See §9.
4. ⚠️ **Phase 6 SHIPPED 2026-09-04 ON PLACEHOLDER DEFAULTS.** The lot rules are built
   and tested, but they run on the metric **euro_latam** profile (§6b) because the §1
   open questions are still unanswered. **Still needed from Manuel to finalize the
   defaults:** (2) his real width-mix product list + whether the mix is a hard ratio
   or soft preference (currently soft 6/8/10 m @ 25/50/25%); (3) alley ROW width +
   whether alleys are required on every block or only some (currently 5 m); (4) whether
   the flag-lot pole area is excluded from countable area (currently EXCLUDED — the
   assumption implemented). Every euro_latam value is flagged at runtime as
   "placeholder — confirm with Manuel"; swap the `euro_latam` profile numbers in
   `settings.rs` once he answers.
