#![warn(missing_docs)]
//! Space3d particles: data-driven effects — sprite layers, ribbons, beams and light flashes —
//! simulated on the fixed tick (GPU compute, or on the CPU on WebGL2) and drawn by a render node
//! (`"space3d/particles"` in `space3d_render::RenderNodes`) that matches the frame's HDR,
//! tonemapping and depth in every mode (graphics spec §2.5, design-particles).
//!
//! ```no_run
//! use space3d_core::prelude::*;
//! use space3d_particles::prelude::*;
//! let mut app = App::new();
//! app.add_plugin(ParticlesPlugin::default());
//! app.spawn_effect(builtin::EXPLOSION, EffectSpawn::at(Vec3::ZERO).scale(2.0));
//! ```
//!
//! Not in the facade prelude (the game has its own `Particle`, `Effect` and `Fade` types): use
//! `space3d::particles::prelude::*`.
//!
//! How it works (design-particles §3, §5):
//! * [`tick_system`] runs on the fixed tick, after the tick's gameplay systems
//!   ([`ParticlesPlugin::TICK_ORDER`]): it follows every [`ParticleEmitter`] (its `Transform`
//!   through the `Parent` chain), decides the emission of the tick (exact rate accumulators,
//!   bursts with their sub-tick time) and records it in a tick log. Emission never depends on the
//!   frame rate.
//! * The `"space3d/particles"` render node replays the pending ticks inside the render frame —
//!   with compute kernels ([`ParticleBackend::Gpu`], 100k+ particles) or in Rust
//!   ([`ParticleBackend::Cpu`], the WebGL2 path) — so `render_to_image` after `n` ticks shows
//!   exactly `n` ticks of simulation, and rendering every tick, rendering once at the end, or
//!   replaying only the last catch-up window give identical images.
//! * Emitters moved after the tick — in `Stage::Update` (how SpaceCorps2027 moves bolts and
//!   avatars), or on frames without a tick at refresh rates above the tick rate — draw their
//!   local-space layers at the render pose, and the render node lays each world-space layer's
//!   emission on from the pose the last tick read to the render pose: the next tick's emission,
//!   previewed (the same spawn indices and random attributes), which the next tick then
//!   simulates. A bolt's trail reaches its head at any frame rate, whichever stage moves it.
//! * Particle `k` of a layer instance always lands in ring slot `base + k mod capacity`, and its
//!   random attributes come from a PCG hash of (layer instance seed, `k`): deterministic, and
//!   shared bit for bit with the shaders.

mod components;
mod cpu;
mod curve;
mod effect;
mod gpu;
mod pack;
mod registry;
mod render;
mod rng;
mod runtime;
mod stats;
#[doc(hidden)]
pub mod testing;

pub use components::{
    BeamEnd, EffectSpawn, FinishAction, OnRemove, ParticleAttractor, ParticleBeam, ParticleEffectFinished,
    ParticleEmitter, SpawnEffect,
};
pub use curve::{ColorRange, CountRange, Curve, FadeCurve, FloatRange, Gradient};
pub use effect::{
    BeamLayer, BuiltinTexture, Burst, EffectPriority, EmitDirection, EmitShape, Facing, Falloff, Flicker, Flipbook,
    FlipbookMode, Force, LUT_WIDTH, LightFlash, MAX_BEAM_SEGMENTS, MAX_FLIPBOOK_FRAMES, MAX_FORCES_PER_LAYER,
    MAX_LAYERS_PER_EFFECT, MAX_RIBBON_POINTS, ParticleBlend, ParticleEffect, ParticleLayer, RibbonFacing, RibbonLayer,
    RibbonProfile, RibbonUv, SimulationSpace, SortMode, Spawn, SpriteRender, SpriteShape, SpriteTarget, SpriteTexture,
    ThrottleResponse,
};
pub use registry::{EffectError, EffectHandle, ParticleEffects, builtin};
pub use runtime::ParticleRuntime;
pub use stats::{EffectState, ParticleStats};

use space3d_core::{App, Events, Plugin, Resources, Stage, SystemCtx, Time};
use space3d_render::{FrameSettings, GraphicsQuality, RenderNodes, Renderer};

