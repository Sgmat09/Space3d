//! The particle runtime (design-particles §5): effect instances (one per emitting entity),
//! layer instances ("entries") with their ring of pool slots, the pool allocator and the tick
//! log. Written only by [`crate::tick_system`] on the fixed tick; the simulation backend replays
//! the log inside the render frame.
//!
//! Steady state allocates nothing: every collection is reused (slabs with free lists, cleared
//! vectors), and growth is counted in `ParticleStats::cpu_reallocations`.

mod alloc;
pub(crate) mod layer;
pub(crate) mod log;
mod path;
pub(crate) mod ribbon;
mod spawn;
#[cfg(test)]
mod tests;

pub(crate) use alloc::RangeAllocator;
pub(crate) use layer::{LayerSim, capacity, ring_speed_for, window_ticks};
pub(crate) use log::{SpawnRecord, TickLog, WorldAttractor, XformRow};
pub(crate) use path::Path;
pub(crate) use ribbon::{RibbonPoint, Track};

use crate::components::{FinishAction, OnRemove, ParticleAttractor, ParticleEmitter};
use crate::effect::{EffectPriority, MAX_LAYERS_PER_EFFECT, ParticleEffect};
use crate::registry::{EffectHandle, ParticleEffects};
use crate::stats::EffectState;
use crate::{ParticleLod, ParticleSettings, rng};
use space3d_core::glam::{Mat4, Quat, Vec3, Vec4};
use space3d_core::{Entity, GlobalTransform, Parent, Transform, World};
use space3d_render::{Camera3d, PointLight, Projection, RenderTarget, Visibility};
use std::collections::{HashMap, VecDeque};
use std::hash::BuildHasherDefault;

/// A hash map with a fixed hasher (deterministic, no per-map random state).
type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<ahash::AHasher>>;

/// A layer instance: one sprite layer of one effect instance and its ring of pool slots.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Entry {
    pub live: bool,
    /// Allocation order (never reused, unlike the entry index, whose reuse depends on when the
    /// log is replayed): orders the draws deterministically.
    pub serial: u64,
    pub instance: u32,
    pub effect: EffectHandle,
    pub layer: u16,
    /// First pool slot of the ring.
    pub base: u32,
    /// Ring size (after a budget shrink).
    pub capacity: u32,
    /// Ring size the layer asked for (hot reload compares it).
    pub full_capacity: u32,
    /// Particles spawned so far (the next spawn index).
    pub spawned: u64,
    /// Last tick this layer instance emitted on (its particles are all dead
    /// `window_ticks(sim.window)` ticks later).
    pub last_spawn_tick: u64,
    /// Continuous emission accumulator (32.32 fixed point; time and distance rates together).
    pub acc: u64,
    pub seed: u32,
    /// Budget shrink (`capacity / ` the ring asked for) applied to spawn counts.
    pub rate_mul: f32,
    /// `ParticleSettings::quality` the ring was sized for (`lod` layers; 1 for the others): the
    /// quality × LOD multiplier never exceeds it, so the ring always holds its particles.
    pub quality_cap: f32,
    pub sim: LayerSim,
}

impl Entry {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        instance: u32,
        effect: EffectHandle,
        layer: u16,
        base: u32,
        capacity: u32,
        seed: u32,
        sim: LayerSim,
        rate_mul: f32,
    ) -> Self {
        Self {
            live: true,
            serial: 0,
            instance,
            effect,
            layer,
            base,
            capacity,
            full_capacity: capacity,
            spawned: 0,
            last_spawn_tick: 0,
            // Half a period: the first particle of a rate comes half its interval in.
            acc: 1 << 31,
            seed,
            rate_mul,
            quality_cap: 1.0,
            sim,
        }
    }
}

/// An effect instance: what one emitter entity plays.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Instance {
    pub live: bool,
    pub entity: Entity,
    /// Mapped from its entity (not detached by a removal, restart or effect change).
    pub attached: bool,
    /// A `Parent` of the chain is gone: frozen, not emitting, drains.
    pub orphaned: bool,
    pub effect: EffectHandle,
    pub restart: u32,
    pub seed: u32,
    /// Effect time at the start of the next tick (seconds).
    pub time: f64,
    pub looping: bool,
    pub duration: f32,
    /// Emission ends here (non-looping): the duration or the last finite burst (a tick starting
    /// at it still emits what falls on it).
    pub end_time: f32,
    pub playing: bool,
    pub finished: bool,
    pub finish: FinishAction,
    pub on_remove: OnRemove,
    pub entries: [u32; MAX_LAYERS_PER_EFFECT],
    pub n_entries: u8,
    /// Ribbon tracks (indices into `Runtime::tracks`).
    pub tracks: [u32; MAX_LAYERS_PER_EFFECT],
    pub n_tracks: u8,
    /// Effect time at which the light flash ends (0 without one).
    pub flash_end: f32,
    /// The light flash's entity while it plays.
    pub light: Option<Entity>,
    pub prev_pos: Vec3,
    pub prev_rot: Quat,
    pub pos: Vec3,
    pub rot: Quat,
    /// World pose read at the last tick or frame observation, before the extrapolation
    /// ([`Runtime::observe_frame`]).
    pub raw_pos: Vec3,
    pub raw_rot: Quat,
    /// Velocity and spin (scaled axis per second) of the motion outside the fixed tick, measured
    /// between frames (an emitter moved in `Stage::Update`): each tick extrapolates the pose by
    /// them to its own time ([`Runtime::ahead`]).
    pub frame_vel: Vec3,
    pub frame_spin: Vec3,
    /// The last measurement of that motion (the extrapolation uses it only while the next one
    /// agrees: steady motion, not a teleport, a start or a stop).
    pub measured_vel: Vec3,
    pub measured_spin: Vec3,
    pub velocity: Vec3,
    /// World distance the emitter moved in the last tick (the render-time bridge's share of a
    /// tick's time-rate emission).
    pub distance: f32,
    /// Quality × LOD rate multiplier of the last tick.
    pub quality: f32,
    /// Distance moved per tick, low-passed over `ParticleEmitter::velocity_smoothing` (−1
    /// before the first motion): what the time rate is paced against.
    pub smoothed: f32,
    /// × the time rate this tick (pacing; 1 without).
    pub pace: f32,
    /// Speed (units/s) the `rate_per_distance` rings are sized for (0: the layers'
    /// `Spawn::max_speed`).
    pub ring_speed: f32,
    /// Consecutive ticks the emitter flew faster than its rings hold.
    pub fast_ticks: u8,
    /// The effect has enabled `rate_per_distance` layers sized by speed (no explicit
    /// `capacity`), and the smallest `max_speed` among them.
    pub dist_speed: Option<f32>,
    /// The quality × LOD level its `lod` layers' rings are sized for ([`ring_level`]).
    pub ring: f32,
    /// A layer was refused (budget): a looping instance retries it every
    /// [`RETRY_TICKS`] ticks.
    pub missing: bool,
    /// The effect has enabled `lod` layers (sized by `ring`).
    pub lod_layers: bool,
    pub tint: Vec4,
    pub scale: f32,
    pub rate_scale: f32,
    pub hidden: bool,
    pub created_tick: u64,
    pub last_spawn_tick: u64,
    pub seen_tick: u64,
}

impl Instance {
    fn emitting(&self) -> bool {
        self.attached
            && !self.orphaned
            && self.playing
            && !self.finished
            && (self.looping || self.time <= self.end_time as f64)
    }

    pub(crate) fn entries(&self) -> &[u32] {
        &self.entries[..self.n_entries as usize]
    }

    pub(crate) fn tracks(&self) -> &[u32] {
        &self.tracks[..self.n_tracks as usize]
    }

    /// Emitting at effect time `time` (the start of a tick) for a layer starting at `start`:
    /// ribbon points and beams.
    pub(crate) fn emitting_at(&self, start: f32, dt: f32) -> bool {
        self.emitting() && self.time + dt as f64 >= start as f64
    }

