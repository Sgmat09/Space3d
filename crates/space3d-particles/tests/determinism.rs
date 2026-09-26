//! Determinism (graphics spec §4.3, design-particles §10): identical runs render identical
//! images; rendering every tick, rendering once after `n` ticks (render on demand) and replaying
//! only the catch-up window after a long gap all give the same image.

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_particles::ParticleBackend;
use space3d_particles::prelude::*;
use space3d_render::*;
use space3d_test::TestApp;

const W: u32 = 256;
const H: u32 = 144;

/// Explosions, hit sparks and a ship with an engine exhaust (world and local layers, rate per
/// distance, inherited velocity) flying a curve, driven by the tick number only.
fn scene(app: &mut App) {
    app.world.spawn((
        Transform::from_xyz(0.0, 140.0, 120.0).looking_at(Vec3::ZERO, Vec3::Y),
        Camera3d::default().with_clear_color(Vec4::new(0.01, 0.01, 0.02, 1.0)),
    ));
    let ship = app.world.spawn((Transform::IDENTITY,));
    app.spawn_effect(
        builtin::ENGINE_EXHAUST,
        EffectSpawn::from_transform(Transform::from_xyz(0.0, 0.0, 3.2))
            .follow(ship)
            .keep(),
    );
    app.add_system(Stage::FixedUpdate, move |ctx: &mut SystemCtx| {
        let tick = ctx.time().tick;
        let a = tick as f32 * 0.03;
        if let Ok(mut t) = ctx.world.get::<&mut Transform>(ship) {
            t.translation = Vec3::new(a.cos() * 50.0, 0.0, a.sin() * 30.0);
            t.rotation = Quat::from_rotation_y(-a);
        }
        if tick.is_multiple_of(40) {
            ctx.spawn_effect(
                builtin::EXPLOSION,
                EffectSpawn::at(Vec3::new(-30.0 + (tick % 120) as f32 * 0.5, 0.0, 0.0)).scale(0.6),
            );
        }
        if tick.is_multiple_of(9) {
            ctx.spawn_effect(
                builtin::HIT_SPARK,
                EffectSpawn::at(Vec3::new(20.0, 0.0, -10.0)).facing(Vec3::new(a.cos(), 0.0, a.sin())),
            );
        }
    });
}

const BACKENDS: [ParticleBackend; 2] = [ParticleBackend::Cpu, ParticleBackend::Gpu];

fn settings(backend: ParticleBackend) -> ParticleSettings {
    ParticleSettings {
        backend,
        ..ParticleSettings::default()
    }
}

/// Every tick rendered (the renderer in the frame loop).
fn every_tick(backend: ParticleBackend, ticks: u64) -> Option<ImageData> {
    let mut app = app_with(W, H, settings(backend))?;
    scene(&mut app);
    app.step_ticks(ticks);
    let img = render(&mut app);
    assert_eq!(gpu_errors(&app), 0);
    Some(img)
}

/// Rendered once, after all the ticks (the storybook's render-on-demand mode); the stats of a
/// second frame (the GPU backend reads its alive count back).
fn on_demand(backend: ParticleBackend, ticks: u64) -> Option<(ImageData, ParticleStats)> {
    let mut base = App::new();
    base.add_plugin(ParticlesPlugin::with_settings(settings(backend)));
    let mut app = TestApp::builder().size(W, H).render_on_demand().build_with(base);
    app.backend()?;
    scene(&mut app);
    app.step_ticks(ticks);
    let img = app.screenshot()?;
    let mut stats = app.resources.expect::<ParticleStats>().clone();
    app.screenshot()?;
    stats.alive = app.resources.expect::<ParticleStats>().alive;
    Some((img, stats))
}

#[test]
fn runs_are_identical() {
    for backend in BACKENDS {
        let Some(a) = every_tick(backend, 120) else { return };
        let b = every_tick(backend, 120).unwrap();
        dump(&a, &format!("determinism_120_{backend:?}"));
        assert_eq!(a, b, "{backend:?}");
    }
}

/// 90 ticks: shorter than every life window, so the on-demand frame replays all 90 ticks.
#[test]
fn render_every_tick_equals_render_on_demand() {
    for backend in BACKENDS {
        let Some(a) = every_tick(backend, 90) else { return };
        let Some((b, stats)) = on_demand(backend, 90) else {
            return;
        };
        assert_eq!(stats.ticks_simulated, 90, "{stats:?}");
        assert_eq!(stats.catchup_resets, 0);
        assert_eq!(a, b, "{backend:?}");
    }
}

