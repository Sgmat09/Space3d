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
ground plane with 400 cubes, `PostProcess::default().without_bloom()`: exposure, AgX, dither).
The criterion groups `frame/{cpu,gpu,throughput}/*` run Direct, then Hdr, one after the other: on
this shared machine a load change between the two corrupts their difference (one F-core review
run measured `frame/cpu/direct` at 477.8 µs, above Hdr's 89.1 µs). The F4 figure recorded here
first (throughput +25 µs, 113.6 vs 138.5 µs) came from such a sequential run and was not
reproduced. Deltas come from `frame/ab` (`cargo bench -p space3d-render --bench frame --
frame/ab`), which interleaves the two modes round by round and reports medians of the paired
deltas:

| `frame/ab` (medians, µs) | Direct | Hdr minimal | paired delta | gate |
|---|---|---|---|---|
| cpu (render stage on the CPU) | 67–75 | 86–97 | +17 to +23 | ≤ +20 µs CPU |
| throughput (frames back to back, a wait every 8th; GPU-bound) | 98–104 | 137–141 | +37 to +40 | ≤ +0.10 ms GPU |
| latency (render + GPU wait, loaded clocks) | 273–278 | 340–351 | +70 to +81 (incl. the CPU delta) | (upper bound) |
| latency, isolated frames 2 ms apart (lowest GPU clocks) | 814–902 | 1064–1297 | +254 to +294 | — (reference) |

Three runs at load average 4–6. The F-core review's own interleaved harness measured throughput
deltas of +44.6, +67.4 and +77.1 µs and a latency delta of +65 to +98 µs. Instructions retired on
the CPU grow by 109k–116k per frame, which is +10 to +17 µs of thread CPU time (median). That is
within the CPU gate at the median, but wall-clock runs reach +23 µs. The extra composite render
pass is most of it (about 30 wgpu-hal allocations and the Metal encoder setup).

**Which GPU regime the gate uses.** The gate is measured with the GPU at its loaded clocks, the
way the benches run: frames back to back. At 1080p the HDR path moves about 33 MB more per frame:
the scene texture is written at 8 B/px instead of D's 4, then the composite reads it back and
writes D. At this GPU's ~550 GB/s, that is about 60 µs uncompressed, the same order as the measured
delta. The loaded cost is mostly memory traffic, not ALU.

When frames are isolated (2–16 ms apart, e.g. a light scene at 60 Hz), the GPU idles at its lowest
performance state and every pass costs about 3× more. Direct's latency goes from ~280 µs to
~850 µs, and the Hdr delta grows to +240 to +290 µs. The review traced the composite pass alone at
258 µs there. By post setting, the delta at 2 ms apart measured +241 µs (default), +191 µs (dither
off), +172 µs (tonemapper `None`) and +126 µs (`PostProcess::passthrough()`).

This is not a gate failure: a scene heavy enough for frame time to matter keeps the GPU at its
loaded clocks. It is still the cost of an idle 60 Hz frame, and it is recorded for P-POST, which
owns `composite.wgsl` and its [`post.composite`] budget. Two ALU cuts are open there:

* On an sRGB `D`, the dither encodes, adds the noise and decodes again, and then the hardware
  encodes once more: 6 `pow` per pixel. Writing through a non-sRGB view of `D` (the
  `S3D_POST_OUT = 1` path) needs 3.
* The two per-frame seed hashes can be hashed once on the CPU.

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
still scene (since the round-1 review not even the dither seeds, which go up 64 frames at a
time).

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

### M1 review — upload ring, honest HDR bench, GPU gates

**Staging churn (fixed).** `Uploader::recall` trimmed idle staging chunks to twice the recent
per-frame peak and ignored the chunks still in flight. With frames queued on the GPU (an unpaced
`App::update` loop), every frame's chunk is still in flight when the next frame allocates, so a
still HDR scene — whose only upload is the composite's 16 bytes of per-frame dither and grain
seeds — created and dropped a 4 MiB `MAP_WRITE` chunk every few frames (0.5–1 ms of CPU and an
`Arc` each: 100–230 per 1200 frames in the review's trace). The ring now also keeps what the
queue depth needs (the busiest frame times the deepest queue of the last one to two 512-frame
windows, plus one), so it grows to the depth once and then creates nothing; the chunks of a
one-off stall are released within two windows. `RenderStats::staging_chunks_created` counts the
chunks created per frame. `tests/alloc_frame.rs` renders a still Hdr scene the way `App::update`
does (the frame counter advancing, a GPU wait every 8th frame): 7 chunks created in 1200 frames
(all while the queue first deepens), 148 allocations per frame (the seed upload's copy and
re-map inside wgpu; 106 when nothing is uploaded), bound 165.

**The frame bench now uploads the seeds.** `benches/frame.rs` advanced nothing between frames,
so its Hdr frames uploaded no seeds and hid both the churn and the upload. Its `render` helper now
advances `Time::frame` like `App::update`. `frame/ab` at load average 9–12 (this machine was
slower than at F4: Direct CPU 90–98 µs instead of 67–75):

| `frame/ab` (medians, µs) | Direct | Hdr minimal | paired delta | same, seeds not uploaded | gate |
|---|---|---|---|---|---|
| cpu | 89.7–98.1 | 118.3–128.5 | +24.2 to +33.2 | +21.5, +21.6 | ≤ +20 µs CPU |
| throughput | 124.5–130.2 | 182.4–184.4 | +50.3 to +58.1 | +31.9, +35.5 | ≤ +0.10 ms GPU |
| latency | 295–306 | 395–400 | +84 to +100 | +74 | (upper bound) |

The GPU gate holds with the upload (the 16-byte copy costs about 20 µs of GPU throughput in a
frame that uploads nothing else; in a frame where anything moves it joins that frame's copies).
The CPU gate is at its limit: +21.5 µs without the upload on this slower machine state, about
+4 µs more with it (an earlier run, where Direct measured 66 µs, gave +16.0 µs with it). The composite's extra render pass in
wgpu-hal is most of it (see F3 / F4). Open: uploading the seeds for a window of frames at once
and selecting them with a dynamic offset would remove the per-frame copy.

**GPU gates (run alone on an idle Dev GPU; not part of `cargo test --workspace` or CI).**
Hosted CI runners have software or paravirtual GPUs, so these are manual Dev-machine gates; each
retries a disturbed run (best of 3):

```sh
cargo test -p space3d-render --lib bloom_gate -- --ignored --nocapture
cargo test -p space3d-particles --lib gpu_100k_within_budget -- --ignored --nocapture
```

| gate | measured at the M1 review (GPU shared: 50–60 % device utilization from other apps) | budget |
|---|---|---|
| bloom 6 mips @1080p | 0.188–0.265 ms (one of two invocations passed; 0.13–0.16 ms on a quiet GPU) | ≤ 0.20 ms |
| particles 100k: sim + compaction, nothing else on the GPU | 0.044 ms (in frames that also draw the 100k billboards: 0.084 ms, reported) | ≤ 0.06 ms |
| particles 100k: draw into the HDR scene texture | 0.350 ms (into Direct's 8-bit target: 0.362 ms, reported) | ≤ 0.40 ms |

**Bloom CPU.** Bloom's render passes cost about 15 µs of wgpu/Metal overhead each (the
renderer's own work per pass allocates nothing): 0.15–0.17 ms of thread CPU per post group at
1080p and above (11 passes; +1.26 M to +1.38 M instructions per frame over the composite alone,
measured in an `App::update` loop by the round-1 review; an earlier estimate here said 0.11 ms),
about 3× the spec's Battle-scene line "post ≤ 0.05 ms". **That line is not met at M1.** Smaller
rects have fewer levels (9 passes at 720p, 5 at 180p). The planned fix is a compute chain (one
compute pass of dependent dispatches on class A backends; the render-pass chain kept for
WebGL2), after M1.

### M1 review, round 1 — frame pacing, gates at M1 close

**Frames in flight (fixed).** Nothing paced `render_frame` without a window: in an `App::update`
/ `step_ticks` loop (the engine's test pattern, headless batch renders) recording ran ahead of
the GPU without bound (Metal queues up to 4096 command buffers), every queued frame kept its
command memory, and the staging ring grew with the queue. `render_frame` now keeps at most three
frames in flight: after submitting a frame it waits for the one submitted three frames earlier
(native only; the browser paces its frames). With a window, presentation already paces frames
and the wait returns at once. The review's harness (1920×1080, 2400 unpaced `App::update`
frames, then 1200 paced ones; release; load 4–10), before → after:

| scene | staging chunks, before / after | footprint before (frame 300 → 2400) | footprint after | µs per frame, frames 300–2400, before / after |
|---|---|---|---|---|
| Direct, still | 1 / 1 | 882 → 1105 MB | 504 → 507 MB | — |
| Hdr minimal, still | 137 (still growing) / 4 | 924 → 2276 MB | 550 MB flat | — |
| Hdr + bloom, one cube moving | 138 / 4 | 1709 → 6435 MB | 696 MB flat | 492 / 316 |
| bloom + GPU particles 100k | 122 / 4 | 2034 → 6000 MB | 745 MB flat | 568 / 469 |
| bloom + CPU particles 16k | 10 / 5 | 886 → 865 MB | 685 MB flat | 487 / 472 |

Throughput does not suffer: three frames in flight keep the GPU busy. `tests/alloc_frame.rs`
"hdr moving, unpaced" renders 1500 frames of 400 moving cubes without waiting: no staging chunk
is created after the first 100 frames (5 were without the pacing), 148 allocations per frame
(bound 167). (The harness figures above predate the seed window below.)

**Post seeds, a window at a time.** The composite's dither and animated-grain seeds changed every
frame and were the only upload of a still Hdr scene: a 16-byte staging copy per post group and
frame (about 3–4 µs of CPU, and 20–40 µs of GPU throughput for the copy in an otherwise idle
frame). They now go up 64 frames at a time — one aligned uniform slot per frame and post group,
selected with a dynamic offset (`SeedWindow`, post/mod.rs) — the seeds themselves unchanged
(spec §4.3; the goldens are bit-exact). A still scene uploads nothing in 63 of 64 frames
(`tests/alloc_frame.rs`: 18 uploading frames in 1184).

**Allocations of the 1080p bloom chain.** `tests/alloc_post.rs` measures bloom at 1920×1080 again
(the 6-level chain of 1080p, 11 bloom passes; 1440p and 4K add the resample pass): 332
allocations per frame, pinned at +10 %, besides the 3-level chain at 320×180 (234 since the
resample pass of the M1 review's round 2; 211 before).

**Gates at M1 close** (Dev machine, 1080p headless):

| gate | measured | status |
|---|---|---|
| Hdr minimal over Direct, CPU ≤ +20 µs | `frame/ab`, round 1, with the seed window: +13.2, +14.9, +14.9, +15.3, +16.6 µs (load 6–7); the same build still copying the seeds every frame: +13.8 to +18.4 µs (load 5–6); the review: +19.3 to +31.8 µs, 4 of 5 runs over (load 7–22) | met on a quiet machine with little margin; heavy load pushes it over. The composite's extra render pass is most of it |
| Hdr minimal over Direct, GPU ≤ +0.10 ms | +31.9 to +37.8 µs throughput with the seed window (+55.6 to +59.9 µs with the per-frame seed copy) | met |
| bloom 6 mips @1080p ≤ 0.20 ms | 0.150–0.159 ms, 3 of 3 invocations (device utilization ≈ 40 %); the review: 0.222–0.261 ms, 0 of 3, with another app keeping the GPU 49–57 % busy | met on an idle GPU; a shared GPU pushes it over |
| Battle CPU "post ≤ 0.05 ms" | bloom alone 0.15–0.17 ms of thread CPU per post group | **not met** (compute chain after M1) |

In real frames bloom costs more GPU than the isolated chain the gate times: `hdr_bloom` minus
`hdr_minimal` throughput measured +244 µs at load 7.6 and +384 µs at load 40 in the review
(the chain plus the composite's bloom reads, against a GPU shared with other work).

**Particles.** Emitters moved after the tick get a render-time bridge (particles.md "Time
model"), built on the CPU on both backends: about 25 ns per bridged trail glow and 45 ns per
exhaust particle on one thread — 25–30 µs for 20 bolts at 3200 units/s moved a frame after the
tick, 0.10–0.12 ms for a Battle fleet of 300 exhausts and 20 bolts (2260 particles; 0.15 ms
before the M1 review's round 2 made the spawn math cheaper) — and, on the GPU backend, one
direct draw per layer and an upload per frame (`tests/alloc.rs`: 307 allocations per frame,
was 304). Moved in `Stage::Update`, as SpaceCorps2027 moves ships and bolts, that fleet costs
the particles node about 0.29 ms of render-thread CPU per frame against 0.14 ms moved in
`FixedUpdate` (1080p, HDR + bloom; 0.35–0.44 ms before), so the Battle scene's "particles
prepare ≤ 0.1 ms" line is missed about 3× until the bridges move to the GPU (particles.md,
"Bridges are not free").

### P-MAT PM1 / PM2 — material maps, blend modes, glTF import

Dev machine as above; measured while other builds kept the machine busy (load averages 12–39),
so every figure is an interleaved A/B against a pre-materials build (the `instancing` bench
binary of `graphics` at 62a28d8) run in the same minutes.

| gate / measurement | measured | status |
|---|---|---|
| ECS 100k static PBR entities ≤ 2.5 ms (`materials/ecs_pbr/100k`: 4 meshes × 8 materials with ORM, normal and emissive maps and a UV transform, render stage, camera looking away) | 2.23 / 2.35 / 2.34 ms | met |
| Direct mode unchanged, `extract_upload/ecs_static_mixed/100k` (map-less materials) | 2.13 / 2.21 / 2.22 ms against 2.10–2.27 ms for the pre-materials binary in the same rounds | within noise (the first version of the material key cost +0.8 ms: the per-entity fast path now compares the material field by field without branches, and the cache holds materials, so only real misses build a key) |
| `materials/transparent_sort/4096` (one 4096-instance alpha batch sorted back to front per frame, camera moving; whole frame) | 0.38–0.41 ms per frame | recorded (the sort is a u64 `sort_unstable` of depth keys; no allocation) |
| steady-state allocations with every PM1/PM2 feature (`tests/alloc_materials.rs`) | 100 per frame on `CpuDirect`, 195 on the GPU culling paths, constant; 0 in the renderer; live bytes +0 over 90 frames | bound 110 / 215 |
| static scene with maps (`tests/materials.rs`) | 0 bytes uploaded, 0 textures created once warm | met |

Not measured yet: the GPU cost of a full screen of mapped pixels (the gate "all maps + IBL +
PCF9 ≤ +0.20 ms" needs PM4/PM5; the maps are sampled behind uniform branches, so map-less
materials run the original instruction stream).
