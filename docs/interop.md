# Interoperability

ItsJustCAD exchanges files through the `import` and `export` commands.

---

## Import

```
import <path>
```

Format is detected by file extension.

### DWG (assisted, via LibreDWG)

DWG import is **assisted**: `import site.dwg` auto-detects a user-installed `dwg2dxf` (LibreDWG) binary — probing the well-known install dirs (`/usr/local/bin`, `/opt/homebrew/bin`, `/usr/bin`, `~/.local/bin`) then `PATH`, the same way the app resolves the LLM CLIs — and converts the referenced file to a temporary DXF, which is then fed through the DXF importer above.

- **License-clean.** LibreDWG is GPLv3, so ItsJustCAD **detects and shells out to** a user-installed binary; it does **not** bundle, link, or depend on it. Same detect-don't-ship stance as the LLM CLIs, keeping the AGPLv3 app's distribution clean.
- **No shell, fixed arguments.** The converter is invoked with a fixed argument vector (`dwg2dxf -o <tmp>.dxf <input>`) against the one referenced file — no shell string, no interpolation, no model-controlled flags. The temp file is cleaned up on every path.
- **Truncation is caught.** LibreDWG can exit 0 while silently dropping the drawing (older versions cannot read AutoCAD-2013 Architectural-Desktop DWGs). ItsJustCAD does **not** trust the exit code: the converted DXF must contain both an `ENTITIES` section and an `EOF` marker, otherwise the import fails with *"DWG conversion incomplete (converter too old or unsupported DWG — try a newer LibreDWG/ODA)"* — never a silent empty import.
- **Missing converter** produces a clear *"install LibreDWG to import DWG (`brew install libredwg`)"* message.

Note: the assisted path is only as capable as the installed converter. For complex/ADT DWGs you need a recent LibreDWG or an ODA-based converter.

### DXF (R12 / R2000)

Entities imported: `LINE`, `LWPOLYLINE`, `POLYLINE`, `CIRCLE`, `ARC`, `TEXT`, `MTEXT`, `DIMENSION`, `HATCH`, `SPLINE`, `ELLIPSE`, `POINT`, `3DFACE`, `INSERT`, `LAYER` table entries, and `BLOCK` definitions from the `BLOCKS` section. Each entity becomes its own logged substrate op (`line`, `polyline`, `circle`, `arc`, `text`, `layer`, `block`, `insert`). The `import` command itself is not logged — the expanded ops are, so replay needs no access to the original file. Blocks **round-trip**: a `BLOCK` definition and its `INSERT` instances export and re-import with their name, insertion point, uniform scale, and rotation preserved.

**Block-body coverage** (widened for real architectural DWG/DXF blocks): a block whose body contains a `HATCH` keeps the hatch boundary as a closed polyline; a `SPLINE` is tessellated to a polyline (fit points, or control points when no fit points are present); a **nested** `INSERT` (a block placed inside another block) has the referenced block's geometry **baked in** at the nested insert's position/rotation/uniform-scale (block definitions are flat, so the nested reference is baked rather than kept live; cyclic references are detected and skipped). A block that mixes mappable and unmappable bodies keeps what maps instead of being discarded.

### OBJ / STL / glTF / GLB / Collada

Triangle meshes are stored verbatim as `MeshLiteral` ops in the op-log, making the file self-contained with no dependency on the source. Collada `.dae` is supported alongside the others.

### 3DM (Rhino / openNURBS)

A pure-Rust openNURBS reader: meshes become `MeshLiteral`, and lines / polylines / NURBS curves become curve ops, each on its original Rhino layer with its name preserved. BREPs and trimmed surfaces are skipped. No opt-in download needed.

### STEP / STP (opt-in)

AP242 / AP203 / AP214 exact-BREP solids are read via OCCT, then tessellated to a mesh with the exact volume reported. Requires the opt-in `kernel-occt` feature; default builds report a clear "needs the exact-BREP tier" error.

### IFC (IFC4 and IFC2x3)

Building elements are reconstructed as typed Frame / Area members (beams, columns, slabs, walls) with their stories and materials where present; the rest falls back to meshes on the `ifc` layer.

### EPW — EnergyPlus Weather

Sets the document location (latitude, longitude, time-zone) from the file header and reports annual climate statistics in the command output. Used as input by `shadowstudy` and `sunhours`.

### GeoJSON

`Polygon` features become closed polylines; `LineString` features become polylines; `Point` features become 0.5 m marker circles. The `properties.name` field sets the object name. Coordinates are projected to local metres when a document location has been set (via `location` or `sun`); otherwise lon/lat are treated as local XY.

### Point clouds — LAS / LAZ / E57

LAS 1.2–1.4 (formats 0–3) and its compressed form **LAZ** are both supported, as is **E57** (ASTM E2807: Cartesian points plus optional RGB / intensity, all sections merged). Every cloud is decimated to ≤ 200 000 points and stored as a `PointLiteral` op on layer `pointcloud`. LAZ is batch-decompressed so the full cloud is never materialised before decimation.

### Scoped workdir (deck-driven import)

```
workdir           # show the granted folder
workdir <path>    # grant a folder
files             # list importable files in it
import <name>     # import a file by bare name inside the workdir
```

