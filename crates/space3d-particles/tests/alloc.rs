//! Steady-state heap allocations (graphics spec §1.1 invariant 2, §4.4): the fixed-tick system
//! allocates nothing, and a rendered frame with live particles allocates only wgpu's bookkeeping
//! (pinned to its measured bound, never growing); `ParticleStats::cpu_reallocations` and
//! `RenderStats::cpu_reallocations` stay 0.
//!
//! The only test in this binary: the counter is global.

mod common;
use common::*;
use space3d_core::prelude::*;
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

/// A ship's engine exhaust flying a circle, an explosion restarted every 6 ticks and a hit spark
/// every 4 at new places (pools of emitters, restarted round-robin: the core's transform
/// propagation allocates for newly spawned and parented entities, which is not what this test
/// measures). Covers bursts, rates, rate per distance, local layers, restarts, draining and
/// finishing.
fn scene(app: &mut App) {
    let ship = app.spawn_effect(builtin::ENGINE_EXHAUST, EffectSpawn::at(Vec3::ZERO).keep());
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

    // 2. Rendered every tick: our code allocates nothing; wgpu's bookkeeping is bounded.
    let Some(mut app) = common::app(320, 180) else { return };
    let backend = app.resources.expect::<Renderer>().capabilities().adapter.backend;
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
    eprintln!("rendered: {first} then {second} allocations per 300 frames ({per_frame}/frame, {backend:?}); {stats:?}");
    assert!(stats.alive > 500 && stats.draw_calls > 5, "{stats:?}");
    assert_eq!(stats.cpu_reallocations, 0, "{stats:?}");
    assert_eq!((render.cpu_reallocations, render.textures_created), (0, 0));
    assert!(second <= first + first / 10, "no growth: {first} → {second}");
    // Measured on Apple M4 Max / Metal (wgpu 30.0.1, test profile) plus ~10 %; twice that
    // elsewhere.
    let bound = if backend == wgpu::Backend::Metal {
        METAL_BOUND
    } else {
        2 * METAL_BOUND
    };
    assert!(per_frame <= bound, "{per_frame} allocations per frame (bound {bound})");
    assert_eq!(gpu_errors(&app), 0);
}

/// wgpu's allocations per frame of the scene above on Metal (measured 184 with 80 particle draws
/// and one SceneRead pass), +10 %.
const METAL_BOUND: u64 = 203;
