# mydrafter

A CAD program for architects, rethought for the LLM era. Rhino-style command
line, 3D viewport, and an LLM drafting companion **inside** the app — not an
external plugin poking at it.

The core bet: **one command substrate**. The human command line and the LLM
emit the same commands. The document *is* the op-log of those commands — undo,
the file format, and replay all derive from it. Every session doubles as a
training transcript.

Pure Rust. AGPLv3. Cross-platform (winit + wgpu + egui).

## Status

MVP: "box on screen, two ways."

- Perspective viewport: infinite grid, Z-up orbit camera (RMB orbit,
  Shift+RMB pan, scroll dolly), click-select, `ze` zoom extents
- Command line: `box`, `extrude`, `line`, `polyline`, `rect`, `circle`, `arc`,
  `ellipse`, `polygon`, `curve` (NURBS), `move`, `copy`, `delete`, `name`,
  `select`, `undo`/`redo` — units (`5`, `250cm`, `500mm`), selectors
  (`last`, `last 3`, `all`, `sel`, names)
- LLM deck pane: describe what to draw; commands stream out of the model and
  execute live; failed commands are fed back for correction automatically
- Save/load: op-log JSON (`save scene.mydrafter.json`, `open …`, Cmd+S/O)

## Run

```sh
cargo run -p mydrafter
```

Try in the command line (bottom):

```
rect 0,0,0 6 4
extrude last 3
circle 12,2 2.5
extrude last 8
ze
```

Or tell the deck (right pane): *"make three 4x4x3 towers in a row, 6m apart"*.

## LLM decks ("cassette player")

Any OpenAI-compatible endpoint or Anthropic. Configure
`~/.config/mydrafter/decks.json`:

```json
{
  "decks": [
    { "name": "ollama", "kind": "openai_compat",
      "base_url": "http://localhost:11434/v1", "model": "gpt-oss:20b" },
    { "name": "claude", "kind": "anthropic",
      "base_url": "https://api.anthropic.com",
      "model": "claude-sonnet-4-6", "api_key": "env:ANTHROPIC_API_KEY" },
    { "name": "kimi", "kind": "openai_compat",
      "base_url": "https://api.moonshot.ai/v1",
      "model": "kimi-k2-0905-preview", "api_key": "env:MOONSHOT_API_KEY" }
  ],
  "active": 0
}
```

`api_key` is a literal or `env:VAR`. Swap cassettes with the combo box in the
deck pane. The model receives the command registry and a scene digest — it
never touches raw geometry; the kernel does the math.

## Architecture

```
crates/
  kernel-mesh/   f64 face-vertex meshes, primitives, extrusion, ear clipping
  kernel-curve/  line/polyline/arc/ellipse + NURBS (own de Boor), tessellation
  kernel-brep/   stub — truck-based BREP planned
  doc/           scene state; knows nothing about how it is mutated
  commands/      THE substrate: Command enum, parser, registry, Session
                 (op-log + inverse-op undo + replay), file io
  deck/          LlmDeck trait, OpenAI-compat + Anthropic SSE adapters,
                 streaming ```draft extractor, system prompt builder
  render/        wgpu pipelines in egui paint callbacks, grid shader,
                 orbit camera, headless renderer
  app/           shell: viewport, command line, deck pane
```

Dev hooks (used by agents and CI):

```sh
# render a scene file without a window
cargo run -p mydrafter-render --example headless -- out.png scene.mydrafter.json
# stream one prompt through the active deck in the terminal
cargo run -p mydrafter-deck --example chat -- "make a 6m cube"
# scripted app run: commands, a deck prompt, a save, a screenshot
MYDRAFTER_RUN="rect 0,0,0 6 4;extrude last 3" \
MYDRAFTER_DECK_RUN="add a tower behind it" \
MYDRAFTER_SAVE=/tmp/scene.json MYDRAFTER_SHOT=/tmp/shot.png cargo run -p mydrafter
```

## License

AGPL-3.0-or-later. Dependency licenses are enforced with
`cargo deny check licenses`.
