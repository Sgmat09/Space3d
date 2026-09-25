# Particles

Data-driven particle effects — explosions, hit sparks, shield ripples, warps, engine exhaust,
laser trails — simulated on the fixed tick and drawn by a render node that matches the frame's
HDR, tonemapping and scene depth in every mode. The normative design is
[`docs/design/graphics.md`](../design/graphics.md) §2.5 (with the area design it amends).

Status: milestones PA0 (runtime, CPU simulation, effect library) and PA1 (CPU backend rendering,
prefabs) are in. The GPU compute backend (PA2), ribbons, beams, flipbooks and light flashes (PA3),
distortion layers, per-particle sorting and LOD calibration (PA4) follow; the API below does not
change for them.

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
| render | `SpriteRender { shape, blend, facing, soft_distance, near_fade, order, sort, target }` |

Random ranges read `1.0` or `(0.5, 1.0)`. Curves: `Constant`, `Linear(a, b)`, `Keys([...])`,
`Smooth([...])` (Catmull-Rom) and `Preset(FadeCurve)` — the game's fades `Linear`, `Out`,
`Flash`, `InOut`, `Linger`, `Burn`, `Hold`. Curves and gradients are baked into a 128-sample LUT
the vertex shader reads, so the CPU never evaluates them per particle.

### Sprites

* **Shapes** (analytic, no textures, crisp at any size — the game's `additive.rs` kinds as
  data): `Glow { core, hardness }` (soft halo, hard disc, white-hot core), `Streak { core, taper }`
  (a capsule; pair it with `VelocityStretched`), `Ring { thickness, focus, fill }` (focus
  sharpens it toward the sprite's +X: shield arcs), `Smoke { breakup, hot_core }`,
  `Vortex { arms, twist }`. `Texture` and flipbooks arrive with PA3 (drawn as a soft glow until
  then, with a warning).
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
| `LASER_TRAIL` | looping head glow + trail laid per unit of distance | `.follow(bolt).tint(color)` |
| `LASER_BEAM` | reserved: beams arrive with PA3 | |

Stories: `Engine/Particles/Builtin/*` and `Engine/Particles/Features/*` in the engine storybook
(`space3d stories`, the editor's storybook viewer, golden tests).

## Time model and determinism

* `tick_system` runs in `Stage::FixedUpdate` (register it yourself with
  `ParticlesPlugin { tick_system: false, .. }` to order it after gameplay). It follows emitters,
  decides each tick's emission — exact 32.32 fixed-point rate accumulators (10/s at 60 Hz is
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

* `ParticleBackend::Auto` (default) and `Cpu` simulate on the CPU today; `Gpu` (compute
  simulation and compaction) arrives with PA2 and `Auto` will pick it when the device has
  culling-class compute and indirect draws. The CPU backend is the WebGL2 / GLES 3.0 path: the
  same tick log, ring slots, RNG, 40-byte instance stream and render pipeline; the vertex shader
  reads no storage buffers.
* Budget: `ParticleSettings::max_particles_cpu` (16 384 native, 8 192 on the web). Each layer
  instance reserves a ring sized from its rates, bursts and life (§5.1 of the area design); over
  the budget rings shrink to 1/8 before a layer instance is refused (`ParticleStats::{shrunk,
  refused}`, logged once per effect). `EffectPriority::Low` stops at 75 % of the pool, `Normal`
  at 90 %.
* `ParticleStats` (resource, per rendered frame): backend, live effects and layer instances,
  pool capacity / reserved, alive particles, ticks replayed, catch-up resets, draw calls, bytes
  uploaded, `cpu_reallocations` (0 in the steady state).
* Measured (Apple M4 Max, test profile): the CPU backend ticks, simulates and packs 16 384
  particles in 0.28 ms on one thread (gate 0.35 ms); `tick_system` handles 500 layer instances
  in 13 µs (gate 60 µs); a frame with ~1000 live particles in 80 draws allocates only wgpu's
  bookkeeping (185 allocations, pinned), our code nothing. `docs/performance.md` collects the
  figures in Phase I.

## WebGL2 notes

Vertex formats `Float32x4`, `Float16x4`, `Uint32x2` only; ≤ 4 bind groups (view, scene, the
particles' LUT and uniforms); the LUT is `Rgba16Float` sampled with `textureLod` in the vertex
shader; the sprite shader translates to GLSL ES 3.00 (tested) and needs no `f16` capability. On
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
| `fx::Effect::Impact { at, dir, color }` | `spawn_effect(builtin::HIT_SPARK, EffectSpawn::at(at).facing(-dir).tint(color))` |
| `fx::Effect::ShieldHit { center, radius, dir }` | `spawn_effect(builtin::SHIELD_RIPPLE, EffectSpawn::at(center).facing(-dir).scale(radius / 20.0).follow(ship))` |
| warp / portal jumps | `builtin::WARP_IN` / `WARP_OUT` |
| engine trails and nozzle flames | `ParticleEmitter::new(builtin::ENGINE_EXHAUST)` per nozzle, `rate_scale` = throttle |
| bolt afterglow | `builtin::LASER_TRAIL` following the bolt (beams: `LASER_BEAM`, PA3) |
| `1 − e^−x` soft clip in the overlay | `direct_soft_clip` in Direct mode; with `PostProcess` the tonemapper and bloom take over |
