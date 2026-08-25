# Plugins

Plugins let you extend ItsJustCAD with your own **commands** and **menu entries**
without forking or writing native code. A plugin is a *declarative macro*: a
named, parameterised list of command templates that compose existing commands.

This is deliberately narrow for safety. A plugin can **never** run arbitrary
native code or bypass the substrate — every line a plugin emits is parsed and
executed as an ordinary command and logged individually in the op-log, exactly
as if you had typed it. That means plugin output is **replay-safe** and
**undo/redo-safe**, and old drawings never break when a plugin is later edited or
removed (replay uses the recorded expanded commands, not the plugin).

## Where plugins live

Plugins are loaded at startup from:

```
~/.config/itsjustcad/plugins/
```

Each plugin is a self-contained folder with a `plugin.json` manifest:

```
~/.config/itsjustcad/plugins/
  greek-column/
    plugin.json
  column-grid/
    plugin.json
```

To install a plugin, drop its folder in that directory and either restart the
app or run `plugin reload` on the command line.

## Manifest format

```json
{
  "name": "greek-column",
  "description": "A simple Doric column: a square shaft of height h.",
  "category": "Classical",
  "params": [
    { "name": "h", "default": "4" }
  ],
  "body": [
    "box 0,0,0 0.3,0.3,{h}"
  ]
}
```

| Field | Required | Meaning |
|---|---|---|
| `name` | yes | The command verb. Must not contain `/`, `\`, or `.`. If it collides with a built-in command the built-in wins. |
| `description` | no | One-line summary shown in `help`, autosuggest, and the menu tooltip. |
| `category` | no | Menu group. The plugin appears under **Plugins ▸ &lt;category&gt;**. Omit it to land directly under the **Plugins** menu. |
| `params` | no | Ordered positional parameters. Each has a `name` and an optional `default`. |
| `body` | yes | The command templates run in order when the plugin is invoked. |

### Parameter substitution

Inside `body` lines you can reference parameters two ways:

* **By index:** `{0}`, `{1}`, … — the Nth argument given on the command line.
* **By name:** `{h}` — matches a declared param `name`.

When an argument is omitted, its declared `default` is used. A parameter with
neither an argument nor a default is an error (the plugin will not run). Unknown
`{tokens}` are left verbatim, so literal braces survive.

## Using a plugin

Once loaded, a plugin verb behaves like any other command:

```
greek-column 6      # square column, height 6
greek-column        # height 4 (the default)
```

Because `greek-column` declares a `category`, it also appears in the menu bar
under **Plugins ▸ Classical**. Parameterless plugins execute immediately from
the menu; parameterised ones prefill the command line so you can supply values.

## Managing plugins from the command line

| Command | Effect |
|---|---|
| `plugin list` | List loaded plugins with their usage and summary. |
| `plugin reload` | Re-scan the plugin directory (pick up new or edited plugins). |
| `plugin define <json>` | Define and persist a plugin from inline JSON. |
| `plugin save <name> <n>` | Capture the last `n` commands as a new plugin. |
| `plugin delete <name>` | Remove a plugin from memory and disk. |

## A worked example

An example plugin ships in the repository at
[`docs/examples/plugins/greek-column/`](examples/plugins/greek-column/plugin.json).
Install and run it:

```
mkdir -p ~/.config/itsjustcad/plugins
cp -r docs/examples/plugins/greek-column ~/.config/itsjustcad/plugins/
```

Then in ItsJustCAD:

```
plugin reload
greek-column 5
```

You will see a 0.3 × 0.3 × 5 column appear, logged as a single `box` command in
the op-log — the plugin is only a convenience layer over the substrate.

## Safety model

* Plugins are **template/macro composition only** — no `eval`, no shell-out, no
  native plugin binaries. This mirrors the hardened posture of the deck.
* Every expanded line goes through the same parser and op-log as typed input, so
  a plugin cannot reach state a normal command could not.
* Plugin names are validated on both write and load to prevent path traversal;
  manifests are written `0600` (owner-only) on Unix.
* A malformed manifest is skipped with a clear warning — it never crashes the
  app or blocks other plugins from loading.
