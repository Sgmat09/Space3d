//! Test hooks (not part of the public API; hidden from the docs): read the simulated pool back
//! for the backend parity tests, and pretend a smaller storage binding limit (the renderer's
//! `TEST_LIMITS` pattern) for the budget tests.

use crate::ParticleBackend;
use crate::render::ParticlesNode;
use space3d_core::App;
use space3d_core::glam::Vec3;
use std::cell::Cell;
use std::sync::{Arc, Mutex};

/// The plugin's render node (resource).
pub(crate) struct NodeHandle(pub Arc<Mutex<ParticlesNode>>);

thread_local! {
    static STORAGE_LIMIT: Cell<Option<u64>> = const { Cell::new(None) };
}

/// Pretend the device's storage bindings (and buffers) hold at most `bytes` on this thread
/// (`None`: the device's limits). The GPU pool budget follows; the device itself is unchanged.
pub fn set_storage_limit(bytes: Option<u64>) {
    STORAGE_LIMIT.set(bytes);
}

pub(crate) fn storage_limit() -> Option<u64> {
    STORAGE_LIMIT.get()
}

/// A live pool slot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SlotState {
    /// Pool slot.
    pub slot: u32,
    /// Position (simulation space).
    pub pos: Vec3,
    /// Velocity.
    pub vel: Vec3,
    /// Age in seconds (negative: delayed).
    pub age: f32,
    /// Lifetime in seconds.
    pub life: f32,
    /// Particle seed.
    pub seed: u32,
    /// Layer instance.
    pub entry: u32,
}

/// The backend of the last frame and every live slot of its pool (the GPU pool is copied and
/// waited for: tests only). `None` before the first frame or without a renderer.
pub fn read_pool(app: &App) -> Option<(ParticleBackend, Vec<SlotState>)> {
    let node = app.resources.get::<NodeHandle>()?.0.clone();
    let renderer = app.resources.get::<space3d_render::Renderer>()?;
    let node = node.lock().unwrap_or_else(|e| e.into_inner());
    let (backend, pool) = node.read_pool(renderer.device(), renderer.queue())?;
    let slots = pool
        .iter()
        .enumerate()
        .filter(|(_, p)| p.vel_life[3] != 0.0)
        .map(|(i, p)| SlotState {
            slot: i as u32,
            pos: p.pos(),
            vel: p.vel(),
            age: p.pos_age[3],
            life: p.vel_life[3],
            seed: p.misc[1],
            entry: p.misc[2],
        })
        .collect();
    Some((backend, slots))
}

/// Size in bytes of the GPU backend's pool buffer (`None` before it exists).
pub fn gpu_pool_bytes(app: &App) -> Option<u64> {
    let node = app.resources.get::<NodeHandle>()?.0.clone();
    let mut node = node.lock().unwrap_or_else(|e| e.into_inner());
    node.gpu_sim()?.pool_buffer().map(|b| b.size())
}

/// The particle load of graphics spec §4.4's "Battle" reference scene, scaled by its fields
/// (used by the battle calibration test, the perf test and the `Engine/Particles/Stress/Battle`
/// story): ships flying circles around the flight plane (on seven levels 10 units apart, so
/// crossing ships pass over each other) with three engine exhausts each (every 30th
/// boosting with two afterburners: ribbons), a laser bolt with its trail every tick, missiles
/// with smoke trails ending in explosions, held lasers and repair beams between ships, and a
/// stream of one-shots — explosions, hit sparks, shield ripples, debris, warps (distortion
/// shockwaves) and EMPs — at random ships, all driven by the fixed tick (deterministic).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Battle {
    /// Ships (three engine exhausts each). Default 300.
    pub ships: u32,
    /// Radius of the disc (world units, on the XZ plane around the origin) the ships fly in.
    /// Default 1200.
    pub radius: f32,
    /// Explosions per second (plus one per missile hit). Default 30: with the missiles' 10,
    /// about 20 light flashes at any time, the reference scene's 20 point lights.
    pub explosions: f32,
    /// Hit sparks per second (half with a shield ripple). Default 120.
    pub hits: f32,
    /// Held lasers and repair beams between ships. Default (12, 4).
    pub beams: (u32, u32),
    /// Laser bolts (one per tick, 32 alive) and missiles (one every 6 ticks, 12 alive).
    pub bolts: bool,
    /// Seed of the placements.
    pub seed: u32,
}

