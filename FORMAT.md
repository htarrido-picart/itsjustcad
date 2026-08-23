# mydrafter file format — op-log v1

`*.mydrafter.json` is a newline-pretty JSON op-log. Opening a file replays
every logged op through the same `apply` path used live. The scene is fully
derived; nothing else is stored.

## Top-level shape

```json
{
  "mydrafter": 1,
  "ops": [ … ]
}
```

| field | type | notes |
|---|---|---|
| `mydrafter` | `u32` | format version, currently **1** |
| `ops` | array of Command | ordered forward log; replayed in array order |

A reader **must** reject files where `mydrafter` > the version it knows. A
reader **must** accept files where `mydrafter` == 1 forever — that is the
v1-replays-forever promise (see below).

## Id semantics

Object ids (`id`, `ids`) are UUID v4 strings
(`"00000000-0000-4000-8000-000000000001"` style). They are `null` / absent
when a command is first typed or emitted by the LLM. `apply` fills them and
**writes them back** into the logged op. Replay therefore reproduces identical
ids across save-load-save cycles; the file is byte-stable on a second save.

Commands that produce multiple objects (e.g. `copy`, `section`) use an `ids`
array; commands that produce one object use `id`. Commands that do not create
objects carry neither field.

## Selector encoding

Selectors appear wherever a command targets existing objects:

```json
{ "sel": "ids",      "ids": ["<uuid>", …] }   // absolute, set after apply
{ "sel": "last",     "n": 1 }                  // most-recently created N
{ "sel": "named",    "name": "tower-a" }
{ "sel": "all" }
{ "sel": "selected" }
```

The LLM and the command line both emit `last / all / named`; ids are only
present in the log after apply writes them back.

## Logged vs. non-logged commands

Only commands that mutate model state appear in the log. These commands are
**never** logged (they are I/O or queries):

`select`, `select_none`, `view_restore`, `view_list`, `print`, `export`,
`import`, `distance`, `area`, `volume`, `bbox`, `undo`, `redo`, `amend`

`import` is special: it expands DXF entities into their equivalent substrate
ops (line, polyline, circle, arc, text, layer), which **are** logged. The
import command itself is not.

## Per-command JSON examples

### Primitives

**box**
```json
{"cmd": "box", "id": "<uuid>", "corner": [0.0, 0.0, 0.0], "size": [5.0, 4.0, 3.0]}
```

**line**
```json
{"cmd": "line", "id": "<uuid>", "a": [0.0, 0.0, 0.0], "b": [10.0, 0.0, 0.0]}
```

**polyline** — `closed: false` may be omitted (default false)
```json
{"cmd": "polyline", "id": "<uuid>",
 "points": [[0,0,0],[4,0,0],[4,3,0]], "closed": false}
```

**rectangle**
```json
{"cmd": "rectangle", "id": "<uuid>", "corner": [0,0,0], "width": 6.0, "height": 4.0}
```

**circle**
```json
{"cmd": "circle", "id": "<uuid>", "center": [12.0, 2.0, 0.0], "radius": 2.5}
```

**arc** — angles in degrees, counter-clockwise
```json
{"cmd": "arc", "id": "<uuid>", "center": [0,0,0], "radius": 3.0,
 "start_deg": 0.0, "end_deg": 180.0}
```

**ellipse**
```json
{"cmd": "ellipse", "id": "<uuid>", "center": [0,0,0], "rx": 4.0, "ry": 2.0}
```

**polygon** — `radius` is circumradius
```json
{"cmd": "polygon", "id": "<uuid>", "center": [0,0,0], "radius": 3.0, "sides": 6}
```

**curve** — NURBS by control points
```json
{"cmd": "curve", "id": "<uuid>", "points": [[0,0,0],[4,6,0],[8,0,0]], "degree": 3}
```

### Solids from profiles

**extrude**
```json
{"cmd": "extrude", "id": "<uuid>",
 "profile": {"sel": "ids", "ids": ["<uuid>"]}, "height": 3.0}
```

**revolve** — optional fields may be omitted for defaults (z-axis, full circle)
```json
{"cmd": "revolve", "id": "<uuid>",
 "profile": {"sel": "last", "n": 1},
 "axis_point": [0,0,0], "axis_dir": [0,0,1], "angle_deg": 270.0}
```

**loft**
```json
{"cmd": "loft", "id": "<uuid>", "targets": {"sel": "last", "n": 3}}
```

**sweep**
```json
{"cmd": "sweep", "id": "<uuid>",
 "profile": {"sel": "named", "name": "section"},
 "rail": {"sel": "named", "name": "path"}}
```

### Booleans

```json
{"cmd": "union",      "id": "<uuid>", "targets": {"sel": "last", "n": 2}}
{"cmd": "difference", "id": "<uuid>",
 "target": {"sel": "ids", "ids": ["<uuid-body>"]},
 "tools":  {"sel": "ids", "ids": ["<uuid-cutter>"]}}
{"cmd": "intersect",  "id": "<uuid>", "targets": {"sel": "last", "n": 2}}
```

### Edit

**move**
```json
{"cmd": "move", "targets": {"sel": "all"}, "delta": [1.0, 0.0, 0.0]}
```

**rotate** — `axis` is a unit vector; `center` defaults to targets' AABB center
```json
{"cmd": "rotate", "targets": {"sel": "last", "n": 1},
 "angle_deg": 45.0, "axis": [0.0, 0.0, 1.0]}
```

**scale**
```json
{"cmd": "scale", "targets": {"sel": "last", "n": 1},
 "factors": [2.0, 1.0, 1.0]}
```