    /// Emitting now (the render node: beams, ribbon heads).
    pub(crate) fn is_emitting(&self) -> bool {
        self.emitting()
    }
}

/// The render-time bridge of a world-space layer instance ([`Runtime::bridge`]): `count`
/// particles from spawn index `first`, spread over `t0..t1` of the tick like the next tick's
/// record, along `row` (from the pose of the last tick, `prev_*`, to the render pose).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Bridge {
    pub first: u64,
    pub count: u32,
    pub t0: f32,
    pub t1: f32,
    pub row: XformRow,
}

/// A finished effect instance of this tick: its event and `FinishAction`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Finished {
    pub entity: Entity,
    pub effect: EffectHandle,
    pub action: FinishAction,
}

/// The runtime state (inside the [`ParticleRuntime`] resource).
#[derive(Debug, Default)]
pub(crate) struct Runtime {
    /// Ticks run.
    pub tick: u64,
    /// Seconds simulated (Σ dt).
    pub time: f64,
    pub instances: Vec<Instance>,
    /// Recent emitter path of every instance (same index).
    pub paths: Vec<Path>,
    free_instances: Vec<u32>,
    by_entity: FastMap<Entity, u32>,
    pub entries: Vec<Entry>,
    /// Bumped whenever an entry is created, freed or gets new layer parameters (the GPU
    /// backend re-uploads its entry table then).
    pub entries_version: u64,
    /// Pool budget for the resolved backend (set by the tick system; `None`:
    /// `ParticleSettings::max_particles_cpu`).
    pub budget: Option<u32>,
    /// The budget in force ([`Self::set_budget`]): new rings are reserved under it even when
    /// the pool is larger (a budget lowered after the pool grew).
    limit: Option<u32>,
    /// Largest `by_entity.capacity()` seen: the map reallocates only beyond it (its
    /// `capacity()` also moves with tombstones, without allocating).
    by_entity_cap: usize,
    /// Entries freed at a tick, reusable once the log no longer holds that tick.
    released_entries: VecDeque<(u64, u32)>,
    free_entries: Vec<u32>,
    next_serial: u64,
    pub pool: RangeAllocator,
    pub log: TickLog,
    effects_version: u64,
    pub finished: Vec<Finished>,
    /// Ribbon tracks (slab) and their point pool.
    pub tracks: Vec<Track>,
    free_tracks: Vec<u32>,
    pub points: Vec<RibbonPoint>,
    pub ribbon_pool: RangeAllocator,
    /// Light flash entities to despawn (their instance ended or detached).
    dead_lights: Vec<Entity>,
    warned_ribbons: Vec<EffectHandle>,
    /// Particles spawned since the start.
    pub spawned: u64,
    /// Layer instances refused / shrunk since the last render.
    pub refused: u32,
    pub shrunk: u32,
    /// Collections that grew since the last render.
    pub reallocations: u32,
    warned_refused: Vec<EffectHandle>,
    warned_unknown: Vec<EffectHandle>,
    /// Width / height of the primary target (the renderer's size; 0: unknown, 16:9 is
    /// assumed), for the off-screen LOD.
    pub aspect: f32,
    /// Emitters new this tick (scratch: started after the others, the most visible first).
    new_emitters: Vec<NewEmitter>,
    /// Seconds from the frame time the emitters' `Transform`s stand for (where `Stage::Update`
    /// left them) to the end of this tick: how far a tick extrapolates the motion measured
    /// between frames (set by the tick system; 0 extrapolates nothing).
    pub ahead: f32,
}

/// World position and rotation of an entity with `t` and `parent`, through the `Parent` chain
/// of `Transform`s (exact per tick, unlike the `GlobalTransform` of the previous frame). The
/// flag is set when a parent is missing.
pub(crate) fn world_pose(world: &World, t: &Transform, parent: Option<&Parent>) -> (Vec3, Quat, bool) {
    let (mut pos, mut rot) = (t.translation, t.rotation);
    let mut next = parent.map(|p| p.0);
    for _ in 0..32 {
        let Some(e) = next else {
            return (pos, rot, false);
        };
        let Ok(pt) = world.get::<&Transform>(e) else {
            return (pos, rot, true);
        };
        pos = pt.translation + pt.rotation * (pt.scale * pos);
        rot = pt.rotation * rot;
        next = world.get::<&Parent>(e).ok().map(|p| p.0);
    }
    (pos, rot, false)
}

fn priority_limit(p: EffectPriority) -> f32 {
    match p {
        EffectPriority::Low => 0.75,
        EffectPriority::Normal => 0.9,
        _ => 1.0,
    }
}

/// The primary camera as the LOD sees it (from ECS transforms and the target's aspect ratio:
/// deterministic).
#[derive(Clone, Copy, Debug)]
struct CameraView {
    pos: Vec3,
    rot: Quat,
    perspective: bool,
    /// Perspective: tan(fov_y / 2); orthographic: half the height.
    extent: f32,
    /// Width / height of the camera's view.
    aspect: f32,
}

/// Margin of the "on screen" test: the view grown by a quarter on each side, so particles do
/// not fly into view from a thinned emitter just off screen.
const ONSCREEN_MARGIN: f32 = 1.25;

impl CameraView {
    /// Whether `p` may be on screen: inside the camera's view grown by [`ONSCREEN_MARGIN`].
    fn sees(&self, p: Vec3) -> bool {
        let v = self.rot.inverse() * (p - self.pos);
        let depth = -v.z;
        let half_h = if self.perspective {
            if depth <= 0.0 {
                return false;
            }
            depth * self.extent
        } else {
            self.extent
        } * ONSCREEN_MARGIN;
        v.y.abs() <= half_h && v.x.abs() <= half_h * self.aspect
    }

    /// The LOD multiplier of an emitter at `p`: distance LOD × `offscreen_scale` off screen.
    fn lod(&self, settings: &ParticleSettings, p: Vec3) -> f32 {
        let off = if settings.offscreen_scale < 1.0 && !self.sees(p) {
            settings.offscreen_scale.clamp(0.0, 1.0)
        } else {
            1.0
        };
        lod_scale(&settings.lod, self.pos.distance(p)) * off
    }
}

/// An emitter seen by the tick: its component, pose, visibility and quality × LOD.
#[derive(Clone, Debug)]
struct NewEmitter {
    entity: Entity,
    em: ParticleEmitter,
    pos: Vec3,
    rot: Quat,
    /// The pose read (before the extrapolation).
    raw: (Vec3, Quat),
    broken: bool,
    hidden: bool,
    quality: f32,
    /// Speed its distance rings are sized for (0: the layers' `max_speed`; set when an
    /// instance restarts for its speed).
    ring_speed: f32,
}

/// Ticks between a looping instance's retries of layers its budget refused.
const RETRY_TICKS: u64 = 15;

/// Levels of ring sizes below the quality ([`ring_level`]).
const RING_LEVELS: i32 = 4;

/// The ring size level (a fraction of the full rings) for the quality × LOD multiplier `t` at
/// quality `q`: `q / 2^k`, the smallest of `q, q/2, … q/16` that holds `t`, or 0 when `t` is 0
/// (an off-screen emitter with `offscreen_scale` 0: one slot per layer). Levels keep a looping
/// emitter drifting across LOD distances from restarting on every tick: it restarts with
/// bigger rings once `t` exceeds its level, with smaller ones once `t` falls to a quarter of it
/// (`Runtime::lod_rings_outgrown`).
pub(crate) fn ring_level(q: f32, t: f32) -> f32 {
    let q = q.clamp(0.0, 1.0);
    if q <= 0.0 || t <= 0.0 {
        return 0.0;
    }
    let k = ((q / t).log2().floor() as i32).clamp(0, RING_LEVELS);
    q / (1u32 << k) as f32
}

