# Security

ItsJustCAD opens files from other people (DXF, IFC, OBJ, glTF, LAS, GeoJSON,
EPW, and its own `.itsjustcad.json`) and can connect to a local or cloud LLM.
We treat all of that as a trust boundary.

## Threat model

**Imported files are treated as hostile.** Parsers are bounds-checked against
crafted input (oversized allocations, out-of-range indices, malformed offsets),
and text carried in from a file (object and layer names) is sanitized before it
is ever shown to the LLM — an imported name cannot smuggle instructions into a
model prompt.

**The LLM cannot silently touch your machine.** Commands the LLM emits run
through the same substrate as commands you type, but any command with a
side effect outside the document — writing a file (`export`, `print`), reading
one (`import`), or reaching the network — is gated behind an explicit
confirmation when it originates from the model. Commands you type yourself are
not gated. The vision-critique feature grants the model read access to exactly
one screenshot file and nothing else.

**Local-first by default.** The app ships pointing at no cloud service. You can
run a fully local model (or your own Ollama) so nothing leaves your machine; a
"local only" toggle refuses cloud sends. API keys live in
`~/.config/itsjustcad/decks.json` (referenced via `env:` by default), written
with restrictive permissions, and are never included in the model prompt or the
saved conversation.

**Plugins** are user-authored macros stored under `~/.config/itsjustcad/`.
Plugin names are validated so a malicious file cannot escape that directory, and
plugin-invoked commands are subject to the same side-effect gate as the LLM.

## Reporting a vulnerability

Please report security issues privately — email the maintainer or open a GitHub
security advisory. **Do not file public issues with working exploit details.**
We'll acknowledge, fix, and credit you.
