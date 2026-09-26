//! Steady-state heap allocations (graphics spec §1.1 invariant 2, §4.4): the fixed-tick system
//! allocates nothing, and a rendered frame with live particles allocates only wgpu's bookkeeping
//! on either backend (pinned to its measured bound, never growing);
//! `ParticleStats::cpu_reallocations` and `RenderStats::cpu_reallocations` stay 0. The GPU
//! backend also keeps to its dispatch budget (≤ 5 per frame, 2 more per extra tick).
//!
//! The only test in this binary: the counter is global.

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_particles::ParticleBackend;
use space3d_particles::prelude::*;
use space3d_render::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);

// SAFETY: forwards to the system allocator; the counter is a plain atomic.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(p, l, n) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// A ship's engine exhaust and missile trail (ribbons) flying a circle with a laser beam to a
/// fixed point, an explosion restarted every 6 ticks and a hit spark every 4 at new places, with
/// their light flashes (pools of emitters, restarted round-robin: the core's transform
/// propagation allocates for newly spawned and parented entities, which is not what this test
/// measures). Covers bursts, rates, rate per distance, local layers, ribbons, beams, light
/// flashes (a `PointLight` entity spawned and despawned per flash), restarts, draining,
/// finishing and render-time bridges (the ship moves in `Stage::Update`, after the tick).
fn scene(app: &mut App) {
    let ship = app.spawn_effect(builtin::ENGINE_EXHAUST, EffectSpawn::at(Vec3::ZERO).keep());
    app.spawn_effect(builtin::MISSILE_TRAIL, EffectSpawn::at(Vec3::ZERO).follow(ship).keep());
    app.spawn_effect(
        builtin::LASER_BEAM,
        EffectSpawn::at(Vec3::ZERO)
            .follow(ship)
            .beam_to(BeamEnd::World(Vec3::new(0.0, 0.0, -40.0)))
            .keep(),
    );
    let blasts: Vec<Entity> = (0..16)
        .map(|_| app.spawn_effect(builtin::EXPLOSION, EffectSpawn::at(Vec3::ZERO).scale(0.5).keep()))
        .collect();
    let sparks: Vec<Entity> = (0..8)
        .map(|_| app.spawn_effect(builtin::HIT_SPARK, EffectSpawn::at(Vec3::ZERO).facing(Vec3::X).keep()))
        .collect();
    app.add_system(Stage::Update, move |ctx: &mut SystemCtx| {
        let tick = ctx.time().tick;
        let a = tick as f32 * 0.02;
        if let Ok(mut t) = ctx.world.get::<&mut Transform>(ship) {
            t.translation = Vec3::new(a.cos() * 60.0, 0.0, a.sin() * 60.0);
            t.rotation = Quat::from_rotation_y(-a);
        }
        let restart = |e: Entity, p: Vec3| {
            if let Ok(mut t) = ctx.world.get::<&mut Transform>(e) {
                t.translation = p;
            }
            if let Ok(mut em) = ctx.world.get::<&mut ParticleEmitter>(e) {
                em.restart();
            }
        };
        if tick.is_multiple_of(6) {
            let e = blasts[(tick / 6) as usize % blasts.len()];
            restart(e, Vec3::new((a * 7.0).sin() * 80.0, 0.0, (a * 3.0).cos() * 80.0));
        }
        if tick.is_multiple_of(4) {
            let e = sparks[(tick / 4) as usize % sparks.len()];
            restart(e, Vec3::new(a.sin() * 30.0, 0.0, 0.0));
        }
    });
}

fn measure(app: &mut App, ticks: u64) -> u64 {
    let before = ALLOCS.load(Relaxed);
    app.step_ticks(ticks);
    ALLOCS.load(Relaxed) - before
}

