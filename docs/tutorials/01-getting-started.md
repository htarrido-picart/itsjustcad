# Tutorial 1 — Getting started

*You will install the app, make your first document, draw and extrude a massing,
orbit it, save it, and print a PDF sheet. About 15 minutes.*

---

## 1. Install and launch

Download the single binary from the
[releases page](https://github.com/htarrido-picart/itsjustcad/releases/latest)
— no installer, no dependencies.

| Platform | First launch |
|---|---|
| **macOS** | Unpack `ItsJustCAD.app`, then right-click it ▸ **Open** (once). |
| **Windows** | Unzip, double-click, **More info ▸ Run anyway** (once). |
| **Linux** | `tar xzf …`, `chmod +x itsjustcad`, then run it. |

The app is not code-signed yet, so your OS shows a one-time "unidentified
developer" warning on the very first launch. After that it just opens.

On the first run it asks two questions — your **units** (meters / millimeters /
feet-inches) and which **CAD you came from** (AutoCAD / Rhino / Revit / none).
The interface adapts its colors, font sizes, and command aliases to match. You
can change either later; pick *meters* and *none* if you're unsure.

The window opens with an empty document and a **command bar** at the bottom.
Everything in this tutorial is typed there, one line at a time, Enter to run.

---

## 2. Draw a massing

Type each line and press Enter. A `box` takes a corner point and a size vector:

```
box 0,0,0 40,30,4
name last podium
box 6,6,4 20,14,24
name last tower
box 10,10,28 12,8,16
name last setback
```

You now have a podium, a tower, and a stepped-back crown. `name last` labels the
most recent object so you can refer to it by name later.

> **Draw by extrusion instead.** Boxes are quick, but most massing starts as a
> 2D footprint pushed up. Try it: `rect 0,0,0 20 14` draws a rectangle, then
> `extrude last 6` pushes it to a 6 m mass. `last` always means the most recent
> object.

---

## 3. Orbit, frame, and shade

Frame everything and switch to a perspective view:

```
persp
ze
display shaded
```

- `persp` is the perspective camera; `top`, `front`, `right` give true-ortho views.
- `ze` (zoom-extents) frames all geometry.
- `display shaded` is the solid view; `wireframe`, `pencil`, `ghosted` are others.

Now use the mouse in the viewport:

| Mouse | Does |
|---|---|
| **Right-drag** | Orbit |
| **Shift + right-drag** | Pan |
| **Scroll wheel** | Zoom |

---

## 4. Save

```
save mytower.itsjustcad.json
```

The saved file is not geometry — it is the **op-log**, the ordered list of the
commands you just typed. Opening it replays them. That is what makes undo,
history-editing, and clean git diffs work. (More in
[FORMAT.md](../../FORMAT.md).)

Re-open it any time with **File ▸ Open**, or from a terminal:

```sh
ItsJustCAD mytower.itsjustcad.json
```

---

## 5. Cut a plan and put it on a sheet

Massing is 3D; a drawing is 2D. Cut a horizontal plan slice at 1.5 m:

```
plan 1.5
```

Wall outlines land as closed polylines on a `sections` layer. Now make a paper
sheet and place a scaled top view on it:

```
sheet plan-01 a3
sheetview plan-01 top 1:100
```

`sheet <name> <size>` makes an A4–A0 paper sheet; `sheetview` drops a scaled
orthographic view onto it.

---

## 6. Print a PDF

```
print plan-01 mytower-plan.pdf
```

The result is a **vector** PDF at true 1:100 scale on an A3 page — not a
screenshot. Open it in any PDF viewer.

---

## Where to go next

- **[Tutorial 2 — Site planning](02-site-planning.md)** subdivides a site and
  reports its yield.
- **[Tutorial 3 — Environmental analysis](03-environmental-analysis.md)** runs a
  sun study you can read in numbers.
- **[Tutorial 4 — the deck](04-deck.md)** hands the same commands to an LLM.
- The **[command reference](../command-reference.md)** has the exact usage for
  every verb you met here (`box`, `rect`, `extrude`, `plan`, `sheet`,
  `sheetview`, `print`).
- Ready-made versions of this model live in
  [`examples/`](../../examples/README.md).
