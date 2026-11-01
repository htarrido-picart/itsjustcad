# wgpu ray-query maturity vs native Metal ray tracing

Research for #258, feeding the build/defer/drop decision on #231 (hardware ray
tracing for `crates/raytrace` and sun/shadow analysis ray casts). Facts and
trade-offs only; no recommendation. Researched 2026-11-01.

Constraints carried from #231: the CPU path stays the test oracle, GPU results
may differ within a documented tolerance, and no GPU work runs in op-log
mutation paths.

## Short answer

Yes, hardware ray tracing is reachable through wgpu on Metal today, but only as
an experimental, `unsafe`-gated feature with open correctness bugs on the Metal
backend. The wgpu version this repo already uses (29.0.4) contains the Metal
implementation, so no upgrade is needed to start; upgrading to wgpu 30 is
blocked by eframe/egui-wgpu 0.35 anyway.

## 1. What this repo uses

- `Cargo.lock`: `wgpu` / `wgpu-core` / `wgpu-hal` / `naga` = 29.0.4.
- `Cargo.toml` pins `wgpu = "29"` with the comment that eframe 0.35 requires
  wgpu ^29 and wgpu must not be bumped to 30 until eframe supports it.
- Latest releases: wgpu 30.0.1 (2026-08-21). egui-wgpu 0.36.0 and 0.37.0
  (2026-10-15) both require `wgpu ^30.0` (crates.io dependency metadata). So a
  wgpu 30 upgrade means an eframe/egui 0.35 -> 0.36+ upgrade too.
- `objc2-metal` 0.3.2 is already in the lockfile (pulled in by wgpu-hal and
  winit/arboard), and its generated bindings include
  `MTLAccelerationStructure`, `MTLAccelerationStructureCommandEncoder`,
  `MTL4AccelerationStructure` and `MTLIntersectionFunctionTable`.
- `crates/raytrace` is a pure-Rust CPU path tracer with its own `Bvh`
  (`crates/raytrace/src/scene.rs`).

## 2. wgpu ray query on Metal: state as of Nov 2026

### When it landed

