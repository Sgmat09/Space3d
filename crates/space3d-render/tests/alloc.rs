//! Steady-state heap allocations of the render stage, counted by a global allocator.
//!
//! The renderer's own per-frame work allocates nothing (its scratch lives in `FrameState` and
//! is reused, see ARCHITECTURE.md); what remains is wgpu's bookkeeping for one frame's command
//! encoder, passes, submission and buffer-mapping callbacks. This test pins that total so a
//! regression on either side shows up, and checks that it does not grow over time (no leak or
//! unbounded cache in the frame loop).
//!
//! It is the only test in this binary: the counter is global, so nothing else may run
//! concurrently.

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

/// Upper bound of allocations per steady-state frame of this test's scene: measured on
/// Apple M4 Max / Metal (wgpu 30.0.1, test profile) plus ~10 %. CPU culling path: 82–86
/// measured; GPU culling paths: 191–195 measured (wgpu-hal's Metal backend allocates in every
/// `dispatch_workgroups` — 7 here — and `set_*_pipeline`; the compute pass, indirect-draw
/// validation and the culling-stats readback mapping add more). A backtrace of every allocation of one such
/// frame shows only wgpu, wgpu-hal and naga frames at the allocation site; the renderer's own
/// direct share is the boxed callbacks it hands to wgpu (staging recall, stats readback).
/// Other backends run different wgpu-hal code (e.g. Vulkan descriptor and command pools), so
/// they get twice the headroom; the no-growth checks below are backend-independent.
fn bound(path: DrawPath, backend: wgpu::Backend) -> u64 {
    let metal = if path.gpu_culling() { 215 } else { 95 };
    if backend == wgpu::Backend::Metal {
        metal
    } else {
        2 * metal
    }
}

const WARM_UP: usize = 30;
const FRAMES: usize = 120;

