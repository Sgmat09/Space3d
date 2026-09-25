# Particles

Data-driven particle effects — explosions, hit sparks, shield ripples, warps, engine exhaust,
laser trails — simulated on the fixed tick and drawn by a render node that matches the frame's
HDR, tonemapping and scene depth in every mode. The normative design is
[`docs/design/graphics.md`](../design/graphics.md) §2.5 (with the area design it amends).

Status: milestones PA0 (runtime, CPU simulation, effect library), PA1 (CPU backend rendering,
prefabs) and PA2 (the GPU compute backend: 100k+ particles, budgets, parity with the CPU
backend) are in. Ribbons, beams, flipbooks and light flashes (PA3), distortion layers,
per-particle sorting and LOD calibration (PA4) follow; the API below does not change for them.

## Quick start

```rust
use space3d::prelude::*;
use space3d::particles::prelude::*; // never in the facade prelude: the game has its own names

app.add_plugin(ParticlesPlugin::default()); // next to RenderPlugin / DefaultPlugins

// One call per effect (an entity is spawned and despawned when the effect has played):
ctx.spawn_effect(builtin::EXPLOSION, EffectSpawn::at(pos).scale(2.0));
ctx.spawn_effect(builtin::HIT_SPARK, EffectSpawn::at(hit).facing(-bolt_dir).tint(laser_color));
ctx.spawn_effect(
    builtin::SHIELD_RIPPLE,
    EffectSpawn::at(ship_pos).facing(-bolt_dir).scale(shield_radius / 20.0).follow(ship),
);
ctx.spawn_effect(builtin::WARP_IN, EffectSpawn::at(portal));
ctx.spawn_effect(builtin::LASER_TRAIL, EffectSpawn::at(muzzle).follow(bolt).tint(tier_color));

// A looping emitter on a ship (a child entity), throttled every frame:
let exhaust = world.spawn((
    Transform::from_xyz(0.0, 0.0, 21.0), // behind the hull (ships face −Z)
    Parent(ship),
    ParticleEmitter::new(builtin::ENGINE_EXHAUST),
));
world.get::<&mut ParticleEmitter>(exhaust)?.rate_scale = throttle; // 0..2
```

`SpawnEffect` is implemented for `World`, `App` and `SystemCtx`. `EffectSpawn` builders:
`at`, `from_transform`, `facing(dir)` (local −Z along `dir`), `follow(entity)` (inserted as
`Parent`: the emitter moves with it), `tint`, `scale`, `velocity`, `seed`, `prewarm(secs)`,
`keep()` (do not despawn when finished; the default for `spawn_effect` is `Despawn`).

## Emitters

