# File Format

ItsJustCAD files use the extension `.itsjustcad.json`. The complete specification is in [FORMAT.md](../FORMAT.md) at the repository root. This page is a quick orientation.

---

## What the format is

A `.itsjustcad.json` file is a **forward op-log**: an ordered JSON array of every command that built the model, from first to last. Opening the file replays each command through the same `apply` path used live. The scene is entirely derived — nothing extra is stored.

```json
{
  "itsjustcad": 1,
  "ops": [
    {"cmd": "box",  "id": "…", "corner": [0,0,0], "size": [6,6,3]},
    {"cmd": "plan", "ids": ["…"], "height": 1.2}
  ]
}
```

This design means:
- **Undo is free**: replay up to one step earlier.
- **Amend is free**: rewrite one op and re-replay.
- **The file IS the history**: no binary blobs, no hidden state.

---

## Version compatibility

The version field is `"itsjustcad": 1`. Legacy files written before the rename carry `"mydrafter": 1`; both spellings load identically.

**v1-replays-forever promise**: any file satisfying version 1 must open without error in all future builds. New fields on existing commands use `#[serde(default)]`; no existing field is removed or renamed.

---

## Design options (branches)

A file may hold named branches of the op-log:

```json
{
  "itsjustcad": 1,
  "ops": [ … ],
  "branches": {"option-a": [ … ], "option-b": [ … ]},
  "branch": "option-a"
}
```

See the `option` command for switching between branches.

---

## What is and is not logged

**Logged** (mutates model state): every geometry and annotation command — including the newer domains: landscape ops (`terrain`, `contours`, `pad`, `plant`, `plantrow`, `miyawaki`, `sitepath`), structural members (`beam`, `column`, `slab`, `wall`, `grid`, `story`, `load`, `support`), `room` tags, `codecheck` runs, and `constrain`.

**Not logged** (I/O or queries): `select`, `print`, `export`, `import`, `distance`, `area`, `volume`, `bbox`, `schedule`, `report`, `plantcatalog`, `rooms`, `undo`, `redo`, `amend`, `option`. View / camera / UI verbs (display, lighting, camera, panels, basemap, `plantsymbols`) and privacy/accessibility toggles (`chatencryption`, `reducemotion`) are session/UI state and are never logged either.

`import` is the notable exception: DXF import expands each entity into its equivalent substrate op (`line`, `polyline`, etc.) which *are* logged. The `import` command itself is not logged, so replay never re-reads the source file. The same "expand once, replay disk-free" pattern applies to `terrain`, `codecheck`, `cutfill`, and `radiation` — the resolved data (mesh, rule set + marker ids, pre-grading heights, EPW irradiance bins) is embedded in the logged op so replay never needs the original file.

---

## Derived document fields

Beyond the `ops` array, a file carries a few small pieces of derived-but-persisted state, all `#[serde(default)]` for back-compat (older files simply omit them):

- **`rooms`** — tagged occupancy regions (name, IBC use group, area, boundary) for IBC egress checks.
- **`param_blocks` / block definitions** — dynamic (parametric) blocks and captured block definitions; instances re-derive geometry from the template.
- **`compliance_reports`** — the per-rule verdicts from `codecheck`, served by `report codecheck`. Each carries the advisory-pre-check disclaimer in its context.
- **analysis reports** — the compact structured summaries stored by environmental and landscape studies (`sunhours`, `facesunhours`, `radiation`, `shadowstudy`, `cutfill`, `plantschedule`, `flowarrows`, `ponding`, `miyawaki`), served by `report`. Regenerated on replay.

Compliance and analysis reports are summaries for critique, never certified results — the app never claims to analyse or certify.

---

## Checkpoint sidecar (optional)

Alongside `myfile.itsjustcad.json` the app may write `myfile.itsjustcad.json.checkpoint` — a compact JSON snapshot of the derived document plus the op-count it reflects. This is a pure cache: deleting it is always safe, and a stale or unreadable checkpoint is silently ignored.

---

See [FORMAT.md](../FORMAT.md) for the full per-command JSON examples and the id/selector encoding.
