# space3d-particles — architecture

User documentation: [`docs/graphics/particles.md`](../../docs/graphics/particles.md). Normative
design: [`docs/design/graphics.md`](../../docs/design/graphics.md) §2.5.

```
ParticleEmitter (ECS) ──tick_system (FixedUpdate)──► runtime ──TickLog──► render node (render_frame)
                          follow Transform/Parent        instances            prepare: replay ticks
                          schedule emission              entries (rings)                 GPU: simulate + emit kernels
                          finish / orphan / clear        pool allocator                  CPU: cpu::simulate / emit
                          events + FinishAction          emitter paths                 instances: GPU mark + scatter
                          budget of the backend          log (trimmed to W)                       CPU pack
                                                                                       cull, sort, upload (staging ring)
                                                                              draw:    one strip per draw item
                                                                                       (indirect on the GPU backend)
```

| module | role |
|---|---|
| `lib.rs` | `ParticlesPlugin`, `ParticleSettings` (`for_quality`: the calibrated tiers), `tick_system` (events and finish actions) |
| `components.rs` | `ParticleEmitter`, `EffectSpawn`, `SpawnEffect`, `ParticleAttractor`, events |
| `effect.rs`, `curve.rs` | the serde data model (layers, shapes, forces, curves, sprites) |
| `registry.rs` | `ParticleEffects` (handles, versions), `builtin` (RON in `assets/particles/`) |
| `runtime/mod.rs` | instances per emitter, entries (layer instances: ring `base..base+capacity`, spawn index, accumulator, compiled `LayerSim`), ribbon tracks, light flashes (`update_lights`), finish/orphan/detach, prewarm, hot reload; the LOD (`CameraView`: distance and off screen from the primary camera and the renderer's aspect), ring levels (`ring_level`, restarts when a looping emitter outgrows its level), the visible-first start of new emitters, retries of refused layers |
| `runtime/spawn.rs` | one tick of one layer instance: a 32.32 rate accumulator (time and distance rates) with exact crossing times, bursts, the throttle's rate response |
| `runtime/ribbon.rs` | ribbon tracks: a ring of points (position, birth, distance along, tint) per ribbon layer instance, appended on the tick |
| `runtime/log.rs` | `TickLog`: spawn records, emitter frames, clears, world attractors per tick; trimming |
| `runtime/layer.rs` | `LayerSim` (plain `Copy` params), capacity (`capacity_at` a quality level) and life-window bounds |
| `runtime/alloc.rs` | first-fit range allocator over the pool |
| `runtime/path.rs` | an emitter's recent path (8 buckets over the life window): the depth-sort center of world-space draws on both backends, the GPU backend's conservative bounds |
| `cpu.rs` | the CPU backend: `Particle` (48 B), `simulate` / `emit`, the pack into `ParticleInstance` (40 B); sub-tick quantization shared with the GPU |
| `gpu/mod.rs` | the GPU backend: pool, tables, emitter frames, spawn records, tick uniforms; the per-tick `simulate` / `emit` dispatches, the `mark` / `scatter` compaction, draw planning (jobs, groups, records, conservative bounds) |
| `gpu/tables.rs` | GPU layouts: entry table (24 words), jobs, spawn records, emitter frames, tick and frame uniforms |
| `gpu/readback.rs` | `ParticleStats::alive` from the draw arguments, 1–3 frames late (never waits) |
| `gpu/sort.rs` | the per-view bitonic sort of `ByDistance` groups (keys, dispatch plan, the sorted instance buffer), skipped when nothing changed |
| `testing.rs` | hidden test hooks: blocking pool readback (parity), pretend storage limit (budgets), the Battle scene's particle load (`battle`: calibration, perf, the Battle story) |
| `rng.rs`, `pack.rs` | PCG streams (bit-identical to WGSL), f16 and octahedral packing |
| `render/mod.rs` | `ParticlesNode` (`RenderNode`): slots (Transparent, Distortion), prepare / prepare_view / prewarm / draw, pipelines per (pass, sprite / strip / distortion, texture variant), the per-view draw lists (color, then distortion) and sorted draws (CPU-sorted copies, GPU sort jobs), GPU state |
| `render/layers.rs` | render layers (sprite, ribbon and beam layers): draw parameters, textures and the curve LUT rows; distortion and sorted flags |
| `render/strips.rs` | the frame's ribbons (tracks + a head at the render pose) and beams (render-time end points, jitter, fade, flicker, end glows) as one `StripPoint` stream, CPU-built on both backends |
| `render/textures.rs` | the procedural built-in textures (generated once per process at effect registration) and the texture bind groups |
| `shaders/particles.wgsl` | declarations shared by the sprite and strip shaders (group 2, noise, the pixel-footprint widening) |
| `shaders/sprite.wgsl` | the sprite vertex / fragment shader: analytic shapes, textures and flipbooks, the minimum footprint |
| `shaders/distortion.wgsl` | distortion sprites (after `sprite.wgsl` and `DISTORTION_WGSL`): offsets by shape (ring, vortex, texture, heat haze) through the inverse Jacobian, depth fade and foreground guard |
| `shaders/sort.wgsl` | the GPU sort's kernels: `sort_local` (512-key blocks in workgroup memory: the initial sort, the merges, the gather) and `sort_global` (one step across blocks) |
| `shaders/strip.wgsl` | ribbons and beams: a segment per instance from two bindings of the point stream, shared side vectors (watertight), tail clipping, round caps |
| `shaders/gpu_common.wgsl`, `simulate.wgsl`, `compact.wgsl` | the compute kernels: `simulate`, `emit` (per tick), `mark`, `scatter` (per frame) |

Invariants:

* **Determinism.** The pool is a pure function of the logged ticks: slot = `base + index mod
  capacity`, attributes = `rnd(particle_seed(entry seed, index), stream)`, no cross-particle
  reads. **Backend parity**: the GPU kernels mirror the Rust simulation; RNG, slots and sub-tick
  times (1/16384 tick) are integers, lifetimes and delays use one `fma` on both sides, deaths are
  `age >= life` of exactly computed values, so the alive sets, slots, ages and lifetimes are
  identical (`tests/parity.rs`). Within a tick a ring keeps only its last `capacity` spawns (the
  GPU writes each slot once). Compaction offsets come from integer sums, never from the order of
  atomics. Entry *indices* may differ between a replayed and a trimmed log (their reuse waits until
  no unreplayed tick references them); draws are ordered by the entry's allocation serial, which
  does not.
* **Catch-up window.** A layer instance's particles are all dead `window_ticks(life + delay, dt)`
  ticks after its last emission; the log keeps the longest such window (+1) when unconsumed and
  sets `reset`, and replaying it from an empty pool is exact (tested).
* **No steady-state allocation.** Slabs with free lists, cleared vectors; growth is counted in
  `ParticleStats::cpu_reallocations`; `tests/alloc.rs` counts heap allocations.
* **One encoder.** Uploads go through `PrepareContext::upload` (the renderer's staging ring);
  only the curve LUT uses `Queue::write_texture`, when the effects change. The GPU backend's
  per-tick emitter frames are copied between that tick's passes. The runtime resource moves out
  of `Resources` (`mem::take`, no allocation) for the node's `prepare`.
* **Strips.** Ribbon histories are CPU state appended on the tick (never replayed), beams are
  rebuilt from the render poses: both are identical on the two backends and for any render
  cadence (in tests, α = 0). Light flashes are `PointLight` child entities spawned and despawned
  by the tick system.
* **Dispatch budget.** 4 per frame with one tick (`simulate`, `emit`, `mark`, `scatter`), 2 more
  per extra tick, plus the sort's per (view, `ByDistance` group) — 1 up to 512 particles, 36 at
  65 536 — when the compaction or the view changed; kernels use `@workgroup_size(64)` and ≤ 4
  storage buffers.
* **Sorting.** Keys are `(order-preserving view depth, instance index)`: unique, so the CPU sort
  and the GPU's bitonic network give the same order whatever the execution order.
* **LOD.** Everything the LOD reads comes from ECS transforms, the camera component and the
  renderer's size: deterministic. Ring levels are powers of two below the quality, so a moving
  emitter restarts rarely (two levels of hysteresis down); restarts drain like hot reloads.
