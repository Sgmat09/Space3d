//! Steady-state heap allocations of the render stage with post effects (graphics spec §4.4
//! "Passes and wgpu allocations"): bloom on one post camera, and on two split-screen post cameras
//! sharing one pooled chain. The renderer's own work allocates nothing (`cpu_reallocations == 0`,
//! `textures_created == 0`); what remains is wgpu's bookkeeping, about 20 allocations per render
//! pass on Metal. Each mode is pinned to its measured bound (+10 %) and must not grow over time.
//!
//! The only test in this binary: the counter is global.

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_render::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};

struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static LIVE_BYTES: AtomicI64 = AtomicI64::new(0);

// SAFETY: forwards to the system allocator; the counters are plain atomics.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        LIVE_BYTES.fetch_add(l.size() as i64, Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        LIVE_BYTES.fetch_add(l.size() as i64, Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        LIVE_BYTES.fetch_add(n as i64 - l.size() as i64, Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(p, l, n) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE_BYTES.fetch_sub(l.size() as i64, Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const WARM_UP: usize = 20;
const FRAMES: usize = 100;

/// Allocations per frame measured on Apple M4 Max / Metal (wgpu 30.0.1, test profile, CPU draw
/// path) plus ~10 %; other backends get twice the headroom. See the module docs.
fn bound(mode: &str, backend: wgpu::Backend) -> u64 {
    let metal = match mode {
        "bloom" => MEASURED_BLOOM * 11 / 10,
        _ => MEASURED_SPLIT * 11 / 10,
    };
    if backend == wgpu::Backend::Metal {
        metal
    } else {
        2 * metal
    }
}

/// Measured maxima (allocations per frame) on the dev machine: 332 with bloom on one post
/// camera — 226 over the composite-only 106 of `tests/alloc_frame.rs`, about 20 per bloom render
/// pass — and 617 for two post groups.
const MEASURED_BLOOM: u64 = 332;
const MEASURED_SPLIT: u64 = 617;

#[test]
fn bloom_allocates_a_small_constant_amount() {
    let Some(mut app) = headless_app(320, 180) else { return };
    let backend = app.resources.expect::<Renderer>().capabilities().adapter.backend;
    set_path(&mut app, DrawPath::CpuDirect);
    let cube = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.2));
    let mut movers = Vec::new();
    for i in 0..400u32 {
        let p = Vec3::new((i % 20) as f32 * 0.3 - 3.0, (i / 20) as f32 * 0.3 - 3.0, 0.0);
        movers.push(app.world.spawn((
            Transform::from_xyz(p.x, p.y, p.z),
            Mesh3d(cube),
            Material::srgb(0.8, 0.4, 0.2).with_emissive(Vec3::splat((i % 7) as f32)),
        )));
    }
    spawn_sun(&mut app);
    let cam = spawn_camera(&mut app, 10.0);
    let second = spawn_camera(&mut app, 10.0);
    app.world.get::<&mut Camera3d>(second).unwrap().active = false;
    let mut frame = 0u32;
    let mut step = |app: &mut App| -> u64 {
        frame += 1;
        let dy = if frame.is_multiple_of(2) { 0.01 } else { -0.01 };
        for &e in &movers {
            app.world.get::<&mut Transform>(e).unwrap().translation.y += dy;
        }
        space3d_core::transform::propagate_transforms(&mut app.world);
        let a0 = ALLOCS.load(Relaxed);
        app.run_stage(Stage::Render);
        let counted = ALLOCS.load(Relaxed) - a0;
        let _ = app
            .resources
            .expect::<Renderer>()
            .device()
            .poll(wgpu::PollType::wait_indefinitely());
        counted
    };
    type Setup = fn(&mut App, Entity, Entity);
    let modes: [(&str, Setup); 2] = [
        ("bloom", |app, cam, _| {
            app.world.insert_one(cam, PostProcess::default()).unwrap();
        }),
        ("bloom split", |app, cam, second| {
            for (e, viewport, order) in [(cam, [0.0, 0.0, 0.5, 1.0], 0), (second, [0.5, 0.0, 0.5, 1.0], 1)] {
                app.world.insert_one(e, PostProcess::default()).unwrap();
                let mut c = app.world.get::<&mut Camera3d>(e).unwrap();
                c.viewport = Some(viewport);
                c.order = order;
                c.active = true;
            }
        }),
    ];
    let mut per_frame = Vec::with_capacity(FRAMES);
    for (name, setup) in modes {
        setup(&mut app, cam, second);
        for _ in 0..WARM_UP {
            step(&mut app);
        }
        let live_before = LIVE_BYTES.load(Relaxed);
        per_frame.clear();
        for _ in 0..FRAMES {
            per_frame.push(step(&mut app));
        }
        let live_growth = LIVE_BYTES.load(Relaxed) - live_before;
        let s = stats(&app);
        assert_eq!(
            (s.cpu_reallocations, s.buffer_reallocations, s.textures_created),
            (0, 0, 0),
            "{name}: {s:?}"
        );
        let groups = if name == "bloom" { 1 } else { 2 };
        // Per group: six levels (prefilter, 5 downsamples, 5 upsamples) and the composite.
        assert_eq!(s.post_groups, groups, "{name}");
        assert_eq!(s.post_passes, groups * 12, "{name}");
        let max = *per_frame.iter().max().unwrap();
        let (first, second) = per_frame.split_at(FRAMES / 2);
        let mean = |v: &[u64]| v.iter().sum::<u64>() as f64 / v.len() as f64;
        eprintln!(
            "{name} on {backend:?}: allocations per frame min {} max {max} mean {:.1}; live bytes \
             {live_growth:+} over {FRAMES} frames ({} render passes of the graphics features)",
            per_frame.iter().min().unwrap(),
            mean(&per_frame),
            s.render_passes,
        );
        let bound = bound(name, backend);
        assert!(
            max <= bound,
            "{name}: up to {max} allocations per frame (bound {bound})"
        );
        assert!(
            mean(second) <= mean(first) + 1.0,
            "{name}: allocations grow over time: {:.1} → {:.1}",
            mean(first),
            mean(second)
        );
        assert!(
            live_growth < 256 << 10,
            "{name}: {live_growth} bytes still allocated after {FRAMES} frames"
        );
    }
    assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
}