/// Speed below which pacing treats an emitter as still (units/s): the softening of
/// [`pace`]'s ratio, so a still emitter emits at its plain time rate.
const PACE_STILL_SPEED: f32 = 8.0;

/// The time-rate multiplier of an instance that moved `distance` this tick of `dt` seconds with
/// `ParticleEmitter::velocity_smoothing` `tau` (updating its low-passed distance): `(d + k) /
/// (d̄ + k)`, the distance against the smoothed one, softened by `k` = [`PACE_STILL_SPEED`] ×
/// `dt`, within 0..4. 1 without pacing or before the emitter first moved (the smoothing starts
/// from its first motion).
fn pace(inst: &mut Instance, tau: f32, distance: f32, dt: f32) -> f32 {
    if tau.is_nan() || tau <= 0.0 || dt <= 0.0 || !distance.is_finite() || inst.orphaned {
        inst.smoothed = -1.0;
        return 1.0;
    }
    if inst.smoothed < 0.0 {
        if distance > 0.0 {
            inst.smoothed = distance;
        }
        return 1.0;
    }
    let a = 1.0 - (-dt / tau).exp();
    let before = inst.smoothed;
    inst.smoothed += a * (distance - inst.smoothed);
    // Against the mean of the smoothed distance before and after this tick: the ratio then
    // averages to 1 over steady periodic motion (either end alone biases it by about ±10 %).
    let k = PACE_STILL_SPEED * dt;
    ((distance + k) / (0.5 * (before + inst.smoothed) + k)).clamp(0.0, 4.0)
}

/// LOD multiplier at `distance` (1 at `near`, `min_scale` at `far`).
fn lod_scale(lod: &ParticleLod, distance: f32) -> f32 {
    if !lod.near.is_finite() || distance <= lod.near {
        return 1.0;
    }
    if !lod.far.is_finite() || lod.far <= lod.near {
        return 1.0;
    }
    let t = ((distance - lod.near) / (lod.far - lod.near)).clamp(0.0, 1.0);
    1.0 + (lod.min_scale.clamp(0.0, 1.0) - 1.0) * t
}

impl Runtime {
    fn capacities(&self) -> [usize; 9] {
        [
            self.instances.capacity() + self.paths.capacity(),
            self.free_instances.capacity(),
            self.entries.capacity(),
            self.released_entries.capacity() + self.free_entries.capacity(),
            self.pool.capacity(),
            self.log.capacity(),
            self.finished.capacity(),
            self.tracks.capacity() + self.free_tracks.capacity() + self.points.capacity(),
            self.ribbon_pool.capacity() + self.dead_lights.capacity() + self.new_emitters.capacity(),
        ]
    }

    /// Particle budget of the pool: the pool grows to it, and a lower budget than the pool
    /// holds limits new reservations at once (live rings above it drain) and gives the free
    /// tail of the pool back (game#14).
    pub fn set_budget(&mut self, budget: u32) {
        self.pool.grow_to(budget);
        self.pool.shrink_to(budget);
        self.limit = Some(budget);
    }

    /// The pool capacity in force: the budget, within the pool's size.
    pub fn pool_capacity(&self) -> u32 {
        self.limit.map_or(self.pool.total(), |l| l.min(self.pool.total()))
    }

    /// Count a reallocation of `by_entity` (its capacity beyond any seen before).
    fn track_by_entity(&mut self) {
        let cap = self.by_entity.capacity();
        if cap > self.by_entity_cap {
            if self.by_entity_cap > 0 {
                self.reallocations += 1;
            }
            self.by_entity_cap = cap;
        }
    }

    /// Entries freed at ticks the log no longer holds become reusable.
    fn release_entries(&mut self, first_unreplayed: u64) {
        while let Some(&(tick, e)) = self.released_entries.front() {
            if tick >= first_unreplayed {
                break;
            }
            self.released_entries.pop_front();
            self.free_entries.push(e);
        }
    }

    fn alloc_entry(&mut self, mut entry: Entry) -> u32 {
        entry.serial = self.next_serial;
        self.next_serial += 1;
        self.entries_version += 1;
        match self.free_entries.pop() {
            Some(i) => {
                self.entries[i as usize] = entry;
                i
            }
            None => {
                self.entries.push(entry);
                self.entries.len() as u32 - 1
            }
        }
    }

    /// Reserve a ring for `cap` slots under the budget and the priority's share of it, shrinking
    /// to 1/2, 1/4 and 1/8 before refusing (design-particles §5 step 1).
    fn reserve(&mut self, cap: u32, priority: EffectPriority) -> Option<(u32, u32)> {
        let limit = (self.pool_capacity() as f32 * priority_limit(priority)) as u32;
        for shift in 0..4 {
            let c = (cap >> shift).max(1);
            if self.pool.reserved() + c > limit {
                continue;
            }
            if let Some(base) = self.pool.alloc(c) {
                return Some((base, c));
            }
            if c == 1 {
                break;
            }
        }
        None
    }

    /// Create the instance of `em` on `entity` (and its layer instances).
    #[allow(clippy::too_many_arguments)]
    fn create(
        &mut self,
        entity: Entity,
        em: &ParticleEmitter,
        handle: EffectHandle,
        effect: &ParticleEffect,
        pos: Vec3,
        rot: Quat,
        settings: &ParticleSettings,
        ring: f32,
        ring_speed: f32,
    ) -> u32 {
        let seed = em.seed.unwrap_or_else(|| {
            let bits = entity.to_bits().get();
            rng::mix(
                rng::mix(bits as u32, (bits >> 32) as u32),
                rng::mix(self.tick as u32, settings.seed),
            )
        });
        let end_time = effect
            .layers
            .iter()
            .filter(|l| l.enabled)
            .flat_map(|l| {
                l.spawn
                    .bursts
                    .iter()
                    .filter(|b| b.cycles != 0)
                    .map(move |b| l.start + b.time + b.interval.max(0.0) * (b.cycles.saturating_sub(1)) as f32)
            })
            .fold(effect.duration, f32::max);
        let inst = Instance {
            live: true,
            entity,
            attached: true,
            orphaned: false,
            effect: handle,
            restart: em.restart,
            seed,
            time: 0.0,
            looping: effect.looping,
            duration: effect.duration,
            end_time,
            playing: em.playing,
            finished: false,
            finish: em.finish,
            on_remove: em.on_remove,
            entries: [u32::MAX; MAX_LAYERS_PER_EFFECT],
            n_entries: 0,
            tracks: [u32::MAX; MAX_LAYERS_PER_EFFECT],
            n_tracks: 0,
            flash_end: effect
                .light
                .as_ref()
                .map_or(0.0, |f| f.start.max(0.0) + f.duration.max(0.0)),
            light: None,
            prev_pos: pos,
            prev_rot: rot,
            pos,
            rot,
            raw_pos: pos,
            raw_rot: rot,
            frame_vel: Vec3::ZERO,
            frame_spin: Vec3::ZERO,
            measured_vel: Vec3::ZERO,
            measured_spin: Vec3::ZERO,
            velocity: em.velocity,
            distance: 0.0,
            quality: 1.0,
            smoothed: -1.0,
            pace: 1.0,
            ring_speed,
            fast_ticks: 0,
            dist_speed: None,
            ring,
            missing: false,
            lod_layers: false,
            tint: em.tint,
            scale: em.scale,
            rate_scale: em.rate_scale,
            hidden: false,
            created_tick: self.tick,
            last_spawn_tick: self.tick,
            seen_tick: self.tick,
        };
        let i = match self.free_instances.pop() {
            Some(i) => {
                self.instances[i as usize] = inst;
                self.paths[i as usize] = Path::default();
                i
            }
            None => {
                self.instances.push(inst);
                self.paths.push(Path::default());
                self.instances.len() as u32 - 1
            }
        };
        self.allocate_layers(i, handle, effect, false);
        self.by_entity.insert(entity, i);
        self.track_by_entity();
        i
    }

