# Command Reference

Every command in ItsJustCAD is a typed verb. The same verbs work in the command bar, in `--run` scripts, and when the LLM deck emits them.

The full machine-readable listing is in [COMMANDS.txt](COMMANDS.txt). This page organises them by category with brief notes.

---

## Selectors

Used wherever a command needs to target objects:

| Selector | Meaning |
|---|---|
| `last` | most recent object |
| `last N` | N most recent |
| `all` | every object |
| `sel` | current click-selection |
| `<name>` | objects given that name with `name` |

---

## 2D Drawing (`Draw2d`)

| Command | Usage |
|---|---|
| `line` | `line <a x,y,z> <b x,y,z>` |
| `polyline` | `polyline <p1> <p2> … [closed]` |
| `rect` | `rect <corner x,y,z> <width> <height>` |
| `circle` | `circle <center x,y,z> <radius>` |
| `arc` | `arc <center x,y,z> <radius> <start deg> <end deg>` |
| `ellipse` | `ellipse <center x,y,z> <rx> <ry>` |
| `polygon` | `polygon <center x,y,z> <radius> <sides>` |

---

## Curves (`Curve`)

| Command | Usage |
|---|---|
| `curve` | `curve <p1> <p2> … [degree N]` — NURBS by control points |
| `interpcurve` | `interpcurve <p1> <p2> … [closed]` — C2 cubic through points |
| `helix` | `helix <center x,y,z> <radius> <height> <turns>` |
| `setpoint` | `setpoint <selector> <index> <x,y,z>` — edit one control point |
| `rebuild` | `rebuild <selector> <count>` — resample to N points |
| `split` | `split <selector> <point x,y>` — split at nearest point |
| `trim` | `trim <target> <cutter> <keep x,y>` — trim at intersections |
| `extend` | `extend <selector> <distance>` — lengthen open ends |
| `join` | `join <selector>` — join end-touching curves |
| `fillet` | `fillet <a> <b> <radius>` — round a corner |
| `offset` | `offset <selector> <distance>` — parallel offset |

---

## Solids (`Solid`)

| Command | Usage |
|---|---|
| `box` | `box <corner x,y,z> <size x,y,z>` |
| `extrude` | `extrude <selector> <height>` |
| `revolve` | `revolve <selector> [axis point] [axis dir] [angle deg]` |
| `loft` | `loft <selector> [guides <selector>]` — skin 2+ stacked closed curves; optional open guide curves bow the skin |
| `blend` | `blend <curve a> <curve b> [bulge]` — Hermite-eased blend sheet between two curves (bulge 1 = ruled) |
| `sweep` | `sweep <profile> <rail>` |
| `sweep2` | `sweep2 <profile> <rail-a> <rail-b>` |
| `railrevolve` | `railrevolve <profile> <rail> <axis pt> <axis dir>` |
| `pipe` | `pipe <curve> <radius> [end radius]` — round pipe |

Example: `loft last 3` · `loft rings guides rail` · `blend top bottom 1.5`

---

## Structure & form-finding (`Structure`)

Parametric shells, lattices, and dynamic-relaxation form-finding. These build geometry only — the app never runs a structural analysis.

| Command | Usage |
|---|---|
| `geodesic` | `geodesic <frequency> <radius> [dome\|full]` — Buckminster-Fuller geodesic dome/sphere |
| `spaceframe` | `spaceframe <nx> <ny> <bay> <depth>` — double-layer space-frame grid |
| `hypar` | `hypar <a> <b> <c> [nu] [nv]` — hyperbolic-paraboloid (Candela) saddle shell |
| `gaussvault` | `gaussvault <span> <length> <rise> [undulate]` — Gaussian catenary brick vault (Dieste) |
| `gridshell` | `gridshell hypar <a> <b> <c> [nu] [nv]` \| `gridshell vault <span> <length> <rise> [undulate] [nu] [nv]` — reciprocal lattice on a doubly-curved surface |
| `funicular` | `funicular <support a> <support b> [segments] [load] [slack] [invert]` — hang a chain to a catenary; `invert` flips to the compression arch (Gaudí) |
| `tensegrity` | `tensegrity <struts> [radius] [height] [twist deg]` — compression struts in a tension net (`tensegrity 3` = classic T-prism) |
| `cablenet` | `cablenet <c0> <c1> <c2> <c3> [n] [sag]` — tensile minimal surface over four anchors (Frei Otto) |
| `minsurf` | `minsurf <closed curve selector> [n]` — stretch a soap-film minimal surface across a closed boundary (aliases `soapfilm`/`minimalsurface`) |

