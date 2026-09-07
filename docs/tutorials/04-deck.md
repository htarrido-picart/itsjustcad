# Tutorial 4 — Working with the deck (the LLM partner)

*You will configure a backend, ask the deck to draw and analyse, watch it plan a
multi-step task, and read a critique grounded in numbers. About 15 minutes.*

The **deck** is the panel on the right of the window. It is not a chatbot bolted
on — it speaks the exact command language you type, draws through the same
substrate, and shares the same document. When it draws a rectangle it runs the
same `rect` you would. There is no second, worse code path for the machine.

> **The deck ships with no backend active.** Nothing leaves your machine until
> you configure one and send a message. Configure it first (step 1), or the
> panel has nothing to talk to.

---

## 1. Configure a backend

Cassettes (backends) live in `~/.config/itsjustcad/decks.json`. Pick one:

**A local model (nothing leaves your machine).** Install [Ollama](https://ollama.com),
pull a model (`ollama pull qwen3`), then:

```json
{
  "decks": [
    { "name": "ollama", "kind": "openai_compat",
      "base_url": "http://localhost:11434/v1", "model": "qwen3", "grammar": true }
  ],
  "active": 0
}
```

**A cloud model.** Set `ANTHROPIC_API_KEY` in your environment, then:

```json
{
  "decks": [
    { "name": "claude", "kind": "anthropic",
      "base_url": "https://api.anthropic.com",
      "model": "claude-sonnet-4-6", "api_key": "env:ANTHROPIC_API_KEY" }
  ],
  "active": 0
}
```

`api_key` is a literal or `env:VAR` (prefer `env:` — don't store keys in plain
text). Switch the active cassette from the dropdown in the deck panel's header.
For airgapped work set `"local_only": true` and only localhost backends run.
Full cassette reference: [deck.md](../deck.md).

---

## 2. Ask it to draw

In the deck panel, type a plain-language request:

> *Make a 20 by 14 m two-storey shell with a 10 by 6 courtyard cut through the
> middle.*

The deck replies with prose plus a fenced block of real commands:

````
```draft
rect 0,0,0 20 14
extrude last 6
name last shell
rect 5,4,0 10 6
extrude last 7
name last court
difference shell court
name last building
```
````

Commands inside a `draft` fence are extracted and run against your document — the
geometry appears immediately. Click any **command card** to inspect, undo, or
amend it. Geometry commands run automatically; anything that touches the
filesystem (`export`, `import`, `print`) asks for confirmation first. That is a
security boundary, not a nag.

The deck can also drive the **view, camera, and window layout** from the same
fence — "frame the courtyard in a top view" reframes the viewport.

---

## 3. Terse mode

Local models are faster when they say less. **Terse mode** makes the deck answer
like a laconic senior drafter — no preamble, a `draft` block over prose, a hard
per-turn token cap — without dropping a single number or warning. It is **on by
default for local backends**, off for cloud. Toggle it under **LLM ▸ Terse
Replies**; the choice persists per cassette.

---

## 4. Let it plan a multi-step task

For anything with several stages, ask big:

> *Lay out a site on a 240 by 180 boundary, subdivide it, add row buildings, and
> report the yield.*

The deck first replies with a **numbered plan and nothing else**, then executes
one step per turn — emitting only that step's commands, reading back results and
errors, retrying a failed step within a budget — and finishes with a
verification turn and a one-line summary. The checklist ticks live in the
transcript, and an in-progress plan is saved with the document, so it resumes
after a relaunch. (Simple one-shot requests skip the plan.)

If a request is ambiguous — a missing dimension, an unclear target — the deck
does **not** guess. It replies with a single `QUESTION:` line; your next message
answers it and the turn continues.

---

## 5. Grounded critique with markdown tables

Run an analysis or a `lotreport`, then ask the deck to read it:

> *Read the sunhours report and tell me where to avoid outdoor seating.*

The deck fetches the structured `report` and grounds its critique in sampled
values and rule ids — *"the north strip averages 0.5 h; move seating to the
south-east where cells hit 9 h."* Numbers, not vibes. It renders tabular answers
(object lists, analysis stats, schedules) as real striped **markdown tables** in
the chat; right-click ▸ **Copy message** keeps them as tables when pasted.

For compliance pre-checks (`codecheck ibc2021` / `ada2010`) the deck is
instructed to frame output as an **advisory pre-check, never a code review** — it
will never claim a design "complies," and points you at a licensed professional
or the AHJ.

Separately, the **critique** button screenshots your viewport and asks the deck
to review the massing visually, like a design critic.

---

## 6. It can teach, and build its own tools

- Ask *"how do I make walls from a centerline?"* and it **explains**
  (`offset → extrude → difference`) instead of just doing it.
- Say *"select this and make it taller"* — it **knows your selection**.
- Ask for a stair generator and it can **author a plugin** mid-conversation
  (`plugin define …`) that then appears in autosuggest and its own vocabulary.

---

## Notes

- Conversations survive restarts and can be **encrypted at rest** (`chatencryption
  on`, opt-in — off by default until the app is code-signed).
- The deck sees the command registry, a scene digest, and your units — never raw
  geometry coordinates. It drafts commands, not JSON.
- Full backend and plugin reference: **[deck.md](../deck.md)**. Every verb the
  deck can emit is in the **[command reference](../command-reference.md)**.