/// Adds the particle resources, the fixed-tick emission system and the `"space3d/particles"`
/// render node. Add it next to `RenderPlugin` (or `DefaultPlugins`); without a renderer effects
/// still tick (and one-shots despawn) but nothing is drawn.
#[derive(Clone, Debug)]
pub struct ParticlesPlugin {
    /// Initial `ParticleSettings` resource (mutable at runtime; read every frame).
    pub settings: ParticleSettings,
    /// Register the built-in effect library (`builtin::*`). Default true.
    pub builtins: bool,
    /// Register [`tick_system`] in `Stage::FixedUpdate` at [`Self::TICK_ORDER`] (after the
    /// gameplay systems of the tick, whenever they were added). Default true; false to place it
    /// yourself.
    pub tick_system: bool,
}

impl Default for ParticlesPlugin {
    fn default() -> Self {
        Self {
            settings: ParticleSettings::default(),
            builtins: true,
            tick_system: true,
        }
    }
}

impl ParticlesPlugin {
    /// Order of [`tick_system`] in `Stage::FixedUpdate` (`App::add_system_ordered`): after the
    /// systems added there with `add_system` / `add_system_named` (order 0), so each tick's
    /// emission starts from where fixed-tick gameplay moved the emitter this tick. Emitters
    /// moved later (in `Stage::Update`, or on frames without a tick) are caught up at render
    /// time: the render node lays their world-space emission on to the render pose, where their
    /// local-space layers and meshes draw (the crate docs, "How it works").
    pub const TICK_ORDER: i32 = 1000;

    /// The defaults.
    pub fn new() -> Self {
        Self::default()
    }
    /// With these settings.
    pub fn with_settings(settings: ParticleSettings) -> Self {
        Self {
            settings,
            ..Self::default()
        }
    }
}

impl Plugin for ParticlesPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(self.settings.clone())
            .init_resource::<ParticleEffects>()
            .init_resource::<ParticleStats>()
            .init_resource::<ParticleRuntime>()
            .add_event::<ParticleEffectFinished>();
        if self.builtins {
            let effects = app.resources.expect_mut::<ParticleEffects>();
            for handle in builtin::ALL {
                if let Some(effect) = builtin::effect(handle) {
                    effects.set_builtin(handle, effect);
                }
            }
        }
        if self.tick_system {
            app.add_system_ordered(Stage::FixedUpdate, Self::TICK_ORDER, "particles::tick", tick_system);
        }
        // The render node (the frame's `RenderNodes` exists with or without a renderer; a
        // `RenderPlugin` added later keeps it).
        let node = app
            .resources
            .init::<RenderNodes>()
            .insert(render::NODE_NAME, render::ParticlesNode::new(&self.settings));
        app.insert_resource(testing::NodeHandle(node));
    }
}

