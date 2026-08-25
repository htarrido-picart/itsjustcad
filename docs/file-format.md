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

**Logged** (mutates model state): every geometry and annotation command.

**Not logged** (I/O or queries): `select`, `print`, `export`, `import`, `distance`, `area`, `volume`, `bbox`, `undo`, `redo`, `amend`, `option`.

`import` is the notable exception: DXF import expands each entity into its equivalent substrate op (`line`, `polyline`, etc.) which *are* logged. The `import` command itself is not logged, so replay never re-reads the source file.

---

## Checkpoint sidecar (optional)

Alongside `myfile.itsjustcad.json` the app may write `myfile.itsjustcad.json.checkpoint` — a compact JSON snapshot of the derived document plus the op-count it reflects. This is a pure cache: deleting it is always safe, and a stale or unreadable checkpoint is silently ignored.

---

See [FORMAT.md](../FORMAT.md) for the full per-command JSON examples and the id/selector encoding.
