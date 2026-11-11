//! CPU / GPU backend parity (graphics spec §5.2 PA2, design-particles §10 guarantee 3): after
//! the same ticks both pools hold the same particles in the same slots — identical seeds, layer
//! instances, ages and lifetimes (the values that decide births and deaths are computed exactly
//! on both), positions and velocities within 1e-3 (relative) — and the images match.

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_particles::prelude::*;
use space3d_particles::testing::{SlotState, read_pool};
use space3d_particles::{
    Burst, EmitDirection, EmitShape, Facing, Falloff, Flicker, FloatRange, Force, OnRemove, ParticleAttractor,
    ParticleBackend, SimulationSpace, Spawn, SpriteRender, SpriteShape,
};
use space3d_render::Camera3d;

const W: u32 = 256;
const H: u32 = 144;

fn settings(backend: ParticleBackend) -> ParticleSettings {
    ParticleSettings {
        backend,
        // The same pool budget on both backends (slots depend on the allocator).
        max_particles: 16_384,
        max_particles_cpu: 16_384,
        ..ParticleSettings::default()
    }
}

/// Every emission shape and direction, spread, flatten, delays, drag and gravity, the three
/// layer forces and a world attractor, local and world space, bursts with cycles, rates, rate
/// per distance on a moving emitter, inherited velocity, emitter-plane facing, flicker, a
/// prewarmed effect and an `OnRemove::Clear` removal, all driven by the tick number.
fn scene(app: &mut App) {
    app.world.spawn((
        Transform::from_xyz(0.0, 90.0, 110.0).looking_at(Vec3::ZERO, Vec3::Y),
        Camera3d::default().with_clear_color(Vec4::new(0.01, 0.01, 0.02, 1.0)),
    ));
    app.world.spawn((
        Transform::from_xyz(15.0, 5.0, -10.0),
        ParticleAttractor {
            strength: 30.0,
            radius: 40.0,
            falloff: Falloff::Linear,
            mask: 1,
        },
    ));
    let shapes = [
        EmitShape::Point,
        EmitShape::Sphere {
            radius: 4.0,
            inner: 0.5,
        },
        EmitShape::Cone {
            angle: 0.4,
            radius: 1.5,
        },
        EmitShape::Ring {
            radius: 5.0,
            width: 1.0,
            arc: 4.0,
        },
        EmitShape::Box {
            half_extents: Vec3::new(3.0, 1.0, 2.0),
        },
        EmitShape::Line { length: 8.0 },
    ];
    let directions = [
        EmitDirection::Shape,
        EmitDirection::Fixed(Vec3::new(0.3, 1.0, 0.2)),
        EmitDirection::Random,
        EmitDirection::Inward,
        EmitDirection::Tangent,
        EmitDirection::Shape,
    ];
    let forces = [
        vec![Force::Attractor {
            position: Vec3::new(0.0, 4.0, 0.0),
            strength: 12.0,
            radius: 0.0,
            falloff: Falloff::InverseSquare,
        }],
        vec![Force::Vortex {
            axis: Vec3::Y,
            strength: 9.0,
            pull: 2.0,
        }],
        vec![Force::Turbulence {
            strength: 14.0,
            frequency: 0.3,
            speed: 1.5,
        }],
        vec![Force::Curl {
            strength: 10.0,
            frequency: 0.3,
            speed: 1.5,
        }],
        vec![],
        vec![],
    ];
    let mut effect = ParticleEffect {
        looping: true,
        ..ParticleEffect::new("parity")
    };
    for (i, ((shape, direction), forces)) in shapes.into_iter().zip(directions).zip(forces).enumerate() {
        effect = effect.layer(ParticleLayer {
            shape,
            direction,
            spread: if i % 2 == 0 { 0.3 } else { 0.0 },
            flatten: if i == 4 { 0.5 } else { 0.0 },
            space: if i == 3 {
                SimulationSpace::Local
            } else {
                SimulationSpace::World
            },
            particle_delay: FloatRange::new(0.0, if i == 1 { 0.2 } else { 0.0 }),
            speed: FloatRange::new(3.0, 12.0),
            speed_scale_exponent: 0.5,
            inherit_velocity: 0.4,
            drag: if i % 3 == 0 { 1.5 } else { 0.0 },
            gravity: Vec3::new(0.0, -4.0, 0.0),
            forces,
            world_forces: if i == 5 { 1 } else { 0 },
            rotation: FloatRange::new(0.0, 6.0),
            angular_velocity: FloatRange::new(-2.0, 2.0),
            random_mirror: true,
            flicker: (i == 2).then_some(Flicker {
                amplitude: 0.5,
                frequency: 8.0,
            }),
            render: SpriteRender {
                facing: if i == 1 { Facing::EmitterPlane } else { Facing::Camera },
                ..SpriteRender::additive(SpriteShape::Glow {
                    core: 0.3,
                    hardness: 0.0,
                })
            },
            spawn: Spawn {
                rate: 40.0 + 10.0 * i as f32,
                rate_per_distance: if i == 0 { 0.5 } else { 0.0 },
                bursts: vec![Burst {
                    count: (3, 6).into(),
                    cycles: 0,
                    interval: 0.35,
                    ..Burst::default()
                }],
                ..Spawn::default()
            },
            lod: false,
            ..ParticleLayer::continuous(40.0, (0.5, 1.4))
        });
    }
    let fx = app.resources.expect_mut::<ParticleEffects>().add(effect).unwrap();
    let mover = app.spawn_effect(
        fx,
        EffectSpawn::from_transform(Transform::from_xyz(-20.0, 0.0, 0.0).with_rotation(Quat::from_rotation_z(0.4)))
            .seed(3)
            .scale(1.3)
            .keep(),
    );
    app.spawn_effect(
        fx,
        EffectSpawn::at(Vec3::new(20.0, 0.0, 10.0))
            .facing(Vec3::new(1.0, 0.2, 0.0))
            .seed(4)
            .prewarm(0.5)
            .keep(),
    );
    app.add_system(Stage::FixedUpdate, move |ctx: &mut SystemCtx| {
        let tick = ctx.time().tick;
        let a = tick as f32 * 0.05;
        if let Ok(mut t) = ctx.world.get::<&mut Transform>(mover) {
            t.translation = Vec3::new(-20.0 + a.sin() * 15.0, a.cos() * 3.0, a * 2.0);
            t.rotation = Quat::from_rotation_y(a) * Quat::from_rotation_z(0.4);
        }
        if tick.is_multiple_of(25) {
            ctx.spawn_effect(
                builtin::EXPLOSION,
                EffectSpawn::at(Vec3::new((tick % 50) as f32 - 10.0, 0.0, -20.0)).scale(0.3),
            );
        }
        if tick == 50 {
            // Cleared at once (its slots are freed and reused).
            let e = ctx.spawn_effect(builtin::HIT_SPARK, EffectSpawn::at(Vec3::new(0.0, 3.0, 0.0)).keep());
            if let Ok(mut em) = ctx.world.get::<&mut ParticleEmitter>(e) {
                em.on_remove = OnRemove::Clear;
            }
        }
        if tick == 60 {
            let hits: Vec<Entity> = ctx
                .world
                .query::<(Entity, &ParticleEmitter)>()
                .iter()
                .filter(|(_, em)| em.effect == builtin::HIT_SPARK)
                .map(|(e, _)| e)
                .collect();
            for e in hits {
                let _ = ctx.world.remove_one::<ParticleEmitter>(e);
            }
        }
    });
}