/// The fixed-tick emission system (registered by the plugin unless `tick_system == false`):
/// follow every [`ParticleEmitter`], schedule its emission for this tick into the tick log, and
/// finish effects that have fully played ([`ParticleEffectFinished`], then their
/// [`FinishAction`]). Allocates nothing in the steady state.
pub fn tick_system(ctx: &mut SystemCtx) {
    let world = &*ctx.world;
    let finished = ctx.resources.scope::<runtime::ParticleRuntime, _>(|res, runtime| {
        let Some(settings) = res.get::<ParticleSettings>() else {
            return false;
        };
        if !settings.enabled {
            return false;
        }
        let Some(effects) = res.get::<ParticleEffects>() else {
            return false;
        };
        let dt = res.get::<Time>().map_or(0.0, Time::fixed_delta_secs);
        let rt = &mut runtime.rt;
        rt.budget = Some(pool_budget(settings, res));
        rt.tick(world, settings, effects, dt);
        let (live, layers) = rt.counts();
        let (total, reserved, spawned) = (rt.pool.total(), rt.pool.reserved(), rt.spawned);
        if let Some(stats) = res.get_mut::<ParticleStats>() {
            stats.effects = live;
            stats.layer_instances = layers;
            stats.pool_capacity = total;
            stats.pool_reserved = reserved;
            stats.spawned = spawned;
        }
        if rt.finished.is_empty() {
            return false;
        }
        let events = res.init::<Events<ParticleEffectFinished>>();
        for f in &rt.finished {
            events.send(ParticleEffectFinished {
                entity: f.entity,
                effect: f.effect,
            });
        }
        true
    });
    // Light flashes: `PointLight` child entities of their emitters while they play.
    let world = &mut *ctx.world;
    ctx.resources.scope::<runtime::ParticleRuntime, _>(|res, runtime| {
        if let Some(effects) = res.get::<ParticleEffects>()
            && res.get::<ParticleSettings>().is_some_and(|s| s.enabled)
        {
            runtime.rt.update_lights(world, effects);
        }
    });
    if !finished {
        return;
    }
    // Finish actions (after the events, which are sent before them).
    let rt = &ctx.resources.expect::<runtime::ParticleRuntime>().rt;
    for f in &rt.finished {
        match f.action {
            FinishAction::Despawn => {
                let _ = ctx.world.despawn(f.entity);
            }
            FinishAction::RemoveEmitter => {
                let _ = ctx.world.remove_one::<ParticleEmitter>(f.entity);
            }
            _ => {}
        }
    }
}

/// The backend `wanted` resolves to on a device with (`Some(true)`) or without culling-class
/// compute and indirect draws, or before a renderer exists (`None`: as with compute).
pub(crate) fn resolve_backend(wanted: ParticleBackend, compute: Option<bool>) -> ParticleBackend {
    match (wanted, compute) {
        (ParticleBackend::Cpu, _) | (_, Some(false)) => ParticleBackend::Cpu,
        _ => ParticleBackend::Gpu,
    }
}

/// Pool budget of the backend the renderer (if any) resolves to: `max_particles_cpu`, or
/// `max_particles` within the device's storage binding and buffer limits.
fn pool_budget(settings: &ParticleSettings, res: &Resources) -> u32 {
    let renderer = res.get::<Renderer>();
    let no_compute = res.get::<FrameSettings>().is_some_and(|f| f.downlevel.no_compute);
    let compute = renderer.map(|r| r.graphics_caps().compute && !no_compute);
    match resolve_backend(settings.backend, compute) {
        ParticleBackend::Cpu => settings.max_particles_cpu,
        _ => {
            let limit = renderer.map_or(u32::MAX, |r| {
                let caps = r.capabilities();
                gpu::pool_limit(caps.max_storage_binding, caps.max_buffer_size)
            });
            settings.max_particles.min(limit)
        }
    }
}

/// Runtime options (resource). Fields are public; construct with `..Default::default()`.
#[derive(Clone, Debug, PartialEq)]
pub struct ParticleSettings {
    /// `false`: nothing ticks, spawns or draws (entities are kept).
    pub enabled: bool,
    /// Simulation backend.
    pub backend: ParticleBackend,
    /// GPU pool budget: 262 144 (65 536 on Android and iOS), within the device's storage
    /// binding limit.
    pub max_particles: u32,
    /// CPU backend budget: 16 384 native, 8 192 on the web.
    pub max_particles_cpu: u32,
    /// Ribbon segment budget. Default 65 536.
    pub max_ribbon_segments: u32,
    /// 0..=1: scales the spawn counts of `lod` layers. Default 1.
    pub quality: f32,
    /// Distance-based rate scaling (off by default).
    pub lod: ParticleLod,
    /// 0 freezes every effect (deterministic). Default 1.
    pub time_scale: f32,
    /// Simulation substeps per tick. Default 1.
    pub substeps: u32,
    /// Ticks replayed after a gap before the simulation resets instead. Default 1200.
    pub max_catchup_ticks: u32,
    /// Extrapolate by `Time::alpha` when rendering. Default true.
    pub extrapolate: bool,
    /// Global switch for depth fades. Default true.
    pub soft_particles: bool,
    /// `1 − e^−x` per channel in `FrameMode::Direct` (no tonemapper there). Default true.
    pub direct_soft_clip: bool,
    /// Sprite size clamp (fill-rate guard), as a fraction of the viewport height; a layer's
    /// `SpriteRender::max_screen_size` overrides it. Default 1.0: the built-ins' big layers
    /// (explosion flash, warp flash) stay unclamped at the top-down framings they are designed
    /// for, so their size and timing do not change with the zoom.
    pub max_screen_size: f32,
    /// Mixed into every emitter seed (replays, variety). Default 0.
    pub seed: u32,
}

