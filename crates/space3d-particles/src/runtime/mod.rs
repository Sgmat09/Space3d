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
mod spawn;
#[cfg(test)]
mod tests;

pub(crate) use alloc::RangeAllocator;
pub(crate) use layer::{LayerSim, capacity, window_ticks};
pub(crate) use log::{SpawnRecord, TickLog, WorldAttractor, XformRow};
pub(crate) use path::Path;

use crate::components::{FinishAction, OnRemove, ParticleAttractor, ParticleEmitter};
use crate::effect::{EffectPriority, MAX_LAYERS_PER_EFFECT, ParticleEffect};
use crate::registry::{EffectHandle, ParticleEffects};
use crate::stats::EffectState;
use crate::{ParticleLod, ParticleSettings, rng};
use space3d_core::glam::{Quat, Vec3, Vec4};
use space3d_core::{Entity, Parent, Transform, World};
use space3d_render::{Camera3d, RenderTarget, Visibility};
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
    pub rate_acc: u64,
    pub dist_acc: u64,
    pub seed: u32,
    /// Budget shrink (`capacity / full_capacity`) applied to spawn counts.
    pub rate_mul: f32,
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
            rate_acc: 1 << 31,
            dist_acc: 1 << 31,
            seed,
            rate_mul,
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
    pub prev_pos: Vec3,
    pub prev_rot: Quat,
    pub pos: Vec3,
    pub rot: Quat,
    pub velocity: Vec3,
    /// World distance the emitter moved in the last tick (the render-time bridge's share of a
    /// tick's time-rate emission).
    pub distance: f32,
    /// Quality × LOD rate multiplier of the last tick.
    pub quality: f32,
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
    /// Entries freed at a tick, reusable once the log no longer holds that tick.
    released_entries: VecDeque<(u64, u32)>,
    free_entries: Vec<u32>,
    next_serial: u64,
    pub pool: RangeAllocator,
    pub log: TickLog,
    effects_version: u64,
    pub finished: Vec<Finished>,
    /// Particles spawned since the start.
    pub spawned: u64,
    /// Layer instances refused / shrunk since the last render.
    pub refused: u32,
    pub shrunk: u32,
    /// Collections that grew since the last render.
    pub reallocations: u32,
    warned_refused: Vec<EffectHandle>,
    warned_unknown: Vec<EffectHandle>,
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
    fn capacities(&self) -> [usize; 8] {
        [
            self.instances.capacity() + self.paths.capacity(),
            self.free_instances.capacity(),
            self.by_entity.capacity(),
            self.entries.capacity(),
            self.released_entries.capacity() + self.free_entries.capacity(),
            self.pool.capacity(),
            self.log.capacity(),
            self.finished.capacity(),
        ]
    }

    /// Particle budget of the pool.
    pub fn set_budget(&mut self, budget: u32) {
        self.pool.grow_to(budget);
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
        let limit = (self.pool.total() as f32 * priority_limit(priority)) as u32;
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
            prev_pos: pos,
            prev_rot: rot,
            pos,
            rot,
            velocity: em.velocity,
            distance: 0.0,
            quality: 1.0,
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
        self.allocate_layers(i, handle, effect);
        self.by_entity.insert(entity, i);
        i
    }

    fn allocate_layers(&mut self, i: u32, handle: EffectHandle, effect: &ParticleEffect) {
        let seed = self.instances[i as usize].seed;
        let mut n = 0usize;
        for (li, layer) in effect.layers.iter().enumerate().take(MAX_LAYERS_PER_EFFECT) {
            if !layer.enabled {
                continue;
            }
            let cap = capacity(layer);
            let Some((base, c)) = self.reserve(cap, effect.priority) else {
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
            entry.full_capacity = cap;
            let e = self.alloc_entry(entry);
            self.instances[i as usize].entries[n] = e;
            n += 1;
        }
        self.instances[i as usize].n_entries = n as u8;
    }

    /// Free the layer instances of `i` (their rings now; the entry slots once the log no longer
    /// references this tick).
    fn free_layers(&mut self, i: u32) {
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
                });
            if same {
                for &e in inst.entries() {
                    let entry = &mut self.entries[e as usize];
                    entry.sim = LayerSim::new(&effect.layers[entry.layer as usize]);
                }
                self.entries_version += 1;
                let inst = &mut self.instances[i as usize];
                inst.looping = effect.looping;
                inst.duration = effect.duration;
            } else if inst.attached {
                self.detach(i, OnRemove::Drain);
            }
        }
    }

    /// Position of the primary camera (LOD), lowest order first.
    fn camera_position(world: &World) -> Option<Vec3> {
        let mut best: Option<(i32, u64, Vec3)> = None;
        for (e, cam, t, parent) in world.query::<(Entity, &Camera3d, &Transform, Option<&Parent>)>().iter() {
            if cam.target != RenderTarget::Primary {
                continue;
            }
            let key = (cam.order, e.to_bits().get());
            if best.is_none_or(|b| key < (b.0, b.1)) {
                best = Some((key.0, key.1, world_pose(world, t, parent).0));
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
        let camera = if settings.lod.near.is_finite() {
            Self::camera_position(world)
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
        for (entity, em, tr, parent, vis) in query.iter() {
            let (pos, rot, broken) = world_pose(world, tr, parent);
            let mut found = self.by_entity.get(&entity).copied();
            if let Some(i) = found {
                let inst = &self.instances[i as usize];
                if inst.effect != em.effect || inst.restart != em.restart {
                    self.detach(i, OnRemove::Drain);
                    found = None;
                }
            }
            let i = match found {
                Some(i) => i,
                None => {
                    let effect = match effects.get(em.effect) {
                        Some(effect) => effect,
                        None => {
                            // A stale handle, one never issued, or a reserved built-in with no
                            // data (`builtin::LASER_BEAM` before PA3): play nothing and finish
                            // this tick, so the event is sent and the `FinishAction` runs (a
                            // `spawn_effect` entity despawns instead of staying forever).
                            if !self.warned_unknown.contains(&em.effect) {
                                self.warned_unknown.push(em.effect);
                                tracing::warn!(
                                    handle = em.effect.index(),
                                    "particle emitter with an unknown effect handle: it plays nothing and finishes"
                                );
                            }
                            &nothing
                        }
                    };
                    let i = self.create(entity, em, em.effect, effect, pos, rot, settings);
                    if em.prewarm > 0.0 && dt > 0.0 {
                        self.prewarm(i, effect, em.prewarm, dt, settings);
                    }
                    i
                }
            };
            let inst = &mut self.instances[i as usize];
            inst.seen_tick = t;
            inst.tint = em.tint;
            inst.scale = em.scale;
            inst.rate_scale = em.rate_scale;
            inst.finish = em.finish;
            inst.on_remove = em.on_remove;
            inst.hidden = vis == Some(&Visibility::Hidden);
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
                self.allocate_layers(i, em.effect, effect);
            }
            let inst = &mut self.instances[i as usize];
            inst.playing = em.playing;
            inst.orphaned |= broken;
            inst.prev_pos = inst.pos;
            inst.prev_rot = inst.rot;
            let distance = if inst.orphaned {
                inst.velocity = Vec3::ZERO;
                0.0
            } else {
                inst.pos = pos;
                inst.rot = rot;
                let moved = pos - inst.prev_pos;
                inst.velocity = if dt > 0.0 { moved / dt } else { Vec3::ZERO } + em.velocity;
                moved.length()
            };
            let lod = camera.map_or(1.0, |c| lod_scale(&settings.lod, c.distance(pos)));
            let quality = settings.quality.clamp(0.0, 1.0) * lod;
            inst.distance = distance;
            inst.quality = quality;
            let Some(effect) = effects.get(em.effect) else {
                continue;
            };
            self.schedule(i, effect, dt, quality, distance, 0.0);
        }
        drop(query);

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
            if !emitting && dt > 0.0 && t >= inst.last_spawn_tick.saturating_add(window as u64) {
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

    /// Emission of instance `i` this tick (records into the log).
    fn schedule(&mut self, i: u32, effect: &ParticleEffect, dt: f32, quality: f32, distance: f32, extra_age: f32) {
        let inst = self.instances[i as usize];
        let emitting = inst.emitting();
        let mut spawned = 0u32;
        for &e in inst.entries() {
            let entry = &mut self.entries[e as usize];
            let layer = &effect.layers[entry.layer as usize];
            let mul = if layer.lod { quality } else { 1.0 } * entry.rate_mul;
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
        let size = s.size.0.abs().max(s.size.1.abs()) * inst.scale.abs() * sprite;
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
            mul: if c.lod { inst.quality } else { 1.0 } * e.rate_mul,
            distance: gap,
            extra_age: 0.0,
        };
        let mut acc = (e.rate_acc, e.dist_acc);
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
            },
        })
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