    /// Whether looping instance `i`, whose emitter moved `speed` units/s this tick, should
    /// restart with bigger `rate_per_distance` rings: it flew faster than they hold for three
    /// ticks in a row (a single jump, a teleport, does not count) and bigger rings exist.
    fn distance_rings_outgrown(&mut self, i: u32, speed: f32) -> bool {
        let inst = &mut self.instances[i as usize];
        let Some(base) = inst.dist_speed else {
            return false;
        };
        if !inst.looping || !inst.attached || inst.finished || inst.orphaned || !speed.is_finite() {
            return false;
        }
        let held = inst.ring_speed.max(base);
        if speed <= held || ring_speed_for(base, speed) <= held {
            inst.fast_ticks = 0;
            return false;
        }
        inst.fast_ticks = inst.fast_ticks.saturating_add(1);
        inst.fast_ticks >= 3
    }

    /// Whether looping instance `i` should restart with rings for the quality × LOD multiplier
    /// `t`: its `lod` rings are too small for it (it came closer, or the quality rose), or four
    /// times too big (it went far away).
    fn lod_rings_outgrown(&self, i: u32, t: f32) -> bool {
        let inst = &self.instances[i as usize];
        if !inst.looping || !inst.attached || inst.finished || inst.orphaned {
            return false;
        }
        let ring = inst.ring;
        inst.lod_layers && (t > ring * 1.0001 || (t <= ring * 0.25 && ring > 0.0))
    }

    /// Reserve the rings of `effect`'s enabled layers for instance `i` — all of them, or with
    /// `retry` those a budget refused before (kept in layer order; a failed retry is not counted
    /// again).
    fn allocate_layers(&mut self, i: u32, handle: EffectHandle, effect: &ParticleEffect, retry: bool) {
        let seed = self.instances[i as usize].seed;
        let ring = self.instances[i as usize].ring;
        let ring_speed = self.instances[i as usize].ring_speed;
        let old = self.instances[i as usize];
        let held = if retry { old.n_entries as usize } else { 0 };
        let mut entries = [u32::MAX; MAX_LAYERS_PER_EFFECT];
        let (mut n, mut k) = (0usize, 0usize);
        let mut missing = false;
        for (li, layer) in effect.layers.iter().enumerate().take(MAX_LAYERS_PER_EFFECT) {
            if !layer.enabled {
                continue;
            }
            if k < held && self.entries[old.entries[k] as usize].layer as usize == li {
                entries[n] = old.entries[k];
                (n, k) = (n + 1, k + 1);
                continue;
            }
            // `lod` layers get rings for the instance's quality × LOD level (looping ones
            // restart with other rings when it moves past that level: `lod_rings_outgrown`);
            // none at level 0 (they would emit nothing: off screen with `offscreen_scale` 0).
            if layer.lod && ring <= 0.0 {
                continue;
            }
            let q = if layer.lod { ring } else { 1.0 };
            let cap = layer::capacity_for(layer, q, ring_speed);
            let Some((base, c)) = self.reserve(cap, effect.priority) else {
                missing = true;
                if retry {
                    continue;
                }
                self.refused += 1;
                if !self.warned_refused.contains(&handle) {
                    self.warned_refused.push(handle);
                    tracing::warn!(
                        effect = %effect.name,
                        layer = %layer.name,
                        "particle budget exhausted: layer instance refused (raise ParticleSettings::max_particles_cpu or lower quality)"
                    );
                }
                continue;
            };
            if c < cap {
                self.shrunk += 1;
            }
            let mut entry = Entry::new(
                i,
                handle,
                li as u16,
                base,
                c,
                rng::mix(seed, li as u32 + 1),
                LayerSim::new(layer),
                c as f32 / cap as f32,
            );
            entry.full_capacity = capacity(layer);
            entry.quality_cap = q;
            if retry {
                // A retried layer starts its emission from this tick.
                entry.last_spawn_tick = self.tick;
            }
            entries[n] = self.alloc_entry(entry);
            n += 1;
        }
        let inst = &mut self.instances[i as usize];
        inst.entries = entries;
        inst.n_entries = n as u8;
        inst.missing = missing;
        inst.lod_layers = effect.layers.iter().any(|l| l.enabled && l.lod);
        inst.dist_speed = effect
            .layers
            .iter()
            .filter(|l| l.enabled && l.capacity.is_none() && l.spawn.rate_per_distance > 0.0)
            .map(|l| {
                if l.spawn.max_speed > 0.0 && l.spawn.max_speed.is_finite() {
                    l.spawn.max_speed
                } else {
                    layer::V_REF
                }
            })
            .reduce(f32::min);
        if !retry {
            self.allocate_tracks(i, handle, effect);
        }
    }

    /// The ribbon tracks of instance `i` (rings in the point pool, under
    /// `ParticleSettings::max_ribbon_segments`).
    fn allocate_tracks(&mut self, i: u32, handle: EffectHandle, effect: &ParticleEffect) {
        let mut n = 0usize;
        let slots = MAX_LAYERS_PER_EFFECT - self.instances[i as usize].n_entries as usize;
        for (li, r) in effect.ribbons.iter().enumerate().take(slots) {
            let sim = ribbon::RibbonSim::new(r);
            let Some(base) = self.ribbon_pool.alloc(sim.max_points) else {
                self.refused += 1;
                if !self.warned_ribbons.contains(&handle) {
                    self.warned_ribbons.push(handle);
                    tracing::warn!(
                        effect = %effect.name,
                        ribbon = %r.name,
                        "ribbon budget exhausted: ribbon refused (raise ParticleSettings::max_ribbon_segments)"
                    );
                }
                continue;
            };
            let need = (base + sim.max_points) as usize;
            if self.points.len() < need {
                self.points.resize(need, RibbonPoint::default());
            }
            let track = Track {
                live: true,
                serial: self.next_serial,
                instance: i,
                effect: handle,
                layer: li as u16,
                base,
                cap: sim.max_points,
                head: 0,
                len: 0,
                last_tick: self.tick,
                sim,
            };
            self.next_serial += 1;
            let k = match self.free_tracks.pop() {
                Some(k) => {
                    self.tracks[k as usize] = track;
                    k
                }
                None => {
                    self.tracks.push(track);
                    self.tracks.len() as u32 - 1
                }
            };
            self.instances[i as usize].tracks[n] = k;
            n += 1;
        }
        self.instances[i as usize].n_tracks = n as u8;
    }

    /// Free the ribbon tracks of instance `i` and its light flash.
    fn free_tracks(&mut self, i: u32) {
        let inst = &mut self.instances[i as usize];
        let n = inst.n_tracks as usize;
        inst.n_tracks = 0;
        let tracks = inst.tracks;
        if let Some(light) = inst.light.take() {
            self.dead_lights.push(light);
        }
        for &k in &tracks[..n] {
            let tr = &mut self.tracks[k as usize];
            if tr.live {
                tr.live = false;
                self.ribbon_pool.free(tr.base, tr.cap);
                self.free_tracks.push(k);
            }
        }
    }

    /// Free the layer instances of `i` (their rings now; the entry slots once the log no longer
    /// references this tick).
    fn free_layers(&mut self, i: u32) {
        self.free_tracks(i);
        let inst = &mut self.instances[i as usize];
        let n = inst.n_entries as usize;
        inst.n_entries = 0;
        let entries = inst.entries;
        for &e in &entries[..n] {
            let entry = &mut self.entries[e as usize];
            if entry.live {
                entry.live = false;
                self.pool.free(entry.base, entry.capacity);
                self.released_entries.push_back((self.tick, e));
                self.entries_version += 1;
            }
        }
    }

    fn free_instance(&mut self, i: u32) {
        self.free_layers(i);
        let inst = &mut self.instances[i as usize];
        if inst.attached && self.by_entity.get(&inst.entity) == Some(&i) {
            self.by_entity.remove(&inst.entity);
        }
        inst.live = false;
        inst.attached = false;
        self.free_instances.push(i);
    }