impl Default for ParticleSettings {
    fn default() -> Self {
        let mobile = cfg!(any(target_os = "android", target_os = "ios"));
        Self {
            enabled: true,
            backend: ParticleBackend::Auto,
            max_particles: if mobile { 65_536 } else { 262_144 },
            max_particles_cpu: if cfg!(target_arch = "wasm32") { 8_192 } else { 16_384 },
            max_ribbon_segments: 65_536,
            quality: 1.0,
            lod: ParticleLod::default(),
            time_scale: 1.0,
            substeps: 1,
            max_catchup_ticks: 1200,
            extrapolate: true,
            soft_particles: true,
            direct_soft_clip: true,
            max_screen_size: 1.0,
            seed: 0,
        }
    }
}

impl ParticleSettings {
    /// Budgets and backend for a quality tier (graphics spec §4.4).
    pub fn for_quality(q: GraphicsQuality) -> Self {
        let _ = q; // STUB(I): the §4.4 preset table
        Self::default()
    }
}

/// Simulation backend (`ParticleStats::backend` reports the one in use).
///
/// Both run the same tick log, ring slots, RNG streams and render pipeline: the alive sets and
/// slots are identical and positions agree within float rounding, so switching backends
/// changes performance, not the look.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ParticleBackend {
    /// `Gpu` when the device has culling-class compute and indirect draws
    /// (`GraphicsCaps::compute`, not masked by `DownlevelOverride::no_compute`), else `Cpu`.
    #[default]
    Auto,
    /// Compute simulation and compaction (`max_particles`, 262 144 by default), drawn with
    /// indirect draws. Falls back to `Cpu` (with a warning) on devices without compute.
    Gpu,
    /// CPU simulation (the WebGL2 path): the same tick log, ring slots, RNG, instance stream
    /// and render pipeline as the GPU backend.
    Cpu,
}

/// Rate multiplier by distance from the primary camera: 1 at `near`, `min_scale` at `far`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParticleLod {
    /// Full rate up to here.
    pub near: f32,
    /// `min_scale` from here.
    pub far: f32,
    /// Rate multiplier at `far`.
    pub min_scale: f32,
}

impl Default for ParticleLod {
    fn default() -> Self {
        Self {
            near: f32::INFINITY,
            far: f32::INFINITY,
            min_scale: 1.0,
        }
    }
}