#[test]
fn steady_state_frames_allocate_a_small_constant_amount() {
    let Some(mut app) = headless_app(256, 256) else { return };
    let backend = app.resources.expect::<Renderer>().capabilities().adapter.backend;
    let (cube, shader, texture) = {
        let a = app.resources.expect_mut::<RenderAssets>();
        let shader = a.add_shader(CustomShader::new(
            "tint",
            "fn fragment_hook(in: VsOut, c: vec4<f32>) -> vec4<f32> { return c.bgra; }",
        ));
        let texture = a.add_image(ImageData::checker(16, 4, [255, 0, 0, 255], [0, 255, 0, 255]));
        (a.add_mesh(MeshData::cube(0.05)), shader, texture)
    };
    // 40k moving ECS entities (parallel gather and diff), two materials, one textured.
    let mut movers = Vec::new();
    for i in 0..40_000u32 {
        let material = if i % 2 == 0 {
            Material::srgb(0.8, 0.4, 0.2)
        } else {
            Material::unlit(Vec4::ONE).with_texture(texture)
        };
        let p = Vec3::new((i % 200) as f32 * 0.06 - 6.0, (i / 200) as f32 * 0.06 - 6.0, 0.0);
        movers.push(
            app.world
                .spawn((Transform::from_xyz(p.x, p.y, p.z), Mesh3d(cube), material)),
        );
    }
    // 100k explicit instances rewritten every frame (3.2 MB: parallel staging copy), and a
    // static f16 buffer drawn with a custom shader.
    let data = (0..100_000)
        .map(|i| InstanceCompact::new(Vec3::new(i as f32 * 0.001, 2.0, -1.0), Quat::IDENTITY, 1.0))
        .collect();
    let dynamic = app
        .resources
        .expect_mut::<InstanceBuffers>()
        .insert(InstanceBuffer::from_vec(data, BufferUsage::Dynamic));
    let half = (0..1000)
        .map(|i| InstanceHalf::new(Vec3::new(i as f32 * 0.01, -2.0, 0.0), Quat::IDENTITY, 1.0))
        .collect();
    let half = app
        .resources
        .expect_mut::<InstanceBuffers>()
        .insert(InstanceBuffer::from_vec(half, BufferUsage::Static));
    app.world.spawn((InstancedMesh {
        mesh: cube,
        material: Material::default(),
        instances: dynamic,
    },));
    app.world.spawn((InstancedMesh {
        mesh: cube,
        material: Material::unlit(Vec4::ONE).with_shader(shader),
        instances: half,
    },));
    spawn_camera(&mut app, 12.0);
    spawn_sun(&mut app);

    let mut frame = 0u32;
    let mut step = |app: &mut App| -> (u64, i64) {
        frame += 1;
        // Simulation work happens outside the counted window.
        let dy = if frame.is_multiple_of(2) { 0.01 } else { -0.01 };
        for &e in &movers {
            app.world.get::<&mut Transform>(e).unwrap().translation.y += dy;
        }
        let buf = app
            .resources
            .expect_mut::<InstanceBuffers>()
            .get_mut::<InstanceCompact>(dynamic)
            .unwrap();
        for v in buf.as_mut_slice() {
            v.translation[1] += dy;
        }
        space3d_core::transform::propagate_transforms(&mut app.world);
        let (a0, l0) = (ALLOCS.load(Relaxed), LIVE_BYTES.load(Relaxed));
        app.run_stage(Stage::Render);
        let counted = (ALLOCS.load(Relaxed) - a0, LIVE_BYTES.load(Relaxed) - l0);
        // Let the GPU catch up (outside the window) so frames do not pile up in flight.
        let _ = app
            .resources
            .expect::<Renderer>()
            .device()
            .poll(wgpu::PollType::wait_indefinitely());
        counted
    };

    let mut per_frame = Vec::with_capacity(FRAMES);
    for path in paths_up_to(best_path(&app)) {
        set_path(&mut app, path);
        for _ in 0..WARM_UP {
            step(&mut app);
        }
        let live_before = LIVE_BYTES.load(Relaxed);
        per_frame.clear();
        for _ in 0..FRAMES {
            per_frame.push(step(&mut app).0);
        }
        let live_growth = LIVE_BYTES.load(Relaxed) - live_before;
        let s = stats(&app);
        assert_eq!((s.cpu_reallocations, s.buffer_reallocations), (0, 0), "{path:?}: {s:?}");
        let max = *per_frame.iter().max().unwrap();
        let (first, second) = per_frame.split_at(FRAMES / 2);
        let mean = |v: &[u64]| v.iter().sum::<u64>() as f64 / v.len() as f64;
        eprintln!(
            "{path:?} on {backend:?}: allocations per frame min {} max {max} mean {:.1}; live bytes \
             {live_growth:+} over {FRAMES} frames ({} batches, {} pipeline binds, {} dispatches)",
            per_frame.iter().min().unwrap(),
            mean(&per_frame),
            s.batches,
            s.pipeline_switches,
            s.compute_dispatches,
        );
        let bound = bound(path, backend);
        assert!(
            max <= bound,
            "{path:?}: up to {max} allocations per frame (bound {bound})"
        );
        assert!(
            mean(second) <= mean(first) + 1.0,
            "{path:?}: allocations grow over time: {:.1} → {:.1}",
            mean(first),
            mean(second)
        );
        assert!(
            live_growth < 256 << 10,
            "{path:?}: {live_growth} bytes still allocated after {FRAMES} frames"
        );
    }

    // Viewport clears (split screen, picture-in-picture) draw one triangle whose color is the
    // pass's blend constant: no buffer, bind group or allocation per frame. Two viewport
    // cameras with clear colors allocate as much as the same cameras without (whose passes
    // only load the target) — measured exactly equal on Metal; other backends' command
    // recording may grow its lists once more.
    let path = best_path(&app);
    set_path(&mut app, path);
    let viewports: Vec<Entity> = [[0.6, 0.05, 0.3, 0.3], [0.05, 0.6, 0.3, 0.3]]
        .into_iter()
        .enumerate()
        .map(|(i, viewport)| {
            app.world.spawn((
                Transform::from_xyz(0.0, 0.0, 12.0),
                Camera3d {
                    order: 1 + i as i32,
                    viewport: Some(viewport),
                    clear_color: None,
                    ..Default::default()
                },
            ))
        })
        .collect();
    let mut mean_per_frame = |app: &mut App| {
        for _ in 0..WARM_UP {
            step(app);
        }
        let live_before = LIVE_BYTES.load(Relaxed);
        let total: u64 = (0..FRAMES).map(|_| step(app).0).sum();
        let live_growth = LIVE_BYTES.load(Relaxed) - live_before;
        assert!(live_growth < 256 << 10, "{live_growth} bytes still allocated");
        assert_eq!(stats(app).cpu_reallocations, 0);
        total as f64 / FRAMES as f64
    };
    let loading = mean_per_frame(&mut app);
    for &e in &viewports {
        app.world.get::<&mut Camera3d>(e).unwrap().clear_color = Some(Vec4::new(1.0, 1.0, 0.0, 1.0));
    }
    let clearing = mean_per_frame(&mut app);
    eprintln!(
        "{path:?} on {backend:?} + 2 viewport cameras: allocations per frame {loading:.1} without \
         clear colors, {clearing:.1} with (viewport clears)"
    );
    let slack = if backend == wgpu::Backend::Metal { 0.5 } else { 4.0 };
    assert!(
        clearing <= loading + slack,
        "{path:?}: viewport clears allocate: {loading:.1} → {clearing:.1} per frame"
    );
}
