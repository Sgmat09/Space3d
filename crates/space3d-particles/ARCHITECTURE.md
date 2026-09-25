# space3d-particles — architecture

User documentation: [`docs/graphics/particles.md`](../../docs/graphics/particles.md). Normative
design: [`docs/design/graphics.md`](../../docs/design/graphics.md) §2.5.

```
ParticleEmitter (ECS) ──tick_system (FixedUpdate)──► runtime ──TickLog──► render node (render_frame)
                          follow Transform/Parent        instances            prepare: replay ticks (CPU sim)
                          schedule emission              entries (rings)               pack instances, cull, sort
                          finish / orphan / clear        pool allocator                upload (staging ring)
                          events + FinishAction          log (trimmed to W)   draw:    one strip per draw item
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
| `cpu.rs` | the CPU backend: `Particle` (48 B), `simulate` / `emit`, the pack into `ParticleInstance` (40 B) |
| `rng.rs`, `pack.rs` | PCG streams (bit-identical to WGSL), f16 and octahedral packing |
| `render/mod.rs` | `ParticlesNode` (`RenderNode`): slots, prepare / prepare_view / prewarm / draw, GPU state |
| `render/layers.rs` | render layers: draw parameters and the curve LUT per sprite layer |
| `shaders/sprite.wgsl` | the sprite vertex / fragment shader (composed with the frame's WGSL modules) |

Invariants:

* **Determinism.** The pool is a pure function of the logged ticks: slot = `base + index mod
  capacity`, attributes = `rnd(particle_seed(entry seed, index), stream)`, no cross-particle
  reads. Entry *indices* may differ between a replayed and a trimmed log (their reuse waits until
  no unreplayed tick references them); draws are ordered by the entry's allocation serial, which
  does not.
* **Catch-up window.** A layer instance's particles are all dead `window_ticks(life + delay, dt)`
  ticks after its last emission; the log keeps the longest such window (+1) when unconsumed and
  sets `reset`, and replaying it from an empty pool is exact (tested).
* **No steady-state allocation.** Slabs with free lists, cleared vectors; growth is counted in
  `ParticleStats::cpu_reallocations`; `tests/alloc.rs` counts heap allocations.
* **One encoder.** Uploads go through `PrepareContext::upload` (the renderer's staging ring);
  only the curve LUT uses `Queue::write_texture`, when the effects change.