    /// The entity lost its emitter (or restarted / changed effect): stop emitting and drain, or
    /// clear the particles now.
    fn detach(&mut self, i: u32, on_remove: OnRemove) {
        let inst = &mut self.instances[i as usize];
        if inst.attached && self.by_entity.get(&inst.entity) == Some(&i) {
            self.by_entity.remove(&inst.entity);
        }
        inst.attached = false;
        // A detached emitter's light flash goes at once (its entity may be gone).
        if let Some(light) = inst.light.take() {
            self.dead_lights.push(light);
        }
        if on_remove == OnRemove::Clear {
            let n = inst.n_entries as usize;
            let entries = inst.entries;
            for &e in &entries[..n] {
                let entry = self.entries[e as usize];
                self.log.clear_range(entry.base, entry.capacity);
            }
            self.free_instance(i);
        }
    }

    /// Hot reload: an effect whose layers keep their capacity updates its layer parameters in
    /// place; one whose structure changed restarts (the old particles drain).
    fn sync_effects(&mut self, effects: &ParticleEffects) {
        if effects.version() == self.effects_version {
            return;
        }
        self.effects_version = effects.version();
        for i in 0..self.instances.len() as u32 {
            let inst = self.instances[i as usize];
            if !inst.live || inst.finished {
                continue;
            }
            let Some(effect) = effects.get(inst.effect) else {
                if inst.attached {
                    self.detach(i, OnRemove::Drain);
                }
                continue;
            };
            let enabled = effect.layers.iter().enumerate().filter(|(_, l)| l.enabled);
            let same = enabled.clone().count() == inst.n_entries as usize
                && enabled.zip(inst.entries()).all(|((li, l), &e)| {
                    let entry = &self.entries[e as usize];
                    entry.layer as usize == li && entry.full_capacity == capacity(l)
                })
                && effect.ribbons.len() == inst.n_tracks as usize
                && effect
                    .ribbons
                    .iter()
                    .zip(inst.tracks())
                    .all(|(r, &k)| self.tracks[k as usize].cap == ribbon::RibbonSim::new(r).max_points);
            if same {
                for &e in inst.entries() {
                    let entry = &mut self.entries[e as usize];
                    entry.sim = LayerSim::new(&effect.layers[entry.layer as usize]);
                }
                for &k in inst.tracks() {
                    let tr = &mut self.tracks[k as usize];
                    tr.sim = ribbon::RibbonSim::new(&effect.ribbons[tr.layer as usize]);
                }
                self.entries_version += 1;
                let inst = &mut self.instances[i as usize];
                inst.looping = effect.looping;
                inst.duration = effect.duration;
                inst.flash_end = effect
                    .light
                    .as_ref()
                    .map_or(0.0, |f| f.start.max(0.0) + f.duration.max(0.0));
            } else if inst.attached {
                self.detach(i, OnRemove::Drain);
            }
        }
    }

    /// Position of the primary camera (LOD), lowest order first.
    fn camera_view(world: &World, target_aspect: f32) -> Option<CameraView> {
        let mut best: Option<(i32, u64, CameraView)> = None;
        for (e, cam, t, parent) in world.query::<(Entity, &Camera3d, &Transform, Option<&Parent>)>().iter() {
            if cam.target != RenderTarget::Primary || !cam.active {
                continue;
            }
            let key = (cam.order, e.to_bits().get());
            if best.is_none_or(|b| key < (b.0, b.1)) {
                let (pos, rot, _) = world_pose(world, t, parent);
                let (perspective, extent) = match cam.projection {
                    Projection::Perspective { fov_y, .. } => (true, (0.5 * fov_y).tan()),
                    Projection::Orthographic { height, .. } => (false, 0.5 * height),
                };
                let aspect = target_aspect * cam.viewport.map_or(1.0, |v| v[2] / v[3].max(1e-6));
                let view = CameraView {
                    pos,
                    rot,
                    perspective,
                    extent,
                    aspect,
                };
                best = Some((key.0, key.1, view));
            }
        }
        best.map(|b| b.2)
    }

    /// One fixed tick of `fixed_dt` seconds (before `time_scale`).
    pub fn tick(&mut self, world: &World, settings: &ParticleSettings, effects: &ParticleEffects, fixed_dt: f32) {
        let before = self.capacities();
        self.finished.clear();
        self.tick += 1;
        let t = self.tick;
        let dt = fixed_dt * settings.time_scale.max(0.0);
        self.set_budget(self.budget.unwrap_or(settings.max_particles_cpu));
        self.ribbon_pool.grow_to(settings.max_ribbon_segments);
        self.release_entries(self.log.first_tick().unwrap_or(t));
        self.sync_effects(effects);
        self.log.begin(t, dt, settings.substeps.max(1), self.time);

        // World attractors: the first 8 in query order.
        for (a, tr, parent) in world
            .query::<(&ParticleAttractor, &Transform, Option<&Parent>)>()
            .iter()
            .take(8)
        {
            let (position, _, _) = world_pose(world, tr, parent);
            self.log.attractor(WorldAttractor {
                position,
                strength: a.strength,
                radius: a.radius,
                falloff: a.falloff,
                mask: a.mask,
            });
        }
        let camera = if settings.lod.near.is_finite() || settings.offscreen_scale < 1.0 {
            Self::camera_view(world, if self.aspect > 0.0 { self.aspect } else { 16.0 / 9.0 })
        } else {
            None
        };

        // What an emitter whose handle has no effect data plays: nothing, finished at once.
        let nothing = ParticleEffect {
            duration: 0.0,
            ..ParticleEffect::default()
        };
        let mut query = world.query::<(
            Entity,
            &ParticleEmitter,
            &Transform,
            Option<&Parent>,
            Option<&Visibility>,
        )>();
        // With a LOD, emitters new this tick start after the others, the most visible first
        // (a full budget refuses the least visible).
        let mut new = std::mem::take(&mut self.new_emitters);
        new.clear();
        for (entity, em, tr, parent, vis) in query.iter() {
            let (raw_pos, raw_rot, broken) = world_pose(world, tr, parent);
            let mut found = self.by_entity.get(&entity).copied();
            let (pos, rot) = match found {
                Some(i) => self.extrapolate(i, raw_pos, raw_rot, broken),
                None => (raw_pos, raw_rot),
            };
            // Quality × LOD (distance, off screen) of this emitter this tick.
            let lod = camera.map_or(1.0, |c| c.lod(settings, pos));
            let quality = settings.quality.clamp(0.0, 1.0) * lod;
            // The emitter's speed this tick (for rings sized by speed).
            let mut speed = 0.0;
            if let Some(i) = found {
                let inst = &self.instances[i as usize];
                if dt > 0.0 && !inst.orphaned {
                    speed = pos.distance(inst.pos) / dt;
                }
                // Another effect, a restart, (looping) `lod` rings sized for another LOD level,
                // or distance rings too small for its speed: the old instance drains and a new
                // one plays on.
                let restart = inst.effect != em.effect || inst.restart != em.restart;
                if restart || self.lod_rings_outgrown(i, quality) || self.distance_rings_outgrown(i, speed) {
                    if !restart {
                        // Rings for the speed it flies at (and never smaller than before).
                        let inst = &self.instances[i as usize];
                        if let Some(base) = inst.dist_speed {
                            speed = ring_speed_for(base, speed).max(inst.ring_speed);
                        }
                    } else {
                        speed = 0.0;
                    }
                    self.detach(i, OnRemove::Drain);
                    found = None;
                } else {
                    speed = 0.0;
                }
            }
            let emitter = NewEmitter {
                entity,
                em: em.clone(),
                pos,
                rot,
                raw: (raw_pos, raw_rot),
                broken,
                hidden: vis == Some(&Visibility::Hidden),
                quality,
                ring_speed: speed,
            };
            match found {
                Some(i) => self.play(i, &emitter, effects, dt),
                // An effect asset still loading: the emitter starts once it has loaded.
                None if effects.is_pending(em.effect) => {}
                None if camera.is_some() => new.push(emitter),
                None => {
                    let i = self.start(&emitter, settings, effects, dt, &nothing);
                    self.play(i, &emitter, effects, dt);
                }
            }
        }
        drop(query);
        new.sort_unstable_by(|a, b| {
            b.quality
                .total_cmp(&a.quality)
                .then(a.entity.to_bits().cmp(&b.entity.to_bits()))
        });
        for emitter in &new {
            let i = self.start(emitter, settings, effects, dt, &nothing);
            self.play(i, emitter, effects, dt);
        }
        self.new_emitters = new;

        // Instances whose entity or emitter is gone; rows; finish.
        for i in 0..self.instances.len() as u32 {
            let inst = self.instances[i as usize];
            if !inst.live {
                continue;
            }
            if inst.attached && inst.seen_tick != t {
                if inst.finished {
                    self.free_instance(i);
                    continue;
                }
                self.detach(i, inst.on_remove);
                if !self.instances[i as usize].live {
                    continue;
                }
            }
            let inst = &mut self.instances[i as usize];
            if !inst.attached {
                inst.prev_pos = inst.pos;
                inst.prev_rot = inst.rot;
            }
            if !inst.attached || inst.orphaned || !inst.playing || inst.finished {
                inst.velocity = Vec3::ZERO;
            }
            if inst.finished {
                continue;
            }
            let inst = *inst;
            self.log.row(XformRow {
                instance: i,
                prev_pos: inst.prev_pos,
                prev_rot: inst.prev_rot,
                pos: inst.pos,
                rot: inst.rot,
                velocity: inst.velocity,
                scale: inst.scale,
                throttle: inst.rate_scale,
            });
            if dt > 0.0 {
                self.instances[i as usize].time += dt as f64;
            }
            let window = inst
                .entries()
                .iter()
                .map(|&e| window_ticks(self.entries[e as usize].sim.window, dt))
                .max()
                .unwrap_or(0);
            self.paths[i as usize].record(t, window.min(1 << 20), inst.prev_pos, inst.pos, inst.velocity.length());
            let emitting = self.instances[i as usize].emitting();
            let ribbons_done = inst.tracks().iter().all(|&k| {
                let tr = &self.tracks[k as usize];
                tr.len == 0 || t >= tr.last_tick.saturating_add(window_ticks(tr.sim.lifetime, dt) as u64)
            });
            let current = &self.instances[i as usize];
            let light_done = current.light.is_none()
                && (!current.attached || current.orphaned || current.time >= current.flash_end as f64);
            if !emitting
                && dt > 0.0
                && t >= inst.last_spawn_tick.saturating_add(window as u64)
                && ribbons_done
                && light_done
            {
                self.finish(i);
            }
        }

        // Keep the log to the catch-up window: frames older than the longest live particle
        // life are not needed to reproduce the pool (§3), so an unconsumed log stays bounded.
        let window = self
            .entries
            .iter()
            .filter(|e| e.live)
            .map(|e| window_ticks(e.sim.window, dt))
            .max()
            .unwrap_or(1);
        self.log.trim(window.saturating_add(1) as usize);
        self.time += dt as f64;
        let after = self.capacities();
        self.reallocations += before.iter().zip(&after).filter(|(a, b)| a != b).count() as u32;
    }

