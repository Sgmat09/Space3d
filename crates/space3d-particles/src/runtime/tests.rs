//! Runtime + CPU simulation tests without a GPU (design-particles §13 unit tests).

use super::*;
use crate::components::{EffectSpawn, SpawnEffect};
use crate::cpu::{CpuSim, Particle};
use crate::curve::FloatRange;
use crate::effect::{Burst, EmitShape, ParticleLayer, Spawn};
use proptest::prelude::*;
use space3d_core::glam::Vec3;

const DT: f32 = 1.0 / 60.0;

struct Harness {
    world: World,
    effects: ParticleEffects,
    settings: ParticleSettings,
    rt: Runtime,
    sim: CpuSim,
}

impl Harness {
    fn new() -> Self {
        Self {
            world: World::new(),
            effects: ParticleEffects::default(),
            settings: ParticleSettings::default(),
            rt: Runtime::default(),
            sim: CpuSim::default(),
        }
    }
    fn add(&mut self, e: ParticleEffect) -> EffectHandle {
        self.effects.add(e).unwrap()
    }
    fn tick(&mut self) {
        self.rt.tick(&self.world, &self.settings, &self.effects, DT);
    }
    /// Tick and replay (a renderer drawing every tick).
    fn step(&mut self) {
        self.tick();
        self.sim.replay(&mut self.rt, self.settings.max_catchup_ticks);
    }
    fn alive(&self) -> Vec<(usize, Particle)> {
        self.sim
            .pool
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, p)| p.alive() && p.pos_age[3] * p.vel_inv_life[3] < 1.0)
            .collect()
    }
}

fn burst(count: u32, life: f32) -> ParticleEffect {
    ParticleEffect {
        duration: 0.0,
        ..ParticleEffect::new("burst").layer(ParticleLayer::burst(count, life).speed((1.0, 2.0)))
    }
}

/// A burst of life 0.5 s emitted on tick 1 finishes on tick 1 + ceil(0.5 · 60) + 1 = 32,
/// exactly, and no particle is alive then.
#[test]
fn finish_on_the_exact_tick() {
    let mut h = Harness::new();
    let fx = h.add(burst(10, 0.5));
    let e = h.world.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO));
    for t in 1..=40u64 {
        h.step();
        let done = h.rt.finished.iter().any(|f| f.entity == e);
        assert_eq!(done, t == 32, "tick {t}");
        if t == 1 {
            assert_eq!(h.alive().len(), 10);
        }
        if t == 29 {
            assert_eq!(h.alive().len(), 10, "alive until their life ends");
        }
        if t >= 32 {
            assert!(h.alive().is_empty(), "tick {t}");
            let s = h.rt.state(e).unwrap();
            assert!(s.finished && !s.emitting && s.reserved == 0);
        }
    }
    assert_eq!(h.rt.pool.reserved(), 0, "the ring was returned");
}

/// Particle `k` of a layer instance lands in slot `base + k mod capacity`, with a 64-bit spawn
/// index.
#[test]
fn ring_slots_follow_spawn_indices() {
    let mut h = Harness::new();
    let mut layer = ParticleLayer::continuous(600.0, 1.0);
    layer.capacity = Some(16);
    let fx = h.add(ParticleEffect {
        looping: true,
        ..ParticleEffect::new("ring").layer(layer)
    });
    h.world.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO).keep());
    h.step();
    let e = h.rt.entries.iter().position(|e| e.live).unwrap();
    // Jump the spawn index across 2³²: slots keep following index mod capacity.
    h.rt.entries[e].spawned = (1u64 << 32) - 3;
    h.step();
    let entry = h.rt.entries[e];
    assert_eq!(entry.spawned, (1u64 << 32) - 3 + 10, "600/s at 60 Hz: 10 per tick");
    for index in entry.spawned - 10..entry.spawned {
        let slot = entry.base as usize + (index % 16) as usize;
        let p = h.sim.pool[slot];
        assert!(p.alive());
        assert_eq!(p.misc[1], crate::rng::particle_seed(entry.seed, index), "index {index}");
    }
    assert!(h.sim.overwrites > 0, "a 16-slot ring at 600/s × 1 s overwrites");
}

