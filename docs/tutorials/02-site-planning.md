# Tutorial 2 — Site planning with intemfit

*You will take a raw site boundary, lay out streets and blocks, subdivide it into
lots, apply setbacks, drop buildings in the buildable envelopes, and read back a
yield report. About 15 minutes.*

> **Advisory, not zoning.** The intemfit tools are geometry aids for exploring a
> layout. The `euro_latam` metric defaults used below (setbacks, lot widths,
> floor heights) are **placeholders** — planning intent, not code. Confirm
> allowable use, density, coverage, and setbacks with the local zoning authority
> before relying on any number here.

---

## 1. Draw the site boundary

Every intemfit workflow starts from one closed boundary curve. A rectangle is
the simplest:

```
units m
rect 0,0,0 240 180
name last site
```

That is a 240 × 180 m parcel (4.32 ha). Any closed curve works — trace an
irregular parcel with `polyline` if you have one.

---

## 2. Lay out streets and blocks

`lotgeneratesite` reads the boundary and generates a road network plus the
blocks between the roads:

```
lotgeneratesite last orthogonal roadwidth=12 blockdepth=60 seed=7
```

- **`orthogonal`** is a rectilinear grid. Other patterns: `skewed`, `organic`,
  `culdesac`, and the non-rectilinear `radial`, `hexagonal`, `voronoi` — swap
  the word and re-run to compare.
- `roadwidth` is the right-of-way (curb to curb); `blockdepth` sets the spacing
  between roads.
- `seed` makes the layout deterministic, so it replays identically.

Roads bake onto a `roads` layer, blocks onto a `blocks` layer, and every block
edge is tagged with the street it fronts — which the next step honours.

---

## 3. Subdivide the blocks into lots

```
lotsubdivide all grid area=1400 width=18 seed=7
```

- **`grid`** is recursive minimum-area subdivision — it splits each block along
  its short axis until every lot is below the target `area`.
- `area` is the minimum lot area (the recursion stop); `width` is the minimum
  lot frontage.
- Every lot keeps a street-fronting edge. Lots bake onto a `lots` layer.

Other methods: `perimeter` (a frontage ring around an interior core). Sticky
defaults for all of these live in `lotsettings`.

---

## 4. Apply setbacks

Turn each lot into a **buildable envelope** by insetting it from its edges:

```
lotsetbacks all front=4 side=0 rear=4
```

- `front` insets from the street edge, `rear` from the opposite edge, `side`
  from the rest.
- `side=0` is a party-wall / medianería condition (row houses share walls) — the
  euro_latam default.

Envelopes bake onto a `setbacks` layer, kept distinct from the lots. A lot too
small for its setbacks collapses cleanly and is reported, never a crash.

---

## 5. Place buildings

```
lotbuilding all typology=row floors=3 roof=gable pitch=30
```

For every lot this computes the envelope, derives a footprint, extrudes it to a
`floors × floorheight` mass, and caps it with a roof — three objects per lot
(2D footprint, 3D mass, 3D roof) on a `buildings` layer.

- **`typology`**: `row` (party-wall terraces, matching `side=0`), `detached`,
  `courtyard`, `slab`.
- **`roof`**: `flat`, `gable`, `hip`, `shed`, or `auto`; `pitch` is the slope.

Frame it to see the result:

```
top
ze
```

![A generated site: row buildings inside setback envelopes on gridded blocks](../screenshots/site-plan.png)

---

## 6. Read the yield report

```
lotreport
```

This prints a Markdown table (it renders as a real grid in the deck panel) with
the numbers that matter — lot count and areas, **gross vs net developable area**
(net subtracts open space and reserved blocks), total **GFA**, building count,
**FAR**, and lot coverage. A sample run of this exact site reports something like:

| Metric | Value |
|---|---|
| Lots | 56 |
| Net developable area | 43 200 m² |
| Total GFA | ~154 000 m² |
| FAR (net) | 3.56 |
| Lot coverage | 66.2 % |

The report is stored on the document, so `report lotyield` re-shows it and the
deck can critique it in numbers (see
[Tutorial 4](04-deck.md)). Change a setting, run `lotreport` again, then
`lotreport compare` for an A/B diff.

---

## Try next

- Add open space before subdividing: `lotopenspace last type=park area=2000`,
  then re-run — watch net area and FAR change.
- Reserve whole blocks: `lotopenspace last reserve=20`.
- Swap the street pattern: re-run step 2 with `voronoi` or `culdesac`.
- The full worked file is
  [`examples/site-plan.itsjustcad.json`](../../examples/README.md).
- Exact usage for every `lot*` verb is in the
  [command reference](../command-reference.md).