Examples: `geodesic 3 5 dome` · `hypar 5 5 5` · `funicular -5,0,0 5,0,0 24 1 1.4 invert` · `cablenet 0,0,0 8,0,0 8,8,3 0,8,3 8 1.5` · `polyline 0,0,1 6,0,-1 6,6,1 0,6,-1 closed` then `minsurf last`

### Live parametric editing

Built-in parametric structures (geodesic, hypar, gaussvault, gridshell, funicular, tensegrity, cablenet, spaceframe) **stay editable after creation** — they appear in the **Parameters** tab (right dock) as cards with a schema-driven editor (sliders / numeric fields / dropdowns / toggles). Editing a value re-derives the mesh live.

| Command | Usage |
|---|---|
| `paramset` | `paramset <selector> <key=value …>` — edit a parameter of a selected parametric structure; its mesh re-derives immediately. E.g. `paramset last frequency=4` · `paramset last rise=5` |
| `freeze` | `freeze <selector>` (alias `bake`) — flatten a parametric structure to a plain static mesh: keeps the geometry but drops the generator + params, so it leaves the Parameters tab and is no longer editable |

---

## Sketch constraints (`Curve`)

A 2D parametric constraint solver (Newton–Raphson with Levenberg–Marquardt damping) over lines and circles/arcs.

| Command | Usage |
|---|---|
| `constrain` | `constrain <kind> <selector> [selector] [value]` — add a constraint and re-solve; kinds: coincident, horizontal, vertical, distance, length, angle, parallel, perpendicular, equal, radius, fixed, tangent, midpoint, on |
| `solveconstraints` | re-run the solver over all stored constraints; reports solved / DOF / redundant / conflicting |
| `constraints` | `constraints [list \| delete <n> \| clear]` — list, delete one, or clear all |

Examples: `constrain horizontal last` · `constrain length name:l1 5` · `constrain perpendicular name:l1 name:l2`

---

## Booleans (`Boolean`)

| Command | Usage |
|---|---|
| `union` | `union <selector>` — merge meshes |
| `difference` | `difference <target> <tool>` — subtract |
| `intersect` | `intersect <selector>` — keep shared volume |

Inputs are consumed; one result mesh replaces them.

---

## Transform (`Transform`)

| Command | Usage |
|---|---|
| `move` | `move <selector> <delta x,y,z>` |
| `rotate` | `rotate <selector> <angle deg> [x\|y\|z] [about <x,y,z>]` |
| `scale` | `scale <selector> <factor \| fx,fy,fz> [about <x,y,z>]` |
| `mirror` | `mirror <selector> <xy\|yz\|xz \| point normal>` |
| `copy` | `copy <selector> <delta x,y,z>` |
| `array` | `array <selector> <nx,ny,nz> <dx,dy,dz>` |
| `polararray` | `polararray <selector> <count> [center] [angle deg]` |

---

## Edit (`Edit`)

| Command | Usage |
|---|---|
| `delete` | `delete <selector>` |
| `name` | `name <selector> <name>` |
| `group` | `group <selector> [name]` |
| `ungroup` | `ungroup <selector>` |
| `select` | `select <selector>` |
| `selectnone` | clear selection |
| `undo` / `redo` | undo/redo last geometry command |
| `amend` | `amend <step> <command…>` — rewrite history at step |
| `option` | `option save <name> \| option <name> \| option list \| option delete <name>` |

---

## Annotation / Layers (`Annotate`)