/// An emitter whose parent disappears stops emitting where it was, drains and finishes.
#[test]
fn orphaned_emitters_stop_drain_and_finish() {
    let mut h = Harness::new();
    let mut layer = ParticleLayer::continuous(60.0, 0.25);
    layer.speed = FloatRange::constant(1.0);
    let fx = h.add(ParticleEffect {
        looping: true,
        ..ParticleEffect::new("trail").layer(layer)
    });
    let ship = h.world.spawn((Transform::from_xyz(5.0, 0.0, 0.0),));
    let e = h
        .world
        .spawn_effect(fx, EffectSpawn::at(Vec3::new(0.0, 0.0, 1.0)).follow(ship));
    for _ in 0..10 {
        h.step();
    }
    assert_eq!(
        h.rt.instances[h.rt.by_entity[&e] as usize].pos,
        Vec3::new(5.0, 0.0, 1.0)
    );
    h.world.despawn(ship).unwrap();
    let spawned = h.rt.spawned;
    let mut finished_at = None;
    for t in 0..40 {
        h.step();
        assert_eq!(h.rt.spawned, spawned, "orphans do not emit");
        if h.rt.finished.iter().any(|f| f.entity == e) {
            finished_at = Some(t);
            break;
        }
    }
    let t = finished_at.expect("the orphan finished");
    assert_eq!(
        t + 1,
        window_ticks(0.25, DT) as usize,
        "one window after its last emission"
    );
    assert!(h.alive().is_empty());
}

/// Removing the emitter drains (`OnRemove::Drain`, no event) or clears (`OnRemove::Clear`, the
/// ring is zeroed at that tick).
#[test]
fn removal_drains_or_clears() {
    for clear in [false, true] {
        let mut h = Harness::new();
        let fx = h.add(ParticleEffect {
            looping: true,
            ..ParticleEffect::new("loop").layer(ParticleLayer::continuous(120.0, 1.0))
        });
        let e = h.world.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO).keep());
        if clear {
            h.world.get::<&mut ParticleEmitter>(e).unwrap().on_remove = OnRemove::Clear;
        }
        for _ in 0..30 {
            h.step();
        }
        let alive = h.alive().len();
        assert!(alive >= 59, "{alive}");
        h.world.despawn(e).unwrap();
        h.step();
        assert!(h.rt.state(e).is_none(), "detached from the entity");
        if clear {
            assert!(h.alive().is_empty(), "cleared at once");
            assert_eq!(h.rt.pool.reserved(), 0);
        } else {
            assert_eq!(h.alive().len(), alive, "draining: nothing emitted, nothing died yet");
            for _ in 0..70 {
                h.step();
                assert!(h.rt.finished.is_empty(), "no event without an entity");
            }
            assert!(h.alive().is_empty());
            assert_eq!(h.rt.pool.reserved(), 0, "freed after draining");
        }
    }
}

/// The emitter frame comes from `Transform`s through the `Parent` chain (exact per tick).
#[test]
fn world_matrix_through_parent_chains() {
    let mut h = Harness::new();
    let fx = h.add(burst(1, 1.0));
    let root = h.world.spawn((Transform::from_xyz(10.0, 0.0, 0.0)
        .with_rotation(Quat::from_rotation_y(std::f32::consts::FRAC_PI_2))
        .with_scale(Vec3::splat(2.0)),));
    let mid = h.world.spawn((Transform::from_xyz(0.0, 1.0, 0.0), Parent(root)));
    let e = h.world.spawn_effect(
        fx,
        EffectSpawn::from_transform(Transform::from_xyz(0.0, 0.0, -5.0)).follow(mid),
    );
    h.tick();
    let inst = h.rt.instances[h.rt.by_entity[&e] as usize];
    // (0, 1, −5) scaled by 2 and turned 90° about Y: (−10, 2, 0), then + (10, 0, 0).
    assert!((inst.pos - Vec3::new(0.0, 2.0, 0.0)).length() < 1e-4, "{}", inst.pos);
    assert!((inst.rot * Vec3::NEG_Z - Vec3::NEG_X).length() < 1e-5);
    let row = h.rt.log.rows.iter().find(|r| r.instance == h.rt.by_entity[&e]).unwrap();
    assert_eq!(row.pos, inst.pos);
}