A **workdir** is a single, user-granted folder the deck (LLM) may list and import files from — not the whole filesystem, and never a shell. Grant one with `workdir <path>` (persisted to `~/.config/itsjustcad/workdir.txt`); `files` lists the importable files inside it, and `import <name>` resolves a bare file name against it. Name resolution is **path-traversal guarded**: absolute paths, `..` components, embedded path separators, and symlink escapes out of the folder are all refused. This keeps the deck's file access bounded to one folder a human explicitly chose.

### Terrain

```
terrain <path.csv>
terrain <path.geojson>
```

`.csv`: Delaunay-triangulate x,y,z survey points (header row optional).  
`.geojson`: triangulate the vertices of elevation contour `LineString` features; elevation is read from the `elevation` or `ele` property; lon/lat are projected when a location is set.

The result is a mesh on layer `terrain`. The `terrain` command expands to a single `MeshLiteral` op in the log — the source file is not referenced on replay.

### OpenStreetMap

```
osmfile <path.json>
```

A saved Overpass API JSON export (`out geom;` query). Each `building`-tagged way footprint is extruded using the `height` tag, `building:levels × 3 m`, or a 9 m fallback. Results land on layer `context`. Lon/lat are projected to local metres when a location is set.

---

## Export

```
export <path>
```

Format is detected by file extension.

### DXF R12

2D entities (lines, polylines, circles, arcs, text, dimensions, hatches) are written at their XY coordinates. Meshes are exploded to their feature edges as polyline entities. Layer colours and lineweights (DXF code 370) are honoured. Block definitions are written to a `BLOCKS` section (`BLOCK`/`ENDBLK` per definition) and instances export as `INSERT` entities carrying insertion point, uniform scale (41/42/43) and rotation (50) — so blocks **round-trip** through export → import. Parametric (dynamic) blocks have no DXF equivalent, so each parametric instance is **baked** to a static block at its current parameter values (the geometry is preserved; the parametric link is not).

### STL (binary)

Triangle meshes only. Curves and 2D geometry are omitted.

### OBJ

Meshes plus curves written as polylines.

### glTF / GLB

Triangle meshes only. GLB is the single-file binary variant.

### SVG

2D entities at their XY coordinates, with layer colours mapped to stroke colour.

### CSV

A tabular schedule of all objects: name, id, layer, type, area, volume.

### IFC4

Typed `IfcBeam` / `IfcColumn` / `IfcSlab` / `IfcWall` with swept-solid bodies, plus the full `IfcStructuralAnalysisModel` graph: `IfcStructuralCurveMember` / `SurfaceMember`, `IfcStructuralPointConnection` with boundary conditions, and `IfcStructuralLoadGroup` with point / linear / planar actions. General meshes fall back to `IfcBuildingElementProxy`. Structural sections, materials, stories, and the reference grid are exported where present. The file header carries a "geometry+topology handoff — no analysis results" note.

### 3DM (Rhino / openNURBS)

```
export /tmp/model.3dm
```

A spec-conformant openNURBS V5 archive: meshes and curves with their names and layers. Rhino 5+ opens it.

### STEP / STP (opt-in)

```
export /tmp/model.step
```

AP242 via OCCT — **faceted**, one BREP face per triangle. Requires the opt-in `kernel-occt` feature.

### SAF — Structural Analysis Format (`.saf` / `.xlsx`)

```
export /tmp/model.xlsx
```

A genuine SAF 2.2.0 `.xlsx` workbook (hand-rolled OOXML — no new dependencies), ready to open in RFEM, SCIA Engineer, AxisVM, or FEM-Design. Both `.saf` and `.xlsx` extensions are accepted. Nine SAF sheets carry:

- `Nodes` — grid intersection points
- `1D Members` — beams and columns with section and material references
- `2D Members` — slabs and walls
- `Cross-sections` — named sections (rect, circle, IWF, pipe)
- `Materials` — E / density
- `Storeys` — story levels
- `Point Supports`, `Point Forces`, and `Line Force` load cases

**Geometry and topology only — never analysis results.** The disclaimer is embedded in the workbook's document properties; the app is an interop bridge, not an analysis tool.

### PDF

PDF export is not via `export` — it uses the `print` command with a named sheet:

```
sheet ground-floor a1
sheetview ground-floor top 1:100
print ground-floor /tmp/ground-floor.pdf
```

Vector output at the exact sheet scale.

---

## Diffusion control images

```
controlimages /tmp/scene
```

Writes three CAD-owned control inputs from the **current view** for hand-off to a diffusion / image-editing tool:

- `<prefix>_depth.png` — near-to-far depth gradient
- `<prefix>_edge.png` — feature-edge linework
- `<prefix>_mask.png` — a flat semantic colour per layer

The in-app `render <prompt…>` flow captures these automatically and drives the configured diffusion backend (ComfyUI / A1111-Forge / Draw Things / Replicate). Backends are user-configured in `~/.config/itsjustcad/render_decks.json`; the app ships with none active and reaches only the endpoint you configure.

---

## Round-trip workflow examples

**DXF → model → DXF**

```
import /tmp/survey.dxf
extrude last 3
export /tmp/model.dxf
```

**IFC coordination**

```
import /tmp/architect.ifc
section all 0,0,1.2 0,0,1
export /tmp/coordination.ifc
```

**Structural handoff**

```
section col rect 0.4 0.4
material steel 200e9 7850
column 0,0,0 0,0,3.5 col material steel
export /tmp/structure.saf
```

**Point cloud + terrain**

```
import /tmp/site.las
terrain /tmp/contours.csv
sun 40.71 -74.01 2024-06-21 14:00
shadowstudy 2024-06-21 09:00 15:00 60
```
