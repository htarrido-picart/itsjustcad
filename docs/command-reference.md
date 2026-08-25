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
| `loft` | `loft <selector>` — skin 2+ stacked closed curves |
| `sweep` | `sweep <profile> <rail>` |
| `sweep2` | `sweep2 <profile> <rail-a> <rail-b>` |
| `railrevolve` | `railrevolve <profile> <rail> <axis pt> <axis dir>` |
| `pipe` | `pipe <curve> <radius> [end radius]` — round pipe |

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
| `hatch` | `hatch <selector> [solid \| lines \| crosshatch \| brick \| concrete \| insulation \| earth]` |
| `dim` | `dim <a x,y,z> <b x,y,z> [offset]` |
| `layer` | `layer <name>` — create/switch current layer |
| `tolayer` | `tolayer <selector> <name>` |
| `layercolor` | `layercolor <layer> <r,g,b>` |
| `layerweight` | `layerweight <layer> <mm>` — print lineweight |
| `hide` / `show` | toggle layer visibility |
| `hideobj` / `showobj` | toggle per-object visibility |
| `color` | `color <selector> <r,g,b \| off>` — per-object colour |
| `units` | `units <m \| cm \| mm \| ft \| in \| ftin>` |
| `block` | `block <selector> <name>` — define a reusable block |
| `insert` | `insert <name> <pos> [rotation deg] [scale]` |
| `blocks` | list block definitions |
| `blocklib` | list on-disk block library |
| `blockload` | load a library block into the document |
| `blocksave` | save a block definition to the library |

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

## Analysis (`Analyze`)

| Command | Usage |
|---|---|
| `distance` | `distance <a> <b>` |
| `area` | `area <selector>` |
| `volume` | `volume <selector>` |
| `bbox` | `bbox <selector>` |
| `schedule` | `schedule [layer]` — name/type/area/volume table |
| `sun` | `sun <lat> <lon> <YYYY-MM-DD> <HH:MM>` |
| `sunoff` | revert to headlight |
| `location` | `location <lat> <lon> [tz-hours]` |
| `shadowstudy` | `shadowstudy <date> <from> <to> <step-min>` |
| `sunhours` | `sunhours <date> [grid-spacing]` |

---

## Structure (`Structure`)

| Command | Usage |
|---|---|
| `material` | `material <name> <E Pa> <density kg/m³>` |
| `section` | `section <name> rect <w> <h> \| circle <d> \| iwf <d> <bf> <tf> <tw> \| pipe <d> <t>` |
| `grid` | `grid <name> x A:0 B:5 … y 1:0 2:4 … [levels …]` |
| `story` | `story <name> <elevation>` |
| `beam` | `beam <a> <b> <section> [material <m>] [rot <deg>]` |
| `column` | `column <a> <b> <section> [material <m>] [rot <deg>]` |
| `slab` | `slab <p1> <p2> … thick <t> [material <m>]` |
| `wall` | `wall <p1> <p2> … thick <t> [material <m>]` |
| `load` | `load <point\|line\|area> [name] <target> <magnitude> <direction>` |
| `support` | `support <x,y,z> <pinned\|fixed\|roller> [axis]` |

---

## Tools (`Tools`)

| Command | Usage |
|---|---|
| `underlay` | `underlay <path.png> [corner x,y] [width]` |
| `underlayopacity` | `underlayopacity <0..1>` |
| `underlayoff` | remove underlay |
| `terrain` | `terrain <path.{csv\|geojson}>` |
| `osmfile` | `osmfile <path.json>` — OpenStreetMap context |

---

## File (`File`)

| Command | Usage |
|---|---|
| `import` | `import <path.{dxf\|obj\|stl\|gltf\|glb\|ifc\|epw\|geojson\|las}>` |
| `export` | `export <path.{dxf\|stl\|obj\|gltf\|glb\|svg\|csv\|ifc\|saf}>` |
| `print` | `print <sheet> <path.pdf>` |

---

## Views

```
view save <name>    # save current camera
view <name>         # restore it
view list           # list saved views
```

App-level (command bar and --run scripts only):

```
top / front / right / persp     # standard views
ze / zoomextents                # fit all
camera <mm lens>                # set focal length
display <mode>                  # wireframe / solid / pencil / shadow
save [path]                     # save document
help [verb]                     # inline help
```

---

See [COMMANDS.txt](COMMANDS.txt) for the full machine-generated listing.
