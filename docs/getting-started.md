# Getting Started

## Install

Download a single binary from the [releases page](https://github.com/htarrido-picart/itsjustcad/releases/latest). No installer, no dependencies.

| Platform | File | First launch |
|---|---|---|
| macOS | `ItsJustCAD.app` or `.dmg` | Right-click → **Open** (once) |
| Windows | `itsjustcad.exe` | More info → **Run anyway** (once) |
| Linux | `.AppImage` | `chmod +x`, then run |

Build from source (requires Rust stable):

```
git clone https://github.com/htarrido-picart/itsjustcad
cd itsjustcad
cargo build --release
```

The binary lands at `target/release/itsjustcad`.

---

## Launch

Double-click the app, or from a terminal:

```
itsjustcad
itsjustcad myproject.itsjustcad.json   # open an existing file
```

The window opens with an empty document and a command bar at the bottom.

---

## First model — a box

Type in the command bar at the bottom of the window and press Enter:

```
box 0,0,0 6,6,3
```

A 6 × 6 × 3 m box appears. The `box` command takes a corner and a size.

Name it so you can refer to it later:

```
name last building
```

Save the document:

```
save myproject.itsjustcad.json
```

---

## The command language at a glance

Every action is a typed command. There is no separate toolbar path — the LLM deck, the command bar, and any `--run` script all flow through the same substrate.

**Selectors** pick which objects a command acts on:

| Selector | Meaning |
|---|---|
| `last` | most recent object |
| `last 3` | three most recent |
| `all` | every object |
| `sel` | current click-selection |
| `<name>` | objects named with `name` |

**Examples**

```
circle 0,0,0 3          # 3 m radius circle at the origin
extrude last 4          # extrude it to 4 m tall
move last 10,0,0        # shift result 10 m east
copy building 0,8,0     # duplicate the named box 8 m north
union last 2            # boolean-union the two meshes
```

Press `?` or type `help` for a full command list. Type `help box` for one command.

---

## Units

Geometry is stored in metres. Display units can be changed at any time:

```
units mm
units ftin
units m
```

Number suffixes work anywhere regardless of display units: `500mm`, `2.5m`, `12ft6in`.

---

## Next steps

- [Tutorial: courtyard building](tutorial.md) — a complete walkthrough
- [Command reference](command-reference.md) — every command
- [Deck (LLM)](deck.md) — the built-in AI drafting partner
- [Interop](interop.md) — DXF, IFC, glTF, SAF exchange
