# ItsJustCAD Documentation

| Page | Description |
|---|---|
| [Getting Started](getting-started.md) | Install, launch, first model |
| [Tutorial](tutorial.md) | Model a courtyard building, cut a plan, export a PDF |
| [Command Reference](command-reference.md) | Every command, organised by category |
| [File Format](file-format.md) | The op-log format (see also [FORMAT.md](../FORMAT.md)) |
| [Deck](deck.md) | The LLM drafting partner: cassettes, local models, plugins |
| [Plugins](plugins.md) | Extend ItsJustCAD with your own commands and menu entries (declarative, safe) |
| [Interop](interop.md) | DXF / IFC / SAF / glTF / LAS import and export |

---

## Building this documentation

mdBook is the intended renderer. If it is installed:

```
cargo install mdbook          # one-time
mdbook build docs/            # outputs docs/book/
mdbook serve docs/            # live-reload at http://localhost:3000
```

A `book.toml` and `src/SUMMARY.md` are not yet present — a future commit will add them when the CI pipeline requires a built HTML site. For now all pages are plain Markdown and render correctly on GitHub.

If mdBook is not installed, every page is readable directly on GitHub or with any Markdown viewer. All relative links in these pages resolve to real files in this `docs/` directory (or to `FORMAT.md` and `README.md` at the repo root).

---

## Screenshots

| | |
|---|---|
| ![viewport](shot-viewport.png) | ![deck](shot-deck.png) |
| ![plan](shot-plan-pencil.png) | ![sun/shadow](shot-sun-shadow.png) |
