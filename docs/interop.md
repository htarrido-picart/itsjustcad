# Interoperability

ItsJustCAD exchanges files through the `import` and `export` commands.

---

## Import

```
import <path>
```

Format is detected by file extension.

### DXF (R12 / R2000)

Entities imported: `LINE`, `LWPOLYLINE`, `POLYLINE`, `CIRCLE`, `ARC`, `TEXT`, and `LAYER` table entries. Each entity becomes its own logged substrate op (`line`, `polyline`, `circle`, `arc`, `text`, `layer`). The `import` command itself is not logged — the expanded ops are, so replay needs no access to the original file.

### OBJ / STL / glTF / GLB

Triangle meshes are stored verbatim as `MeshLiteral` ops in the op-log, making the file self-contained with no dependency on the source.

### IFC (IFC4 and IFC2x3)

Building elements are tessellated into meshes and placed on the `ifc` layer. Structural metadata (sections, materials, stories) is read where present.

### EPW — EnergyPlus Weather

Sets the document location (latitude, longitude, time-zone) from the file header and reports annual climate statistics in the command output. Used as input by `shadowstudy` and `sunhours`.

### GeoJSON

`Polygon` features become closed polylines; `LineString` features become polylines; `Point` features become 0.5 m marker circles. The `properties.name` field sets the object name. Coordinates are projected to local metres when a document location has been set (via `location` or `sun`); otherwise lon/lat are treated as local XY.

### LAS 1.2–1.4 point cloud

Formats 0–3 are supported. The cloud is decimated to ≤ 200 000 points and stored as a `PointLiteral` op on layer `pointcloud`. LAZ (compressed LAS) is not supported.

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

2D entities (lines, polylines, circles, arcs, text, dimensions, hatches) are written at their XY coordinates. Meshes are exploded to their feature edges as polyline entities. Layer colours and lineweights (DXF code 370) are honoured.

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

Building elements exported as `IfcBuildingElementProxy` (general meshes), `IfcMember` (beams and columns), `IfcSlab`, and `IfcWall`. Structural sections, materials, stories, and the reference grid are exported where present.

### SAF — Structural Analysis Format

```
export /tmp/model.saf
```

A ZIP of CSVs matching the SAF 2.2.0 sheet names, ready to open in RFEM, SCIA Engineer, or FEM-Design. Includes:

- `Nodes` — grid intersection points
- `1D Members` — beams and columns with section and material references
- `2D Members` — slabs and walls
- `Cross-sections` — named sections (rect, circle, IWF, pipe)
- `Materials` — E / density
- `Storeys` — story levels
- `Point Supports` and `Point Forces`, `Line Force` load cases

### PDF

PDF export is not via `export` — it uses the `print` command with a named sheet:

```
sheet ground-floor a1
sheetview ground-floor top 1:100
print ground-floor /tmp/ground-floor.pdf
```

Vector output at the exact sheet scale.

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