| Command | Usage |
|---|---|
| `text` | `text <pos x,y,z> <words…> [height]` |
| `hatch` | `hatch <selector> [solid \| lines \| crosshatch \| brick \| concrete \| insulation \| earth \| ansi31..ansi38 [spacing]]` — patterns incl. the ANSI standard set (iron/steel/bronze/plastic/fire-brick/marble/lead/aluminum) |
| `dim` | `dim <a> <b> [offset]` — each anchor is a point `x,y,z` **or** an associative binding `@<object>.<endpoint>` that follows the object when it moves (`<endpoint>` = `start`\|`end`\|`center`\|`cN` bbox-corner 0–7\|`vN` vertex). E.g. `dim @wall.start @wall.end` |
| `layer` | `layer <name>` — create/switch current layer |
| `tolayer` | `tolayer <selector> <name>` |
| `layercolor` | `layercolor <layer> <r,g,b>` |
| `layerweight` | `layerweight <layer> <mm>` — print lineweight |
| `layerrename` | `layerrename <from> <to>` |
| `layerdelete` | `layerdelete <layer>` — objects move to the default layer |
| `layerorder` | `layerorder <layer> <n>` — sort order in the Layers panel |
| `layerlock` | `layerlock <layer> <on\|off>` |
| `layerlinetype` | `layerlinetype <layer> <continuous\|dashed\|dotted\|dashdot>` |
| `hide` / `show` | toggle layer visibility |
| `hideobj` / `showobj` | toggle per-object visibility |
| `color` | `color <selector> <r,g,b \| off>` — per-object colour |
| `lineweight` | `lineweight <selector> <mm \| iso-name \| off>` — per-object print lineweight |
| `showweights` | `showweights <on\|off>` — draw real lineweights in the viewport |
| `material2` | `material2 <selector> <concrete\|glass\|metal\|wood \| roughness=.. metallic=.. color=r,g,b \| off>` — render appearance material (distinct from structural `material`) |
| `units` | `units <m \| cm \| mm \| ft \| in \| ftin>` |
| `block` | `block <selector> <name>` — define a reusable block |
| `insert` | `insert <name> <pos> [rotation deg] [scale] [param=value …]` |
| `pblock` | `pblock <name> [param=default …] : templ ; templ ; …` — dynamic (parametric) block |
| `param` | `param <selector> <key=value …>` — edit a dynamic-block instance's params |
| `blockdelete` | `blockdelete <name>` — delete a block definition (refuses while instances exist) |
| `blocks` | list block definitions |
| `blocklib` | list on-disk block library |
| `blockload` | load a library block into the document |
| `blocksave` | `blocksave <name> [description]` — save a block definition to the library |

---

## Sections / Drawings (`Dimension`)

| Command | Usage |
|---|---|
| `section` | `section <selector> <point> <normal>` — plane cut |
| `plan` | `plan <height>` — horizontal plan cut at z |
| `elevation` | `elevation <north\|south\|east\|west> [depth]` |
| `sheet` | `sheet <name> [a4\|a3\|a2\|a1\|a0]` |
| `sheetview` | `sheetview <sheet> <top\|front\|right\|persp> <scale>` |
| `sheetdim` | `sheetdim <sheet> <x1,y1> <x2,y2> [offset mm]` |
| `sheettable` | `sheettable <sheet> [layer]` — schedule table on sheet |
| `print` | `print <sheet> <path.pdf>` |

---

## Measure & schedule (`Analyze`)

| Command | Usage |
|---|---|
| `distance` | `distance <a> <b>` |
| `area` | `area <selector>` |
| `volume` | `volume <selector>` |
| `bbox` | `bbox <selector>` |
| `schedule` | `schedule [layer]` — name/type/area/volume table |
| `report` | `report [analysis\|codecheck]` — structured summary of stored environmental analyses and compliance runs |

---

## Environmental analysis (`Analyze`)

Solar and radiation studies. `curvature` (a NURBS analysis) also lives on the `analysis` layer — see Curves.

| Command | Usage |
|---|---|
| `sun` | `sun <lat> <lon> <YYYY-MM-DD> <HH:MM>` — solar lighting direction (NOAA SPA) |
| `sunoff` | revert to headlight shading |
| `location` | `location <lat> <lon> [tz-hours]` — observer location for the studies below |
| `shadowstudy` | `shadowstudy <date> <from> <to> <step-min>` — ground shadows per time step |
| `sunhours` | `sunhours <date> [grid-spacing]` — ground sun-hours heatmap (occlusion ray-cast) |
| `facesunhours` | `facesunhours <selector> <date>` — per-face insolation on the selected mesh |
| `radiation` | `radiation <selector> <path.epw>` — annual kWh/m²·yr per face from EPW DNI/DHI |
| `sunpath` | `sunpath [radius] [year]` — yearly sun-path dome diagram |

Studies need a `location` (or an imported EPW). Every run stores a structured report — read it back with `report` (e.g. `report facesunhours`) and use the numbers to critique the design.

Examples: `location 40.71 -74.01 -5` · `shadowstudy 2024-06-21 09:00 15:00 120` · `facesunhours last 2024-06-21` · `radiation last site.epw`

---

## Code compliance (`Analyze`)

Geometric compliance **pre-checks**. These are advisory only — they never certify a design or replace a code review; verify with a licensed professional / AHJ.

| Command | Usage |
|---|---|
| `codecheck` | `codecheck <pack> [story]` — evaluate a compliance pack against the model (embedded: `demo`, `ibc2021`, `ada2010`) |
| `checkrules` | `checkrules list \| load <path.json>` — list loaded packs or load one from disk |
| `report codecheck` | per-rule verdicts (pass/fail/warn/info), measured vs required, violating object ids |

