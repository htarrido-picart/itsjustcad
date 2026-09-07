# Example documents

Ready-to-open sample models plus the scripts that build them. Every file here is
a plain `.itsjustcad.json` op-log — the ordered list of commands that built the
model — so you can open it, inspect it, undo through it, or diff it in git.

## Open a sample

Launch ItsJustCAD and use **File ▸ Open**, or from a terminal:

```sh
ItsJustCAD examples/massing.itsjustcad.json
```

| File | What it is |
|---|---|
| `massing.itsjustcad.json` | A podium-and-tower massing — a few boxes. Orbit it (right-mouse drag). |
| `courtyard.itsjustcad.json` | A courtyard building with a plan cut, a dimensioned A3 sheet, ready to `print`. |
| `site-plan.itsjustcad.json` | A full intemfit site: streets, blocks, lots, setbacks, row buildings, and a `lotreport` yield. |

## Rebuild from a script

The `scripts/` folder holds the command scripts that generate each sample. Run
one with `--run` to reproduce the document (and, optionally, save it):

```sh
ItsJustCAD --run examples/scripts/massing.script --out /tmp/massing.itsjustcad.json
```

Add `--headless --shot out.png` to render an offscreen PNG with no window:

```sh
ItsJustCAD --run examples/scripts/site-plan.script --headless --shot /tmp/site.png
```

Scripts are one command per line; `#` starts a comment. See the
[command reference](../docs/command-reference.md) for every verb.

## Print the courtyard sheet to PDF

The courtyard sample already defines an A3 sheet named `plan-01`. To export a
vector PDF, open it and type:

```
print plan-01 courtyard.pdf
```

> **Advisory note.** The site-plan sample uses the `euro_latam` metric defaults
> for setbacks, lot widths, and floor heights. These are placeholder planning
> aids, not code. Confirm allowable use, density, and setbacks with the local
> zoning authority before relying on any yield number.