/// 600 ticks: the unconsumed log was trimmed to the catch-up window, the on-demand frame resets
/// the pool and replays only that window, and the image is still identical.
#[test]
fn catch_up_reset_equals_every_tick() {
    for backend in BACKENDS {
        let Some(a) = every_tick(backend, 600) else { return };
        let Some((b, stats)) = on_demand(backend, 600) else {
            return;
        };
        dump(&b, &format!("determinism_600_{backend:?}"));
        assert!(stats.ticks_simulated < 600, "{stats:?}");
        assert_eq!(stats.catchup_resets, 1, "{stats:?}");
        assert!(stats.alive > 100, "{stats:?}");
        assert_eq!(a, b, "{backend:?}");
    }
}

/// The two backends draw the same image (the parity of `tests/parity.rs`, on this scene).
#[test]
fn backends_draw_the_same_image() {
    let Some(cpu) = every_tick(ParticleBackend::Cpu, 120) else {
        return;
    };
    let Some(gpu) = every_tick(ParticleBackend::Gpu, 120) else {
        return;
    };
    let diff = space3d_test::compare_images(&cpu, &gpu, &space3d_test::CompareOptions::default());
    assert!(diff.passed, "{}", diff.summary());
}

/// 200 engine exhausts (171 slots each: more than the CPU backend's 16 384 budget holds) on a
/// device with `downlevel` (`None`: the device as it is), ticked `ticks` with the renderer in
/// the frame loop or held aside (render on demand); the stats after one frame.
fn exhaust_fleet(downlevel: Option<DownlevelOverride>, on_demand: bool, ticks: u64) -> Option<ParticleStats> {
    let mut base = App::new();
    base.add_plugin(ParticlesPlugin::default());
    let mut builder = TestApp::builder().size(W, H);
    if on_demand {
        builder = builder.render_on_demand();
    }
    let mut app = builder.build_with(base);
    app.backend()?;
    if let Some(downlevel) = downlevel {
        app.insert_resource(FrameSettings {
            downlevel,
            ..FrameSettings::default()
        });
    }
    app.world.spawn((
        Transform::from_xyz(0.0, 400.0, 0.0).looking_at(Vec3::ZERO, Vec3::NEG_Z),
        Camera3d::default(),
    ));
    for i in 0..200 {
        let at = Vec3::new((i % 20) as f32 * 20.0 - 200.0, 0.0, (i / 20) as f32 * 20.0 - 100.0);
        app.spawn_effect(builtin::ENGINE_EXHAUST, EffectSpawn::at(at).keep());
    }
    app.step_ticks(ticks);
    app.screenshot()?;
    Some(app.resources.expect::<ParticleStats>().clone())
}

/// The pool budget follows the device whether or not the renderer is in the world while the
/// app ticks (graphics spec §4.3): a render-on-demand app used to size its rings for the GPU
/// budget (262 144) even with `DownlevelOverride::webgl2()`, and kept them once the CPU
/// backend took over (the Battle at High: 165 971 slots reserved against a budget of 16 384,
/// and a different image than rendering every tick).
#[test]
fn the_budget_is_the_same_on_demand_and_every_tick() {
    for downlevel in [None, Some(DownlevelOverride::webgl2())] {
        let Some(every) = exhaust_fleet(downlevel, false, 30) else {
            return;
        };
        let demand = exhaust_fleet(downlevel, true, 30).unwrap();
        eprintln!("{downlevel:?}\n  every tick {every:?}\n  on demand {demand:?}");
        assert_eq!(every.backend, demand.backend, "{downlevel:?}");
        assert_eq!(
            (
                every.pool_capacity,
                every.pool_reserved,
                every.effects,
                every.layer_instances
            ),
            (
                demand.pool_capacity,
                demand.pool_reserved,
                demand.effects,
                demand.layer_instances
            ),
            "{downlevel:?}"
        );
        assert!(demand.pool_reserved <= demand.pool_capacity, "{demand:?}");
        if downlevel.is_some() {
            assert_eq!(demand.backend, Some(ParticleBackend::Cpu));
            assert_eq!(demand.pool_capacity, ParticleSettings::default().max_particles_cpu);
        }
    }
}