Rules are declarative JSON (`*.checks.json`), so editions swap in without code and packs are LLM-authorable. Failures get coloured markers on the `compliance` layer. Room-level IBC egress checks need tagged rooms (see Structure ▸ `room`).

Examples: `codecheck ibc2021 L1` · `codecheck ada2010` then `report codecheck` · `checkrules load /tmp/ada.checks.json`

---

## Structural members (`Structure`)

Frame/area members, materials, sections, loads, and supports. All are geometry + interoperability metadata — the app records loads/supports but **never runs an analysis**.

| Command | Usage |
|---|---|
| `material` | `material <name> <E Pa> <density kg/m³>` — structural material (stored, never analysed) |
| `section` | `section <name> rect <w> <h> \| circle <d> \| iwf <d> <bf> <tf> <tw> \| pipe <d> <t>` |
| `grid` | `grid <name> x A:0 B:5 … y 1:0 2:4 … [levels …]` |
| `story` | `story <name> <elevation>` |
| `room` | `room <closed-curve> <occupancy> [name]` — tag a region with an IBC use group + area (feeds `codecheck ibc2021`) |
| `rooms` | list the tagged occupancy regions |
| `beam` | `beam <a> <b> <section> [material <m>] [rot <deg>]` |
| `column` | `column <a> <b> <section> [material <m>] [rot <deg>]` |
| `slab` | `slab <p1> <p2> … thick <t> [material <m>]` |
| `wall` | `wall <p1> <p2> … thick <t> [material <m>]` |
| `load` | `load <point\|line\|area> [name] <target> <magnitude> <direction>` |
| `support` | `support <x,y,z> <pinned\|fixed\|roller> [axis]` |

Occupancies for `room`: assembly, business, residential, mercantile, educational, storage, institutional. Example: `room sel business` · `room #suite assembly Lobby`

---

## Tools (`Tools`)

| Command | Usage |
|---|---|
| `underlay` | `underlay <path.png> [corner x,y] [width]` |
| `underlayopacity` | `underlayopacity <0..1>` |
| `underlayoff` | remove underlay |
| `osmfile` | `osmfile <path.json>` — OpenStreetMap building context |

---

## Landscape & site (`Tools` / `Analyze`)

Terrain, grading, planting, hardscape, and drainage. Terrain-derived studies (`cutfill`, `flowarrows`, `ponding`, `plantschedule`) store reports readable with `report`. Drainage tools are **advisory visualisations, not hydrology engineering**.

| Command | Usage |
|---|---|
| `terrain` | `terrain <path.{csv\|geojson}>` — build a terrain surface mesh on layer `terrain` |
| `contours` | `contours <interval> [major-every]` — extract contour polylines from the terrain mesh |
| `pad` | `pad <x,y> <width> <depth> <elev> [slope]` — grade a flat building pad with side slopes |
| `cutfill` | `cutfill` — report cut/fill volumes vs the pre-grading snapshot from the first `pad` |
| `plant` | `plant <species> <x,y[,z]> [age-years]` — place one plant from the 33-species catalog (temperate + Caribbean / Valle del Cauca / Guayaquil tropical packs) |
| `plantrow` | `plantrow <species> <a x,y> <b x,y> <spacing>` — a row of plants |
| `miyawaki` | `miyawaki <closed-region> [density]` — dense native Miyawaki mini-forest (climate-band aware, seeded) |
| `plantcatalog` | `plantcatalog [region\|zone]` — list catalog species, optionally filtered by region tag or Köppen zone |
| `plantschedule` | `plantschedule <path.csv>` — export a planting schedule CSV |
| `sitepath` | `sitepath <curve-selector> <width>` — hardscape ribbon draped on terrain, with a 1:12 accessible-slope warning |
| `flowarrows` | `flowarrows [n]` — steepest-descent drainage arrows on the terrain |
| `ponding` | `ponding` — mark local-minima ponding spots on the terrain |

Examples: `terrain /tmp/survey.csv` · `contours 0.5 5` · `pad 10,10 20 15 3.5 2` then `cutfill` · `plant royal-palm 10,5` · `plantrow tilia 0,0 30,0 6` · `miyawaki last 3` · `sitepath last 1.5`

---

## Ray tracer (accurate PBR)

A built-in Rust path tracer that renders the actual model — real materials, sun shadows, and global illumination — with no external backend. Complements the AI diffusion render below (that reimagines the view; the ray tracer renders it faithfully).

