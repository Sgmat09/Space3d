//! Budgets on the GPU backend (graphics spec §5.2 PA2, design-particles §5 step 1): layer
//! instances past the pool budget shrink (to 1/8) and are then refused, priorities stop at their
//! share of the pool, the pool stays within the device's storage binding limit (the renderer's
//! `TEST_LIMITS` pattern: `testing::set_storage_limit`), and none of it causes a GPU error.

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_particles::prelude::*;
use space3d_particles::{EffectPriority, ParticleBackend, testing};

fn gpu_app(settings: ParticleSettings) -> Option<App> {
    let app = app_with(
        160,
        90,
        ParticleSettings {
            backend: ParticleBackend::Gpu,
            ..settings
        },
    )?;
    if !app
        .resources
        .expect::<space3d_render::Renderer>()
        .graphics_caps()
        .compute
    {
        eprintln!("skipping: no compute");
        return None;
    }
    Some(app)
}

/// A one-layer effect: a 400-slot ring of long-lived particles, spawned 300 at a time.
fn ring(app: &mut App, priority: EffectPriority) -> EffectHandle {
    let mut layer = ParticleLayer::burst(300, 5.0);
    layer.capacity = Some(400);
    layer.lod = false;
    app.resources
        .expect_mut::<ParticleEffects>()
        .add(ParticleEffect {
            priority,
            ..ParticleEffect::new("ring").layer(layer)
        })
        .unwrap()
}

#[test]
fn over_budget_layers_shrink_then_refuse() {
    let Some(mut app) = gpu_app(ParticleSettings {
        max_particles: 1000,
        ..ParticleSettings::default()
    }) else {
        return;
    };
    camera(
        &mut app,
        Vec3::new(0.0, 0.0, 30.0),
        Vec3::ZERO,
        Vec4::new(0.0, 0.0, 0.0, 1.0),
    );
    let fx = ring(&mut app, EffectPriority::Normal);
    // 400 + 400, then Normal priority stops at 90 %: the third shrinks to 100 (1/4), the
    // fourth does not fit even at 1/8.
    let spawn = |app: &mut App| app.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO).keep());
    for _ in 0..2 {
        spawn(&mut app);
    }
    app.step_ticks(1);
    let s = stats(&app);
    assert_eq!(
        (s.pool_capacity, s.pool_reserved, s.shrunk, s.refused),
        (1000, 800, 0, 0),
        "{s:?}"
    );
    spawn(&mut app);
    app.step_ticks(1);
    let s = stats(&app);
    assert_eq!((s.pool_reserved, s.shrunk, s.refused), (900, 1, 0), "{s:?}");
    spawn(&mut app);
    app.step_ticks(1);
    let s = stats(&app);
    assert_eq!((s.pool_reserved, s.shrunk, s.refused), (900, 0, 1), "{s:?}");
    // Everything that was admitted simulates and draws; 300 + 300 + 100 (the shrunk ring
    // spawns its share: 300 × 100/400 = 75) particles.
    app.step_ticks(2);
    let s = settled_stats(&mut app);
    assert_eq!(s.backend, Some(ParticleBackend::Gpu));
    assert_eq!(s.alive, 675, "{s:?}");
    assert_eq!(gpu_errors(&app), 0);
}

#[test]
fn priorities_stop_at_their_share_of_the_pool() {
    let Some(mut app) = gpu_app(ParticleSettings {
        max_particles: 1000,
        ..ParticleSettings::default()
    }) else {
        return;
    };
    let low = ring(&mut app, EffectPriority::Low);
    let high = ring(&mut app, EffectPriority::High);
    // Low stops at 75 %: 400, then 200 (400 would reach 800 > 750), then 100 (700). High may
    // use it all: 200 (900), then 100 (1000).
    for fx in [low, low, low, high, high] {
        app.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO).keep());
    }
    app.step_ticks(1);
    let s = stats(&app);
    assert_eq!((s.pool_reserved, s.shrunk, s.refused), (1000, 4, 0), "{s:?}");
    // One more Low effect finds no room under 75 %: refused.
    app.spawn_effect(low, EffectSpawn::at(Vec3::ZERO).keep());
    app.step_ticks(1);
    let s = stats(&app);
    assert_eq!((s.pool_reserved, s.shrunk, s.refused), (1000, 0, 1), "{s:?}");
    assert_eq!(gpu_errors(&app), 0);
}

/// A device whose storage bindings hold 3000 particles: the GPU pool budget follows (instead of
/// `max_particles`), reservations past it shrink or are refused, and the pool buffer never
/// exceeds the limit.
#[test]
fn the_pool_stays_within_the_storage_binding_limit() {
    let limit = 3000 * 48;
    testing::set_storage_limit(Some(limit));
    let app = gpu_app(ParticleSettings::default());
    let Some(mut app) = app else {
        testing::set_storage_limit(None);
        return;
    };
    camera(
        &mut app,
        Vec3::new(0.0, 0.0, 30.0),
        Vec3::ZERO,
        Vec4::new(0.0, 0.0, 0.0, 1.0),
    );
    let fx = ring(&mut app, EffectPriority::High);
    for _ in 0..10 {
        app.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO).keep());
    }
    app.step_ticks(3);
    testing::set_storage_limit(None);
    let s = settled_stats(&mut app);
    assert_eq!(s.pool_capacity, 3000, "{s:?}");
    assert!(s.pool_reserved <= 3000 && s.pool_reserved >= 2800, "{s:?}");
    let bytes = testing::gpu_pool_bytes(&app).expect("GPU pool");
    assert!(bytes <= limit, "pool buffer {bytes} B > limit {limit} B");
    assert!(s.alive > 1500, "{s:?}");
    assert_eq!(gpu_errors(&app), 0);
}

/// game#14 on the GPU backend: a `max_particles` lowered after the pool grew is enforced at
/// once (new rings stop at the lowered budget: shrunk, then refused), and the stats report the
/// budget in force (`pool_capacity`), the slots the pool buffer holds (`pool_allocated`) and
/// the simulated extent (`pool_high_water`).
#[test]
fn lowered_max_particles_is_enforced() {
    let Some(mut app) = gpu_app(ParticleSettings {
        max_particles: 262_144,
        ..ParticleSettings::default()
    }) else {
        return;
    };
    camera(
        &mut app,
        Vec3::new(0.0, 0.0, 30.0),
        Vec3::ZERO,
        Vec4::new(0.0, 0.0, 0.0, 1.0),
    );
    let fx = ring(&mut app, EffectPriority::Normal);
    app.step_ticks(1);
    render(&mut app);
    let s = stats(&app);
    assert_eq!(s.pool_capacity, 262_144, "{s:?}");
    app.resources.expect_mut::<ParticleSettings>().max_particles = 4_000;
    for _ in 0..20 {
        app.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO).keep());
    }
    app.step_ticks(1);
    render(&mut app);
    let s = stats(&app);
    eprintln!("{s:?}");
    assert_eq!(s.pool_capacity, 4_000);
    assert!(s.pool_reserved <= 4_000, "{s:?}");
    // Ten rings of 400 fit 4000 at `Normal` priority (90 %): nine; the other eleven were
    // refused (counted in the frame that ticked).
    assert_eq!(
        (s.effects, s.layer_instances),
        (20, 9),
        "the lowered budget binds: {s:?}"
    );
    assert!(
        s.pool_high_water <= 4_000 && s.pool_high_water >= s.pool_reserved,
        "{s:?}"
    );
    assert!(s.pool_allocated >= s.pool_high_water, "{s:?}");
    assert_eq!(gpu_errors(&app), 0);
}