`ParticleEmitter` plays an effect at its entity's `Transform`, followed through the `Parent`
chain every tick (exact, not last frame's `GlobalTransform`):

| field | meaning |
|---|---|
| `playing` | `false` stops emitting; live particles play out (`stop()`; setting it back after the effect finished plays it again) |
| `rate_scale` | × continuous rates (throttle), clamped to each layer's `Spawn::max_rate_scale` |
| `scale` | sizes, shape extents, offsets, speeds^`speed_scale_exponent` (1 ≈ a 40-unit hull for the built-ins) |
| `tint` | linear RGBA × every `tinted` layer (rgb may exceed 1) |
| `velocity` | extra velocity for `inherit_velocity` (the measured motion is added) |
| `seed` | `None`: derived from the entity, the first tick and `ParticleSettings::seed` |
| `prewarm` | seconds simulated at once on first sight (ambient loops) |
| `finish` | `Keep`, `Despawn` or `RemoveEmitter` once a non-looping effect has fully played |
| `on_remove` | `Drain` (particles play out) or `Clear` when the component or entity goes away |
| `restart` | bump it (`restart()`) to start over; the old particles drain |

An emitter whose parent disappears is *orphaned*: it stops where it was, drains, then finishes
(a destroyed ship's exhaust lingers and fades). `ParticleEffectFinished { entity, effect }` is
sent when an effect has emitted everything and all its particles are gone, before its
`FinishAction`. `ParticleRuntime::state(entity)` reports `time`, `emitting`, `finished` and the
pool slots `reserved`.

## Authoring effects

An effect is data (`ParticleEffect`, serde, RON): sprite layers (ribbon and beam layers from
PA3). Every struct has defaults, so RON lists only what differs:

```ron
ParticleEffect(
    name: "sparks",
    duration: 0.1,
    layers: [(
        spawn: (bursts: [(count: (20, 30))]),
        shape: Sphere(radius: 2.0, inner: 1.0),
        speed: (80.0, 200.0), drag: 2.5,
        lifetime: (0.3, 0.6),
        size: 1.5,
        color_over_life: (keys: [(0.0, (5.0, 3.4, 1.4, 1.0)), (1.0, (1.6, 0.35, 0.04, 1.0))]),
        alpha_over_life: Preset(Linear),
        render: (shape: Streak(core: 0.6, taper: 1.0), facing: VelocityStretched(scale: 0.04, min: 1.0, max: 16.0)),
    )],
)
```

```rust
let handle = app.resources.expect_mut::<ParticleEffects>().add(ParticleEffect::from_ron(src)?)?;
// or in Rust:
let fx = ParticleEffect::new("sparks").layer(
    ParticleLayer::burst((20, 30), (0.3, 0.6)).speed((80.0, 200.0)).drag(2.5)
        .render(SpriteRender::additive(SpriteShape::Streak { core: 0.6, taper: 1.0 }).stretched(0.04)),
);
```

`ParticleEffects::set` replaces an effect (hot reload): layers that keep their capacity update in
place; a changed structure restarts running instances (their old particles drain).

### Layer reference

| group | fields |
|---|---|
| emission | `spawn: Spawn { rate, rate_per_distance, max_rate_scale, bursts: [Burst { time, count, cycles (0 = forever), interval }] }`, `start` (delay after the effect starts), `capacity` (override the ring size) |
| where | `shape`: `Point`, `Sphere { radius, inner }`, `Cone { angle, radius }` (around −Z), `Ring { radius, width, arc }` (XZ), `Box { half_extents }`, `Line { length }` (X); `offset`; `flatten` + `flatten_axis` (squash toward a plane: top-down effects) |
| motion | `direction`: `Shape`, `Fixed(v)`, `Random`, `Inward`, `Tangent`; `spread` (cone half-angle); `speed`; `speed_scale_exponent`; `inherit_velocity`; `drag` (1/s, exact exponential); `gravity`; `forces` (≤ 4: `Attractor`, `Vortex`, `Turbulence`); `world_forces` (mask of `ParticleAttractor` entities, ≤ 8 per tick); `space`: `World` (stay where emitted) or `Local` (move with the emitter) |
| life | `lifetime`, `particle_delay` (appear later, staggered bursts) |
| look | `size` + `size_over_life`, `aspect`, `rotation` + `angular_velocity`, `random_mirror`, `color_over_life` (linear HDR gradient), `start_color` (random between two), `alpha_over_life`, `coverage` + `coverage_over_life` (premultiplied), `intensity`, `flicker`, `tinted` |
| quality | `lod` (quality and distance LOD may reduce counts; bursts keep ≥ 1) |
| render | `SpriteRender { shape, blend, facing, soft_distance, near_fade, order, sort, target, max_screen_size }` |

Random ranges read `1.0` or `(0.5, 1.0)`. Curves: `Constant`, `Linear(a, b)`, `Keys([...])`,
`Smooth([...])` (Catmull-Rom) and `Preset(FadeCurve)` — the game's fades `Linear`, `Out`,
`Flash`, `InOut`, `Linger`, `Burn`, `Hold`. Curves and gradients are baked into a 128-sample LUT
the vertex shader reads, so the CPU never evaluates them per particle.

### Sprites

* **Shapes** (analytic, no textures, crisp at any size — the game's `additive.rs` kinds as
  data): `Glow { core, hardness }` (soft halo, hard disc, white-hot core), `Streak { core, taper }`
  (a capsule whose width and brightness taper toward its tail, with a hot head; pair it with
  `VelocityStretched`), `Ring { thickness, focus, fill }` (focus sharpens it toward the sprite's
  +X: shield arcs; the band sits inside the rim, which fades over 1.5 pixels),
  `Smoke { breakup, hot_core }` (four noise octaves), `Vortex { arms, twist }`. `Texture` and
  flipbooks arrive with PA3 (drawn as a soft glow until then, with a warning).
* **Size clamp**: a sprite is at most `ParticleSettings::max_screen_size` of the viewport high
  (default 1.0, a fill-rate guard); `SpriteRender::max_screen_size` (0 = the setting) raises or
  lowers it per layer — the warp vortices use 4.0, so their collapse keeps its size and timing
  at any zoom.
* **Blends**: one premultiplied equation (`One, OneMinusSrcAlpha`). `Additive` adds light
  (alpha 0), `Alpha` covers by its opacity, `Premultiplied` adds its light *and* covers by a
  separate coverage curve (fire turning into smoke). `ParticleBlend::from_blend_mode` maps the
  material `BlendMode`s; the results match the material equations.
* **Facing**: `Camera` (screen-aligned, rotated), `VelocityStretched { scale, min, max }`
  (`scale` seconds of motion, clamped to `min..max` × the size), `Plane { normal }` (in the layer's
  space: rings on the flight plane), `EmitterPlane` (the emitter's XZ plane at spawn, its X axis
  as the sprite's +X).
* **Order**: particles draw after the built-in transparent meshes, in one render node slot:
  `SpriteRender::order` first, then alpha/premultiplied before additive, then emitters back to
  front for sorted layers. `Material::render_order` orders meshes only (two independent order
  spaces; interleaving needs OIT, deferred).

### Soft particles and scene depth

With `ParticleSettings::soft_particles` (default on) the node's Transparent slot reads scene
depth, so it draws in the view's SceneRead pass (graphics spec §1.6) and `soft_distance` fades
sprites where they meet geometry. Per depth access of the pass:

| pass | how particles test and fade |
|---|---|
| 1×, read-only depth (Metal, Vulkan, DX12, WebGPU) | hardware depth test + `s3d_soft_fade` |
| MSAA, one view | the frame resolves depth (minimum of samples), then as 1× |
| no read-only depth (WebGL2 / GLES) | no depth attachment: a manual test (`s3d_scene_occluded`) + the fade |
| MSAA on GL, or several views on one MSAA target | inside the MSAA pass: hardware test only, no fade |

Particles behind geometry are hidden in every mode. `soft_particles: false` puts the slot in the
scene pass (hard test only, no extra pass).

### Colors in every frame mode

Particles call the frame's output tail (`s3d_output`) with the pass encoding, exposure and
tonemapper, so they look like meshes in every mode: linear HDR into the HDR scene texture (their
values above 1 feed bloom), tonemapped per fragment in the Ldr fallback, sRGB-encoded where the
target needs it. Direct mode has no tonemapper: there each sprite soft-clips with `1 − e^−x`
(`ParticleSettings::direct_soft_clip`, default on), the game's current look.

**Authoring for HDR.** The built-ins are calibrated for the HDR frame a game with `PostProcess`
renders (AgX + `Bloom::glow()`, threshold 1): what should glow is authored well above 1 (laser
trail 5× and head 10× the tint, explosion flash 5, sparks and embers up to 15, fireball 4.5), so
it blooms and reaches white, and colors keep low off-channels (a red tint of `(1, 0.1, 0.05)`,
shield blue `(0.12, 0.55, 2.8)`) so AgX desaturates them toward white only where they are
hottest. Plumes and trails are soft glows without cores or rims (`core` only on heads and
nozzles), so overlapping chains read as one beam or flame instead of beads. For more saturation
and contrast in effect-heavy scenes, `Tonemapper::AgXPunchy` (post.md) keeps saturated
emissives saturated. `Engine/Particles/HDR/*` shows every built-in through `PostProcess::default()`
(with a `tonemapper` knob); `Engine/Particles/Builtin/*` the same scenes in Direct mode.

## Built-in effects

`builtin::*` handles are reserved (registered when `ParticlesPlugin::builtins`); the data is RON
in `crates/space3d-particles/assets/particles/` (`builtin::SOURCES`), the game's recipes:

| handle | what | spawn |
|---|---|---|
| `EXPLOSION` | flash, afterglow, fireball billows (premultiplied), hot core, sparks, embers, two shockwave rings, smoke | `EffectSpawn::at(p).scale(hull / 40)` |
| `HIT_SPARK` | flash, sparks thrown along −Z, a ring, smoke; tinted | `.facing(-bolt_dir).tint(color)` |
| `SHIELD_RIPPLE` | focused arc toward −Z, faint bubble, surface flash, streaks; local-space | `.facing(-bolt_dir).scale(radius / 20).follow(ship)` |
| `WARP_IN` / `WARP_OUT` | vortex, streaks rushing in / out, flash, two rings | `EffectSpawn::at(portal)` |
| `ENGINE_EXHAUST` | looping plume along +Z, sparks, nozzle glow; `rate_scale` = throttle | emitter child behind the hull |
| `LASER_TRAIL` | looping white-hot head + a continuous beam laid per unit of distance | `.follow(bolt).tint(color)` (a saturated tint: `(1, 0.1, 0.05)`) |
| `LASER_BEAM` | reserved: beams arrive with PA3 (until then an emitter with it plays nothing and finishes at once, warned once) | |

An emitter whose handle has no effect data — a removed effect, `EffectHandle::INVALID`, or
`LASER_BEAM` before PA3 — plays nothing and finishes on its first tick (warned once per
handle): the `ParticleEffectFinished` event is sent and its `FinishAction` runs, so a
`spawn_effect` entity is despawned instead of staying forever.

Stories: `Engine/Particles/Builtin/*` (Direct mode), `Engine/Particles/HDR/*` (the same through
`PostProcess::default()`), `Engine/Particles/Features/*` and `Engine/Particles/Stress/100k` (tag
`perf`: 100 000 particles on the GPU backend) in the engine storybook (`space3d stories`, the
editor's storybook viewer, golden tests).

## Time model and determinism

* `tick_system` runs in `Stage::FixedUpdate` at `ParticlesPlugin::TICK_ORDER` (1000, through
  `App::add_system_ordered`): after every system added with `add_system` / `add_system_named`,
  whenever they were added, so it sees where gameplay moved each emitter this tick. (Run before
  the movement, a fast emitter's world-space layers would be laid one tick behind its
  local-space layers and meshes, which draw at the render-time pose: a bolt's head floating
  25 units ahead of its trail at 1500 units/s.) `ParticlesPlugin { tick_system: false, .. }`
  leaves the placement to you. It follows emitters, decides each tick's emission — exact 32.32 fixed-point rate accumulators (10/s at 60 Hz is
  exactly 100 particles in 600 ticks), bursts on the tick that contains them with their sub-tick
  time, emission spread over the emitter's motion within the tick — and records it in a tick log.
* The render node replays the pending ticks inside the render frame. Particle *k* of a layer
  instance lands in ring slot `base + k mod capacity`; its random attributes are a PCG hash of
  (layer instance seed, *k*) (bit-identical in Rust and WGSL). The simulated state is a pure
  function of the ticks, so `render_to_image` after `n` ticks shows exactly `n` ticks,
  rendering every tick equals rendering once at the end, and when nothing renders for a while
  the log keeps only the last catch-up window `W` (the longest particle life) and replays it
  from an empty pool with the identical result (`ParticleStats::catchup_resets`).
  `max_catchup_ticks` (1200) bounds one frame's replay.
* Rendering twice without a tick replays nothing and uploads nothing: identical images.
* Without a renderer effects still tick, finish and despawn (servers simply do not add the
  plugin; `TestApp::no_render` works).

## Backends, budgets, stats

* `ParticleBackend::Auto` (default) picks the GPU backend when the device has culling-class
  compute and indirect draws (`GraphicsCaps::compute`, masked by
  `DownlevelOverride::no_compute`), else the CPU backend. `Gpu` asks for it explicitly (and falls
  back to the CPU with a warning without compute); `Cpu` forces the CPU backend, the WebGL2 /
  GLES 3.0 path. Both run the same tick log, ring slots, RNG streams, 40-byte instance stream
  and render pipeline (the vertex shader reads no storage buffers): switching changes
  performance, not the look. A switch at runtime carries the CPU pool over to the GPU; the other
  way the live particles are dropped (the GPU pool is not read back).
* Budgets: `ParticleSettings::max_particles` (GPU: 262 144, 65 536 on Android and iOS, within
  the device's storage binding and buffer limits) and `max_particles_cpu` (16 384 native, 8 192
  on the web). The tick system uses the budget of the backend the renderer resolves to. Each
  layer instance reserves a ring sized from its rates, bursts and life (§5.1 of the area
  design); over the budget rings shrink to 1/8 before a layer instance is refused
  (`ParticleStats::{shrunk, refused}`, logged once per effect). `EffectPriority::Low` stops at
  75 % of the pool, `Normal` at 90 %, `High` may use it all.
* `ParticleStats` (resource, per rendered frame): backend, live effects and layer instances,
  pool capacity / reserved, alive particles (CPU: this frame; GPU: read back from the draw
  arguments 1–3 frames late, `alive_frame` says which), ticks replayed, catch-up resets,
  compute dispatches, draw calls, bytes uploaded, `cpu_reallocations` (0 in the steady state).
  The frame's `RenderStats` counts the GPU backend's compute passes (`compute_passes`, reported
  with `PrepareContext::count_compute_passes`) and dispatches (`node_dispatches`).

### The GPU backend

The pool lives in a storage buffer (48 bytes a particle) that grows 1.5× as rings are reserved
(the old contents are copied on the GPU); `prepare` records everything into the frame's encoder
before the view passes:

| step | when | work |
|---|---|---|
| uploads | changed | entry table (a layer instance's parameters, 384 B) when entries change; the pending ticks' spawn records and tick uniforms; the emitter frames that moved (per tick); the draws' jobs and records when they change |
| `simulate` | per tick | a thread per pool slot below the high-water mark: age, death (`age >= life`), gravity, drag, the layer's forces and world attractors, every substep |
| `emit` | per tick with spawns | a thread per new particle: ring slot `base + index mod capacity`, shape, direction, speed, lifetime and delay, sub-tick pre-aging |
| `mark` | per frame, when anything changed | a workgroup per 64 ring positions of a drawn layer instance: the mask of visible particles, its count, added to its block's total and its draw's `DrawIndirectArgs` |
| `scatter` | same | each visible particle writes its 40-byte instance at `draw start + earlier blocks + earlier records + bits below it` |

That is 4 dispatches for a frame with one tick and 2 more per extra tick (budget: 5 + 2), with
`@workgroup_size(64)` and at most 4 storage buffers per kernel (the GLES 3.1 / WebGPU minimum).
Draws are indirect (one per unsorted additive layer, one per sorted layer instance), bound at
their region of the instance buffer, so neither `BASE_VERTEX` nor `INDIRECT_FIRST_INSTANCE` is
needed. The frame's culling uses conservative bounds for GPU draws: the emitter's path over the
life window (kept by the tick system in 8 buckets) grown by the layer's reach (spawn shape,
initial speed with drag, gravity and forces over the life window, the largest sprite); layers
under world attractors are never culled.

**Parity with the CPU backend** (`tests/parity.rs`): the kernels mirror the Rust simulation
operation for operation. Integer parts (RNG, ring slots, sub-tick emission times, quantized to
1/16384 of a tick) are bit-identical, and the values that decide births and deaths are computed
exactly on both (lifetimes and delays with one fused multiply-add, ages as sums of the tick's
`dt`), so both backends hold the same particles in the same slots with identical ages and
lifetimes; positions and velocities agree within 1e-5 relative (1e-3 asserted) and images within
the golden tolerance. The draw order is the same too: instances oldest first per ring, draws
sorted by a center both backends compute from the emitter path.

**Determinism**: output positions come from prefix sums over bit masks and integer sums, never
from the order of atomics; rendering every tick, rendering once after `n` ticks and a catch-up
reset give identical images on either backend (`tests/determinism.rs`).

## Performance

Measured on the Dev machine (Apple M4 Max, Metal, wgpu 30.0.1); the GPU gates use GPU
timestamps with frames back to back (`gpu::tests::gpu_100k_within_budget`, the stress scene of
`Engine/Particles/Stress/100k` at 1920×1080). The GPU gate is ignored by default: the other GPU
tests of the lib binary run in parallel with it, and the hosted CI runners (software or
paravirtual GPUs) cannot measure Dev-machine budgets. Run it alone on an idle Dev machine
(`SPACE3D_PERF_BUDGET_SCALE` relaxes it elsewhere):

```sh
cargo test -p space3d-particles --lib gpu_100k_within_budget -- --ignored --nocapture
```


| item | measured | gate (graphics spec §4.4) |
|---|---|---|
| GPU: 100k alive, one tick of `simulate` + `emit` and the compaction (`mark` + `scatter`), nothing else on the GPU | 0.018 + 0.028 = **0.046 ms** | ≤ 0.06 ms |
| the same inside frames that also draw the 100k billboards (reported, not gated: see below) | 0.08–0.09 ms | — |
| GPU: draw 100k billboards into the HDR scene texture (8–16 px, 12 px average, spread over 1080p: 14.9 M fragments, ≈ 7× overdraw; "overdraw 2" cannot hold with 12 px sprites at 1080p, so this is the heavier reading) | **0.35 ms** | ≤ 0.40 ms |
| the same draws into Direct mode's 8-bit sRGB target (reported) | 0.37–0.44 ms idle, up to 0.8 ms with other GPU load | — |
| GPU backend CPU cost: `prepare` + draw recording, 500 moving layer instances, one tick | 0.14 ms idle machine (0.2–0.3 ms under load) | ≤ 0.1 ms (Battle scene, Phase I) |
| CPU backend: tick + simulate + pack 16 384 particles, one thread (test profile) | 0.28 ms | ≤ 0.35 ms |
| `tick_system`, 500 layer instances | 13 µs | ≤ 60 µs |
| allocations per frame, ~1000 particles, 80–90 draws (wgpu's bookkeeping; our code: 0) | CPU backend 183, GPU backend 304 | pinned +10 % (`tests/alloc.rs`) |

**Gate caveats, plainly.** The simulation gate is measured with the draws of the previous frame
out of the way. With the camera drawing the 100k billboards every frame, the two particle
passes overlap that draw (or wait for it before rewriting the instances it reads) and their
timestamps span 0.08–0.09 ms — over the 0.06 ms line; the test reports that figure as
`in frames that draw`. What a frame pays for the whole particle system (simulation, compaction
and the draws) is about 0.2 ms of throughput, within the two gates' sum (0.46 ms). The draw
gate is measured into the HDR scene texture, where the Battle scene draws particles (they feed
bloom); into Direct mode's 8-bit target the same draws are fill-bound and swing with other GPU
load (reported).
Isolated frames let the GPU drop its clocks (about 3× slower). Each figure is the median of 40
frames, best of up to 3 runs (other GPU work on a shared machine disturbs single runs). The CPU
cost of `prepare` is not a milestone gate yet; it goes to rebuilding and uploading about 70 KB
a frame for 500 layer instances (jobs and records ~40 KB, since every spawning ring's tail
moves each tick; spawn records; the emitter frames that moved) and to wgpu's per-copy and
per-pass overhead.

## WebGL2 notes

WebGL2 and GLES 3.0 have no compute shaders: particles run on the CPU backend there (the GPU
kernels translate to GLSL ES 3.10 for GLES 3.1 devices, tested, and pack half floats with integer
arithmetic, so they need no `f16` capability either). Vertex formats `Float32x4`, `Float16x4`,
`Uint32x2` only; ≤ 4 bind groups (view, scene, the particles' LUT and uniforms); the LUT is
`Rgba16Float` sampled with `textureLod` in the vertex shader; the sprite shader translates to
GLSL ES 3.00 (tested) and needs no `f16` capability. On
GL scene depth is sampled without an attachment and tested in the shader; with MSAA on GL there
is no soft fade (hard test only). The GL backend (ANGLE, GLES 3.0) renders the particle scenes
and stories within the perceptual tolerance of Metal (`tests/gl.rs`).

## Migrating the game's effects

| game (`client/src/game`) | particles |
|---|---|
| `additive.rs` `Kind::Glow` / `Streak` / `Ring` / `Smoke` / `Vortex` | `SpriteShape::Glow` / `Streak` (+ `VelocityStretched`) / `Ring` / `Smoke` / `Vortex` |
| `Sprite::alpha > 0` (smoke that darkens) | `ParticleBlend::Premultiplied` with `coverage` |
| `fx::particles::Fade` | `Curve::Preset(FadeCurve::…)` (`Flicker` = `Linear` + `Flicker`) |
| `fx::Effect::Explosion { at, scale }` | `spawn_effect(builtin::EXPLOSION, EffectSpawn::at(at).scale(scale))` |
| `fx::Effect::Impact { at, dir, color }` | `spawn_effect(builtin::HIT_SPARK, EffectSpawn::at(at).facing(-dir).tint(color.extend(1.0)))` (the game's `color` is a `Vec3`; `tint` takes linear RGBA) |
| `fx::Effect::ShieldHit { center, radius, dir }` | `spawn_effect(builtin::SHIELD_RIPPLE, EffectSpawn::at(center).facing(-dir).scale(radius / 20.0).follow(ship))` |
| warp / portal jumps | `builtin::WARP_IN` / `WARP_OUT` |
| engine trails and nozzle flames | `ParticleEmitter::new(builtin::ENGINE_EXHAUST)` per nozzle, `rate_scale` = throttle |
| bolt afterglow | `builtin::LASER_TRAIL` following the bolt (beams: `LASER_BEAM`, PA3) |
| `1 − e^−x` soft clip in the overlay | `direct_soft_clip` in Direct mode; with `PostProcess` the tonemapper and bloom take over |