/// Run `ticks` ticks on `backend` (rendering every tick, or once at the end) and read the pool.
fn run(backend: ParticleBackend, ticks: u64, every_tick: bool) -> Option<(Vec<SlotState>, ImageData, ParticleStats)> {
    let mut app = app_with(W, H, settings(backend))?;
    let actual = app
        .resources
        .expect::<space3d_render::Renderer>()
        .graphics_caps()
        .compute;
    if backend == ParticleBackend::Gpu && !actual {
        eprintln!("skipping: the device has no compute");
        return None;
    }
    scene(&mut app);
    if every_tick {
        app.step_ticks(ticks);
    } else {
        // Without the renderer in the loop: one frame replays every tick at the end.
        let renderer = app.resources.remove::<space3d_render::Renderer>().unwrap();
        app.step_ticks(ticks);
        app.resources.insert(renderer);
    }
    let img = render(&mut app);
    let stats = stats(&app);
    let (used, pool) = read_pool(&app).expect("pool");
    assert_eq!(used, backend, "{stats:?}");
    assert_eq!(gpu_errors(&app), 0);
    Some((pool, img, stats))
}

fn close(a: Vec3, b: Vec3) -> bool {
    (a - b).abs().max_element() <= 1e-3 * a.abs().max_element().max(1.0)
}