    /// The pose instance `i` emits from this tick, for the pose read (`raw_*`): extrapolated
    /// by the motion measured between frames ([`Self::observe_frame`]) to the end of the tick
    /// ([`Self::ahead`]). Gameplay that moves an emitter in `Stage::Update` sets one pose per
    /// frame; without this, every tick of a frame running two or more read that pose, the first
    /// laid the whole frame's motion and the others stacked their particles on one point (beads
    /// one frame of motion apart at 30 or 45 fps). An emitter moved on the tick (or still)
    /// measures no such motion and emits from exactly the pose read.
    fn extrapolate(&mut self, i: u32, raw_pos: Vec3, raw_rot: Quat, broken: bool) -> (Vec3, Quat) {
        let ahead = self.ahead;
        let inst = &mut self.instances[i as usize];
        inst.raw_pos = raw_pos;
        inst.raw_rot = raw_rot;
        if broken || ahead <= 0.0 || !inst.attached || (inst.frame_vel == Vec3::ZERO && inst.frame_spin == Vec3::ZERO) {
            return (raw_pos, raw_rot);
        }
        (
            raw_pos + inst.frame_vel * ahead,
            (Quat::from_scaled_axis(inst.frame_spin * ahead) * raw_rot).normalize(),
        )
    }

    /// Measure the motion of every emitter since the last tick or frame observation, at the
    /// end of the frame (`frame_motion_system`, last in `Stage::PostUpdate`): motion gameplay
    /// gave it after the ticks, over this frame's `delta` (`Time::delta`), which the next
    /// frame's ticks extrapolate ([`Self::extrapolate`]). Allocates nothing.
    pub fn observe_frame(&mut self, world: &World, delta: f32) {
        let span = delta;
        for inst in &mut self.instances {
            if !inst.live || !inst.attached || inst.orphaned || inst.finished {
                continue;
            }
            let Ok(t) = world.get::<&Transform>(inst.entity) else {
                continue;
            };
            let parent = world.get::<&Parent>(inst.entity).ok();
            let (pos, rot, broken) = world_pose(world, &t, parent.as_deref());
            let (mut vel, mut spin) = (Vec3::ZERO, Vec3::ZERO);
            if !broken && span > 0.0 {
                vel = (pos - inst.raw_pos) / span;
                // The shorter way round.
                let d = rot * inst.raw_rot.inverse();
                let d = if d.w < 0.0 { -d } else { d };
                spin = d.to_scaled_axis() / span;
            }
            if !vel.is_finite() || !spin.is_finite() {
                (vel, spin) = (Vec3::ZERO, Vec3::ZERO);
            }
            // Extrapolate only steady motion: this frame's measurement agrees with the last one
            // (within a quarter). A teleport, a start or a stop is laid as it happened, never
            // projected a frame past it.
            let agrees = |a: Vec3, b: Vec3| (a - b).length() <= 0.25 * a.length().max(b.length());
            let steady = agrees(vel, inst.measured_vel) && agrees(spin, inst.measured_spin);
            (inst.frame_vel, inst.frame_spin) = if steady { (vel, spin) } else { (Vec3::ZERO, Vec3::ZERO) };
            (inst.measured_vel, inst.measured_spin) = (vel, spin);
            inst.raw_pos = pos;
            inst.raw_rot = rot;
        }
    }

    /// Append this tick's ribbon points of instance `i` (its pose at the end of the tick).
    fn record_ribbons(&mut self, i: u32, dt: f32) {
        let inst = self.instances[i as usize];
        if inst.n_tracks == 0 || dt <= 0.0 {
            return;
        }
        let now = self.time + dt as f64;
        for &k in inst.tracks() {
            let tr = self.tracks[k as usize];
            if !tr.live || !inst.emitting_at(tr.sim.start, dt) {
                continue;
            }
            let pos = inst.pos + inst.rot * (tr.sim.offset * inst.scale);
            let newest = tr.newest().map(|s| &self.points[s]);
            let Some(dist) = ribbon::due(&tr.sim, newest, pos, now, inst.scale) else {
                continue;
            };
            let tint = if tr.sim.tinted { inst.tint } else { Vec4::ONE };
            let slot = (tr.base + tr.head) as usize;
            self.points[slot] = RibbonPoint {
                pos,
                time: now,
                dist,
                tint,
            };
            let tr = &mut self.tracks[k as usize];
            tr.head = (tr.head + 1) % tr.cap;
            tr.len = (tr.len + 1).min(tr.cap);
            tr.last_tick = self.tick;
        }
    }

