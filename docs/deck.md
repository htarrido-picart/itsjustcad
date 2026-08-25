# The Deck — LLM Drafting Partner

The deck is the LLM panel embedded in the app. It speaks the same command language you type, draws with the same substrate, and shares the same document. There is no second code path for the machine.

---

## Opening the deck

Click the deck button in the toolbar, or press the keybinding shown on the button. The deck panel opens on the right side of the viewport.

---

## How it works

You type a request in natural language. The deck responds with prose and, inside fenced code blocks, real commands:

````
```draft
rect 0,0,0 8 6
extrude last 3
name last house
```
````

Commands inside a `draft` fence are extracted, parsed, and run against your document as if you had typed them. The geometry appears immediately.

Pure geometry commands (no file I/O) run automatically. Commands that touch the filesystem — `export`, `import`, `print`, `underlay`, etc. — show a confirmation prompt first. This is a security boundary: the deck cannot silently write or read files you have not approved.

---

## Cassettes

A **cassette** is one configured LLM backend. The deck ships with four defaults:

| Name | Kind | Notes |
|---|---|---|
| `claude-code` | Claude Code CLI subprocess | Uses your Claude subscription; no API key needed |
| `ollama` | OpenAI-compatible | Local model via Ollama; grammar-constrained by default |
| `claude` | Anthropic API | Set `ANTHROPIC_API_KEY` in the environment |
| `kimi` | OpenAI-compatible | Kimi K2; set `MOONSHOT_API_KEY` |

Switch the active cassette in the deck panel's header dropdown.

### Configuration file

Cassettes are stored in `~/.config/itsjustcad/decks.json` (mode 0600 on Unix). Edit it to add, remove, or reorder cassettes. The format:

```json
{
  "decks": [
    {
      "name": "my-local",
      "kind": "openai_compat",
      "base_url": "http://localhost:11434/v1",
      "model": "llama3.3",
      "grammar": true
    },
    {
      "name": "claude",
      "kind": "anthropic",
      "base_url": "https://api.anthropic.com",
      "model": "claude-sonnet-4-6",
      "api_key": "env:ANTHROPIC_API_KEY"
    }
  ],
  "active": 0,
  "local_only": false
}
```

`api_key` can be a literal string or `"env:VAR_NAME"` to read from the environment (recommended — do not store keys in plain text).

### Local-only mode

When `local_only: true` is set in `decks.json`, only cassettes with localhost base URLs are visible and runnable. Any attempt to send to a remote endpoint is blocked. Useful for airgapped workflows or when the document contains sensitive data.

---

## Grammar-constrained decoding

When `grammar: true` is set on an `openai_compat` cassette, the deck attaches a GBNF grammar (derived live from the command registry) to each request. Local models that support grammar-constrained decoding (llama.cpp's server, Ollama ≥ 0.3) can only emit real verbs inside `draft` fences. This dramatically reduces hallucinated or malformed commands from smaller models.

Cloud endpoints (OpenAI, Anthropic) ignore the grammar field — leave it `false` for them.

---

## Persistent sessions (Claude Code cassette)

The `claude-code` cassette keeps a provider-side session alive across turns. The deck sends only the newest message to the subprocess rather than the full transcript, so long conversations stay fast and do not bloat the context window.

Each MCP server configured in `.claude/` settings is isolated per turn — the deck cannot exfiltrate data to one tool while working on another.

---

## Plugins — the deck writes its own tools

The deck can define new commands at runtime by writing plugin files. A plugin is a named macro: a parameterised list of command-template lines stored at `~/.config/itsjustcad/plugins/<name>.plugin.json`.

Example the deck might emit:

```json
{
  "name": "column-grid",
  "description": "Grid of columns at nx × ny bays",
  "params": [{"name": "nx", "default": "5"}, {"name": "ny", "default": "3"}],
  "body": [
    "box 0,0,0 0.4,0.4,3",
    "array last {0},{1},1 3,4,0"
  ]
}
```

Invoking `column-grid 6 4` substitutes `{0}` → `6`, `{1}` → `4` and runs each line through the substrate. **The expanded commands, not the plugin call, land in the op-log.** Replay never re-expands plugins, so files are stable even after a plugin is edited or deleted.

---

## What the deck sees

The deck's system prompt includes:

- the full command registry (every verb, usage, and summary)
- the selector grammar
- a brief summary of the current document (object count, layer names)
- the current display units

It does not see raw geometry coordinates or the full op-log by default — it drafts commands, not JSON.