/// `use space3d_particles::prelude::*;` (or `space3d::particles::prelude::*`).
pub mod prelude {
    pub use crate::{
        BeamEnd, BeamLayer, EffectHandle, EffectSpawn, FinishAction, ParticleAttractor, ParticleBeam, ParticleEffect,
        ParticleEffectFinished, ParticleEffects, ParticleEmitter, ParticleLayer, ParticleSettings, ParticleStats,
        ParticlesPlugin, RibbonLayer, SpawnEffect, builtin,
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use space3d_core::prelude::*;

    #[test]
    fn plugin_inserts_resources_and_ticks() {
        let mut app = App::new();
        app.add_plugin(ParticlesPlugin::default());
        assert!(app.resources.contains::<ParticleSettings>());
        assert!(app.resources.contains::<ParticleEffects>());
        assert!(app.resources.contains::<ParticleStats>());
        assert!(app.system_names(Stage::FixedUpdate).contains(&"particles::tick"));
        let e = app.spawn_effect(builtin::EXPLOSION, Vec3::ZERO);
        app.step_ticks(3);
        assert!(app.world.contains(e));
        let state = app.resources.expect::<ParticleRuntime>().state(e).unwrap();
        assert!(state.emitting || state.time > 0.0);
        assert!(state.reserved > 0 && !state.finished);
        assert!(app.resources.expect::<RenderNodes>().contains("space3d/particles"));
        let manual = ParticlesPlugin {
            tick_system: false,
            ..ParticlesPlugin::with_settings(ParticleSettings::for_quality(GraphicsQuality::Low))
        };
        let mut app = App::new();
        app.add_plugin(manual);
        assert!(!app.system_names(Stage::FixedUpdate).contains(&"particles::tick"));
    }

    /// The tick follows emitters to where fixed-tick gameplay systems moved them this tick, even
    /// when those were added after the plugin (movers in other stages: `tests/follow.rs`).
    #[test]
    fn tick_runs_after_gameplay_systems() {
        let mut app = App::new();
        app.add_plugin(ParticlesPlugin::default());
        let bolt = app.world.spawn((Transform::IDENTITY,));
        app.spawn_effect(builtin::LASER_TRAIL, EffectSpawn::at(Vec3::ZERO).follow(bolt));
        app.add_system(Stage::FixedUpdate, move |ctx: &mut SystemCtx| {
            let x = ctx.time().tick as f32 * 25.0 + 25.0;
            if let Ok(mut t) = ctx.world.get::<&mut Transform>(bolt) {
                t.translation.x = x;
            }
        });
        assert_eq!(
            app.system_names(Stage::FixedUpdate).last(),
            Some(&"particles::tick"),
            "the tick runs last"
        );
        app.step_ticks(4);
        let bolt_x = app.world.get::<&Transform>(bolt).unwrap().translation.x;
        assert_eq!(bolt_x, 100.0);
        let rt = &app.resources.expect::<ParticleRuntime>().rt;
        let inst = rt
            .instances
            .iter()
            .find(|i| i.live && i.entity != bolt)
            .expect("instance");
        assert_eq!(inst.pos.x, bolt_x, "emission follows this tick's pose");
        assert_eq!(inst.prev_pos.x, bolt_x - 25.0);
    }

    /// An emitter whose handle has no effect data (a removed effect's stale handle,
    /// `EffectHandle::INVALID`, a built-in without `ParticlesPlugin::builtins`) plays nothing and
    /// finishes: its event is sent once and its `FinishAction` runs, so `spawn_effect` entities
    /// do not leak.
    #[test]
    fn unknown_effect_handles_finish() {
        let mut app = App::new();
        app.add_plugin(ParticlesPlugin::default());
        let effects = app.resources.expect_mut::<ParticleEffects>();
        let stale = effects.add(ParticleEffect::new("gone")).unwrap();
        effects.remove(stale);
        let beam = app.spawn_effect(stale, Vec3::ZERO);
        let invalid = app.spawn_effect(EffectHandle::INVALID, Vec3::ZERO);
        let kept = app.spawn_effect(stale, EffectSpawn::at(Vec3::ZERO).keep());
        let mut reader = EventReader::default();
        app.step_ticks(1);
        let finished: Vec<Entity> = reader
            .read(app.resources.expect::<Events<ParticleEffectFinished>>())
            .map(|f| f.entity)
            .collect();
        assert_eq!(finished.len(), 3, "{finished:?}");
        assert!(!app.world.contains(beam) && !app.world.contains(invalid));
        assert!(app.world.contains(kept));
        let state = app.resources.expect::<ParticleRuntime>().state(kept).unwrap();
        assert!(state.finished && !state.emitting && state.reserved == 0);
        app.step_ticks(30);
        let again = reader
            .read(app.resources.expect::<Events<ParticleEffectFinished>>())
            .count();
        assert_eq!(again, 0, "finished once");
        assert!(app.world.contains(kept));
        assert_eq!(app.resources.expect::<ParticleStats>().effects, 0);
    }

    #[test]
    fn settings_defaults() {
        let s = ParticleSettings::default();
        assert!(s.enabled && s.soft_particles && s.direct_soft_clip && s.extrapolate);
        assert_eq!(
            (s.backend, s.substeps, s.max_catchup_ticks),
            (ParticleBackend::Auto, 1, 1200)
        );
        assert!(ParticleLod::default().near.is_infinite());
    }
}