/// Without a consumer the log keeps only the catch-up window; replaying it from an empty pool
/// gives exactly the pool of a replay of every tick.
#[test]
fn catch_up_reset_equals_full_replay() {
    let setup = || {
        let mut h = Harness::new();
        let mut plume = ParticleLayer::continuous(90.0, (0.2, 0.4));
        plume.shape = EmitShape::Cone {
            angle: 0.3,
            radius: 0.5,
        };
        plume.speed = FloatRange::new(5.0, 9.0);
        plume.drag = 2.0;
        plume.gravity = Vec3::new(0.0, -3.0, 0.0);
        plume.spawn.rate_per_distance = 2.0;
        let looping = h.add(ParticleEffect {
            looping: true,
            ..ParticleEffect::new("plume").layer(plume)
        });
        let pops = h.add(ParticleEffect {
            duration: 0.0,
            ..ParticleEffect::new("pop").layer(ParticleLayer {
                shape: EmitShape::Sphere {
                    radius: 1.0,
                    inner: 0.0,
                },
                speed: FloatRange::new(2.0, 6.0),
                particle_delay: FloatRange::new(0.0, 0.1),
                ..ParticleLayer::burst((5, 9), (0.3, 0.6))
            })
        });
        let e = h.world.spawn_effect(looping, EffectSpawn::at(Vec3::ZERO).keep());
        (h, e, pops)
    };
    let drive = |h: &mut Harness, e: Entity, pops: EffectHandle, t: u32| {
        h.world.get::<&mut Transform>(e).unwrap().translation =
            Vec3::new((t as f32 * 0.05).sin() * 4.0, 0.0, t as f32 * 0.02);
        if t.is_multiple_of(17) {
            h.world
                .spawn_effect(pops, EffectSpawn::at(Vec3::new(t as f32 * 0.1, 1.0, 0.0)));
        }
    };
    let (mut every, e1, p1) = setup();
    let (mut once, e2, p2) = setup();
    for t in 0..300 {
        drive(&mut every, e1, p1, t);
        every.step();
        drive(&mut once, e2, p2, t);
        once.tick();
        let window = once
            .rt
            .entries
            .iter()
            .filter(|e| e.live)
            .map(|e| window_ticks(e.sim.window, DT))
            .max()
            .unwrap();
        assert!(once.rt.log.frames.len() <= window as usize + 1, "bounded log");
    }
    assert!(once.rt.log.reset, "the log was trimmed");
    once.sim.replay(&mut once.rt, 1200);
    assert_eq!(once.sim.resets, 1);
    // Entry indices (`misc[2]`) are internal handles whose reuse follows the replays; the
    // particles themselves are identical.
    let strip = |v: Vec<(usize, Particle)>| {
        v.into_iter()
            .map(|(slot, mut p)| {
                p.misc[2] = 0;
                (slot, p)
            })
            .collect::<Vec<_>>()
    };
    let (a, b) = (strip(every.alive()), strip(once.alive()));
    assert!(a.len() > 20, "{}", a.len());
    assert_eq!(a, b, "identical alive sets, slots and state");
}

/// Prewarm synthesizes the past emission: a looping 60/s effect prewarmed by 1 s starts full.
#[test]
fn prewarm_fills_the_pool_at_once() {
    let mut h = Harness::new();
    let fx = h.add(ParticleEffect {
        looping: true,
        ..ParticleEffect::new("ambient").layer(ParticleLayer::continuous(60.0, 1.0).speed(1.0))
    });
    h.world
        .spawn_effect(fx, EffectSpawn::at(Vec3::ZERO).prewarm(1.0).keep());
    h.step();
    let alive = h.alive();
    assert!((58..=61).contains(&alive.len()), "{}", alive.len());
    let (lo, hi) = alive
        .iter()
        .map(|(_, p)| p.pos_age[3])
        .fold((f32::MAX, f32::MIN), |(a, b), x| (a.min(x), b.max(x)));
    assert!(lo < 0.05 && hi > 0.9, "ages spread over the prewarm: {lo}..{hi}");
}

