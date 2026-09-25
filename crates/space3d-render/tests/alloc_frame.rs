//! Steady-state heap allocations of the render stage per frame mode (graphics spec §4.4
//! "Passes and wgpu allocations"): Direct, Hdr (scene texture + composite), the Ldr fallback,
//! render-scale passthrough and render nodes (Opaque and Transparent slots in the scene pass, a
//! depth-reading slot in a SceneRead pass). The renderer's own work allocates nothing in any mode
//! (`cpu_reallocations == 0`, `textures_created == 0`); what remains is wgpu's bookkeeping,
//! which grows with the extra pass. Each mode is pinned to its measured bound (+10 %) and must
//! not grow over time.
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

/// Allocations per frame measured on Apple M4 Max / Metal (wgpu 30.0.1, test profile, CPU
/// draw path) plus ~10 %; other backends get twice the headroom. Measured: Direct 76; Hdr, Ldr
/// and passthrough 106 each — one render pass (the composite) more, and wgpu-hal's Metal backend
/// allocates about 30 times per render pass; nodes 119–120 (Direct + one SceneRead pass
/// and three node draws); Hdr with its per-frame post seeds uploaded (16 bytes: one staging
/// copy, as in `App::update`, where the frame counter advances) 150.
fn bound(mode: &str, backend: wgpu::Backend) -> u64 {
    let metal = match mode {
        "direct" => 84,
        "nodes" => 132,
        "hdr_seeds" => 165,
        _ => 117,
    };
    if backend == wgpu::Backend::Metal {
        metal
    } else {
        2 * metal
    }
}

#[test]
fn every_frame_mode_allocates_a_small_constant_amount() {
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
    type Setup = fn(&mut App, Entity);
    let modes: [(&str, Setup); 5] = [
        ("direct", |_, _| {}),
        // The composite alone: bloom's passes are measured in `tests/alloc_post.rs`.
        ("hdr", |app, cam| {
            app.world
                .insert_one(cam, PostProcess::default().without_bloom())
                .unwrap()
        }),
        ("ldr", |app, cam| {
            app.world.insert_one(cam, PostProcess::default()).unwrap();
            app.resources.expect_mut::<FrameSettings>().downlevel.no_float_targets = true;
        }),
        ("passthrough", |app, _| {
            app.resources.expect_mut::<FrameSettings>().render_scale = 0.75
        }),
        ("nodes", |app, _| {
            use common::nodes::*;
            let nodes = app.resources.expect_mut::<RenderNodes>();
            let mut solid = Solid::new(
                "solid",
                vec![
                    Solid::slot(PhaseSlot::new(RenderPhase::Opaque, -1), [0.1, 0.1, 0.1, 1.0]),
                    SolidSlot {
                        slot: PhaseSlot::new(RenderPhase::Transparent, 5),
                        color: [0.9, 0.9, 0.1, 1.0],
                        rect: [-0.5, -0.5, 0.0, 0.0],
                    },
                ],
                log(),
            );
            solid.quiet = true;
            nodes.insert("solid", solid);
            nodes.insert("probe", DepthProbe::new().0);
        }),
    ];
    let mut per_frame = Vec::with_capacity(FRAMES);
    for (name, setup) in modes {
        let _ = app.world.remove_one::<PostProcess>(cam);
        *app.resources.expect_mut::<FrameSettings>() = FrameSettings::default();
        setup(&mut app, cam);
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
        let intermediate = !matches!(name, "direct" | "nodes");
        assert_eq!(s.intermediate_groups, u32::from(intermediate), "{name}");
        if name == "nodes" {
            assert_eq!((s.node_draws, s.scene_read_passes), (3, 1), "{name}");
        }
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

    // A still Hdr scene the way `App::update` renders it: the frame counter advances (the post
    // seeds, 16 bytes, are uploaded every frame) and frames queue up on the GPU (waited for
    // every 8th). Regression (review): the staging ring trimmed the chunks still in flight and
    // created and dropped a 4 MiB chunk every few frames (0.5–1 ms each, an `Arc` per chunk:
    // 100–230 per 1200 frames). Now it grows to the queue's depth (at most 8 chunks here, each
    // created once, when the GPU first falls that far behind) and keeps them.
    let _ = app.world.remove_one::<PostProcess>(cam);
    *app.resources.expect_mut::<FrameSettings>() = FrameSettings::default();
    app.world
        .insert_one(cam, PostProcess::default().without_bloom())
        .unwrap();
    let (mut max, mut sum, mut counted_frames, mut reallocs, mut chunks, mut bytes) = (0, 0, 0, 0, 0, 0);
    for i in 0..1200u32 {
        app.resources.expect_mut::<Time>().frame += 1;
        let a0 = ALLOCS.load(Relaxed);
        app.run_stage(Stage::Render);
        let counted = ALLOCS.load(Relaxed) - a0;
        let s = stats(&app);
        let created = s.buffer_reallocations + s.cpu_reallocations + s.textures_created + s.staging_chunks_created;
        chunks += s.staging_chunks_created;
        if i >= 16 {
            reallocs += s.buffer_reallocations + s.cpu_reallocations + s.textures_created;
            bytes = bytes.max(s.bytes_uploaded);
            if created == 0 {
                max = max.max(counted);
                sum += counted;
                counted_frames += 1;
            }
        }
        if i % 8 == 7 {
            let _ = app
                .resources
                .expect::<Renderer>()
                .device()
                .poll(wgpu::PollType::wait_indefinitely());
        }
    }
    let mean = sum as f64 / counted_frames.max(1) as f64;
    eprintln!(
        "hdr still, frames queued: allocations per frame mean {mean:.1} max {max}, {chunks} staging chunks created in 1200 frames and {reallocs} other buffers or textures after the first 16, up to {bytes} B uploaded"
    );
    assert!(chunks <= 10, "{chunks} staging chunks created: the staging ring churns");
    assert_eq!(reallocs, 0, "nothing else is created for a still scene");
    assert!(bytes <= 16, "a still post group uploads its seeds only: {bytes} B");
    // wgpu retires queued submissions in batches (the frame after a wait retires eight): those
    // frames may allocate more; the mean stays the waited-for frame's.
    let bound = bound("hdr_seeds", backend);
    assert!(
        mean <= bound as f64,
        "hdr queued: {mean:.1} allocations per frame (bound {bound})"
    );
    assert!(
        max <= 2 * bound,
        "hdr queued: up to {max} allocations per frame (bound {})",
        2 * bound
    );
    assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
}
