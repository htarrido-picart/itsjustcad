# #322 facesunhours measurement (not for merge)

Raw data behind the #322 comment. Facts and options only, not a decision.

- Build: `origin/main` at `33c8314`, `[profile.release]` (opt-level "s", fat LTO), `CARGO_PROFILE_RELEASE_STRIP=false CARGO_PROFILE_RELEASE_DEBUG=line-tables-only`. Apple M6, 12 cores.
- Fixture: the M6 `site.json` from #286/#302/#314 (51,441,719 B, 569,116 triangles). Not committed.
- Each run: `itsjustcad --open site.json --run <script> --headless`. HW RT is the default. `ITSJUSTCAD_HWRT=cpu` forces the CPU path.

## Files

- `times.txt`: whole-process wall / CPU-s, best of 3, unmodified binary, 1-minute load 3.2-3.4. `load` = open only.
- `times.loaded-machine.txt`: the same, an earlier triple at load 32-35 (noisy, not used).
- `times.extra.txt`: save, undo and DXF export runs (best of 2): `load_save`, `fsh_save`, `fsh_undo`, `load_dxf`, `fsh_dxf`.
- `timed.{hw,cpu}.{1,2,3}.txt`: stderr of a binary with `timing-instrumentation.diff` applied (cumulative ms since the start of `exec_face_sun_hours`, plus `session_run_total` and `drop_session`), with `ITSJUSTCAD_HWRT_STATS=1`. Runs alternate hw, cpu. The diff was never committed as code.
- `phase.{hw,cpu}.txt`: `phase.py` output: CPU samples per phase (`phases.txt` regexes over the whole inlined call stack via `atos -i`) with top items.
- `samples/*.txt.gz`: `/usr/bin/sample <pid> 30 1` whole-process profiles. `samp.undo.txt.gz` is 10 s into the `undo` run.
- `fsh_twice.report.txt`: `report` after one and after two `facesunhours all` runs.
- `run.sh`, `samp.sh`, `phase.py`, `classify.py` (from #314), `*.script`: how to reproduce. Scripts hold absolute scratch paths.
