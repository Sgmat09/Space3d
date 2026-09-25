# Performance

Space3d's rules for staying fast are simple: **never block the frame**, **draw many things with
few draw calls**, and **upload only what changed**. This page explains the mechanisms and how to
measure them. The renderer internals are documented in
[crates/space3d-render/ARCHITECTURE.md](../crates/space3d-render/ARCHITECTURE.md).

## Never block the frame

* File, network and database I/O return `space3d_tasks::Task`s (tokio natively,
  `wasm-bindgen-futures` on the web). Systems `poll()` them or register callbacks with
  `AsyncQueue::spawn_then` / `track`, which run on the main thread in `PreUpdate`.
* CPU-heavy work (mesh generation, pathfinding, decoding) goes to the compute pool with
  `spawn_compute` / `AsyncQueue::compute_then`.
* The asset server reads bytes on the I/O runtime, decodes on the compute pool and inserts the
  result in `PreUpdate`; SQLite runs on its own worker thread.
* `Task::block()` exists for tools and tests only — never call it from a system.

`examples/async_loading` streams 12 textures and a model over HTTP with 0.2–1.6 s of artificial
latency each, builds a 24k-triangle mesh on the compute pool, and reports the longest frame
while all of that is in flight. In a debug build on an M4 Max the 14 assets arrive over ~1.4 s
while ~155 frames are rendered and the longest frame stays around 17–22 ms (texture uploads and
first-use pipeline creation) — no frame ever waits for a download or a decode.

## Draw many things with few draw calls

**Automatic instancing.** Every entity with `Mesh3d + Material + GlobalTransform` is sorted by
(pipeline, material, mesh); each run becomes one instanced draw. Equal materials are interned,
so 1,000 entities with equal-but-separate `Material` values are still one draw.

**Explicit instance buffers** for 100k–1M+ objects without per-entity ECS cost:

```rust
let buffer = ctx.resources.expect_mut::<InstanceBuffers>().insert(
    InstanceBuffer::from_vec(instances /* Vec<InstancePacked> */, BufferUsage::Dynamic),
);
ctx.world.spawn((InstancedMesh { mesh, material, instances: buffer },));
```

| instance type | size | contents |
|---|---|---|
| `InstanceTransform` | 64 B | model matrix |
| `InstanceTransformColor` | 80 B | matrix + color |
| `InstanceCompact` | 32 B | position, uniform scale, f32 quaternion |
| `InstancePacked` | 32 B | position, scale, snorm16 quaternion, unorm8 color, texture layer |

Any `Pod` struct with an `InstanceLayout` works; the shader is generated for its layout.

**GPU-driven culling.** On adapters with compute + indirect draws, a compute pass frustum-culls
instances (64 per workgroup, compacted inside the workgroup) and writes indirect draw arguments;
batches become one `multi_draw_indexed_indirect(_count)` per pipeline run. The chosen path is in
`RenderStats::draw_path`:

| path | used on | culling |
|---|---|---|
| `GpuMultiDrawCount` | Vulkan, DX12 | compute, empty batches skipped by the GPU |
| `GpuMultiDraw` | Metal, WebGPU with `indirect-first-instance` | compute |
| `GpuIndirect` | other compute-capable adapters | compute |
| `CpuDirect` | WebGL2, GLES, downlevel | CPU sphere/AABB tests |

**One mesh arena.** All meshes share one vertex buffer (36-byte vertices) and one index buffer,
so multi-draws cover many meshes without rebinding.

## Upload only what changed

* **Staging belt.** All buffer uploads are copied straight into mapped staging memory (4 MiB
  chunks) that is recycled once the GPU is done — no staging allocations in the steady state.
  Copies of 1 MiB or more are split across worker threads.
* **Dirty ranges for user buffers.** Every mutating `InstanceBuffer` accessor (`set`,
  `get_mut`, `range_mut`, `push`, …) records a range; adjacent ranges within 16 instances merge,
  at most 64 ranges are kept, and more than 50 % dirty becomes one full copy. `Static` buffers
  upload once; `Stream` buffers upload everything every frame without bookkeeping.
* **Shadow diffing for renderer buffers.** ECS instances, per-view data, materials and indirect
  argument templates keep a CPU shadow; only differing bytes are uploaded. **A static scene with
  a still camera uploads 0 bytes per frame; moving one entity uploads one 64-byte matrix.**
* **No per-frame reallocations.** Buffers grow 1.5× (meshes 2×) and are reused.

### `instancing_million`

```sh
cargo run -p instancing_million --release                 # 1,000,000 cubes, animated band
cargo run -p instancing_million --release -- --static     # no edits: 0 bytes uploaded per frame
cargo run -p instancing_million --release -- --count 250000 --band 65536
```

Measured on an Apple M4 Max (Metal, `GpuMultiDraw` path, release build):

