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

The deck also drives **view, camera, and window-layout** verbs from the same fence — reframing, display modes, lenses, panel/dock/split changes — never just geometry.

---

## Working style

### Terse mode

Terse mode makes the deck answer like a laconic senior drafter: no preamble or filler, sentence fragments, a `draft` block over prose, and a hard per-turn token cap. Fewer tokens means faster local inference without losing any number, command, or warning.

It is **on by default for all local (OpenAI-compatible) backends** and off for cloud backends, with a per-cassette override. Toggle it from **LLM ▸ Terse Replies** (in-window or the native menu); the choice persists in `decks.json`.

### Clarify before acting

On an ambiguous request — a missing dimension, an unclear target ("make it bigger" with several objects selected), a placement it would have to invent — the deck does not guess. It replies with a single `QUESTION:` line and no commands. Your next message answers it and the turn continues normally.

### Plan-execute for multi-step tasks

For a prolonged task with several distinct stages, the deck first replies with a numbered **plan** and nothing else. It then executes one step per turn — emitting only that step's commands, reading back results and errors, retrying failed steps within a bounded budget — and finishes with a verification turn (`bbox all`, `schedule`, `report`) and a one-line summary. The checklist ticks live in the transcript, and an in-progress plan is saved with the document so it resumes after a relaunch. Simple one-shot requests skip the plan.

### Markdown tables

The chat transcript renders markdown pipe tables as real striped grids. The deck is prompted to format tabular answers — object lists, analysis stats, schedules, `report` output — as tables, which are both easier to read and cheaper in tokens. Right-click ▸ **Copy message** copies the raw markdown, so pasted tables stay tables.

---

## Grounded critique — analysis, report, and `critique`

The deck can review a design in numbers, not vibes. After you run an environmental analysis (`sunhours`, `facesunhours`, `radiation`, `shadowstudy`) or a compliance pre-check (`codecheck`), the substrate stores a compact structured report on the document. The deck fetches it with the read-only `report` verb (`report`, `report facesunhours`, `report codecheck`) and grounds its feedback in sampled values and rule ids — "the north face at (0,10,2) gets 0.5 h; don't put the terrace there" or "stair riser 0.21 m > max 0.178 m (IBC 1011.5.2): deepen the run".

Compliance output is always framed as an **advisory pre-check, never a code review** — the deck is instructed never to claim a design "complies" and to point you at a licensed professional / AHJ.

Separately, the **critique** button screenshots the viewport and asks the deck to review the massing visually, like a design critic.

---

## Chat encryption (opt-in)

Chat-session files live at `~/.config/itsjustcad/chats/<uuid>.json`. By default they are stored in plaintext. Enable at-rest encryption with:

```
chatencryption on      # alias: encryptchats on
chatencryption off
```

When on, sessions are encrypted with AES-256-GCM keyed by a per-user key in the OS keychain (transparent auto-unlock, at most one keychain prompt per launch). The container is self-describing, so turning it on or off is lossless either way — existing stores load and re-save in whichever mode is selected. The default is **OFF** because the app is not yet code-signed and an always-on default would prompt every user for keychain access on first chat. On systems with no keychain (headless / CI / bare Linux) it falls back to plaintext with a one-time warning — never a hard fail. The setting persists in `ui.json`.

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
- the view/camera app-verbs and the UI/layout verbs it may also drive
- the selector grammar
- interpretation guidance for environmental critique and compliance pre-checks (with the mandatory advisory framing)
- the clarify, plan, and markdown-table conventions
- a brief summary of the current document (object count, layer names)
- the current display units

It does not see raw geometry coordinates or the full op-log by default — it drafts commands, not JSON.
