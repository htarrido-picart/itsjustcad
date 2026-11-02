# Profiling the suite on the Apple M6 (#256, M6 half)

Map: #254 (Performance Improvement). Ticket: #256. Fixture: #255 / PR #264.
Measured 2026-11-01 on main at `6cfb95b`. No code was changed.

## Machine and method

- Apple M6, 12 cores (2 Super, 4 Performance, 6 Efficiency), 24 GB RAM.
- The iOS Simulator was running in the background during the runs and used about 1.5 cores, so wall times are noisy. The tables report the **minimum of 3 to 5 runs**.
- Two unstripped builds of `main` (`CARGO_PROFILE_RELEASE_STRIP=false`, separate target dir):
  - `release`: `opt-level = "z"`, fat LTO, `codegen-units = 1`. This is the shipped profile.
  - `quick`: `opt-level = 1`, no LTO, 16 codegen units.
- Each site operation ran as a separate process: `itsjustcad --open perf-site.itsjustcad.json --run <one-verb>.script --headless`, under `/usr/bin/time -l`. The operation time is the process time minus the load-only time.
- CPU% = (user + sys) / real. 100% means one core; the M6 has 12.
- Hotspots come from `sample <pid> 60 1` against the release build, demangled with `rustfilt`. The figures count leaf samples summed over all threads.
- Threshold (#254): a candidate earns "build" only if it targets an operation **over 500 ms on M6** (or over 2 s on iPhone 12 mini) with a plausible 2x gain or better.

## Results

Times are wall-clock seconds. "Over" means over 500 ms on M6 for the release (shipped) build.

| Operation | release | quick | CPU% (rel) | Verdict | Top hotspot |
|---|---|---|---|---|---|
| `bench_analysis_200_boxes`: sunhours | 0.044 | 0.032 | par | under | BVH ray traversal |
| bench: facesunhours | 0.008 | 0.005 | par | under | same |
| bench: shadowstudy | 0.007 | 0.007 | par | under | convex hull |
| bench: radiation | 0.028 | 0.021 | par | under | same as sunhours |
| Site: build from script (`--run perf-site.script`) | 145.9 | 52.9 | ~65 | **over** | `exec::terrain_surface` full-document scan per plant (#255) |
| Site: load (`--open`, checkpoint) | 0.44 | 0.26 | ~90 | under | serde_json `Geometry`/`Command` deserialize (`Content` buffering) |
| Site: `sunhours 2024-06-21 20` (24,888 cells) | 0.60 | 0.29 | 147 | **over** (release only) | `Bvh::ray_candidates` |
| Site: `facesunhours all 2024-06-21` (413,724 faces) | 4.96 | 2.29 | 417 | **over** | `Bvh::ray_candidates` |
| Site: `radiation all synth.epw` (413,724 faces) | 14.7 | 9.2 | 733 | **over** | `Bvh::ray_candidates` |
| Site: `shadowstudy 2024-06-21 06:00 20:00 30` | 1.31 | 1.12 | 84 | **over** | `Document::insert` (BTreeMap insert) + convex hull |
| Site: `export site.svg` (53 MB) | 1.25 | 0.87 | 80 | **over** | `svg::project_scene` → `feature_edges`; `svg_num` float formatting |
| Site: `sheet` + `sheetview` + `print site.pdf` (21 MB) | 0.65 | 0.59 | 87 | over, marginal | `feature_edges`, memmove |
| Site: headless `--shot` (1280 px, wgpu/Metal) | 5.9 | 8.7 | ~75 | **over** | `SceneRenderer::set_scene` → `object_bind_group`: one uniform buffer and bind group per object |
| Site: `raytrace` (800x500, 48 spp) | 5.0 | 2.7 | 354 | **over** | `Bvh::ray_candidates` + `ray_triangle` |
| Import: LAS 5M points (140 MB) | 0.04 | 0.04 | — | under | (stride-decimated to 200k; only 200k records decoded) |
| Import: LAS 15M points (420 MB) | 0.06 | 0.08 | — | under | same |
| Import: LAZ 5M points (58 MB) | 2.38 | 2.40 | ~75 | **over** | `laz::ArithmeticDecoder::decode_symbol` (sequential) |
| Import: LAZ 15M points (147 MB) | 6.91 | 4.70 | ~75 | **over** | same |

The site build splits as in #255: `miyawaki` plus `plantrow` take nearly all of it. On this run the quick build took 52.9 s against 145.9 s for release.

### Against the step-1 bench in PHASES.md (12-core Intel, quick)

| Kernel | Intel quick | M6 quick | M6 release |
|---|---|---|---|
| sunhours | 85 ms | 32–43 ms | 44–79 ms |
| radiation | 58 ms | 21–26 ms | 28–34 ms |
| facesunhours | 16 ms | 5–7 ms | 8–33 ms |
| shadowstudy | 14 ms | 7–8 ms | 7–35 ms |

The 200-box bench is about 2 to 3 times faster on the M6, and every kernel stays under 100 ms. **It no longer represents a slow operation.** The site document is the workload that crosses the threshold.

## What clears the threshold

**Under 500 ms on M6 (defer):** the whole 200-box bench, loading the site document, and raw (uncompressed) LAS import. LAS import stays cheap at any size up to the 512 MiB cap, because `las::parse` decodes only every `stride`-th record (`MAX_POINTS = 200_000`).

**Over 500 ms on M6 (eligible for "build" if a 2x gain is plausible):**

1. **Ray casting through the BVH** is the largest single cost: `radiation` 14.7 s, `facesunhours` 5.0 s, `raytrace` 5.0 s and `sunhours` 0.6 s (release). In every profile, `kernel_mesh::bvh::Bvh::ray_candidates` is the top leaf, ahead of `ray_triangle`, malloc/realloc/free and memmove. The cause is visible in the code:
   - `ray_candidates` (crates/kernel-mesh/src/bvh.rs:73) walks every node whose AABB the ray touches. It does this along the **whole infinite ray**, with no `t_max` and no near-to-far order. It collects every leaf primitive into a fresh `Vec<u32>`, and its traversal stack is a heap `Vec` too.
   - Only after that do `MeshBvh::ray_occluded` and `ray_hit` (bvh.rs:238, 250) and `raytrace::scene::intersect`/`occluded` (crates/raytrace/src/scene.rs:192, 222) test triangles. A shadow ray therefore cannot stop at its first hit during traversal. A closest-hit ray cannot prune subtrees beyond its best `t`.
   - On a 2.4 × 4 km terrain at low sun angles, each ray's candidate list is long. That explains why the 200-box bench is fast and the site is not.
   - The same function serves picking (`doc/src/pick.rs:224`) and osnap.
   - A plausible ≥2x gain on CPU alone: traverse and test in one pass, exit early for occlusion, order front to back with `t_max` pruning for closest hit, and keep the stack on the stack. This comes before any GPU work (#258).
2. **Planting during the site build** (146 s release, 53 s quick): already diagnosed in #255 as `terrain_surface` scanning the full document per plant. It is ticketed.
3. **Headless shot / scene upload** (about 6 s): `set_scene` creates one `create_buffer_init` uniform buffer and one bind group **per object** (`object_bind_group`, crates/render/src/renderer.rs:1314). With roughly 15k objects, that accounts for about 60% of the main-thread samples, plus Metal buffer allocation and staging teardown. The desktop and iOS viewports likely share this when they open a large document. The time is driver-bound, so the build profile does not change it much.
4. **LAZ import** (2.4 s per 5M points, about 0.45 µs per point): this is the sequential arithmetic decoder in the `laz` crate. All records are decompressed even though only 1 in `stride` is kept. A parallel path is possible: LAZ 1.4 chunk tables allow parallel chunk decode (`laz` has a `parallel` feature). The step-1 notes in PHASES.md call the stream sequential, which holds only for the current single-decoder use.
5. **SVG export** (1.25 s) and **shadowstudy** (1.3 s) are mostly single-threaded:
   - SVG: `feature_edges` and `merge_collinear` per mesh, then `svg_num` → `core::fmt` exact float formatting (the `dragon` slow path) for a 53 MB file.
   - Shadowstudy: inserting thousands of hull objects into the document `BTreeMap`, plus page faults (RSS 830 MB).
   - Both sit near the threshold. A 2x gain is plausible but small in absolute terms.
6. **PDF print** (about 0.6 s) is marginal. Defer.

## Parallel vs single-threaded

| Operation | Status |
|---|---|
| `radiation` | Parallel per face: about 7.3 cores busy. `__psynch_cvwait` (idle rayon workers) is still large, so the work is unevenly split or has serial phases (BVH build, document insert). |
| `facesunhours` | Parallel, about 4.2 cores. |
| `raytrace` | Parallel by pixel, about 3.5 cores. |
| `sunhours` | Parallel, about 1.5 cores. The grid is small and the serial setup dominates. |
| Load, SVG, PDF, `shadowstudy`, site build, LAZ import, scene upload | Single-threaded, 0.75 to 1.1 cores. `shadowstudy` is parallel per stamp, but its kernel is tiny; the serial document insert dominates. |

## Release (`opt-level = "z"`) vs `quick` (`opt-level = 1`)

**Release is slower on every CPU-bound operation:**

| Operation | release / quick |
|---|---|
| Site build | 2.8x (146 s vs 53 s) |
| `facesunhours` | 2.2x |
| `sunhours` | 2.0x |
| `raytrace` | 1.85x |
| Load | 1.7x |
| `radiation` | 1.6x |
| LAZ 15M | 1.5x |
| SVG | 1.4x |

The release profile also shows many `OUTLINED_FUNCTION_*` frames. These come from the machine outliner, which is consistent with optimizing for size. The bench shows the same direction: release runs 1.3x to 1.6x slower than quick.

The profile is decided in a separate ticket. On these numbers, switching the shipped profile to an `opt-level = 3` (or 2) build is the cheapest broad win: about 1.5x to 2.8x, with no code change. This also applies to iOS, because `build_xcframework.sh` builds the core with `--release`.

## Fixture caveats (for #255 follow-ups, not fixed here)

- The face count in `facesunhours all` and `radiation all` is **413,724**, exactly the literal-mesh triangles (terrain plus plants). The 9,984 `lot_building` buildings are not mesh geometry, so they neither receive nor cast shadows in these verbs. A site with real building meshes would cost more per ray.
- `sunhours` samples a ground grid at z = 0, below most of the terrain (terrain sits above z = 0). Most cells score 0 h, which may let rays terminate cheaply. The 40,000-cell cap forces a spacing of 20 m or more on this site.
- `facesunhours` averages 0.5 h on the June solstice in Cali, with 385k of 414k faces at 2 h or less. This looks low and is worth a correctness check: possible self-occlusion between plant faces and terrain. It was not investigated.

## iOS half: what exists and what is missing

The iOS app (`../itsjustcad-ios`) links the Rust core through `itsjustcad-ffi`, built `--release` by `build_xcframework.sh`.

**Existing hooks:**

| Hook | Builds | Behavior |
|---|---|---|
| `IJC_CMD` env var (Engine.swift `launchSteps`) | All | Runs newline-separated command lines after the autosave restore. No timing. |
| `IJC_DEBUG_HISTORY=<line>` (Engine.swift `runDebugScript`) | DEBUG only | Times one command line. Logs `[ijc-swift] debug history <line> -> <ok> in <s> s`, then calls `undoLastCommand()` twice. This is the only timing hook. |
| `ijc_open_start` / `_progress` / `_take` | — | The background open job (#168). Nothing logs its duration. |
| `ijc_import_file` | — | Import from Files. Not timed. |
| `UIFileSharingEnabled` + `LSSupportsOpeningDocumentsInPlace` | — | Lets the 51 MB document be copied in through Finder. |

Bare filenames in commands resolve to `Application Support/attachments` (the core workdir).

**Missing** (none of this is built; these are the gaps):

1. **No raytrace on iOS.** `itsjustcad-ffi` does not depend on `crates/raytrace`, and the verb lives only in the desktop `app` crate (`headless.rs`, `app.rs`).
2. **No way to run `bench_analysis_200_boxes` on device.** It is a `#[test]` in `itsjustcad-commands`. It would need either an FFI bench entry point, `cargo-dinghy`, or the same scene built with `IJC_CMD`.
3. **No multi-line timed script hook.** `IJC_DEBUG_HISTORY` times one line and then undoes. There are no `os_signpost` / `OSSignposter` intervals around `ijc_run_command`, the open job or import, so Instruments cannot attribute wall time to a command.
4. **The xcframework is stripped.** `[profile.release] strip = true`, so Time Profiler shows no Rust symbols unless the core is rebuilt with `CARGO_PROFILE_RELEASE_STRIP=false`. Optionally add `CARGO_PROFILE_RELEASE_DEBUG=line-tables-only`.
5. **No headless shot equivalent.** Scene upload is measured only by opening the document in the live viewport.

## Human checklist for the iOS runs

Run this once each on the iPhone 12 mini, iPhone 14 Pro and iPad, all on mains power and at room temperature.

1. On the Mac, in `itsjustcad`, build the site file:
   `cargo build --release -p itsjustcad && target/release/itsjustcad --run examples/scripts/perf-site.script --headless --out perf-site.itsjustcad.json`
2. Make the synthetic inputs:
   - `synth.epw`: the generator is under "Reproduce" below.
   - `s5000000.laz`: `genlas.py` below, with `laspy[lazrs]` in a venv.
3. Rebuild the xcframework **unstripped**:
   `CARGO_PROFILE_RELEASE_STRIP=false CARGO_PROFILE_RELEASE_DEBUG=line-tables-only ./build_xcframework.sh`
   Optionally build a second time with `CARGO_PROFILE_RELEASE_OPT_LEVEL=3` to get profile-comparison numbers.
4. In Xcode, select the **Debug** scheme (the hooks are `#if DEBUG`; the Rust core is still `--release`) and run once on the device so the app is installed.
5. Copy the files in:
   - Finder → device → Files → ItsJustCAD: drop in `perf-site.itsjustcad.json`.
   - Xcode → Window → Devices and Simulators → ItsJustCAD → Download Container. Put `synth.epw` and `s5000000.laz` in `AppData/Library/Application Support/attachments/`, then use Replace Container.
6. **Load.** In the app, open `perf-site.itsjustcad.json` from Files. Time from tap to viewport with a stopwatch, or better, use Instruments (step 10). Record the time to first frame and whether the device stays responsive. Wait for the autosave; later launches then restore the document.
7. **Analysis verbs.** For each line below, set the scheme's Environment Variable `IJC_DEBUG_HISTORY` to the line. Relaunch from Xcode and read `[ijc-swift] debug history … in X s` in the console. Each launch restores the site from the autosave. The hook then undoes **twice**: once for its own op, and once more, which can remove one op of the document itself. If the autosave captures that, the document drifts. Re-open `perf-site.itsjustcad.json` from Files before each new verb. Run each line 3 times and keep the minimum.
   - `sunhours 2024-06-21 20`
   - `facesunhours all 2024-06-21`
   - `radiation all synth.epw`
   - `shadowstudy 2024-06-21 06:00 20:00 30`
   - `export site.svg`
   - `sheet s1 a1`, then `sheetview s1 top 5000`, then time `print s1 site.pdf`. The sheet lines need `IJC_CMD`; set them there, and put the `print` line in `IJC_DEBUG_HISTORY`.
8. **LAZ import.** Set `IJC_DEBUG_HISTORY` to `import s5000000.laz`, after starting a blank document. The Mac numbers were measured as an import into an empty document.
9. **Bench scene.** This cannot be run as-is (see "Missing" item 2). Skip it, or record "n/a: needs FFI bench hook".
10. **Hotspots.** Product → Profile → Time Profiler, attached to the device. Repeat steps 6 to 8 while recording. Export the heaviest stack trace for each operation. Note the CPU-core usage track: is the work parallel, and do the iPhone 12 mini's 2 performance cores saturate?
11. **Raytrace.** It cannot run on iOS (see "Missing" item 1). Record "n/a".
12. **Record in #256**, per device: operation, minimum time, verdict against **2 s on iPhone 12 mini** (and 500 ms on the others for reference), the top hotspot, and any thermal throttling (`ProcessInfo.thermalState`, or rerun after a cooldown).

## Reproduce (M6)

```sh
S=/tmp/perfm6; mkdir -p $S
git worktree add ../itsjustcad-perf main && cd ../itsjustcad-perf
export CARGO_TARGET_DIR=$S/target CARGO_PROFILE_RELEASE_STRIP=false
cargo build --profile quick -p itsjustcad && cargo build --release -p itsjustcad
$S/target/quick/itsjustcad --run examples/scripts/perf-site.script --headless --out $S/perf-site.itsjustcad.json
# one verb per script; time load + verb, subtract load-only
echo "radiation all $S/synth.epw" > $S/radiation.script
/usr/bin/time -l $S/target/release/itsjustcad --open $S/perf-site.itsjustcad.json --run $S/radiation.script --headless
# hotspots
$S/target/release/itsjustcad --open ... --run ... --headless & sample $! 60 1 -mayDie -file $S/samp.txt
rustfilt < $S/samp.txt | less
# bench
cargo test --profile quick -p itsjustcad-commands --lib bench_analysis -- --ignored --nocapture
cargo test --release      -p itsjustcad-commands --lib bench_analysis -- --ignored --nocapture
```

Other verb scripts: `sunhours 2024-06-21 20`, `facesunhours all 2024-06-21`, `shadowstudy 2024-06-21 06:00 20:00 30`, `export $S/site.svg`, `sheet s1 a1` / `sheetview s1 top 5000` / `print s1 $S/site.pdf`, `persp` / `raytrace $S/rt.png`, and `--shot $S/shot.png` with an empty script.

`synth.epw` uses the same format as `write_synth_epw` in exec.rs tests, with the location set to Cali:

```python
f = open("synth.epw", "w")
f.write("LOCATION,Cali,VC,COL,TMY3,000000,3.45,-76.53,-5.0,1000.0\n")
for kw in ["DESIGN CONDITIONS,0", "TYPICAL/EXTREME PERIODS,0", "GROUND TEMPERATURES,0",
           "HOLIDAYS/DAYLIGHT SAVINGS,No,0,0,0", "COMMENTS 1,x", "COMMENTS 2,y",
           "DATA PERIODS,1,1,Data,Sunday,1/1,12/31"]:
    f.write(kw + "\n")
for m in range(1, 13):
    for h in range(1, 25):
        dni, dhi = (500, 100) if 8 <= h <= 17 else (0, 0)
        f.write(f"1999,{m},21,{h},60,A7,10.0,5.0,80,81100,0,0,300,600,{dni},{dhi}\n")
```

`genlas.py <n> <out.las|out.laz>` writes a synthetic point cloud (`pip install "laspy[lazrs]" numpy`): LAS 1.2, point format 1, a 500 × 500 m sine terrain.

```python
import sys, numpy as np, laspy
n, out = int(sys.argv[1]), sys.argv[2]
rng = np.random.default_rng(1)
h = laspy.LasHeader(point_format=1, version="1.2"); h.scales = [0.001]*3; h.offsets = [0, 0, 0]
las = laspy.LasData(h)
x = rng.uniform(0, 500, n); y = rng.uniform(0, 500, n)
las.x, las.y = x, y; las.z = 10*np.sin(x/30)*np.cos(y/40) + rng.normal(0, .05, n)
las.intensity = rng.integers(0, 65535, n, dtype=np.uint16); las.gps_time = np.arange(n)*1e-5
las.write(out)
```