    /// Play the light flashes: a `PointLight` child entity of each emitter whose flash is on
    /// (spawned when it starts, updated every tick, despawned when it ends), and despawn the
    /// lights of instances that ended. Called by the tick system after [`Self::tick`].
    pub fn update_lights(&mut self, world: &mut World, effects: &ParticleEffects) {
        let before = self.dead_lights.capacity();
        for e in self.dead_lights.drain(..) {
            let _ = world.despawn(e);
        }
        for i in 0..self.instances.len() {
            let inst = self.instances[i];
            if !inst.live || inst.flash_end <= 0.0 && inst.light.is_none() {
                continue;
            }
            let flash = effects.get(inst.effect).and_then(|e| e.light.as_ref());
            let tau = inst.time as f32 - flash.map_or(0.0, |f| f.start);
            let active =
                flash.filter(|f| inst.attached && !inst.orphaned && !inst.finished && tau >= 0.0 && tau < f.duration);
            match active {
                Some(f) => {
                    let k = f.intensity.eval((tau / f.duration.max(1e-6)).clamp(0.0, 1.0));
                    let color = if f.tinted {
                        f.color * inst.tint.truncate()
                    } else {
                        f.color
                    };
                    let light = PointLight {
                        color,
                        // × scale²: the same light at the same distance relative to the effect.
                        intensity: if inst.hidden {
                            0.0
                        } else {
                            f.peak * k.max(0.0) * inst.scale * inst.scale
                        },
                        range: f.range * inst.scale.abs(),
                    };
                    let o = f.offset * inst.scale;
                    let local = Transform::from_xyz(o.x, o.y, o.z);
                    let alive = inst.light.filter(|&e| world.contains(e));
                    match alive {
                        Some(e) => {
                            if let Ok(mut l) = world.get::<&mut PointLight>(e) {
                                *l = light;
                            }
                            if let Ok(mut t) = world.get::<&mut Transform>(e) {
                                *t = local;
                            }
                        }
                        None => {
                            let global = Mat4::from_rotation_translation(inst.rot, inst.pos) * local.matrix();
                            let e = world.spawn((local, GlobalTransform(global), light, Parent(inst.entity)));
                            self.instances[i].light = Some(e);
                        }
                    }
                }
                None => {
                    if let Some(e) = self.instances[i].light.take() {
                        let _ = world.despawn(e);
                    }
                }
            }
        }
        if self.dead_lights.capacity() != before {
            self.reallocations += 1;
        }
    }

    /// Despawn every light flash entity (the particles are disabled).
    pub fn remove_lights(&mut self, world: &mut World) {
        for e in self.dead_lights.drain(..) {
            let _ = world.despawn(e);
        }
        for inst in &mut self.instances {
            if let Some(e) = inst.light.take() {
                let _ = world.despawn(e);
            }
        }
    }

    /// Create the instance of a new emitter (its rings for its LOD level) and prewarm it.
    fn start(
        &mut self,
        n: &NewEmitter,
        settings: &ParticleSettings,
        effects: &ParticleEffects,
        dt: f32,
        nothing: &ParticleEffect,
    ) -> u32 {
        let em = &n.em;
        let effect = match effects.get(em.effect) {
            Some(effect) => effect,
            None => {
                // A stale handle, one never issued, or a built-in whose data was not registered:
                // play nothing and finish this tick, so the event is sent and the `FinishAction`
                // runs (a `spawn_effect` entity despawns instead of staying forever).
                if !self.warned_unknown.contains(&em.effect) {
                    self.warned_unknown.push(em.effect);
                    tracing::warn!(
                        handle = em.effect.index(),
                        "particle emitter with an unknown effect handle: it plays nothing and finishes"
                    );
                }
                nothing
            }
        };
        let ring = ring_level(settings.quality, n.quality);
        let i = self.create(
            n.entity,
            em,
            em.effect,
            effect,
            n.pos,
            n.rot,
            settings,
            ring,
            n.ring_speed,
        );
        let inst = &mut self.instances[i as usize];
        (inst.raw_pos, inst.raw_rot) = n.raw;
        if em.prewarm > 0.0 && dt > 0.0 {
            self.prewarm(i, effect, em.prewarm, dt, settings);
        }
        i
    }

    /// One tick of instance `i` for its emitter `n`: follow it, restart a finished one played
    /// again, retry refused layers, schedule the emission and record the ribbons.
    fn play(&mut self, i: u32, n: &NewEmitter, effects: &ParticleEffects, dt: f32) {
        let (t, em) = (self.tick, &n.em);
        let inst = &mut self.instances[i as usize];
        inst.seen_tick = t;
        inst.tint = em.tint;
        inst.scale = em.scale;
        inst.rate_scale = em.rate_scale;
        inst.finish = em.finish;
        inst.on_remove = em.on_remove;
        inst.hidden = n.hidden;
        if inst.finished
            && em.playing
            && !inst.playing
            && let Some(effect) = effects.get(em.effect)
        {
            // Played again after it finished (stopped, then restarted by `playing`).
            inst.finished = false;
            inst.time = 0.0;
            inst.created_tick = t;
            inst.last_spawn_tick = t;
            self.allocate_layers(i, em.effect, effect, false);
        }
        let inst = &mut self.instances[i as usize];
        inst.playing = em.playing;
        inst.orphaned |= n.broken;
        inst.prev_pos = inst.pos;
        inst.prev_rot = inst.rot;
        let distance = if inst.orphaned {
            inst.velocity = Vec3::ZERO;
            0.0
        } else {
            inst.pos = n.pos;
            inst.rot = n.rot;
            let moved = n.pos - inst.prev_pos;
            inst.velocity = if dt > 0.0 { moved / dt } else { Vec3::ZERO } + em.velocity;
            moved.length()
        };
        inst.distance = distance;
        inst.quality = n.quality;
        inst.pace = pace(inst, em.velocity_smoothing, distance, dt);
        let Some(effect) = effects.get(em.effect) else {
            return;
        };
        // A looping instance whose budget refused a layer retries it now and then (spread over
        // the ticks by instance).
        let inst = &self.instances[i as usize];
        if inst.missing && inst.looping && inst.attached && !inst.finished && (t + i as u64).is_multiple_of(RETRY_TICKS)
        {
            self.allocate_layers(i, em.effect, effect, true);
        }
        self.schedule(i, effect, dt, n.quality, distance, 0.0);
        self.record_ribbons(i, dt);
    }

    /// Emission of instance `i` this tick (records into the log).
    fn schedule(&mut self, i: u32, effect: &ParticleEffect, dt: f32, quality: f32, distance: f32, extra_age: f32) {
        let inst = self.instances[i as usize];
        let emitting = inst.emitting();
        let mut spawned = 0u32;
        for &e in inst.entries() {
            let entry = &mut self.entries[e as usize];
            let layer = &effect.layers[entry.layer as usize];
            let mul = if layer.lod { quality.min(entry.quality_cap) } else { 1.0 } * entry.rate_mul;
            let n = spawn::spawn_tick(
                entry,
                layer,
                &spawn::SpawnTick {
                    entry: e,
                    time: inst.time,
                    dt,
                    emitting,
                    looping: inst.looping,
                    duration: inst.duration,
                    rate_scale: inst.rate_scale,
                    mul,
                    distance,
                    extra_age,
                    pace: if layer.space == crate::effect::SimulationSpace::World {
                        inst.pace
                    } else {
                        1.0
                    },
                },
                &mut self.log,
            );
            if n > 0 {
                entry.last_spawn_tick = self.tick;
            }
            spawned += n;
        }
        if spawned > 0 {
            self.spawned += spawned as u64;
            self.instances[i as usize].last_spawn_tick = self.tick;
        }
    }

