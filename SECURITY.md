# Security

ItsJustCAD opens files authored by other people (DXF, IFC, OBJ, STL, glTF/GLB,
Collada, LAS/LAZ, E57, 3DM, GeoJSON, OSM/Overpass, EPW, and its own
`.itsjustcad.json`), loads user- and LLM-authored JSON (plugins, block
libraries, check-rule packs, deck configs), and can connect to a local or cloud
LLM. We treat all of that as a trust boundary.

## Supported versions

Security fixes land on `main` and ship in the next release. Only the latest
release is supported — there are no long-lived maintenance branches. If you run
an older build, update before reporting.

## Reporting a vulnerability

Report privately to **htarrido@pm.me**, or open a GitHub *security advisory*
(Security ▸ Report a vulnerability). **Do not file a public issue with a working
exploit.** We will acknowledge, fix, and credit you. Please include a minimal
reproducer (a crafted file or JSON) where possible — the smaller the better.

## Threat model

**Imported files are treated as hostile.** Parsers are bounds-checked against
crafted input: file-supplied counts and offsets go through checked arithmetic
(no silent `usize` wrap that would bypass a length gate), oversized allocations
are capped (point clouds decimate to a fixed budget; mesh vertex/index/face
counts and Collada file size are hard-capped), and index reads derived from file
data use guarded access (`.get()` / validated ranges) so malformed geometry
drops bad elements instead of panicking. JSON imports (GeoJSON, Overpass) go
through `serde_json`, which enforces a nesting-depth limit, so deeply nested
input returns a clean error rather than overflowing the stack.

**Imported names cannot smuggle instructions into the model.** Text carried in
from a file (object and layer names) is sanitized at a single choke point before
it reaches the LLM: backticks are stripped, control characters and newlines are
neutralized, whitespace is collapsed, the string is length-capped, and the
result is wrapped in explicit untrusted-data delimiters. A crafted layer name
containing a fake ```` ```draft ```` fence or "IGNORE PREVIOUS INSTRUCTIONS"
cannot forge a command block or break out of its context. Names are also
truncated so a giant imported name cannot blow up token cost.

**The LLM cannot silently touch your machine.** Commands the LLM emits run
through the same substrate as commands you type, but any command with a side
effect outside the document — writing a file (`export`, `print`), reading one
(`import`), or reaching the network — is gated behind an explicit confirmation
when it originates from the model. Commands you type yourself are not gated. The
vision-critique feature grants the model read access to exactly one screenshot
file and nothing else.

**Config, plugin, and pack names cannot escape their directory.** Plugins, block
libraries, and check-rule packs live under `~/.config/itsjustcad/`. Every name
that is used to build a filesystem path (plugin name, block name, pack name) is
validated to reject path separators, `..`, and `.` *before* the path is
constructed, so a name like `../../etc/passwd` cannot read or write outside the
config directory. Plugin bodies are declarative macros parsed by the substrate,
not shell commands — there is no shell-injection path — and plugin-invoked
commands are subject to the same side-effect gate as the LLM.

**Local-first by default.** The app ships pointing at no cloud service. You can
run a fully local model (or your own Ollama) so nothing leaves your machine; a
"local only" toggle refuses cloud sends. API keys live in
`~/.config/itsjustcad/decks.json` (referenced via `env:` by default), written
with restrictive permissions, and are never included in the model prompt or the
saved conversation.

## What is hardened (and what is accepted)

- **Point clouds (LAS/LAZ, E57):** decimate to a fixed `MAX_POINTS` budget;
  record length and offsets are validated against the file length before
  decode; LAZ decompression tolerates a truncated tail (returns what decoded
  cleanly) rather than trusting the header's point count.
- **Meshes (STL/glTF/GLB/OBJ/Collada):** binary STL and GLB chunk sizing use
  checked arithmetic; glTF accessor and Collada vertex/index/face counts are
  hard-capped; Collada files over 512 MB are rejected; interleaved-index reads
  are `.get()`-guarded so a bad input offset drops the face.
- **3DM / IFC:** allocation hints are clamped to the remaining buffer;
  out-of-range vertex indices are handled without panicking; no internal
  deflate path is trusted for the 3DM zip-bomb vector.
- **JSON loaders:** path-traversal-validated names; malformed JSON returns a
  clean error (never a panic) via `serde_json`'s depth-limited parser.
- **Accepted / low-risk:** the SD control-image loader reads
  `<prefix>_depth.png` from an app-generated prefix in a private runtime dir —
  the fixed suffix and internal-only prefix mean it cannot read an arbitrary
  attacker-named file. Config files (`decks.json`, `*.checks.json`) are read
  whole into memory but live in the user's own config dir and are small by
  design.

## Re-running the audit

Untrusted-input hardening is re-run before each release: audit every parser and
JSON loader for unbounded allocations, integer overflow on file-supplied
sizes/offsets, unguarded index reads, panics/`unwrap()` on the parse path, and
path traversal on config names; fix findings with a regression test per fix
(a malformed input that previously panicked or over-allocated now returns a
clean error).
