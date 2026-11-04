## Resolution: re-profile at `0e96955` (traversal fix #303 included)

**Setup.** Apple M6 (12 cores). Built `origin/main` at `0e96955`: `[profile.release]` (`opt-level = "s"`, fat LTO), with `strip = false` and line-table debug info for symbols. The fixture is the same `site.json` as #286, with 569,116 triangles, buildings included.

**Method.** Same as #286. Each verb runs in its own `--open site.json --run <verb>.script --headless` process, best of 3. The best load-only run (0.22 s wall, 0.21 CPU-s) is subtracted. Cores = op CPU-s ÷ op wall. Profiles come from `/usr/bin/sample <pid> 30 1` over the whole process, so they include the ~0.2 s load. Each self sample is attributed through `atos -i` (inlined frames), because traversal is mostly inlined into its callers now:
- **traversal**: any frame in `ray_any_hit` / `ray_closest_hit` / `traverse` / `TriBvh::ray_occluded` / `ray_aabb` / `node_bounds` / `NodeStack`;
- **triangle**: `ray_triangle`;
- the rest, by top items.

Idle waits (`__psynch_cvwait`, condvar, rayon sleep) are excluded. `sunhours` takes ~90 ms, so it was sampled from a script that repeats it 20 times.

**Noise: low.** The 1-minute load average was 2.2 to 3.1 for every run (#286: 9 to 17). Within each triple, wall times varied by no more than 0.02 s and CPU-s by no more than 1.5%.

| Command | wall (s) | CPU-s | cores | >500 ms | traversal | `ray_triangle` | everything else (top items) |
|---|---|---|---|---|---|---|---|
| `sunhours 2024-06-21 20` | 0.09 | 0.43 | ~4.8 | under | 31% | 7% | BVH build (`select_nth_unstable_by`/`partition`/`append_subtree`) 38%; doc lookups `BTreeMap::get` + `memcmp` ~15% (`scene_triangles`); `getentropy` 2% |
| `facesunhours all 2024-06-21` | 1.07 | 4.67 | ~4.4 | **over** | **65%** | 14% | `getentropy` 8% (UUIDs, load and result insert); BTreeMap insert/get ~3%; malloc/free ~1%; BVH build 3% |
| `radiation all synth.epw` | 2.43 | 20.43 | ~8.4 | **over** | **78%** | 16% | `getentropy` 2%; per-face closure <1%; solar math no longer visible (<0.5%) |
| `raytrace` (persp, 800x500, 48 spp) | 1.24 | 10.15 | ~8.2 | **over** | **53%** | **24%** | inlined render loop (`render_pass` rayon helper, no deeper line info) 12%; `memmove` 3%; `pow` 2%; tonemap/`sincos`/`resolve` ~2% |

**Delta vs #286.** #286 used the same fixture and profile. The wall deltas are inflated by #286's heavy load, so compare CPU-s.
- `sunhours`: wall 0.32 → 0.09 s; CPU 0.8 → 0.43 s. The parallel BVH build (#297) is the main factor here. The build is still the largest single cost.
- `facesunhours`: wall 3.00 → 1.07 s; CPU 15.0 → 4.67 s (−69%).
- `radiation`: wall 12.19 → 2.43 s; CPU 80.4 → 20.4 s (−75%). That includes #296: the ~15% of solar math has gone.
- `raytrace`: wall 2.40 → 1.24 s; CPU 14.3 → 10.15 s (−29%).
- `Bvh::ray_candidates` and the per-ray `Vec` growth (#286: 40 to 64%, and 9 to 12%) no longer appear. Traversal is now `ray_aabb` + `node_bounds` + `NodeStack` push/pop, inlined into `ray_occluded` / `ray_closest_hit`.

**Per blocked ticket: facts, not decisions.**
- #259 SIMD: Box/traversal work is 53 to 78% of busy samples; the exact triangle test is 14 to 24%. Together, these make up 77 to 94% in the three verbs that are over 500 ms (`facesunhours` 1.07 s, `radiation` 2.43 s, `raytrace` 1.24 s). `sunhours` is under (0.09 s).
- #260 GPU compute (mlx-rs vs wgpu): The time is now almost entirely ray queries: shadow any-hit in `facesunhours`/`radiation`, and closest-hit plus shadow in `raytrace`. The BVH build and the document work stay on the CPU and are small, except in `sunhours`, where the build is 38%. `radiation` keeps 8.4 cores busy for 2.43 s (20.4 CPU-s). The same three verbs are over 500 ms.
- #261 hardware RT: The work that RT hardware replaces (traversal + triangle test) is 79% (`facesunhours`), 94% (`radiation`) and 77% (`raytrace`) of busy samples. The same three verbs are over 500 ms.
- Nearest-child-first traversal (deferred opt-in from #268): Traversal is 53% of `raytrace` busy samples and dominates `ray_closest_hit`, which is the path ordering would prune. On this evidence, it looks worth measuring. It would not help the any-hit verbs (`facesunhours`/`radiation`).

**Other findings.**
- `facesunhours` uses only ~4.4 cores against ~8.4 for `radiation`. Part of the reason is serial `getentropy`/BTreeMap work (UUIDs, result insert).
- Pitfall: the #286 `raytrace.script` holds an absolute output path. Copying it wrote the first 3 PNGs into the old scratch folder; this was fixed before the timings used here.

Raw data (times, samples, `classify.py`): scratchpad `reprofile-302/` (local only).