impl Default for Battle {
    fn default() -> Self {
        Self {
            ships: 300,
            radius: 1200.0,
            explosions: 30.0,
            hits: 120.0,
            beams: (12, 4),
            bolts: true,
            seed: 1,
        }
    }
}

/// What a [`battle`] spawned (resource): its ships and the explosions (entity, tick).
#[derive(Clone, Debug, Default)]
pub struct BattleLog {
    /// The ships (entities with a `Transform`, facing −Z).
    pub ships: Vec<space3d_core::Entity>,
    /// Every explosion spawned: its entity and tick.
    pub explosions: Vec<(space3d_core::Entity, u64)>,
}

/// Build the [`Battle`] into `app` (after `ParticlesPlugin`): its ships and beams now, its
/// one-shots on the fixed tick. Inserts a [`BattleLog`].
pub fn battle(app: &mut App, b: Battle) {
    use crate::components::{BeamEnd, EffectSpawn, ParticleEmitter, SpawnEffect};
    use crate::registry::builtin;
    use crate::rng::{mix, rnd};
    use space3d_core::glam::{Quat, Vec4};
    use space3d_core::{Entity, Stage, SystemCtx, Transform};

    let tints = [
        Vec4::new(0.3, 0.9, 2.0, 1.0),
        Vec4::new(2.0, 0.9, 0.2, 1.0),
        Vec4::new(0.4, 2.0, 0.6, 1.0),
    ];
    let lasers = [
        Vec4::new(1.0, 0.0, 0.0, 1.0),
        Vec4::new(0.0, 0.5, 1.0, 1.0),
        Vec4::new(0.2, 1.0, 0.1, 1.0),
    ];
    // Ship k: anchor, circle radius, angular speed (rad per tick) and phase.
    #[derive(Clone, Copy)]
    struct Orbit {
        anchor: Vec3,
        r: f32,
        w: f32,
        phase: f32,
    }
    let seed = b.seed;
    let orbits: Vec<Orbit> = (0..b.ships)
        .map(|k| {
            let s = mix(seed, k);
            let a = rnd(s, 1) * std::f32::consts::TAU;
            let d = b.radius * rnd(s, 2).sqrt();
            // Seven flight levels 10 units apart (−30..30, more than a hull is tall): crossing
            // circles pass over each other instead of through, but for one pair in seven.
            let level = ((rnd(s, 7) * 7.0).floor().min(6.0) - 3.0) * 10.0;
            Orbit {
                anchor: Vec3::new(a.cos() * d, level, a.sin() * d),
                r: 40.0 + 80.0 * rnd(s, 3),
                w: (0.004 + 0.006 * rnd(s, 4)) * if rnd(s, 5) < 0.5 { 1.0 } else { -1.0 },
                phase: rnd(s, 6) * std::f32::consts::TAU,
            }
        })
        .collect();
    let pose = move |o: &Orbit, tick: u64| {
        let a = o.phase + o.w * tick as f32;
        let p = o.anchor + Vec3::new(a.cos(), 0.0, a.sin()) * o.r;
        // Heading: the circle's tangent (local −Z along it).
        let dir = Vec3::new(-a.sin(), 0.0, a.cos()) * o.w.signum();
        (p, Quat::from_rotation_arc(Vec3::NEG_Z, dir))
    };
    let mut ships = Vec::with_capacity(b.ships as usize);
    for (k, o) in orbits.iter().enumerate() {
        let (p, r) = pose(o, 0);
        let ship = app.world.spawn((Transform::from_xyz(p.x, p.y, p.z).with_rotation(r),));
        let s = mix(seed ^ 0x5eed, k as u32);
        let tint = tints[k % 3];
        let boost = k % 30 == 0;
        for x in [-8.0, 0.0, 8.0] {
            let nozzle = Transform::from_xyz(x, 0.0, 21.0);
            let e = app.spawn_effect(
                builtin::ENGINE_EXHAUST,
                EffectSpawn::from_transform(nozzle).follow(ship).tint(tint).keep(),
            );
            if let Ok(mut em) = app.world.get::<&mut ParticleEmitter>(e) {
                em.rate_scale = 0.9 + 0.8 * rnd(s, 1);
            }
            if boost && x != 0.0 {
                app.spawn_effect(
                    builtin::ENGINE_AFTERBURNER,
                    EffectSpawn::from_transform(nozzle).follow(ship).tint(tint).keep(),
                );
            }
        }
        ships.push(ship);
    }
    // Held lasers and repair beams between a ship and its nearest neighbour.
    let n = ships.len();
    for i in 0..(b.beams.0 + b.beams.1) as usize {
        if n < 2 {
            break;
        }
        let k = (i * 7) % n;
        let near = (0..n)
            .filter(|&j| j != k)
            .min_by(|&x, &y| {
                let d = |j: usize| orbits[j].anchor.distance_squared(orbits[k].anchor);
                d(x).total_cmp(&d(y))
            })
            .unwrap_or(k);
        let (from, to) = (ships[k], ships[near]);
        let (fx, tint) = if i < b.beams.0 as usize {
            (builtin::LASER_BEAM, lasers[i % 3])
        } else {
            (builtin::REPAIR_BEAM, Vec4::ONE)
        };
        app.spawn_effect(
            fx,
            EffectSpawn::from_transform(Transform::from_xyz(0.0, 0.0, -20.0))
                .follow(from)
                .beam_to(BeamEnd::Entity(to))
                .tint(tint)
                .keep(),
        );
    }
    app.insert_resource(BattleLog {
        ships: ships.clone(),
        explosions: Vec::new(),
    });
    // Bolts and missiles in flight: (entity, velocity, last tick).
    let mut flying: Vec<(Entity, Vec3, u64, bool)> = Vec::new();
    let every = |per_second: f32| (60.0 / per_second.max(1e-3)).round().max(1.0) as u64;
    let (explosion_every, hit_every) = (every(b.explosions), b.hits / 60.0);
    app.add_system(Stage::FixedUpdate, move |ctx: &mut SystemCtx| {
        let tick = ctx.time().tick;
        // Ships.
        for (o, &ship) in orbits.iter().zip(&ships) {
            let (p, r) = pose(o, tick);
            if let Ok(mut t) = ctx.world.get::<&mut Transform>(ship) {
                t.translation = p;
                t.rotation = r;
            }
        }
        let n = ships.len();
        if n == 0 {
            return;
        }
        let s = mix(seed ^ 0xba77, tick as u32);
        let ship_at = |ctx: &SystemCtx, i: u32| {
            let e = ships[(mix(s, i) % n as u32) as usize];
            let t = ctx.world.get::<&Transform>(e).map(|t| *t).unwrap_or_default();
            (e, t)
        };
        // Bolts and missiles: move, then end (a missile explodes).
        let dt = ctx.time().fixed_delta_secs();
        let mut k = 0;
        while k < flying.len() {
            let (e, v, end, missile) = flying[k];
            if tick >= end {
                let at = ctx
                    .world
                    .get::<&Transform>(e)
                    .map(|t| t.translation)
                    .unwrap_or_default();
                let _ = ctx.world.despawn(e);
                if missile {
                    let x = ctx.spawn_effect(builtin::EXPLOSION, EffectSpawn::at(at).scale(0.8));
                    if let Some(log) = ctx.resources.get_mut::<BattleLog>() {
                        log.explosions.push((x, tick));
                    }
                }
                flying.swap_remove(k);
                continue;
            }
            if let Ok(mut t) = ctx.world.get::<&mut Transform>(e) {
                t.translation += v * dt;
            }
            k += 1;
        }
        if b.bolts {
            // A laser bolt every tick (32 ticks of flight at 1500 u/s).
            let (_, t) = ship_at(ctx, 1);
            let dir = t.rotation * Vec3::NEG_Z;
            let at = t.translation + dir * 25.0;
            let bolt = ctx.world.spawn((Transform::from_xyz(at.x, at.y, at.z),));
            ctx.spawn_effect(
                builtin::LASER_TRAIL,
                EffectSpawn::at(Vec3::ZERO)
                    .follow(bolt)
                    .tint(lasers[(tick % 3) as usize]),
            );
            flying.push((bolt, dir * 1500.0, tick + 32, false));
            // A missile every 6 ticks (72 ticks at 600 u/s, then an explosion).
            if tick.is_multiple_of(6) {
                let (_, t) = ship_at(ctx, 2);
                let dir = t.rotation * Vec3::NEG_Z;
                let at = t.translation + dir * 25.0;
                let missile = ctx
                    .world
                    .spawn((Transform::from_xyz(at.x, at.y, at.z).with_rotation(t.rotation),));
                ctx.spawn_effect(builtin::MISSILE_TRAIL, EffectSpawn::at(Vec3::ZERO).follow(missile));
                flying.push((missile, dir * 600.0, tick + 72, true));
            }
        }
        // One-shots.
        if tick.is_multiple_of(explosion_every) {
            let (_, t) = ship_at(ctx, 3);
            let x = ctx.spawn_effect(
                builtin::EXPLOSION,
                EffectSpawn::at(t.translation).scale(0.6 + 0.8 * rnd(s, 4)),
            );
            if let Some(log) = ctx.resources.get_mut::<BattleLog>() {
                log.explosions.push((x, tick));
            }
        }
        let hits = (hit_every * (tick + 1) as f32).floor() - (hit_every * tick as f32).floor();
        for h in 0..hits as u32 {
            let (ship, t) = ship_at(ctx, 10 + h);
            let from = Vec3::new(rnd(s, 20 + h) - 0.5, 0.0, rnd(s, 40 + h) - 0.5);
            ctx.spawn_effect(
                builtin::HIT_SPARK,
                EffectSpawn::at(t.translation + from.normalize_or_zero() * 12.0)
                    .facing(from)
                    .tint(lasers[(h % 3) as usize]),
            );
            if h % 2 == 0 {
                ctx.spawn_effect(
                    builtin::SHIELD_RIPPLE,
                    EffectSpawn::at(Vec3::ZERO).facing(from).scale(1.2).follow(ship),
                );
            }
        }
        if tick.is_multiple_of(12) {
            let (_, t) = ship_at(ctx, 5);
            ctx.spawn_effect(builtin::DEBRIS_SPARKS, EffectSpawn::at(t.translation).facing(Vec3::Y));
        }
        if tick.is_multiple_of(60) {
            let (_, a) = ship_at(ctx, 6);
            let (_, c) = ship_at(ctx, 7);
            ctx.spawn_effect(
                builtin::WARP_IN,
                EffectSpawn::at(a.translation + Vec3::X * 60.0).scale(0.5),
            );
            ctx.spawn_effect(
                builtin::WARP_OUT,
                EffectSpawn::at(c.translation - Vec3::X * 60.0).scale(0.5),
            );
        }
        if tick.is_multiple_of(150) {
            let (_, t) = ship_at(ctx, 8);
            ctx.spawn_effect(builtin::EMP, EffectSpawn::at(t.translation).scale(1.0));
        }
    });
}
