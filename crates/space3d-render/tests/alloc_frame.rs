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
/// and three node draws); a depth view (shadow map) 103 (Direct + one depth-only pass drawing
/// the 400 casters); GPU timings 100 (Direct + the timestamp writes, a deferred resolve and
/// the two callbacks wgpu boxes per timed frame); a still Hdr scene whose frame counter advances (as in `App::update`)
/// with frames queued on the GPU 136 (150 while it uploaded its post seeds every frame; now a
/// window of 64 frames' seeds goes up every 64th frame); 400 moving cubes in a loop that never
/// waits for the GPU (three frames in flight) 148.
fn bound(mode: &str, backend: wgpu::Backend) -> u64 {
    let metal = match mode {
        "direct" => 84,
        "nodes" => 132,
        "hdr_seeds" => 165,
        "hdr_moving" => 167,
        "depth_views" => 113,
        "timings" => 110,
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
    let modes: [(&str, Setup); 7] = [
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
        // A depth view (shadow map, graphics spec §1.12) over Direct: one depth-only pass drawing
        // the casters, culled against the light's frustum.
        ("depth_views", |app, _| {
            let light = glam::camera::rh::proj::directx::orthographic(-4.0, 4.0, -4.0, 4.0, 0.0, 20.0)
                * glam::camera::rh::view::look_at_mat4(Vec3::new(0.0, 0.0, 10.0), Vec3::ZERO, Vec3::Y);
            space3d_render::testing::add_depth_view(
                app.resources.expect_mut::<Renderer>(),
                light,
                512,
                Default::default(),
            );
        }),
        // GPU timings (graphics spec §2.2.6) over Direct, where the device offers timestamps.
        ("timings", |app, _| {
            *app.resources.expect_mut::<FrameSettings>() = FrameSettings::default().with_gpu_timing(true);
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
        space3d_render::testing::clear_depth_views(app.resources.expect_mut::<Renderer>());
        if name == "timings" && !app.resources.expect::<Renderer>().graphics_caps().timestamps {
            eprintln!("timings: skipped (no TIMESTAMP_QUERY)");
            continue;
        }
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
        let intermediate = !matches!(name, "direct" | "nodes" | "depth_views" | "timings");
        assert_eq!(s.intermediate_groups, u32::from(intermediate), "{name}");
        if name == "nodes" {
            assert_eq!((s.node_draws, s.scene_read_passes), (3, 1), "{name}");
        }
        if name == "depth_views" {
            assert_eq!((s.depth_views, s.shadow_casters), (1, 400), "{name}");
        }
        if name == "timings" {
            let t = app.resources.expect::<GpuTimings>();
            assert!(t.scope_ms("scene") > 0.0, "{name}: {t:?}");
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
    // seeds change every frame: a window of 64 frames' seeds is uploaded at once) and frames
    // queue up on the GPU (waited for every 8th). Regression (review): the staging ring trimmed
    // the chunks still in flight and created and dropped a 4 MiB chunk every few frames
    // (0.5–1 ms each, an `Arc` per chunk: 100–230 per 1200 frames). Now it grows to the queue's
    // depth (each chunk created once, when the GPU first falls that far behind) and keeps them.
    let _ = app.world.remove_one::<PostProcess>(cam);
    *app.resources.expect_mut::<FrameSettings>() = FrameSettings::default();
    app.world
        .insert_one(cam, PostProcess::default().without_bloom())
        .unwrap();
    let (mut max, mut sum, mut counted_frames, mut reallocs, mut chunks, mut bytes) = (0, 0, 0, 0, 0, 0);
    let mut uploading = 0;
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
            uploading += u32::from(s.bytes_uploaded > 0);
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
        "hdr still, frames queued: allocations per frame mean {mean:.1} max {max}, {chunks} staging chunks created in 1200 frames and {reallocs} other buffers or textures after the first 16, {uploading} frames uploading, up to {bytes} B"
    );
    assert!(chunks <= 10, "{chunks} staging chunks created: the staging ring churns");
    assert_eq!(reallocs, 0, "nothing else is created for a still scene");
    // A still post group uploads nothing but its seed window: 64 frames × one aligned slot,
    // every 64th frame (it uploaded its 16 bytes of seeds every frame).
    assert!(
        bytes <= 64 * 256,
        "a still post group uploads its seed window only: {bytes} B"
    );
    assert!(uploading <= 1184 / 64 + 1, "{uploading} of 1184 frames uploaded");
    // wgpu retires queued submissions in batches (the frame after a wait retires eight): those
    // frames may allocate more; the mean stays the waited-for frame's.
    let seeds_bound = bound("hdr_seeds", backend);
    assert!(
        mean <= seeds_bound as f64,
        "hdr queued: {mean:.1} allocations per frame (bound {seeds_bound})"
    );
    assert!(
        max <= 2 * seeds_bound,
        "hdr queued: up to {max} allocations per frame (bound {})",
        2 * seeds_bound
    );

    // The same scene, cubes moving (their instances uploaded every frame), rendered in a loop
    // that never waits for the GPU — `App::update` / `step_ticks` without a window, the engine's
    // test pattern and headless batch renders. Regression (review): nothing paced the frames,
    // the queue deepened without bound (Metal accepts 4096 command buffers) and the staging
    // ring followed it, creating a 4 MiB chunk every few dozen frames (137 by frame 2400) while
    // the process grew by gigabytes. `render_frame` keeps at most three frames in flight now.
    let (mut late_chunks, mut chunks, mut max, mut sum) = (0, 0, 0, 0);
    let frames = 1500u32;
    for i in 0..frames {
        app.resources.expect_mut::<Time>().frame += 1;
        let dy = if i.is_multiple_of(2) { 0.01 } else { -0.01 };
        for &e in &movers {
            app.world.get::<&mut Transform>(e).unwrap().translation.y += dy;
        }
        space3d_core::transform::propagate_transforms(&mut app.world);
        let a0 = ALLOCS.load(Relaxed);
        app.run_stage(Stage::Render);
        let counted = ALLOCS.load(Relaxed) - a0;
        let s = stats(&app);
        chunks += s.staging_chunks_created;
        if i >= 100 {
            late_chunks += s.staging_chunks_created;
            max = max.max(counted);
            sum += counted;
        }
    }
    let mean = sum as f64 / (frames - 100) as f64;
    eprintln!(
        "hdr moving, unpaced: allocations per frame mean {mean:.1} max {max}, {chunks} staging chunks created in {frames} frames ({late_chunks} after the first 100)"
    );
    assert_eq!(late_chunks, 0, "the staging ring follows an unbounded queue");
    assert!(
        chunks <= 6,
        "{chunks} staging chunks for at most three frames in flight"
    );
    let bound = bound("hdr_moving", backend);
    assert!(
        mean <= bound as f64,
        "hdr unpaced: {mean:.1} allocations per frame (bound {bound})"
    );
    assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0);
}