/// `same_entries`: layer instance indices match too (their reuse depends on when the log is
/// replayed, so a catch-up replay may number them differently).
#[track_caller]
fn assert_parity(cpu: &[SlotState], gpu: &[SlotState], same_entries: bool) {
    assert!(cpu.len() > 300, "{} particles", cpu.len());
    let slots = |p: &[SlotState]| p.iter().map(|s| s.slot).collect::<Vec<_>>();
    assert_eq!(slots(cpu), slots(gpu), "identical alive sets and slots");
    let mut worst = 0.0f32;
    for (c, g) in cpu.iter().zip(gpu) {
        assert_eq!(c.seed, g.seed, "slot {}", c.slot);
        assert!(
            !same_entries || c.entry == g.entry,
            "slot {}: entry {} vs {}",
            c.slot,
            c.entry,
            g.entry
        );
        assert_eq!(
            (c.age, c.life),
            (g.age, g.life),
            "slot {}: exact ages and lifetimes",
            c.slot
        );
        assert!(close(c.pos, g.pos), "slot {}: position {} vs {}", c.slot, c.pos, g.pos);
        assert!(close(c.vel, g.vel), "slot {}: velocity {} vs {}", c.slot, c.vel, g.vel);
        worst = worst.max((c.pos - g.pos).abs().max_element() / c.pos.abs().max_element().max(1.0));
    }
    eprintln!(
        "{} particles; worst relative position difference {worst:.2e}",
        cpu.len()
    );
}

#[test]
fn cpu_and_gpu_pools_are_identical() {
    let Some((cpu, cpu_img, _)) = run(ParticleBackend::Cpu, 150, true) else {
        return;
    };
    let Some((gpu, gpu_img, stats)) = run(ParticleBackend::Gpu, 150, true) else {
        return;
    };
    assert_parity(&cpu, &gpu, true);
    dump(&cpu_img, "parity_cpu");
    dump(&gpu_img, "parity_gpu");
    // The images: the same sprites in the same order, within the golden tolerance.
    let diff = space3d_test::compare_images(&cpu_img, &gpu_img, &space3d_test::CompareOptions::default());
    eprintln!("CPU vs GPU image: {}", diff.summary());
    dump(&space3d_test::diff_image(&cpu_img, &gpu_img, 2), "parity_diff");
    assert!(diff.passed, "{}", diff.summary());
    assert_eq!(stats.backend, Some(ParticleBackend::Gpu));
}

/// The GPU backend rendering once after 400 ticks (its log trimmed to the catch-up window, the
/// pool reset and the window replayed in one frame) gives the CPU backend's pool of a frame per
/// tick.
#[test]
fn gpu_catch_up_matches_cpu_every_tick() {
    let Some((cpu, _, _)) = run(ParticleBackend::Cpu, 400, true) else {
        return;
    };
    let Some((gpu, _, stats)) = run(ParticleBackend::Gpu, 400, false) else {
        return;
    };
    assert!(stats.ticks_simulated > 50 && stats.ticks_simulated < 400, "{stats:?}");
    assert_eq!(stats.catchup_resets, 1, "the unconsumed log was trimmed: {stats:?}");
    assert_parity(&cpu, &gpu, false);
}

/// Substeps and a time scale: still the same pools.
#[test]
fn substeps_and_time_scale_keep_parity() {
    let pools = [ParticleBackend::Cpu, ParticleBackend::Gpu].map(|backend| {
        let mut app = app_with(
            W,
            H,
            ParticleSettings {
                substeps: 3,
                time_scale: 0.7,
                ..settings(backend)
            },
        )?;
        if !app
            .resources
            .expect::<space3d_render::Renderer>()
            .graphics_caps()
            .compute
        {
            return None;
        }
        scene(&mut app);
        app.step_ticks(90);
        render(&mut app);
        let (used, pool) = read_pool(&app)?;
        assert_eq!(used, backend);
        Some(pool)
    });
    let [Some(cpu), Some(gpu)] = pools else {
        eprintln!("skipping: no GPU adapter with compute");
        return;
    };
    assert_parity(&cpu, &gpu, true);
}