- wgpu v29.0.0 (2026-03-18) CHANGELOG, Metal section: "Implements ray-tracing
  acceleration structures for metal backend" (gfx-rs/wgpu#8071, merged
  2026-02-18). Same release moved wgpu-hal Metal to `objc2` bindings (#5641).
- The PR author notes the ray examples (`ray_scene`, `ray_shadows`,
  `ray_cube_compute`, `ray_cube_fragment`, `ray_traced_triangle`) run, but
  "current CI runner is too old ... as it does not support hardware ray
  tracing". CI did not exercise it on Metal at merge time.
- v30.0.0 (2026-07-01): "Guard against invalid calls to ray query functions on
  Metal" (#9442); a combined
  `Limits::max_buffers_and_acceleration_structures_per_shader_stage` for
  Metal's shared buffer argument table; AABB (procedural) BLAS geometry (#9290).

### How it is gated

- Feature flag: `wgpu::Features::EXPERIMENTAL_RAY_QUERY` (WGSL `enable
  wgpu_ray_query`). Native-only.
- All `EXPERIMENTAL_*` features also require the device descriptor's
  `experimental_features` token, created with
  `unsafe { wgpu::ExperimentalFeatures::enabled() }`. Its doc
  (`wgpu-types/src/tokens.rs`) says these "may result in undefined behavior
  when used incorrectly" and that the caller acknowledges "there may be
  UB-containing bugs in these apis ... hit by calling otherwise safe code".
- Feature doc: "THIS IS EXPERIMENTAL: Features enabled by this may have major
  bugs in it and are expected to be subject to breaking changes."
- Doc drift: the `EXPERIMENTAL_RAY_QUERY` doc comment in both 29.0.4 and
  trunk still lists "Supported platforms: Vulkan" only, yet the Metal adapter
  sets it (see below). Trust the adapter code, not the doc.

### Which Apple GPUs get it

`wgpu-hal/src/metal/adapter.rs` (29.0.4, unchanged on trunk):

```rust
supports_raytracing: if available!(macos = 15.0, ios = 18.0, tvos = 18.0, visionos = 2.0) {
    device.supportsRaytracing() && device.supportsRaytracingFromRender()
} else { false },
...
features.set(F::EXPERIMENTAL_RAY_QUERY, self.supports_raytracing);
```

- OS floor: macOS 15 / iOS & iPadOS 18.
- GPU floor: whatever reports `MTLDevice.supportsRaytracing` and
  `supportsRaytracingFromRender`. Per Apple's Metal Feature Set Tables
  (2026-10-23), "Ray tracing in compute pipelines" and "in render pipelines"
  start at **Apple6** (A13). So wgpu exposes the feature on A14, A16, M1, M2
  as well, not only on RT-hardware GPUs. On those it runs Metal's
  non-hardware-accelerated path. wgpu has no flag that distinguishes
  "hardware RT" from "Metal RT API available"; the app would need to check
  `MTLGPUFamily::Apple9` (or the chip) itself via `as_hal` /
  `wgpu_hal::metal::Device::raw_device()`.

### What is supported / missing on Metal

- Supported: BLAS/TLAS build, update, compaction-size query, copy; ray
  queries in compute and fragment shaders (naga MSL backend emits a
  `_RayQuery` wrapper around Metal's `intersector`).
- Not on Metal: ray-tracing pipelines (`EXPERIMENTAL_RAY_TRACING_PIPELINES`;
  ray-gen/hit/miss stages hit `unimplemented!()` in wgpu-hal Metal; open
  design question gfx-rs/wgpu#8560). Not needed for shadow rays / ray casting;
  ray query is sufficient.
- naga MSL: `RAY_QUERY_MODERN_SUPPORT: bool = false; //TODO` (29.0.4 and
  trunk), i.e. the older intersector API path.
- `EXPERIMENTAL_RAY_HIT_VERTEX_RETURN` doc lists Vulkan only.

### Open correctness bugs on Metal (open as of 2026-11-01)

- gfx-rs/wgpu#9215 "Metal BLAS -> TLAS builds need synchronisation":
  `place_acceleration_structure_barrier` is a no-op on Metal, so a TLAS build
  can consume a still-building BLAS. Fix PR #10292 open.
- PR #10287 "[metal] correct ray query semantics and BLAS transform handling",
  open. States Metal intersectors were configured column-major while wgpu
  supplies row-major BLAS transforms, "silently corrupting instance placement
  (visible as rays missing geometry that provably intersects it)"; culling and
  duplicate-intersection flags not forwarded; missing ray-query state machine.
  Tested on M4 Max.
- PR #9757: calling `supportsRaytracing` on an Xcode `CaptureMTLDevice`
  faults; workaround open.
- gfx-rs/wgpu#10777 (cross-backend): TLAS built from a sparse instance array
  can give the wrong index.

Implication for #231's tolerance model: instanced geometry with non-identity
transforms is affected by the open transform bug, and BLAS->TLAS in one
encoder can race, on the released 29.x/30.x versions. A CPU-oracle comparison
test would be expected to catch these, but they are not "within tolerance"
differences.

## 3. Native Metal ray tracing behind a feature flag

- Bindings: `metal-rs` README: "Use of this crate is deprecated ... For new
  development, please use `objc2` and `objc2-metal`." wgpu itself moved to
  objc2 in v29. `objc2-metal` 0.3.2 (already locked) exposes the
  acceleration-structure and intersection-function-table APIs, including
  Metal 4 (`MTL4AccelerationStructure`).
- API maturity: Metal ray tracing is a stable, shipping Apple API
  (`MTLDevice.supportsRaytracing` since macOS 11 / iOS 14;
  `supportsRaytracingFromRender` since macOS 12 / iOS 15). Same Apple6+ GPU
  floor as above; hardware acceleration from Apple9.
- Hardware-only features available natively, not via wgpu: per the Feature
  Set Tables, Apple9+ adds "Acceleration structures containing row-major
  matrices", "Ray tracing with per-component motion interpolation" and
  "Direct access to on-chip ray-intersection result storage"; plus
  intersection function tables (custom/curve primitives).
- Interop: wgpu-hal exposes `metal::Device::raw_device()`, so a native path can
  share the wgpu `MTLDevice` and buffers via `as_hal` rather than a second
  device. Shaders would be hand-written MSL, not WGSL.
- Cost: Apple-only code path (no Vulkan/DX12 parity), `unsafe` objc2 calls,
  own synchronization and resource lifetime; but no dependency on wgpu's
  experimental flag or its open Metal bugs, and not tied to the eframe/wgpu
  version lock.

## 4. Trade-offs (no decision)

wgpu `EXPERIMENTAL_RAY_QUERY`:
- + Portable WGSL; same code also runs on Vulkan RT GPUs (the only platform the feature doc lists).
- + Available in the wgpu 29 already locked; no upgrade needed to prototype.
- - `unsafe` experimental token; documented UB risk; breaking changes expected.
- - Open Metal correctness bugs (transforms, BLAS/TLAS sync) in released
  versions; fixes would arrive in a wgpu >= 31 release, which in turn needs
  an eframe that supports it.
- - Feature also reported on non-RT-hardware GPUs (A13-A16, M1/M2), so
  "hardware RT" must be detected separately.

Native Metal (objc2-metal) behind a cargo feature:
- + Stable Apple API; full Apple9+ hardware feature set; no wgpu-version lock.
- + Can reuse wgpu's `MTLDevice` via `raw_device()`.
- - Apple-only; separate MSL shaders; more `unsafe`; more code to own.

Both:
- CPU `crates/raytrace` remains the oracle. GPU traversal is fp32 with
  driver-defined intersection behaviour (not specified by Apple or wgpu), so
  grazing/edge hits can differ from the CPU BVH; the tolerance must cover
  that.
- Neither needs to touch op-log mutation paths; acceleration structures are
  derived, rebuildable render-side state.

## 5. Device RT hardware

Hardware-accelerated RT starts with Apple9: A17 Pro and M3 (Apple newsroom
2023-10-30, "hardware-accelerated ray tracing comes to the Mac for the first
time"; Apple tech talk "Explore GPU advancements in M3 and A17 Pro").
Feature Set Tables: A14 = Apple7, A15/A16 = Apple8, A17 Pro = Apple9.

- Apple M6 Mac: **yes**. Apple newsroom 2026-08 (M6 / M5 Ultra) and 2026-10
  (MacBook Pro / iMac with M6): "hardware-accelerated ray tracing",
  "third-generation ray-tracing engine".
- iPhone 12 mini (A14, Apple7): **no** RT hardware. Metal RT API and wgpu
  `EXPERIMENTAL_RAY_QUERY` still available (iOS 18+), non-accelerated.
- iPhone 14 Pro (A16, Apple8): **no** RT hardware. Same as above.
- iPad: depends on chip.
  - RT hardware: A17 Pro (iPad mini 7th gen, 2024), A20 Pro (iPad mini,
    2026-10 newsroom: "hardware-accelerated mesh shading and ray tracing"),
    M3 (iPad Air 2025), M4 (iPad Pro 2024, iPad Air 2026-03), M5 (iPad Pro
    2025), and later.
  - No RT hardware: A12-A16 iPads (incl. iPad 10th gen A14, iPad 11th gen
    A16, iPad mini 6 A15), M1 and M2 iPad Air/Pro.
  - Check in Settings > General > About > Model Name, or at runtime with
    `supportsFamily(.apple9)`.

## Sources

- wgpu CHANGELOG (trunk): https://github.com/gfx-rs/wgpu/blob/trunk/CHANGELOG.md
- wgpu-hal 29.0.4 `src/metal/adapter.rs`, `src/metal/command.rs`,
  `src/metal/device.rs`; wgpu-types 29.0.4 `src/features.rs`,
  `src/tokens.rs`; naga 29.0.4 `src/back/msl/writer.rs` (local cargo
  registry; trunk checked on GitHub for the same lines)
- gfx-rs/wgpu PR #8071, #10287, #10292, #9757; issues #9215, #8560, #10777
- crates.io dependency metadata for egui-wgpu 0.36.0 / 0.37.0
- metal-rs README: https://github.com/gfx-rs/metal-rs
- Apple Metal Feature Set Tables (2026-10-23):
  https://developer.apple.com/metal/Metal-Feature-Set-Tables.pdf
- Apple docs: `MTLDevice.supportsRaytracing`,
  `MTLDevice.supportsRaytracingFromRender`
- Apple newsroom: M3 (2023-10), M6 and M5 Ultra (2026-08), MacBook Pro / iMac
  with M6 (2026-10), iPad Air with M4 (2026-03), iPad mini (2026-10)
- Apple tech talk 111375 "Explore GPU advancements in M3 and A17 Pro"