/// Over the budget, rings shrink (to 1/8) before they are refused; low priorities stop at
/// 75 % of the pool.
#[test]
fn budget_shrinks_then_refuses() {
    let mut h = Harness::new();
    h.settings.max_particles_cpu = 100;
    let mut layer = ParticleLayer::burst(1, 1.0);
    layer.capacity = Some(80);
    let fx = h.add(ParticleEffect::new("big").layer(layer.clone()));
    let low = h.add(ParticleEffect {
        priority: crate::effect::EffectPriority::Low,
        ..ParticleEffect::new("low").layer(layer)
    });
    h.world.spawn_effect(fx, Vec3::ZERO);
    h.tick();
    assert_eq!((h.rt.pool.reserved(), h.rt.shrunk, h.rt.refused), (80, 0, 0));
    h.world.spawn_effect(fx, Vec3::ZERO);
    h.tick();
    assert_eq!((h.rt.pool.reserved(), h.rt.shrunk), (90, 1), "shrunk to 80/8 = 10");
    h.world.spawn_effect(low, Vec3::ZERO);
    h.tick();
    assert_eq!(h.rt.refused, 1, "past 75 % for low priority");
    assert_eq!(h.rt.pool.reserved(), 90);
}

/// Restarting (bumping `restart`) or stopping and playing again start the effect over; the old
/// particles drain.
#[test]
fn restart_and_play_again() {
    let mut h = Harness::new();
    let fx = h.add(burst(4, 0.2));
    let e = h.world.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO).keep());
    h.step();
    assert_eq!(h.rt.spawned, 4);
    h.world.get::<&mut ParticleEmitter>(e).unwrap().restart();
    h.step();
    assert_eq!(h.rt.spawned, 8, "restarted");
    assert_eq!(h.alive().len(), 8, "the old burst drains");
    for _ in 0..20 {
        h.step();
    }
    assert!(h.rt.state(e).unwrap().finished);
    h.world.get::<&mut ParticleEmitter>(e).unwrap().stop();
    h.step();
    h.world.get::<&mut ParticleEmitter>(e).unwrap().playing = true;
    h.step();
    assert_eq!(h.rt.spawned, 12, "played again after it finished");
}

/// Delayed particles age hidden in place; gravity and drag integrate like the closed form.
#[test]
fn delays_gravity_and_drag() {
    let mut h = Harness::new();
    let fx = h.add(ParticleEffect {
        duration: 0.0,
        ..ParticleEffect::new("fall").layer(ParticleLayer {
            particle_delay: FloatRange::constant(0.1),
            gravity: Vec3::new(0.0, -10.0, 0.0),
            drag: 0.5,
            ..ParticleLayer::burst(1, 2.0)
        })
    });
    h.world.spawn_effect(fx, Vec3::ZERO);
    for _ in 0..5 {
        h.step();
        let (_, p) = h.alive()[0];
        assert!(p.pos_age[3] < 0.0 && p.pos() == Vec3::ZERO, "hidden while delayed");
    }
    for _ in 0..66 {
        h.step();
    }
    let (_, p) = h.alive()[0];
    let t = p.pos_age[3];
    // v(t) = −g(1 − e^{−kt})/k, falling from rest.
    let v = -10.0 * (1.0 - (-0.5 * t).exp()) / 0.5;
    assert!((p.vel().y - v).abs() < 0.05 * v.abs(), "{} vs {v}", p.vel().y);
    assert!(p.pos().y < 0.0 && p.pos().x == 0.0);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]
    /// The computed ring capacity holds every particle alive at once (no early overwrite) for
    /// random rates, bursts, lifetimes and delays.
    #[test]
    fn capacity_bound_holds(
        rate in 0.0f32..400.0,
        count in 0u32..40,
        cycles in 0u32..4,
        interval in 0.0f32..0.4,
        life in (0.02f32..1.5, 0.0f32..1.0),
        delay in (0.0f32..0.3, 0.0f32..0.3),
        looping in any::<bool>(),
    ) {
        let mut h = Harness::new();
        let layer = ParticleLayer {
            spawn: Spawn {
                rate,
                bursts: vec![Burst { time: 0.05, count: (count / 2, count).into(), cycles, interval }],
                ..Spawn::default()
            },
            lifetime: FloatRange::new(life.0, life.0 + life.1),
            particle_delay: FloatRange::new(delay.0, delay.0 + delay.1),
            ..ParticleLayer::default()
        };
        let fx = h.add(ParticleEffect { looping, duration: 1.0, ..ParticleEffect::new("p").layer(layer) });
        h.world.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO).keep());
        for _ in 0..150 {
            h.step();
        }
        prop_assert_eq!(h.sim.overwrites, 0);
    }
}
