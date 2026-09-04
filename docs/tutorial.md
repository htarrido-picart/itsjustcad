# Tutorial: Courtyard Building

This tutorial models a small courtyard building from scratch, then cuts a plan, places it on a sheet, and exports a PDF. Every command here is real and can be typed as shown.

Estimated time: 20 minutes.

---

## 1. Set up the document

```
units m
layer walls
```

---

## 2. Draw the outer shell

A 20 × 14 m rectangle extruded to 6 m (two storeys):

```
rect 0,0,0 20 14
extrude last 6
name last shell
```

---

## 3. Cut the courtyard void

A 10 × 6 m courtyard centred in the footprint:

```
rect 5,4,0 10 6
extrude last 7
name last court-void
difference shell court-void
name last building
```

The void is slightly taller (7 m) than the shell so the difference cuts cleanly through.

---

## 4. Add a structural grid

```
grid main x A:0 B:5 C:10 D:15 E:20  y 1:0 2:4.67 3:9.33 4:14  levels 0,3,6
```

---

## 5. Define a column section and material

```
section col rect 0.4 0.4
material concrete 30e9 2400
```

---

## 6. Place columns at the grid intersections

Ground floor (z = 0 → 3):

```
column 0,0,0   0,0,3   col material concrete
column 5,0,0   5,0,3   col material concrete
column 10,0,0  10,0,3  col material concrete
column 15,0,0  15,0,3  col material concrete
column 20,0,0  20,0,3  col material concrete
column 0,14,0  0,14,3  col material concrete
column 5,14,0  5,14,3  col material concrete
column 10,14,0 10,14,3 col material concrete
column 15,14,0 15,14,3 col material concrete
column 20,14,0 20,14,3 col material concrete
group last 10 cols-gf
```

---

## 7. Cut a plan at 1.2 m

```
plan 1.2
```

Wall outlines land as closed polylines on layer `sections`. Geometry below the cut (if any) projects to layer `sections-proj`.

---

## 8. Style the plan layers

```
layercolor sections 0,0,0
layerweight sections 0.35
layercolor sections-proj 0.4,0.4,0.4
layerweight sections-proj 0.13
hide cols-gf
```

---

## 9. Add a dimension

```
dim 0,0 20,0 1
text 10,-2,0 "20 000" 0.4
```

---

## 10. Set up a sheet and place the plan view

```
sheet ground-floor a1
sheetview ground-floor top 1:100
```

---

## 11. Print to PDF

```
print ground-floor /tmp/ground-floor.pdf
```

The PDF is a vector file at the correct 1:100 scale on an A1 sheet.

---

## 12. Save the document

```
save courtyard.itsjustcad.json
```

---

## What to explore next

- Add the first-floor slab: `slab 0,0 20,0 20,14 0,14 thick 0.25 material concrete`
- Cut a section through the courtyard: `section all 10,0,3 0,1,0`
- Run a shadow study: `location 40.71 -74.01 -5` then `shadowstudy 2024-06-21 09:00 15:00 60`
- Export IFC for coordination: `export /tmp/courtyard.ifc`

---

# Mini-flow: site, terrain, planting, and a sun study

A short second walkthrough — build ground, grade a pad, plant a tree row, then
study the sun and read the numbers back.

## 1. Set the location and build terrain

The location georeferences the document and drives every solar study.

```
location 40.71 -74.01 -5
terrain /tmp/survey.csv
contours 0.5 5
```

`contours` extracts contour polylines from the terrain mesh — minor lines on
layer `contours`, index (major) lines on `contours-major`.

## 2. Grade a building pad

```
pad 30,20 24 16 4.0 2
cutfill
```

`pad` cuts a flat 24 × 16 m pad at elevation 4.0 with 2:1 side slopes. `cutfill`
reports the cut and fill volumes vs the pre-grading terrain and stores a report.

## 3. Plant a row of street trees

```
plantrow tilia 6,4 42,4 8
plantsymbols on
```

A row of lindens every 8 m, draped on the terrain. `plantsymbols on` overlays
2D plan glyphs so the top view reads as a planting plan. (Browse the catalog
with `plantcatalog` — or `plantcatalog caribbean` for the tropical pack.)

## 4. Run a sun study and read the report

```
sunhours 2024-06-21 1.5
report sunhours
```

`sunhours` casts rays to the sun every 30 min across the date and colours a
ground heatmap; planted canopies occlude sun like any mesh. `report sunhours`
prints min/avg/max, a distribution, and the worst/best sample locations — the
numbers to critique the design with.

---

# Mini-flow: a code-compliance pre-check

Tag a room, run a check pack, and read the verdicts.

> **Advisory only.** `codecheck` runs geometric pre-checks — it never certifies
> that a design complies with any code. Verify with a licensed professional / AHJ.

## 1. Tag a room and name an exit

```
rect 0,0,0 12 8
room sel business Office
name last office
```

`room` tags a closed region with an IBC use group and computes its area; it feeds
the occupant-load / exit-count / travel-distance checks. Exits are objects whose
name contains `exit` (e.g. `name last exit-1`).

## 2. Run the pack and read the report

```
codecheck ibc2021
report codecheck
```

`report codecheck` lists per-rule verdicts (pass / fail / warn / info) with the
measured value vs the required threshold, the IBC section reference, and the ids
of any violating objects — which also get coloured markers on the `compliance`
layer. Swap `ibc2021` for `ada2010` for accessibility geometry, or
`checkrules load <path.checks.json>` to bring your own pack.