| Command | Usage |
|---|---|
| `raytrace` | `raytrace [out.png] [samples] [size]` — path-trace the current view to a PNG. Positional and order-tolerant: a filename sets the output, small numbers are samples-per-pixel, large numbers are the image width (e.g. `raytrace shot.png 128 1200`). Runs headless (pure CPU, no GPU device needed). |

In the GUI, **Render ▸ Raytrace…** opens a progressive window: a live preview that refines as samples accumulate, with controls for samples, bounces, resolution, sun, and sky, plus **Stop** and **Save PNG**.

Examples: `raytrace` · `raytrace dusk.png 256` · `raytrace hero.png 128 1600`

---

## Diffusion render (AI)

| Command | Usage |
|---|---|
| `controlimages` | `controlimages <path-prefix>` — write depth/edge/mask control PNGs from the current view |
| `render` | `render <prompt…>` \| `render cancel` — AI-render the current view via the configured diffusion backend |

`render` reimagines the current view from a text prompt, guided by control images (depth/edge/mask) derived from the model. It ships with **no backend active** — configure one of:

- **A cloud/remote backend** (ComfyUI / A1111-Forge / Draw Things / Replicate) in `~/.config/itsjustcad/render_decks.json` via `render backends` / `render use <name>` / `render test`, or
- **A fully local, offline backend** via the **Render Setup** panel: download a local Stable Diffusion model and point it at the user-installed `sd` (stable-diffusion.cpp) binary. Once detected, `render <prompt>` runs locally with nothing leaving your machine.

> Local render needs the user-installed `sd` binary plus a downloaded model — not fully one-click yet; the Render Setup panel walks you through both.

It captures the control images, runs an async job, and opens the result in an "AI Render" window. See [interop.md](interop.md) for control-image detail.

---

## File (`File`)

| Command | Usage |
|---|---|
| `import` | `import <path.{dxf\|obj\|stl\|gltf\|glb\|dae\|3dm\|step\|stp\|ifc\|epw\|geojson\|las\|laz\|e57}>` |
| `export` | `export <path.{dxf\|stl\|obj\|gltf\|glb\|svg\|csv\|ifc\|saf\|xlsx\|3dm\|step\|stp}>` |
| `print` | `print <sheet> <path.pdf>` |

STEP/STP import + export need the opt-in `kernel-occt` feature. See [interop.md](interop.md) for per-format detail.

---

## Views

```
view save <name>    # save current camera
view <name>         # restore it
view list           # list saved views
```

App-level verbs (command bar, `--run` scripts, and the deck — they change the viewport/UI, never the model or op-log):

```
top / bottom / front / back / left / right / persp   standard views
ze / zoomextents · zs / zoomselected                 fit all / fit selection
display shaded|wireframe|xray|ghosted|pencil         viewport display mode
light working|sun|presentation                       lighting model
sketchup                                             SketchUp look preset
sketchy on|off · edgefx k=v …                        hand-drawn NPR edges + tuning
profiles on|off · meshedges on|off                   thick profile / feature edges
plantsymbols on|off                                  2D top-view planting plan symbols
gumball on|off|toggle                                transform gizmo
camera 2point|persp|pano|fisheye [fov]|<n>mm         projection / lens
camera phone <lens>                                  phone-camera sim (iphone-ultrawide, pixel-tele, galaxy-main, …)
basemap [osm|sat] [span_m] [opacity] | basemap off   georeferenced site underlay
reducemotion on|off                                  accessibility: stop animated progress
chatencryption on|off                                encrypt chat sessions at rest (default OFF, opt-in)
save [path] · help [verb]                            save document / inline help
```

### Object snap

Rhino-style precision snapping while drawing. A clickable **osnap chip** in the status bar opens a toggle popup; the same set is under **View ▸ Object Snap…**. The `osnap` verb toggles the master switch or any individual snap.

```
osnap on|off|toggle                                  master snap switch
osnap <kind> on|off                                  toggle one snap kind
   kinds: end · mid · cen (center) · int (intersection) · perp
          tan (tangent) · qua (quadrant) · near (nearest) · nod (node)
          vtx (vertex) · grid
```

Example: `osnap on` · `osnap end on` · `osnap perp off` · `osnap grid on`

UI/layout verbs (also deck-drivable): `panel show|hide`, `panel chat|sessions|layers|blocks|plugins|parameters|sheets`, `dock left|right`, `split 1|2|4`, `workspace <name>`, `theme dark|light`.

---

See [COMMANDS.txt](COMMANDS.txt) for the full machine-generated listing.