#[test]
fn steady_state_allocations() {
    // 1. The simulation alone (no renderer): the tick system allocates nothing.
    let mut sim = App::new();
    sim.add_plugin(ParticlesPlugin::default());
    scene(&mut sim);
    // Collections reach their peak sizes within the first 1500 ticks (the last growth measured
    // happens between ticks 900 and 1500).
    sim.step_ticks(1500);
    let allocs = measure(&mut sim, 600);
    let stats = sim.resources.expect::<ParticleStats>().clone();
    eprintln!("no renderer: {allocs} allocations in 600 ticks ({stats:?})");
    assert!(stats.effects > 20 && stats.pool_reserved > 1000, "{stats:?}");
    assert_eq!(allocs, 0, "the fixed-tick system allocates nothing in the steady state");

    // 2. Rendered every tick, on each backend: our code allocates nothing; wgpu's bookkeeping
    //    is bounded.
    for backend in [ParticleBackend::Cpu, ParticleBackend::Gpu] {
        let settings = ParticleSettings {
            backend,
            ..ParticleSettings::default()
        };
        let Some(mut app) = common::app_with(320, 180, settings) else {
            return;
        };
        let adapter = app.resources.expect::<Renderer>().capabilities().adapter.backend;
        let compute = app.resources.expect::<Renderer>().graphics_caps().compute;
        if backend == ParticleBackend::Gpu && !compute {
            eprintln!("no compute: GPU backend not measured");
            continue;
        }
        camera(
            &mut app,
            Vec3::new(0.0, 180.0, 160.0),
            Vec3::ZERO,
            Vec4::new(0.0, 0.0, 0.0, 1.0),
        );
        scene(&mut app);
        app.step_ticks(600);
        let first = measure(&mut app, 300);
        let second = measure(&mut app, 300);
        let stats = app.resources.expect::<ParticleStats>().clone();
        let render = app.resources.expect::<RenderStats>().clone();
        let per_frame = (first.max(second) as f64 / 300.0).ceil() as u64;
        eprintln!(
            "{backend:?} backend, rendered: {first} then {second} allocations per 300 frames ({per_frame}/frame, {adapter:?}); {stats:?}"
        );
        assert_eq!(stats.backend, Some(backend));
        assert!(stats.alive > 500 && stats.draw_calls > 5, "{stats:?}");
        assert!(stats.ribbon_segments > 20, "ribbons and beams drawn: {stats:?}");
        assert_eq!(stats.cpu_reallocations, 0, "{stats:?}");
        assert_eq!((render.cpu_reallocations, render.textures_created), (0, 0));
        assert!(second <= first + first / 10, "no growth: {first} → {second}");
        // Measured on Apple M4 Max / Metal (wgpu 30.0.1, test profile) plus ~10 %; twice that
        // elsewhere.
        let metal = match backend {
            ParticleBackend::Gpu => METAL_BOUND_GPU,
            _ => METAL_BOUND,
        };
        let bound = if adapter == wgpu::Backend::Metal {
            metal
        } else {
            2 * metal
        };
        assert!(per_frame <= bound, "{per_frame} allocations per frame (bound {bound})");
        if backend == ParticleBackend::Gpu {
            // Graphics spec §4.4: ≤ 5 dispatches for a frame with one tick (4 here), 2 more per
            // extra tick, plus the sort of the `ByDistance` groups in view (the explosions' and
            // the missiles' smoke: one dispatch each under 512 particles).
            assert_eq!(stats.ticks_simulated, 1);
            assert!(stats.dispatches <= 5 + 2, "{stats:?}");
            // The node reports its compute passes (the tick's simulate + emit pass, the
            // compaction pass and the sort pass) and dispatches in the frame's `RenderStats`.
            assert_eq!(render.compute_passes, 3, "{render:?}");
            assert_eq!(render.node_dispatches, stats.dispatches, "{render:?}");
            let renderer = app.resources.remove::<Renderer>().unwrap();
            app.step_ticks(3);
            app.resources.insert(renderer);
            common::render(&mut app);
            let s = app.resources.expect::<ParticleStats>().clone();
            assert_eq!(s.ticks_simulated, 3);
            assert!(s.dispatches <= 5 + 2 * 2 + 2, "{s:?}");
            let r = app.resources.expect::<RenderStats>();
            assert!((4..=6).contains(&r.compute_passes), "{r:?}");
        }
        assert_eq!(gpu_errors(&app), 0);
    }
}

/// wgpu's allocations per frame of the scene above on Metal with the CPU backend (measured 213
/// with 89 particle draws — 4 of them ribbon and beam strips — and one SceneRead pass; 184 before
/// the scene had ribbons and beams; 176 since the smoke is sorted per particle: one draw per
/// layer instead of one per emitter), +10 %.
const METAL_BOUND: u64 = 235;

/// The same with the GPU backend (measured 335: two compute passes with 4 dispatches, the tick's
/// uploads, the alive-count readback, the indirect draws, the direct draw of the exhaust's
/// render-time bridge — the scene moves it in `Stage::Update` — and the strip draws; 307 before
/// the ribbons and beams; 329 with the smoke's sort pass and fewer smoke draws), +10 %.
const METAL_BOUND_GPU: u64 = 369;