    /// Synthesize the emission of the `secs` before now (records with extra age, emitted this
    /// tick; forces other than gravity and drag are skipped for the synthesized past).
    fn prewarm(&mut self, i: u32, effect: &ParticleEffect, secs: f32, dt: f32, settings: &ParticleSettings) {
        let n = ((secs / dt).round() as u32).min(settings.max_catchup_ticks);
        for v in 0..n {
            self.instances[i as usize].time = v as f64 * dt as f64;
            let quality = settings.quality.clamp(0.0, 1.0);
            self.schedule(i, effect, dt, quality, 0.0, (n - v) as f32 * dt);
        }
        self.instances[i as usize].time = n as f64 * dt as f64;
    }

    fn finish(&mut self, i: u32) {
        let inst = self.instances[i as usize];
        self.free_layers(i);
        if inst.attached {
            self.finished.push(Finished {
                entity: inst.entity,
                effect: inst.effect,
                action: inst.finish,
            });
            let inst = &mut self.instances[i as usize];
            inst.finished = true;
            inst.velocity = Vec3::ZERO;
        } else {
            self.free_instance(i);
        }
    }

    /// Where the draw of layer instance `e` sorts by depth: the render position of a
    /// local-space layer's emitter, else the middle of the emitter's path over the life window.
    /// A pure function of the ticks (and the render pose), so it orders draws the same way on
    /// both backends and for any render cadence.
    pub fn entry_center(&self, e: &Entry, poses: &[(Vec3, Quat)]) -> Vec3 {
        let i = e.instance as usize;
        let pose = poses.get(i).map_or(Vec3::ZERO, |p| p.0);
        if e.sim.local {
            return pose;
        }
        match self.paths.get(i).and_then(|p| p.bounds(self.tick)) {
            Some((lo, hi, _)) => (lo + hi) * 0.5,
            None => self.instances.get(i).map_or(pose, |inst| inst.pos),
        }
    }

    /// Conservative world bounds of the particles of layer instance `e` (GPU backend culling;
    /// the CPU backend measures them): the emitter's path over the life window (its render
    /// position for local-space layers) grown by [`LayerSim::reach`] and the largest sprite
    /// (`sprite` world units per unit of size).
    pub fn entry_bounds(&self, e: &Entry, poses: &[(Vec3, Quat)], sprite: f32) -> (Vec3, Vec3) {
        let i = e.instance as usize;
        let inst = &self.instances[i];
        let pose = poses.get(i).map_or(inst.pos, |p| p.0);
        let (lo, hi, speed) = if e.sim.local {
            (pose, pose, 0.0)
        } else {
            match self.paths.get(i).and_then(|p| p.bounds(self.tick)) {
                Some(b) => b,
                None => (inst.pos, inst.pos, inst.velocity.length()),
            }
        };
        let s = &e.sim;
        let size = s.size.0.abs().max(s.size.1.abs()) * self.draw_tint_scale(e).1.abs() * sprite;
        let r = Vec3::splat(s.reach(inst.scale, speed) + size);
        (lo - r, hi + r)
    }

    /// The render-time bridge of world-space layer instance `e`, whose emitter moved after the
    /// last tick (gameplay that moves it in `Stage::Update`, or frames between ticks): the
    /// continuous emission the next tick would make if the emitter ended it at its render pose
    /// (`poses`), laid from the pose the last tick read to the render pose. Its particles are
    /// the next tick's (the same spawn indices, random attributes and spread), so a world-space
    /// trail reaches the emitter's local-space layers and meshes at any frame rate, and the
    /// bridge becomes the simulated trail at the next tick. The time rate counts the share of a
    /// tick's motion the gap stands for (none for an emitter that was still at the last tick);
    /// the distance rate counts the gap. A pure function of the ticks and the render pose.
    /// `None` without emission (a local-space layer, a stopped, hidden or orphaned emitter,
    /// `time_scale` 0, or nothing counted).
    pub fn bridge(&self, e: &Entry, poses: &[(Vec3, Quat)], dt: f32) -> Option<Bridge> {
        if !e.live || e.sim.local || dt <= 0.0 {
            return None;
        }
        let inst = self.instances.get(e.instance as usize)?;
        if !inst.live || inst.hidden || !inst.emitting() {
            return None;
        }
        let &(pos, rot) = poses.get(e.instance as usize)?;
        let gap = pos.distance(inst.pos);
        if !gap.is_finite() {
            return None;
        }
        let share = if inst.distance > 0.0 {
            (gap / inst.distance).min(1.0) as f64
        } else {
            0.0
        };
        let c = &e.sim.spawn;
        let tick = spawn::SpawnTick {
            entry: 0,
            time: inst.time,
            dt,
            emitting: true,
            looping: inst.looping,
            duration: inst.duration,
            rate_scale: inst.rate_scale,
            mul: if c.lod { inst.quality.min(e.quality_cap) } else { 1.0 } * e.rate_mul,
            distance: gap,
            extra_age: 0.0,
            pace: inst.pace,
        };
        let mut acc = e.acc;
        let (n, t0, t1) = spawn::continuous(c, &mut acc, &tick, share)?;
        let count = n.min(e.capacity as u64) as u32;
        (count > 0).then_some(Bridge {
            first: e.spawned,
            count,
            t0,
            t1,
            row: XformRow {
                instance: e.instance,
                prev_pos: inst.pos,
                prev_rot: inst.rot,
                pos,
                rot,
                velocity: inst.velocity,
                scale: inst.scale,
                throttle: inst.rate_scale,
            },
        })
    }

    /// The tint and effect scale layer instance `e` draws with: the emitter's tint (white for
    /// untinted layers) and scale, times the layer's response to the throttle
    /// (`ThrottleResponse::{intensity, size}`).
    pub fn draw_tint_scale(&self, e: &Entry) -> (Vec4, f32) {
        let Some(inst) = self.instances.get(e.instance as usize) else {
            return (Vec4::ONE, 1.0);
        };
        let tint = if e.sim.tinted { inst.tint } else { Vec4::ONE };
        let (size, bright) = e.sim.draw_throttle(inst.rate_scale);
        // A `lod` layer thinned by quality × LOD (its ring level) draws its fewer particles
        // bigger and brighter by (1 / level)^¼ each, so a sparse plume stays continuous and
        // keeps most of its light (level 0.25: × 1.41; 1: unchanged).
        let lod = if e.sim.spawn.lod && e.quality_cap < 1.0 {
            e.quality_cap.max(1.0 / 16.0).recip().sqrt().sqrt()
        } else {
            1.0
        };
        (
            (tint.truncate() * (bright * lod)).extend(tint.w),
            inst.scale * size * lod,
        )
    }

    /// Layer instance `e` may still have particles alive (it emitted within its life window).
    pub fn may_be_alive(&self, e: &Entry, dt: f32) -> bool {
        e.live && e.spawned > 0 && self.tick <= e.last_spawn_tick.saturating_add(window_ticks(e.sim.window, dt) as u64)
    }

    /// State of the instance playing on `entity`.
    pub fn state(&self, entity: Entity) -> Option<EffectState> {
        let &i = self.by_entity.get(&entity)?;
        let inst = &self.instances[i as usize];
        Some(EffectState {
            time: inst.time as f32,
            emitting: inst.emitting(),
            finished: inst.finished,
            reserved: inst.entries().iter().map(|&e| self.entries[e as usize].capacity).sum(),
        })
    }

    /// Live effect instances and layer instances.
    pub fn counts(&self) -> (u32, u32) {
        let instances = self.instances.iter().filter(|i| i.live && !i.finished).count() as u32;
        let entries = self.entries.iter().filter(|e| e.live).count() as u32;
        (instances, entries)
    }
}

/// Read-only view of the particle runtime (resource), for gameplay and UI queries.
#[derive(Debug, Default)]
pub struct ParticleRuntime {
    pub(crate) rt: Runtime,
}

impl ParticleRuntime {
    /// State of the effect instance playing on `entity` (`None` without one).
    pub fn state(&self, entity: Entity) -> Option<EffectState> {
        self.rt.state(entity)
    }
}