| run | instances | triangles | draw calls | uploaded / frame | frame rate |
|---|---|---|---|---|---|
| default (animated band) | 1,000,000 | 12,000,000 | 1 (indirect) | 512 KiB | 120 fps (vsync-limited) |
| `--static` | 1,000,000 | 12,000,000 | 1 (indirect) | 0 KiB | 120 fps (vsync-limited) |

1,000,000 `InstancePacked` cubes are 32 MB (30.5 MiB) of instance data. Each frame the example
rewrites a band of 16,384 instances — **512 KiB uploaded instead of 30.5 MiB** — and the window
title (plus stdout every 120 frames) shows FPS, draw path, draw calls and bytes uploaded. `C`
toggles the CPU path, `F` frustum culling, `Space` pauses the wave; F3 shows the engine debug
overlay.

## Measuring

* `RenderStats` resource: draw calls (direct / indirect / multi), batches, instances, triangles,
  `bytes_uploaded`, `instance_bytes_uploaded`, buffer reallocations, compute dispatches, draw
  path, meshes/textures uploaded.
* **F3** debug overlay (FPS, frame-time graph, ticks, render stats) and **F4** profiler
  (per-system timings from `tracing` spans) with the `ui` feature.
* In tests: `assert_frame_budget`, `RenderBudget` (max draw calls / bytes uploaded / …),
  `assert_max_allocations` — see [testing.md](testing.md).
* Benchmarks: `space3d bench` (= `cargo bench`); the engine has criterion benches for the
  schedule (`space3d-core`), instancing (`space3d-render`), texture ops, SQLite inserts and
  physics.

## Build settings

Generated projects (and the engine workspace) use:

```toml
[profile.dev]
opt-level = 1                  # your code: fast to compile, not unbearably slow
[profile.dev.package."*"]
opt-level = 3                  # dependencies: fully optimized once
[profile.release]
lto = "thin"
codegen-units = 1
```

Release web builds additionally run `wasm-opt -Oz` when binaryen is installed.

## Graphics upgrade baselines

Measured values of the graphics upgrade's gates (`docs/design/graphics.md` §4.4). Each gate is
checked against the numbers recorded here; a gate is relaxed only with a new measurement and a
reason.

### F — foundation (F0 baseline, Direct mode)

Dev machine: Apple M4 Max, Metal, wgpu 30.0.1, Rust 1.98.1.

`benches/instancing.rs` (`cargo bench -p space3d-render --bench instancing -- --warm-up-time 1
--measurement-time 3`, bench profile; criterion median and range). The "Direct mode" gate is
±5 % of these.

| bench | time |
|---|---|
| `extract_upload/explicit/100k` | 918 µs (716 µs – 1.30 ms) |
| `extract_upload/explicit/1000k` | 2.16 ms (2.11 – 2.21 ms) |
| `extract_upload/ecs/100k` | 1.79 ms (1.76 – 1.84 ms) |
| `extract_upload/ecs_static_mixed/100k` | 2.18 ms (2.13 – 2.24 ms) |
| `mesh_arena/upload_1000` | 1.72 ms (1.70 – 1.74 ms) |

Steady-state heap allocations per frame (`tests/alloc.rs`, test profile; all of them inside
wgpu/wgpu-hal, 0 in the renderer; bounds 95 CPU / 215 GPU paths on Metal):

| path | allocations per frame (min / max / mean) |
|---|---|
| `CpuDirect` | 82 / 86 / 83.6 |
| `GpuIndirect`, `GpuMultiDraw` | 191 / 195 / 192.6 |
| `GpuMultiDraw` + 2 viewport cameras | 314.6 with or without viewport clears |

Live bytes grow by 0 over 120 frames on every path; `cpu_reallocations` and
`buffer_reallocations` are 0.

Engine storybook (`cargo run -p space3d-stories --example gallery`, dev profile; setup +
simulation + capture per story; the first story includes device creation):

| story | ms |
|---|---|
| Engine/Basics/Lit Cube | 158.5 |
| Engine/Materials/Material Grid | 16.2 |
| Engine/Materials/Single Material | 15.9 |
| Engine/Instancing/10k Cubes | 63.1 |
| Engine/Textures/Checker Plane | 67.6 |
| Engine/Lights/Point Lights | 16.8 |
| Engine/Camera/Projection | 16.4 |
| Engine/Transforms/Hierarchy | 16.1 |
| Engine/Physics/Falling Bodies | 16.9 |

The engine goldens (`crates/space3d-stories/tests/__screenshots__/Engine/`) were already exact
for the merged renderer at F0 (no re-bless needed): they are the baseline of every
"bit-identical" claim. `cargo test -p space3d-stories --test storybook -- --ignored
engine_goldens_are_exact` checks them with tolerance 0.

### F3 / F4 — output tail, `GpuView`, HDR frame modes

`GpuView` grew to its frozen 1312 bytes (1536-byte stride); forward modules no longer depend on the
output encoding (an override). Direct mode is unchanged: the engine goldens are bit-exact, and the
steady-state allocations of `tests/alloc.rs` are identical to F0 (82–86 CPU path, 191–195 GPU
paths, 314.6 with two viewport cameras).

