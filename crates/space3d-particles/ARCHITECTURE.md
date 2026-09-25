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
| `lib.rs` | `ParticlesPlugin`, `ParticleSettings`, `tick_system` (events and finish actions) |
| `components.rs` | `ParticleEmitter`, `EffectSpawn`, `SpawnEffect`, `ParticleAttractor`, events |
| `effect.rs`, `curve.rs` | the serde data model (layers, shapes, forces, curves, sprites) |
| `registry.rs` | `ParticleEffects` (handles, versions), `builtin` (RON in `assets/particles/`) |
| `runtime/mod.rs` | instances per emitter, entries (layer instances: ring `base..base+capacity`, spawn index, accumulators, compiled `LayerSim`), finish/orphan/detach, prewarm, hot reload |
| `runtime/spawn.rs` | one tick of one layer instance: 32.32 rate accumulators, rate per distance, bursts |
| `runtime/log.rs` | `TickLog`: spawn records, emitter frames, clears, world attractors per tick; trimming |
| `runtime/layer.rs` | `LayerSim` (plain `Copy` params), capacity and life-window bounds |
| `runtime/alloc.rs` | first-fit range allocator over the pool |
| `runtime/path.rs` | an emitter's recent path (8 buckets over the life window): the depth-sort center of world-space draws on both backends, the GPU backend's conservative bounds |
| `cpu.rs` | the CPU backend: `Particle` (48 B), `simulate` / `emit`, the pack into `ParticleInstance` (40 B); sub-tick quantization shared with the GPU |
| `gpu/mod.rs` | the GPU backend: pool, tables, emitter frames, spawn records, tick uniforms; the per-tick `simulate` / `emit` dispatches, the `mark` / `scatter` compaction, draw planning (jobs, groups, records, conservative bounds) |
| `gpu/tables.rs` | GPU layouts: entry table (24 words), jobs, spawn records, emitter frames, tick and frame uniforms |
| `gpu/readback.rs` | `ParticleStats::alive` from the draw arguments, 1–3 frames late (never waits) |
| `testing.rs` | hidden test hooks: blocking pool readback (parity), pretend storage limit (budgets) |
| `rng.rs`, `pack.rs` | PCG streams (bit-identical to WGSL), f16 and octahedral packing |
| `render/mod.rs` | `ParticlesNode` (`RenderNode`): slots, prepare / prepare_view / prewarm / draw, GPU state |
| `render/layers.rs` | render layers: draw parameters and the curve LUT per sprite layer |
| `shaders/sprite.wgsl` | the sprite vertex / fragment shader (composed with the frame's WGSL modules) |
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
* **Dispatch budget.** 4 per frame with one tick (`simulate`, `emit`, `mark`, `scatter`), 2 more
  per extra tick; kernels use `@workgroup_size(64)` and ≤ 4 storage buffers.
