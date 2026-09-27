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

In real frames bloom costs more GPU than the isolated chain the gate times: Hdr + bloom minus Hdr
minimal throughput measured +244 µs at load 7.6 and +384 µs at load 40 in the review's own
harness (the chain plus the composite's bloom reads, against a GPU shared with other work).
`frame/ab` has the `hdr_bloom` mode since M1.1 (below) to reproduce it from the tree.

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

### F7 / F8 and M1.1 — depth views, GPU timings, bloom memory

`cargo bench -p space3d-render --bench frame -- frame/ab` (1080p headless, release) now
interleaves four modes — Direct, Hdr minimal, `hdr_bloom` (`PostProcess::default()`: the composite
and the default bloom) and `depth_view` (Direct plus one 2048² depth view of the scene's 400
cubes and ground plane, injected with `space3d_render::testing`) — and prints each mode's GPU
timings per pass (`FrameSettings::gpu_timing`) and the bloom chain's GPU time at other sizes.
Two runs at load average 29–33 (other builds on the machine); medians in µs, paired deltas
against Direct in brackets:

| `frame/ab` | Direct | Hdr minimal | Hdr + bloom | depth view |
|---|---|---|---|---|
| cpu, load 29 | 78.0 | 100.0 (+19.8) | 271.1 (+179.5) | 110.0 (+36.7) |
| cpu, load 33 | 207.1 | 243.4 (+39.0) | 697.8 (+489.3) | 227.1 (+45.9) |
| throughput, load 29 | 130.8 | 163.8 (+33.1) | 420.3 (+293.9) | 172.8 (+43.3) |
| latency, load 29 | 293.8 | 377.8 (+76.3) | 701.7 (+400.2) | 379.0 (+76.9) |
| GPU timings, load 29 (ms) | cull 0.013, scene 0.048 | + post.composite 0.099 | + post.bloom 0.216, post.composite 0.068 | + shadow 0.024 |

**Bloom CPU, one figure.** Bloom adds 0.15–0.18 ms of render-thread CPU per post group at 1080p
over the composite alone (the M1 record 0.15–0.17 ms; +0.16 ms in the load-29 run above; the
load-33 run, +0.45 ms, shows how far very heavy load pushes it). `docs/graphics/post.md` and
`docs/graphics/frame.md` quote this figure (post.md said about 0.10 ms, frame.md 0.10–0.17).
The Battle line "post ≤ 0.05 ms" stays not met (compute chain).

**Bloom by size** (`post.bloom` in frames, medians of 48, load ≈ 30): 720p 0.196 ms, 1080p
0.227 ms, 1440p 0.254 ms, 4K 0.607 ms, the `GraphicsQuality::Low` chain at the T3 rect
(1440 × 810, 4 levels, 270 cap) 0.122 ms. The half-rect prefilter of the M1 review's round 2
costs GPU time wherever the half rect is larger than level 0 (the round-3 review's interleaved
sweeps, best-of chain times: +4 % at 1440p, +25 % at the Low rect, +40 % at 4K, nothing at 1080p);
post.md's claim that the chain's time was unchanged held at 1080p only and is corrected. Capping
the prefilter target at twice level 0, the review's suggestion, changes nothing at these sizes
(the half rect is at most twice level 0 there); a cheaper prefilter kernel would change the
halo, so the cost is recorded, not traded.

**Bloom memory** (M1.1 review finding: `target_bytes` counted 2.8 MB). `tests/footprint.rs`
(`cargo test -p space3d-render --release --test footprint -- --ignored --nocapture`; macOS; each
scene in its own process, 1080p, footprint after 600 frames):

| scene | one frame in flight | three frames in flight | `target_bytes` | render passes |
|---|---|---|---|---|
| Direct | 490.0 MB | — | 0 | 0 |
| Hdr minimal | 512.2 MB | 540.8 MB | 24.9 MB | 1 |
| Hdr + bloom | 571.7 MB | 688.4 MB | 27.6 MB | 12 |
| depth view 2048² | 513.4 MB | — | 0 (the map: 16.8 MB) | 1 |
| raw wgpu, 1 / 11 empty passes into 32 × 32 | 453.7 / 481.6 MB | — | — | 1 / 11 |

Each Metal render pass holds about 2.8 MB of driver memory per frame in flight, independent of
its attachment's size: bloom's chain holds +59.5 MB paced and +147.6 MB unpaced against +2.7 MB
of textures, as the review measured (56–61 / 151 MB). Counted now: the docs (frame.md "Cost",
post.md) state `target_bytes + render_passes × 2.8 MB × frames in flight`, the spec's §4.4 VRAM
lines are amended, and the footprint test fails above 80 MB (paced) or 200 MB (unpaced) of bloom.
Not reduced: every chain level is its own render pass (a level is read while the next is
written), and the renderer's frames in flight follow the window (two to three); the reduction
is the planned compute chain (one compute pass of dependent dispatches, about one pass's
memory), the same follow-up as bloom's CPU.

**Depth views** (F7): one 2048² map adds 0.024 ms of GPU (`shadow`), +37 to +46 µs of CPU per
frame (culling of the casters, one pass, their draws) and 23 MB of process memory (the map and
its pass). `tests/alloc_frame.rs` "depth_views": 103 allocations per frame on Metal (Direct 76 +
the depth pass), bound 113; the renderer's own work allocates nothing, a still scene uploads
nothing (`tests/depth_views.rs`), and GPU paths add no culling dispatch.

**GPU timings** (F8): `tests/alloc_frame.rs` "timings": 100 allocations per frame on Metal (Direct
76 + timestamp writes, the deferred resolve and the two callbacks wgpu boxes per timed frame),
bound 110; off, timing costs nothing.

### PO5 / PO6 — distortion, the all-post gate

Recorded by the post area (P-POST, M2; `benches/post.rs`, `docs/graphics/post.md` "Hardening").
Dev machine, release, 1080p headless, load average 11–13 from other builds.

| gate (spec §4.4) | measured | result |
|---|---|---|
| all post on (AE compute, bloom, FXAA, CA, grain, grading, LUT, distortion) ≤ 0.60 ms | `post/gate`: 0.387 ms, the paired throughput delta against Direct (frames back to back, interleaved rounds; measured like "Hdr minimal over Direct"); the whole frame takes 0.49 ms of throughput | met |

`post/ab` (throughput per frame over Direct, µs): composite +45.9, bloom +236.8, bloom +
distortion (two shockwaves and a haze) +264.0, every effect +387.9. The per-pass timestamps of the
same frames sum to 1.0–1.3 ms: on Apple GPUs a pass's scope includes its overlap with its
neighbours (bloom's scope reads 0.17 ms alone and 0.30–0.39 ms inside the full chain), so they are
reference figures, not the gate. Distortion: one render pass (the Distortion pass's own scope 0.05
ms at 1080p, 0.52 ms at 4K), 2.1 MB of buffer at half resolution, 33 allocations per frame on
Metal (`tests/alloc_post.rs`: 267 with bloom; every effect 351). Render-stage CPU
(`post/cpu/*`): composite 0.12 ms, bloom 0.26 ms, every effect 0.25 ms — post's CPU line of the
Battle budget (≤ 0.05 ms) is still not met; bloom's 11 render passes are most of it.

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

### P-MAT PM3 / PM4 — skybox, image-based lighting

Dev machine (M4 Max, Metal), release, 1080p headless, frames back to back; load averages 7–33 from
other lanes' builds and GPU tests. Commands:
`cargo test -p space3d-render --release --test environment -- --ignored --nocapture ibl_bake_gate`
and `... forward_full_screen_gate`.

| gate / measurement | measured | status |
|---|---|---|
| IBL bake ≤ 3 ms one-time (spec §4.4 T1; 1024² LDR faces → 256² specular + 32² irradiance; `ibl.bake` GPU scope, medians of 5 cubemaps) — default path: compute dispatches | upload (mip chain, 10 dispatches) 0.18–0.32 ms + bake (7 dispatches) 0.52–0.95 ms = **0.69–1.26 ms** over 5 runs | met |
| the same on the fragment path (WebGL2's; `no_compute` on Metal): a render pass per face and mip | upload 60 passes 1.0–1.2 ms + bake 42 passes 2.2–7.1 ms = **3.4–8.2 ms** | recorded (T3 budget ≤ 50 ms total, ≤ 8 ms per frame with `bake_passes_per_frame`) |
| per-pass cost on Metal | a 64² cubemap's 66 near-empty passes: 4.2–10.8 ms at load 30, 0.54 ms as 7 compute dispatches | the reason for the compute path |
| forward full-screen lit pixels, all maps + IBL (+ PCF9 with PM5) vs today's shader ≤ +0.20 ms (a plane filling 1080p, sun + 4 point lights; legacy Classic material without maps or environment vs base color, normal, ORM and emissive maps at 1024², Standard shading and image-based lighting) | `scene` 0.169 → 0.257 ms; median paired difference **+0.088 ms** (3 runs identical; an earlier unpaired run under GPU contention: +0.15 / +0.22) | met without PCF9 (PM5 adds its taps) |
| steady-state allocations with PM1–PM4 in use (`tests/alloc_materials.rs`: + skybox, environment, Standard shading) | 114 per frame on `CpuDirect`, 208 on the GPU culling paths, constant; 0 in the renderer; live bytes +0 over 90 frames | bound 125 / 230 (PM2's 100 / 195 + the skybox draw and the 7-binding view bind group) |
| still camera with a skybox and an environment (`environment_steady_state`) | 0 bytes uploaded, 0 textures created, 0 bake steps, identical frames | met |
| engine goldens (`engine_goldens_are_exact`) | exact: the image-based term and the Standard multiple-scattering factor are behind a uniform branch / an override | met |

Memory (not a gate): a radiance cube is `faces² × 6 × 4/3` texels at 4 B (sRGB) or 8 B
(`Rgba16Float`): a 1024² LDR sky 33.6 MB, the stories' 512² HDR nebula 16.8 MB; an environment at
the defaults 4.2 MB (256² × 6 faces × 6 mips + 32² irradiance, `Rgba16Float`); the BRDF LUT 16 KiB.

### P-MAT PM5 / PM6 — directional shadow, hardening

Dev machine (M4 Max, Metal), release, frames back to back; load averages 4–9. Commands:
`cargo test -p space3d-render --release --test shadows -- --ignored --nocapture shadow_map_gate`,
`cargo test -p space3d-render --release --test environment -- --ignored --nocapture forward_full_screen_gate`,
`cargo bench -p space3d-render --bench materials`.

| gate / measurement | measured | status |
|---|---|---|
| shadow 2048², 100k instanced casters ≤ 0.30 ms (`shadow` GPU scope; 100k cube entities — one instanced batch — on a 400 × 400 field, all inside the map, `ShadowFit::top_down`, PCF9; median of 120 frames) | **0.276 ms** (min 0.27, 3 runs) on `GpuMultiDraw` | met |
| the same cubes from one explicit `InstanceBuffer<InstanceCompact>` (recorded) | 0.66 ms; its camera `scene` pass is slower by the same factor (0.94 vs 0.63 ms), with a matrix layout too (0.63 ms), on `CpuDirect` more so (1.46 ms) | recorded at PM5; **not reproduced** at the M2 merge (explicit = ECS within noise, see "M2 merge" below): most likely measured under other GPU load |
| forward full-screen lit pixels, all maps + IBL + PCF9 vs today's shader ≤ +0.20 ms (as in PM4, plus the sun's 2048² shadow with the 9-tap filter on every pixel) | `scene` 0.169 → 0.288 ms; median paired difference **+0.119 ms** (2 runs identical) | met (the first PCF version, a loop over dynamically indexed vectors, cost +0.24 ms; the unrolled kernels +0.03 ms over PM4) |
| ECS 100k static PBR entities ≤ 2.5 ms (`materials/ecs_pbr/100k`) | 2.27 ms | met |
| shadow view CPU at 100k ECS (`materials/shadow_view/ecs{,_off}/100k`: the render stage with and without the shadow, camera seeing 4.7k of 100k cubes) | `CpuDirect` 1.59 → 2.00 ms (**+0.40 ms**: the depth view culls every caster on the CPU like a camera); best path (`GpuMultiDraw`) 1.64 → 1.74 ms (**+0.10 ms**, no extra dispatch) | design budget +0.4 ms: met |
| the same with one explicit 100k-instance buffer (`…/explicit{,_off}/100k`, `CpuDirect`) | 0.31 → 0.48 ms (+0.17 ms) | recorded |
| steady-state allocations with PM1–PM5 in use (`tests/alloc_materials.rs`: + the sun's shadow, refitted every frame to the moving camera, alpha-masked casters) | 151 per frame on `CpuDirect`, 266 on the GPU culling paths, constant; live bytes +0 over 90 frames. With the shadow's `strength` at 0 (light picked, pipelines specialized, no depth view) exactly PM4's 114 / 208: the lighting's per-frame work allocates nothing, the depth pass and its draws are wgpu's 37 / 58 | bound 165 / 290 |
| still scene with a shadow (`static_scene_uploads_nothing_with_shadows`, MSAA 4) | 0 bytes uploaded, 0 textures created, 0 reallocations, identical frames; a new `map_size` creates one texture | met |
| engine goldens (`engine_goldens_are_exact`) | exact (without a casting light `S3D_SHADOW_TAPS` is 0 and the shadow code is compiled out) | met |

Memory: a shadow map is `map_size²` × 4 B (`Depth32Float`): 16.8 MB at 2048², 4.2 MB at 1024²,
plus one render pass per frame (≈ 2.8 MB of Metal driver memory per frame in flight, see "F7 /
F8 and M1.1").

### M2 merge — gates re-measured on the merged tree (post + materials + particles)

Foundation maintainer, `graphics` after merging graphics-post, graphics-materials and
graphics-particles (commits 8ae4b22, 4c54e68, f31dfe1). Dev machine (M4 Max, Metal), release,
each gate run alone, but the GPU was shared: another app kept it at 72–78 % device utilization
(`ioreg` "Device Utilization %") and the load average was 5–8.

| gate (spec §4.4) | command | measured | result |
|---|---|---|---|
| shadow 2048², 100k instanced casters ≤ 0.30 ms | `--test shadows -- --ignored shadow_map_gate` | ECS 0.277 / 0.277 / 0.278 ms (3 runs, `GpuMultiDraw`) | met |
| forward full screen, all maps + IBL + PCF9 ≤ +0.20 ms | `--test environment -- --ignored forward_full_screen_gate` | legacy 0.181 → 0.298 ms, paired +0.121 ms | met |
| IBL bake ≤ 3 ms (1024² faces) | `--test environment -- --ignored ibl_bake_gate` | compute 0.98 ms (upload 0.32 + bake 0.66); fragment path 5.5 ms (recorded); HDR skies gated since the M2 final review (below) | met |
| all post ≤ 0.60 ms | `cargo bench -p space3d-render --bench post -- post/gate` | 0.405 ms over Direct (throughput) | met |
| particles 100k: sim + compaction ≤ 0.06 ms, draw ≤ 0.40 ms | `-p space3d-particles --lib -- --ignored gpu_100k_within_budget` | 0.048 ms; 0.382 ms into the HDR scene texture | met |
| bloom 6 levels at 1080p ≤ 0.20 ms | `--lib -- --ignored bloom_gate_1080p` | best 0.193 ms in the first invocation; 0.211 and 0.232 ms (best of 3 runs each) in the next two | soft: the test's own note says 30–70 % device utilization from other apps reads 0.19–0.30 ms; the lane measured 0.15–0.17 ms on an idle GPU |

**Explicit instance buffers vs ECS** (PM5 recorded explicit buffers as 2.4× slower in every
pass): not reproduced. The same 100k cubes from one `InstanceBuffer<InstanceCompact>`: shadow
pass 0.275 ms vs ECS 0.277 ms on `GpuMultiDraw`, 0.324 vs 0.275 ms on `CpuDirect`; the camera's
`scene` pass 0.46–0.51 ms for both, on both paths (3 runs each).

Allocations per frame: every allocation test (`space3d-render` `alloc`, `alloc_frame`,
`alloc_materials`, `alloc_post`; `space3d-particles` `alloc`; `space3d-core` `alloc_transforms`)
passes its lane's bound on the merged tree, on Metal and on ANGLE (GL).


### Phase I — the Battle reference scene and the quality presets

Integration builder, `graphics` after the M2 merges. The reference scene of spec §4.4 is
`space3d_stories::showcase` (`spawn_battle` + `dress_battle`): 300 ships of three procedural
hulls with base color, packed ORM, normal and emissive maps (one material per faction: 9
instanced batches), 60 asteroids, the nebula sky as `Skybox` and `EnvironmentLight`, one sun with
a `DirectionalShadow` (`ShadowFit::top_down`) and a shadowless fill light (M2 final review), the particle load of
`space3d_particles::testing::battle` (900 engine exhausts, 64 ribbons, 16 beams, 30 explosions a
second with their light flashes — 41 point lights —, hits, shield ripples, warps' shockwaves),
`battle_look()` (AgX punchy, +0.5 EV, bloom, vignette) and two 256² render-target ship previews
(4× MSAA, no post). The egui HUD of windowed runs is not part of the reference (the stories and
the example show the previews through a HUD camera; the perf harness leaves it out: it costs
0.08 ms of CPU).

Measured with `cargo test --release -p space3d-stories --test perf_graphics -- --ignored
--nocapture` (`battle_dev_gates`; `cargo bench -p space3d-stories --bench graphics --
graphics/report` prints the same), 1920 × 1080 headless, Apple M4 Max, Metal, content of
1024² hull maps and 1024² sky faces, 240 warm-up frames, medians of 60 frames. The machine was
shared: another session ran the game (≈ 120 % CPU) and an iOS simulator app (≈ 100 % CPU and its
Metal host), load average 6.8–7.4. Three runs, every tier, High passing both gates each time:

| tier | GPU frame (frozen-frame throughput + particle sim) | CPU `render_frame` (back to back / after an idle GPU) | render passes | targets | particles alive |
|---|---|---|---|---|---|
| **High** (gates) | **1.49 ms** (1.27 + 0.22) | **1.45 ms** / 1.39–1.43 ms | 19 | 133.5 MB | 101 800 |
| Medium | 1.39 ms (1.21 + 0.18) | 1.38–1.41 ms | 16 | 129.3 MB | 61 000 |
| Low | 0.71–0.75 ms (CPU particles) | 0.77–0.78 ms | 13 | 25.3 MB | 5 300 |
| Ultra | 1.48–1.50 ms (1.26 + 0.22) | 1.43–1.47 ms | 18 | 129.3 MB | 101 800 |

Gates (spec §4.4, Dev): **Battle total GPU ≤ 3.0 ms — met (1.49 ms)**; **CPU `render_frame` ≤
1.5 ms — met at 1.45 ms, a 3 % margin** on the shared machine (the first measurements, with the
previews post-processed and the HUD camera, read 1.85 ms: see below); **frame and post targets
≤ 220 MB (T1/T2) — 133.5 MB** (`RenderStats::target_bytes`; asserted by `battle_steady_state`, which
also checks no CPU reallocations and no textures created over 30 steady frames, 0 GPU errors).
Draws: 12 built-in (hulls, rocks, sky; 4 of them shadow) + 114 particle draws; 1 post group.

How the GPU frame is measured: the frozen frame (no ticks) rendered back to back is the
frame's throughput — the GPU's time when the GPU is the slower side, else the CPU's — and the
particle simulation passes that only ticking frames record (`particles.sim`, `.compact`,
`.sort` timestamps, 0.22 ms) are added. Pass scopes of a ticking frame (they overlap on Apple
GPUs, so their sum, 4.0 ms, overstates the frame): cull 0.023, shadow 0.046, scene 0.227, depth
resolve 0.231, scene_read 0.693, distortion 0.584, post.bloom 0.748, post.composite 0.756,
particles.sort 0.127 ms.

Where the CPU goes (High, `sample` of the release build, `render_frame` = 100 %):
* **58 % wgpu's command encoding** (`CommandEncoder::finish`: wgpu 30 records passes and encodes
  them to Metal at `finish`): beginning the 19 render passes (27 % of it), **wgpu's indirect-draw
  validation** (`VALIDATION_INDIRECT_CALL`, on in release by default; 25 % of it — the particles
  draw with 100+ indirect draws), draws and binds. Every render pass costs about 15–25 µs of CPU:
  the first version of the scene post-processed the two previews (two more bloom chains and
  composites: 41 passes) and measured 1.85 ms; the reference scene's previews are one pass each
  (spec §4.4: "RT 2"), 19 passes in all. With `WGPU_VALIDATION_INDIRECT_CALL=0` the same frame
  takes 1.33–1.35 ms (not a default: it guards GPU-written indirect arguments).
* **35 % the particle node's `prepare`** (≈ 0.5 ms; spec budget 0.1 ms for the GPU backend):
  the tick replay's uploads (a ticking frame uploads **2.3 MB**, 2.3 MB of it particles: mostly the
  CPU-built ribbon and beam strips, 40 B a point, rebuilt every frame), emitter transforms of
  the 900 moving engines, the entry tables, and bounds. The particles lane's own gates (100k
  sim + compaction ≤ 0.06 ms GPU, draws ≤ 0.40 ms) are GPU gates and are met.
* The rest (extraction, planning, lighting, post planning, the scene passes' own recording):
  under 0.1 ms.

Levers, if the CPU gate needs margin on slower machines: incremental strip uploads (only the
points that moved) and a GPU strip builder in the particles lane; batching the particles' indirect
draws (one multi-draw per pipeline); turning off wgpu's indirect validation for engine-written
arguments (a foundation decision); fewer passes (the compute bloom chain the post lane lists).

Visual check: every Showcase golden (`Engine/Showcase/{SpaceCorps Look, Battle, WebGL2 SpaceCorps
Look, WebGL2 Battle}`) and the example's headless 1280 × 720 PNG (Medium, recommended for the M4
Max as an integrated GPU) were inspected.

### M2 review — fixes and the gates they touch

Graphics maintainer, `graphics` after Phase I, answering the M2 reviewers and the game's M1
feedback (game#1–#15).

**Direct mode, `benches/instancing.rs` (spec §4.4, ±5 % of F0).** The review measured
`extract_upload/ecs/100k` (100k *moving* ECS entities, `CpuDirect`) at 1.91–2.04 ms against F0's
1.66–1.76 ms in interleaved same-machine runs, and bisected the jump to the materials merge
(4c54e68); explicit instance buffers and the static paths are within noise. Cause: the
`Material` component the spec extends (§2.7, +12 fields) grew from 64 to **160 bytes**
(`size_of::<Material>()`), and extraction reads each entity's material in full every frame to
intern it (the branch-free `same_material` compares every field against the last material;
any field may differ): about 10 MB more memory traffic per frame at 100k, 2.5 ns per entity.
The intern path is already a one-compare fast path; there is no change detection to skip the
read. The gate is relaxed for that path (spec §4.4 amendment: within +20 % of F0, recorded
here); 100k+ moving instances belong in `InstanceBuffers`.

**VRAM (spec §4.4, T1/T2 ≤ 220 MB).** `RenderStats::target_bytes` did not count the shadow
map although `battle_steady_state` said so; it does now, and `RenderStats::lighting_bytes`
counts the lighting area's cubemaps and baked environments. The Battle at High, 1080p:
**146.0 MB** of frame, post and shadow targets (the 2048² shadow map 16.8 MB; the bloom chain
lost its 1280 × 720 resampled level, below) and **83.9 MB** of sky and IBL — the 1024² HDR sky is
`Rgba16Float` with mips (67 MB), twice the spec line's 33 MB for a 4-byte format. 230 MB of
textures plus the 23 MB particle pool: **not met, by the sky's format** (lever: HDR cubemaps in
`Rg11b10Ufloat` where renderable, deferred). Process memory adds the per-pass driver memory of
the M1.1 amendment (≈ 2.8 MB per render pass per frame in flight on Metal: the review measured
550 MB of frame-related `phys_footprint` at High, 240 MB of it post). `battle_steady_state`
now asserts the two parts (≤ 150 MB and ≤ 90 MB) instead of claiming the 220 MB line.

**Battle CPU sub-budgets (spec §4.4: particles `prepare` ≤ 0.1 ms, post ≤ 0.05 ms).** Not met:
the review measured `render_frame` 1.42–1.47 ms at High (the top-line 1.5 ms gate met) with
particles ≈ 1.05 ms of it (the node's `prepare` ≈ 0.5 ms and its 2.3 MB of strip uploads per
ticking frame) and post ≈ 0.38 ms (the bloom chain's render passes). Recorded as not met in the
spec; the levers — a GPU strip builder with incremental uploads, the compute bloom chain, no
wgpu indirect validation for engine-written arguments — are deferred. The Battle now has 18
render passes at High (19 before: the bloom resample is gone).

**Bloom chain at High vs Ultra (1080p).** High's 720 cap made level 0 1280 × 720, resampled
*up* from the 960 × 540 prefilter (a bigger target and a pass for no detail), so High cost
more than Ultra (19 passes / 133.5 MB against 18 / 129.3 MB). A level 0 is now never larger
than half the rect, and Ultra is 8 / 720: at 1080p High runs 7 levels and Ultra 8 at 960 × 540
(13 and 15 bloom passes), Ultra one wider level at every resolution. Small groups gain too:
a 320 × 192 preview's chain loses its resample pass.

**Small post groups (game#15).** `Bloom::min_rect_height` skips the chain for groups shorter
than it (UI previews: 240): four 320 × 192 previews with post next to a 1280 × 720 main view
were 47 render passes, 5 post groups; with bloom off on them 15 (post.md, "Small views").

**`ParticleStats::cpu_reallocations` in the steady state.** It flagged frames without any
allocation: the runtime compared `HashMap::capacity()` of its entity map, which moves with
tombstones as emitters come and go. The map now counts only a capacity beyond any seen before
(a real reallocation; `runtime::tests::emitter_churn_counts_no_reallocations`), and
`battle_steady_state` asserts 0 over 30 frames after the battle's load has built up (6 s; the
growth of pools and scratch buffers while explosions, missiles and smoke accumulate over the
first 5 s is warm-up).

### M2 review, round 1 — steady state off High, the budget without a renderer, VRAM, CA

Graphics maintainer, answering the second round of the M2 review.

**`ParticleStats::cpu_reallocations` off High on Metal.** `battle_steady_state` failed on ANGLE
(1 reallocation in the 30 measured frames), and long runs at Low and Medium on Metal flagged
reallocations 10–22 s in (Low: 7 of 1500 frames after a 10 s warm-up, Medium: 2). They were real:
the Battle's load is random (hits, bolts and missiles by tick hash), so it keeps setting small new
peaks, and the collections whose length the effect instances, layer instances or ribbon tracks
bound — the instance and entry free lists, the pool's free ranges, the finish list, the entity
map, the CPU simulation's per-instance and per-entry tables, the CPU sort's buffers — grew on
their own at each new peak of their own length long after the instance and entry tables had
stopped growing. They are now reserved with those tables (the entity map at most half full, so
its tombstones rehash it in place): the same probe (960 × 540, 600 warm-up ticks + 1500 frames)
flags nothing at Low, Medium and High on Metal and at High on ANGLE.
`battle_steady_state_low_and_medium` runs Low and Medium for 900 frames after ten seconds on
every backend.

**The budget with the renderer held aside (spec §4.3).** The tick sized the particle pool for the
GPU budget whenever no `Renderer` was in the world — `TestApp::render_on_demand` (every story),
the web runner's frames while its renderer is created — even with
`DownlevelOverride::webgl2()`, and the rings it reserved then stayed once the CPU backend took
over (the Battle at High with webgl2 on demand: 165 971 slots reserved against a budget of
16 384; the GL story runs simulated their CPU backend in a GPU-sized pool). The tick now reads
the device from the renderer, the `RendererDevice` a held-aside renderer leaves, or the last
device it saw; `no_compute` resolves to the CPU backend without a device; before any device the
budget is the smaller one. `tests/determinism.rs::the_budget_is_the_same_on_demand_and_every_tick`.

**VRAM (spec §4.4, T1/T2 ≤ 220 MB): met.** HDR radiance cubes are `Rg11b10Ufloat` where it
renders and filters (`GraphicsCaps::compact_hdr_format`), else `Rgba16Float`: the Battle at High,
1080p, has 146.0 MB of targets and shadow + 50.4 MB of sky (33.5 MB) and IBL = 196.4 MB of
textures, 219.4 MB with the 23 MB particle pool (230 + 23 before). The sky's pixels move by at
most 3–4 levels of 255 after tonemapping. GL keeps `Rgba16Float` (83.9 MB of sky and IBL).
Process memory keeps the per-render-pass driver memory of the M1.1 amendment on top.

**Chromatic aberration.** Round 0's spectral split ran up to 25 taps a pixel apart wherever the
split exceeded 1.5 px, most of the frame at typical intensities: the composite at 1080p (minimum
`post.composite` GPU time over about 160 frames per setting, interleaved) cost 0.098 ms at CA
0.03, 0.126 at 0.15, 0.150 at 0.25, 0.199 at 0.5 and 0.248 at 1.0. With taps at most 1.5 px apart
and at most 17: 0.098, 0.119, 0.137, 0.172, 0.202 (no CA: 0.091).

**Particle uniforms on WebGL2.** Chrome's WebGL2 on ANGLE / Metal reports a 32-byte uniform
offset alignment (256 natively): the particle node's 112-byte per-layer parameters, placed at
multiples of the alignment, overlapped and overran their buffer (a validation error on the
showcase's first WebGL2 frame in the browser). They are strided by their size rounded up to the
alignment now. The showcase's web page renders into its own canvas and takes `?webgl2` to test
that path in a browser with WebGPU.

**Battle gates after round 1** (release `battle_dev_gates`, three runs at a load average of about
20): High GPU frame 1.59 / 1.59 / 1.56 ms (gate 3.0), CPU `render_frame` 1.48 / 1.46 / 1.48 ms
(gate 1.5); Medium 1.49 / 1.48 / 1.45 GPU, 1.48 / 1.43 / 1.48 CPU; Low 0.74 / 0.75 / 0.71 GPU,
0.76 / 0.79 / 0.80 CPU. The 1.5 ms CPU line is met with little margin, as the review found: the
particles' `prepare` (≈ 1 ms with its strip uploads) and post (≈ 0.35 ms) sub-budgets stay not
met, their levers (a GPU strip builder with incremental uploads, the compute bloom chain)
deferred (spec §4.4 amendment). Round 1 adds no per-frame work: the companion reservations run
only when an instance or entry table grows, the velocity smoothing is a few multiplies per
emitter per tick.

### M2 final review — HDR sky bake, shadow fill, T3's particle budget

Graphics maintainer, answering the M2 final review. Dev machine, release; load averages 14–40
from other processes (a simulator app and a Python job at 100 % each), so pairs of runs are
interleaved with the previous commit (8b324fe) built alongside.

**IBL bake of HDR skies (spec §4.4: ≤ 3 ms one-time).** Round 1 made HDR radiance cubes
`Rg11b10Ufloat` for the VRAM line, but core WebGPU cannot write that format from compute, so
their conversion and mips fell back to a render pass per face and mip (66 for 1024² faces, after
a copy through a temporary `Rgba16Float` cube). The review's probe (the showcase's 1024² sky with
its environment, `ibl.bake` GPU scope, upload and bake in one frame) measured 9.6 ms at High, 74
steps, against 1.1–1.2 ms and 18 steps before round 1; `ibl_bake_gate` measured LDR faces only. Where
the adapter writes `Rg11b10Ufloat` from compute (`GraphicsCaps::compact_hdr_storage`, requested
through `Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES`: Apple GPUs, most Vulkan and D3D12
drivers) the compact cube is converted and mipmapped by one dispatch per mip again; elsewhere
with compute (WebGPU) HDR skies stay `Rgba16Float` (`GraphicsCaps::hdr_sky_format`: +33.5 MB for a
1024² sky rather than 66 render passes once; 8-bit skies are unaffected).

| measurement | before | after |
|---|---|---|
| the review's probe, showcase sky 1024², High (steps, `ibl.bake`) | 74 steps, 9.6–9.7 ms | 19 steps, 1.46 ms |
| the same at Low / Medium / Ultra | 8.5–8.9 / 9.0 / 9.9–10.4 ms | 0.42 / 0.55 / 1.92 ms |
| `ibl_bake_gate`, 1024² LDR faces (upload + bake, medians of 5) | 0.98 ms | 0.55, 1.14 ms (two runs) |
| `ibl_bake_gate`, 1024² HDR faces (`Rgba16Float` source, new) | — | 1.85, 1.82 ms (11 + 7 dispatches) |
| `ibl_bake_gate`, 4096 × 2048 HDR panorama (new) | — | 2.15, 1.95 ms (12 + 7) |
| fragment path (WebGL2's, recorded): LDR / HDR faces / HDR panorama | — | 4.0–8.3 / 8.8–9.4 / 7.4–9.6 ms |
| Battle VRAM at High, 1080p (`battle_steady_state`) | 196.4 MB | 196.4 MB (sky + IBL 50.4 MB) |

**Shadow fill.** The showcase lit shadowed sides with the nebula's irradiance (≈ 0.03) × 0.7 and
a 0.015 ambient: 1.6 % of a sunlit top's light on a side facing away from the sun, 3 % on a top in
a cast shadow — black after tonemapping. Diffuse IBL × 6 (`ENV_DIFFUSE`), a shadowless violet
`DirectionalLight` at 0.35 against the 2.2 sun (`spawn_fill`) and ambient 0.03 give 15 %, 17 % in
a cast shadow, 72 % on a side facing the sun (`showcase::tests::fill_keeps_shadowed_sides_readable`,
Metal and ANGLE alike). Cost, with the Look's and Battle's asteroids now 64 × 40 spheres with rock
detail maps: the Battle's GPU frame at High 1.52–1.55 ms against 1.46–1.48 ms for the previous
commit in four interleaved pairs (+0.04–0.08 ms: the fill in the forward loop, the shadow pass
0.049 → 0.071 ms); CPU `render_frame` 1.46–1.50 against 1.43–1.55 ms (unchanged; gate 1.5 ms,
met in every pair at load 14–20; at load 30–40 both commits read 1.54–1.57 ms).

**T3's particle budget.** The Battle at Low on the web (8 192, quality 0.125) held 5 s but not
20: the load keeps setting small new peaks, and at 8b324fe it refused 393 layer instances in 20 s
from 5.25 s in (seed 1), 307 from 3.1 s (seed 3), 562 from 1.1 s (seed 4), 4 from 16.2 s (seed 2)
and 1 185 at 21:9 from the first tick, peaks 6 171–6 780 slots — engine plumes and afterburners
among them. With the engines' sparks and heat haze left out below quality 0.3
(`ParticleLayer::min_quality`), their plumes `Normal` in a `Low` effect and an explosion's smoke,
embers and debris `Low` in a `High` one (`ParticleLayer::priority`), and quality 0.1 on the web:
no refusal in 20 s at 16:9 with any of the four seeds, peaks 4 481–5 187 slots (84 % of the
engines' 6 144 at most), every engine in view burning; at 21:9, 103 `Low` layer instances (engine
details, combat's leftovers) refused, no plume (`tests/battle.rs::battle_fits_the_web_budget_at_low`). The 5 s table in
[particles.md](graphics/particles.md) is re-measured with the ships on seven flight levels.

**Other gates on this tree.** `battle_gpu_cost` passes (particle draws 0.300 ms);
`battle_steady_state` and `battle_steady_state_low_and_medium`: no reallocations, no textures or
render objects created in the steady state.
