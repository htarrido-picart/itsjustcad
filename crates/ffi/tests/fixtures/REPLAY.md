# Deck prompt replay: how to answer a prompt

The offline replay eval measures the FFI cloud deck (`ijc_deck_send`) without
API calls. An agent plays the model. Each prompt file is the exact request the
app would send.

## Your job, for one prompt `<id>`

1. Read **only** `prompts/<id>.md`. Do not read other prompt files, other
   answers, the manifest, the eval code or the repository. The model sees only
   this request.
2. The file has two parts:
   - `=== SYSTEM PROMPT ===`: the system prompt. The current scene, with the
     object ids, is its last section ("Current scene").
   - `=== USER MESSAGE ===`: the user's request. It is the only message in the
     conversation.
   Ignore the `<!-- … -->` comment on the first line.
3. Act as the deck model. Follow the system prompt exactly, as a careful model
   would on this one turn. Use your own judgment, but do not use any knowledge
   of how the eval scores answers.
4. Write the reply the model would stream, and nothing else:
   - one short sentence of chat;
   - then a fenced block that opens with a line of exactly ` ```draft ` and
     closes with a line of exactly ` ``` `;
   - inside it, one command per line, all the commands the request needs, in
     order, using only verbs and argument orders from the system prompt.
   No other wrapper, no explanation of your reasoning, no notes to the eval.
   If the system prompt tells you to ask a question instead, the reply is that
   one `QUESTION: …` line, with no block.
5. Save it as plain text to `answers/<id>.txt` (UTF-8, ending with a newline).

## Example

`answers/tuning0.txt`:

````text
Here is a 10 m box at the origin.
```draft
box 0,0,0 10,10,10
```
````

## Scoring (for whoever runs it, not for the answerer)

```sh
cargo run -p itsjustcad-ffi --example deck_replay -- dump  --out <dir>
cargo run -p itsjustcad-ffi --example deck_replay -- score --answers <dir>/answers
```

`score` replays each reply through the FFI extractor and `normalize_selectors`,
rejects the lines `ijc_validate_command` refuses, runs the rest on a fresh
document after the setup lines, and judges it like the iOS `CloudDeckEvalTests`
and `EvalJudge`. Object ids are random per document, so `score` maps the ids
the dumped prompt showed (`<dir>/scenes.json`, or `--scenes <json>`) to this
run's ids before replaying. Add `--only id1,id2` to score a subset. The manifest
(`deck_eval_manifest.json`) is generated from the iOS manifest with
`scripts/eval_manifest_to_json.py`. The core-only `here` cases (#128) live in
`deck_here_manifest.json`: each has the host `context` that
`ijc_deck_set_context` would hold, and a `near` (the new object is at an `x,y`
point) or `question` (a `QUESTION:` and no commands) check. Without
`--manifest`, both files are used.