**mirror** — canonical planes: `"xy"`, `"yz"`, `"xz"`
```json
{"cmd": "mirror", "targets": {"sel": "last", "n": 1},
 "plane": {"plane": "yz"}}
```

**copy**
```json
{"cmd": "copy", "ids": ["<uuid>"],
 "targets": {"sel": "last", "n": 1}, "delta": [6.0, 0.0, 0.0]}
```

**array** — `counts` is [x, y, z]; originals occupy cell (0,0,0)
```json
{"cmd": "array", "ids": ["<uuid>", "<uuid>"],
 "targets": {"sel": "last", "n": 1},
 "counts": [3, 2, 1], "delta": [6.0, 0.0, 0.0]}
```

**polar_array** — `total_angle_deg` defaults to 360
```json
{"cmd": "polar_array", "ids": ["<uuid>", "<uuid>"],
 "targets": {"sel": "last", "n": 1},
 "count": 4, "center": [0.0, 0.0, 0.0]}
```

**offset**
```json
{"cmd": "offset", "id": "<uuid>",
 "target": {"sel": "last", "n": 1}, "distance": 0.5}
```

**fillet**
```json
{"cmd": "fillet", "id": "<uuid>",
 "a": {"sel": "ids", "ids": ["<uuid-a>"]},
 "b": {"sel": "ids", "ids": ["<uuid-b>"]},
 "radius": 1.0}
```

**join**
```json
{"cmd": "join", "id": "<uuid>", "targets": {"sel": "last", "n": 3}}
```

**split**
```json
{"cmd": "split", "ids": ["<uuid-a>", "<uuid-b>"],
 "target": {"sel": "last", "n": 1}, "point": [3.0, 0.0, 0.0]}
```

**trim**
```json
{"cmd": "trim", "id": "<uuid>",
 "target": {"sel": "last", "n": 1},
 "cutter": {"sel": "named", "name": "grid"},
 "keep": [1.0, 0.0, 0.0]}
```

**extend**
```json
{"cmd": "extend", "targets": {"sel": "last", "n": 1}, "distance": 2.0}
```

**delete**
```json
{"cmd": "delete", "targets": {"sel": "ids", "ids": ["<uuid>"]}}
```

**name**
```json
{"cmd": "name", "targets": {"sel": "last", "n": 1}, "name": "north-wall"}
```

### Groups

```json
{"cmd": "group",   "targets": {"sel": "last", "n": 3}, "name": "tower-a"}
{"cmd": "ungroup", "targets": {"sel": "named", "name": "tower-a"}}
```

### Layers

```json
{"cmd": "layer",       "name": "walls"}
{"cmd": "to_layer",    "targets": {"sel": "last", "n": 1}, "layer": "walls"}
{"cmd": "layer_color", "layer": "walls", "color": [0.9, 0.1, 0.1]}
{"cmd": "hide",        "layer": "walls"}
{"cmd": "show",        "layer": "walls"}
```

### Per-object visibility

```json
{"cmd": "hide_obj", "targets": {"sel": "last", "n": 1}}
{"cmd": "show_obj", "targets": {"sel": "last", "n": 1}}
```

### Sections

**section** — writes closed polylines to layer "sections"
```json
{"cmd": "section", "ids": ["<uuid>", "<uuid>"],
 "targets": {"sel": "all"},
 "point": [0.0, 0.0, 1.5], "normal": [0.0, 0.0, 1.0]}
```

**plan** — horizontal section at z = height
```json
{"cmd": "plan", "ids": ["<uuid>"], "height": 1.5}
```

### Drafting

**dim** — measured value derived at display time, not stored
```json
{"cmd": "dim", "id": "<uuid>",
 "a": [0,0,0], "b": [6,0,0], "offset": 1.0}
```

**text**
```json
{"cmd": "text", "id": "<uuid>",
 "pos": [3.0, -1.0, 0.0], "text": "6 000", "height": 0.4}
```

**hatch** — solid fill or line pattern
```json
{"cmd": "hatch", "id": "<uuid>",
 "target": {"sel": "last", "n": 1},
 "pattern": {"type": "solid"}}

{"cmd": "hatch", "id": "<uuid>",
 "target": {"sel": "last", "n": 1},
 "pattern": {"type": "lines", "angle_deg": 45.0, "spacing": 0.5}}
```

### Units

```json
{"cmd": "units", "units": "m"}
```

Valid values: `"m"`, `"cm"`, `"mm"`, `"ft_in"`.

### Sheets / layouts

```json
{"cmd": "sheet",      "name": "ground-floor", "paper": "a1_landscape"}
{"cmd": "sheet_view", "sheet": "ground-floor", "direction": "top", "scale": 100.0}
```

### Named views

```json
{"cmd": "view_save", "name": "entry",
 "camera": {"target": [4.5, -2.0, 1.25], "distance": 27.5,
            "yaw": -0.75, "pitch": 1.2,
            "fov_y": 0.7854, "ortho": false}}
```

`camera` is `null` / absent when typed; the app fills it before apply.

## The v1-replays-forever promise

Any file that satisfies `"mydrafter": 1` and validates against the schema above
**must** open without error in all future builds of this project. New fields
added to existing commands use `#[serde(default)]` so old files that omit them
still load. No existing field is ever removed or renamed. The version number is
bumped only for genuinely breaking changes, with a migration path described in
a `FORMAT_MIGRATION.md` alongside the bump.

Concretely: the io.rs test suite contains `pre_*` tests that load
hand-written minimal v1 JSON and assert that newly-added features default
gracefully. Every new command or field must add such a test.