`benches/instancing.rs` after F4 (same command as F0; the machine was shared with other builds,
load average 6–18, so the ranges are wide): `explicit/100k` 581 µs, `explicit/1000k` 1.77 ms,
`ecs/100k` 1.69 ms, `ecs_static_mixed/100k` 2.68 ms (2.14 – 3.87 ms), `mesh_arena/upload_1000`
2.10 ms (1.88 – 2.45 ms). The medians of the first three are below F0; the last two overlap F0
within their noise ranges.

Gate "Hdr minimal (composite only) over Direct" (`benches/frame.rs`, 1080p headless, a lit
ground plane with 400 cubes; `cargo bench -p space3d-render --bench frame -- --warm-up-time 3
--measurement-time 8`):

| bench | Direct | Hdr minimal | delta | gate |
|---|---|---|---|---|
| `frame/cpu/*` (render stage on the CPU) | 58.9–63.6 µs | 70.6–78.4 µs | +10 to +15 µs | ≤ +20 µs |
| `frame/throughput/*` (frames back to back; GPU-bound) | 113.6 µs | 138.5 µs | +25 µs (incl. the CPU delta) | ≤ +0.10 ms GPU |
| `frame/gpu/*` (one frame, CPU + GPU latency) | 304 µs | 373 µs | +69 µs | (upper bound) |

The per-frame GPU latency (`frame/gpu`) is the noisiest under load (runs between 0.3 and 1.2 ms
were seen); the throughput numbers and the quiet run above are the recorded values. GPU
timestamps per pass arrive with `GpuTimings` (F8).

Allocations per frame by mode (`tests/alloc_frame.rs`: 400 moving lit cubes, CPU draw path,
320×180; all inside wgpu/wgpu-hal; bounds +10 %):

| mode | allocations per frame | bound |
|---|---|---|
| Direct | 76 | 84 |
| Hdr (post camera) | 106 | 117 |
| Ldr fallback (`no_float_targets`) | 106 | 117 |
| Ldr passthrough (render scale 0.75) | 106 | 117 |

The one extra render pass (the composite) costs 30 wgpu-hal allocations on Metal; the renderer
itself allocates nothing, creates no textures after the first frame and uploads nothing for a
still scene (only the 16-byte dither seed of each post group when frames advance).

### F5 / F6 — render nodes, scene depth, overlay stages

Direct mode is unchanged: the engine goldens are bit-exact, and `tests/alloc.rs` still measures
82–86 (CPU path) / 191–195 (GPU paths) / 314.6 (two viewport cameras) allocations per frame. The
built-in skybox and distortion-mesh nodes are asked for work every frame (`prepare` once,
`prepare_view` per view) and report none; overlay stages are read every frame. None of it
allocates.

Allocations per frame (`tests/alloc_frame.rs`, same scene as F4):

| mode | allocations per frame | bound |
|---|---|---|
| Direct, Hdr, Ldr fallback, passthrough | 76 / 106 / 106 / 106 (unchanged) | 84 / 117 |
| nodes: an Opaque and a Transparent slot in the scene pass, a depth-reading slot in a SceneRead pass | 119 | 132 |

The SceneRead pass (one render pass) accounts for about 30 of the 43 extra; `cpu_reallocations`,
`buffer_reallocations` and `textures_created` are 0 and live bytes do not grow.

`benches/frame.rs` `frame/cpu/direct`, alternating the F4 build and this one on the same machine
(load average 5–13, so single runs vary by up to 3×): the quietest pair measured 60.9 µs (F4) and
60.5 µs (F5/F6); `frame/throughput/direct` 112.0 µs (F4 recorded 113.6 µs). The node and
overlay-stage bookkeeping is below the measurement noise.

### F-core review — Direct-mode CPU on a loaded machine

A review A/B of `render_frame` (main at eb77fef vs `graphics`, same harness, 1080p headless,
Direct mode, medians of 120 frames) saw `explicit/200k` on the CPU path at 192 µs vs 206 µs.
Wall-clock medians are not usable on this shared machine: the same binary measured 198 µs and
45 µs in consecutive runs (load average 20–110; the process lands on efficiency cores or not).
Instructions retired per frame (`/usr/bin/time -l`, slope between 1000 and 3000 frames, so setup
cancels; includes the Metal driver and the GPU wait) do not depend on the load:

| scene (CPU path) | main | graphics | delta |
|---|---|---|---|
| 200k explicit instances, 1 batch | 662 k | 657 k | −0.7 % |
| 10k ECS cubes, 7 batches | 5.30 M | 5.18 M | −2.2 % |
| empty (camera only) | 548 k | 559 k | +2.0 % (+11 k: plan, nodes, overlay stages, post bookkeeping) |

Direct mode costs the same work per frame as before the upgrade, plus a fixed ~11 k
instructions (a few µs). The ±5 % gate on `benches/instancing.rs` still needs its wall-clock
re-run on a quiet machine before Phase I records final values.
