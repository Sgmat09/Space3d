//! Frame-mode benchmarks at 1080p (graphics spec §4.4 gates "Hdr minimal over Direct", bloom,
//! shadow). Skipped (with a message) when no GPU adapter exists.
//!
//! Modes: `direct` (no post), `hdr_minimal` (a post camera without bloom: the composite only),
//! `hdr_bloom` (`PostProcess::default()`: the composite and the default `Bloom::glow()` chain),
//! `depth_view` (Direct plus one 2048² depth view drawing the scene's casters, as a shadow map
//! does; injected with `space3d_render::testing`).
//!
//! * `frame/cpu/<mode>`: the render stage on the CPU (extraction, planning, recording and
//!   submission; the GPU is waited for outside the measurement).
//! * `frame/gpu/<mode>`: the render stage and the wait for the GPU — one frame's CPU + GPU
//!   latency.
//! * `frame/throughput/<mode>`: frames recorded back to back (a wait every 8th): the time per
//!   frame is the GPU's when it is the bottleneck.
//! * `frame/ab` (printed after the criterion groups; run alone with `-- frame/ab`): the modes
//!   interleaved round by round, so load changes hit all alike, reporting per mode the median
//!   of the paired deltas against Direct for the CPU time, the throughput and the latency; the
//!   latency of isolated frames 2 ms apart, where the GPU idles between frames and drops to its
//!   lowest clocks; and each mode's GPU timings per pass (`FrameSettings::gpu_timing`, loaded
//!   GPU; medians). The §4.4 gates are measured on the loaded GPU (throughput, timings); the
//!   isolated figure is reported for reference (every pass costs about 3× more there).
//!
//! The scene fills the frame: a lit ground plane, 400 lit cubes (emissive up to 6) and a sun.

use criterion::{Criterion, criterion_group};
use space3d_core::prelude::*;
use space3d_render::*;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Direct,
    HdrMinimal,
    HdrBloom,
    DepthView,
}

const MODES: [(&str, Mode); 4] = [
    ("direct", Mode::Direct),
    ("hdr_minimal", Mode::HdrMinimal),
    ("hdr_bloom", Mode::HdrBloom),
    ("depth_view", Mode::DepthView),
];

fn app(mode: Mode) -> Option<App> {
    sized_app(mode, (1920, 1080))
}

fn sized_app(mode: Mode, (width, height): (u32, u32)) -> Option<App> {
    let renderer = match pollster::block_on(Renderer::new_headless(width, height)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping frame benches: {e}");
            return None;
        }
    };
    let mut app = App::new();
    app.add_plugin(RenderPlugin::default());
    app.insert_resource(renderer);
    let (cube, plane) = {
        let a = app.resources.expect_mut::<RenderAssets>();
        (a.add_mesh(MeshData::cube(0.3)), a.add_mesh(MeshData::plane(40.0)))
    };
    app.world.spawn((
        Transform::from_xyz(0.0, -1.0, 0.0),
        Mesh3d(plane),
        Material::srgb(0.4, 0.42, 0.45),
    ));
    for i in 0..400u32 {
        app.world.spawn((
            Transform::from_xyz((i % 20) as f32 * 0.5 - 5.0, -0.7, (i / 20) as f32 * -0.5),
            Mesh3d(cube),
            Material::srgb(0.8, 0.4, 0.2).with_emissive(Vec3::splat((i % 7) as f32)),
        ));
    }
    app.world.spawn((
        Transform::IDENTITY.looking_at(Vec3::new(-0.3, -1.0, -0.6), Vec3::Y),
        DirectionalLight {
            color: Vec3::ONE,
            intensity: 2.0,
        },
    ));
    let cam = app.world.spawn((
        Transform::from_xyz(0.0, 1.5, 4.0).looking_at(Vec3::new(0.0, -1.0, -4.0), Vec3::Y),
        Camera3d::default(),
    ));
    match mode {
        Mode::Direct => {}
        Mode::HdrMinimal => app
            .world
            .insert_one(cam, PostProcess::default().without_bloom())
            .unwrap(),
        Mode::HdrBloom => app.world.insert_one(cam, PostProcess::default()).unwrap(),
        Mode::DepthView => {
            // The sun's view of the cube field: an orthographic box around it.
            let view =
                glam::camera::rh::view::look_at_mat4(Vec3::new(3.0, 10.0, 6.0), Vec3::new(0.0, -1.0, -5.0), Vec3::Y);
            let proj = glam::camera::rh::proj::directx::orthographic(-12.0, 12.0, -12.0, 12.0, 0.0, 40.0);
            let bias = wgpu::DepthBiasState {
                constant: 2,
                slope_scale: 1.5,
                clamp: 0.0,
            };
            space3d_render::testing::add_depth_view(app.resources.expect_mut::<Renderer>(), proj * view, 2048, bias);
        }
    }
    for _ in 0..5 {
        app.step_ticks(1);
        wait(&app);
    }
    let s = app.resources.expect::<RenderStats>();
    assert_eq!(
        s.intermediate_groups,
        u32::from(matches!(mode, Mode::HdrMinimal | Mode::HdrBloom))
    );
    assert_eq!(s.depth_views, u32::from(mode == Mode::DepthView));
    Some(app)
}

/// One frame of the render stage as `App::update` runs it: `Time::frame` advances first, so
/// the post camera's per-frame seeds (dither, grain: 16 bytes) are uploaded every frame like in
/// a real loop (with `run_stage` alone the frame counter stood still and an Hdr frame uploaded
/// nothing, hiding the staging traffic it causes).
fn render(app: &mut App) {
    app.resources.expect_mut::<Time>().frame += 1;
    app.run_stage(Stage::Render);
}

fn wait(app: &App) {
    let _ = app
        .resources
        .expect::<Renderer>()
        .device()
        .poll(wgpu::PollType::wait_indefinitely());
}

fn frame(c: &mut Criterion) {
    let mut group = c.benchmark_group("frame");
    group.sample_size(40);
    for (name, mode) in MODES {
        let Some(mut app) = app(mode) else { return };
        group.bench_function(format!("cpu/{name}"), |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let t = Instant::now();
                    render(&mut app);
                    total += t.elapsed();
                    wait(&app);
                }
                total
            })
        });
        group.bench_function(format!("gpu/{name}"), |b| {
            b.iter(|| {
                render(&mut app);
                wait(&app);
            })
        });
        // Frames recorded back to back (waiting every 8th): the time per frame is the larger
        // of the CPU and the GPU time, here the GPU's.
        group.bench_function(format!("throughput/{name}"), |b| {
            b.iter_custom(|iters| {
                let t = Instant::now();
                for i in 0..iters {
                    render(&mut app);
                    if i % 8 == 7 {
                        wait(&app);
                    }
                }
                wait(&app);
                t.elapsed()
            })
        });
    }
    group.finish();
}

/// Median of `v` (sorted in place).
fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// (CPU median, throughput per frame, latency median) of one round, in µs, after 30 ms of
/// frames back to back (so the GPU runs at its loaded clocks).
fn ab_round(app: &mut App) -> [f64; 3] {
    let spin = Instant::now();
    while spin.elapsed() < Duration::from_millis(30) {
        render(app);
    }
    wait(app);
    let mut cpu = Vec::with_capacity(16);
    for _ in 0..16 {
        let t = Instant::now();
        render(app);
        cpu.push(t.elapsed().as_secs_f64() * 1e6);
        wait(app);
    }
    let n = 64;
    let t = Instant::now();
    for i in 0..n {
        render(app);
        if i % 8 == 7 {
            wait(app);
        }
    }
    wait(app);
    let throughput = t.elapsed().as_secs_f64() * 1e6 / n as f64;
    let mut latency = Vec::with_capacity(16);
    for _ in 0..16 {
        let t = Instant::now();
        render(app);
        wait(app);
        latency.push(t.elapsed().as_secs_f64() * 1e6);
    }
    [median(&mut cpu), throughput, median(&mut latency)]
}

/// Latency median (µs) of isolated frames `idle` apart.
fn idle_round(app: &mut App, idle: Duration) -> f64 {
    let mut v = Vec::with_capacity(30);
    for _ in 0..30 {
        std::thread::sleep(idle);
        let t = Instant::now();
        render(app);
        wait(app);
        v.push(t.elapsed().as_secs_f64() * 1e6);
    }
    median(&mut v)
}

/// Median GPU milliseconds per pass label over `frames` timed frames back to back (the loaded
/// GPU), then timing off again.
fn gpu_timings(app: &mut App, frames: usize) -> Vec<(&'static str, f64)> {
    app.resources.expect_mut::<FrameSettings>().gpu_timing = true;
    let spin = Instant::now();
    while spin.elapsed() < Duration::from_millis(30) {
        render(app);
    }
    let mut samples: Vec<(&'static str, Vec<f64>)> = Vec::new();
    let mut last = None;
    let mut i = 0;
    while samples.first().is_none_or(|(_, v)| v.len() < frames) && i < frames * 4 {
        render(app);
        if i % 8 == 7 {
            wait(app);
        }
        i += 1;
        let t = app.resources.expect::<GpuTimings>();
        if t.frame == last || t.frame.is_none() {
            continue;
        }
        last = t.frame;
        let mut per_label: Vec<(&'static str, f64)> = Vec::new();
        for s in &t.scopes {
            match per_label.iter_mut().find(|(l, _)| *l == s.label) {
                Some((_, ms)) => *ms += s.ms as f64,
                None => per_label.push((s.label, s.ms as f64)),
            }
        }
        for (label, ms) in per_label {
            match samples.iter_mut().find(|(l, _)| *l == label) {
                Some((_, v)) => v.push(ms),
                None => samples.push((label, vec![ms])),
            }
        }
    }
    wait(app);
    app.resources.expect_mut::<FrameSettings>().gpu_timing = false;
    samples.into_iter().map(|(l, mut v)| (l, median(&mut v))).collect()
}

/// `frame/ab`: the modes interleaved (the order rotating per round); medians of the per-round
/// values and of the paired deltas against Direct; then each mode's GPU timings per pass.
fn frame_ab() {
    let filter = std::env::args().skip(1).find(|a| !a.starts_with('-'));
    if filter.is_some_and(|f| !"frame/ab".contains(f.as_str())) {
        return;
    }
    let mut apps = Vec::new();
    for (_, mode) in MODES {
        let Some(app) = app(mode) else { return };
        apps.push(app);
    }
    let n = apps.len();
    let rounds = 24;
    // values[mode][metric], deltas[mode][metric] (against Direct).
    let mut values = vec![vec![Vec::new(); 4]; n];
    let mut deltas = vec![vec![Vec::new(); 4]; n];
    // Loaded GPU, then (separately, so the clocks can drop) isolated frames.
    for r in 0..rounds {
        let mut round = vec![[0.0; 3]; n];
        for j in 0..n {
            let k = (j + r) % n;
            round[k] = ab_round(&mut apps[k]);
        }
        for k in 0..n {
            for m in 0..3 {
                values[k][m].push(round[k][m]);
                deltas[k][m].push(round[k][m] - round[0][m]);
            }
        }
    }
    for r in 0..rounds / 2 {
        let mut round = vec![0.0; n];
        for j in 0..n {
            let k = (j + r) % n;
            round[k] = idle_round(&mut apps[k], Duration::from_millis(2));
        }
        for k in 0..n {
            values[k][3].push(round[k]);
            deltas[k][3].push(round[k] - round[0]);
        }
    }
    println!("frame/ab (interleaved rounds, medians in µs; the paired delta against Direct in brackets)");
    let labels = ["cpu", "throughput", "latency", "latency, frames 2 ms apart"];
    for (m, label) in labels.iter().enumerate() {
        let mut line = format!("  {label:<28}");
        for k in 0..n {
            let v = median(&mut values[k][m]);
            if k == 0 {
                line += &format!(" {}: {v:8.1}", MODES[k].0);
            } else {
                line += &format!(" | {}: {v:8.1} ({:+7.1})", MODES[k].0, median(&mut deltas[k][m]));
            }
        }
        println!("{line}");
    }
    if !apps[0].resources.expect::<Renderer>().graphics_caps().timestamps {
        println!("frame/ab: no TIMESTAMP_QUERY, no GPU timings");
        return;
    }
    println!("frame/ab GPU timings (FrameSettings::gpu_timing, loaded GPU, medians in ms):");
    for (k, app) in apps.iter_mut().enumerate() {
        let timings = gpu_timings(app, 48);
        let line: Vec<String> = timings.iter().map(|(l, ms)| format!("{l} {ms:.3}")).collect();
        println!("  {:<12} {}", MODES[k].0, line.join(", "));
    }
    drop(apps);
    // The default bloom chain at other sizes (the prefilter halves the rect; above 1080p a
    // resample pass makes level 0 of it), and the Low preset's chain (4 levels, 270 cap) at the
    // T3 rect (1440 × 810, render scale 0.75 of 1080p).
    println!("frame/ab bloom GPU time by size (post.bloom, loaded GPU, medians in ms):");
    for (label, size, low) in [
        ("1280x720", (1280, 720), false),
        ("1920x1080", (1920, 1080), false),
        ("2560x1440", (2560, 1440), false),
        ("3840x2160", (3840, 2160), false),
        ("1440x810 Low", (1440, 810), true),
    ] {
        let Some(mut app) = sized_app(Mode::HdrBloom, size) else {
            return;
        };
        if low {
            let cam = app
                .world
                .query::<(Entity, &Camera3d)>()
                .iter()
                .map(|(e, _)| e)
                .next()
                .expect("camera");
            let mut bloom = Bloom::glow();
            bloom.mip_count = 4;
            bloom.max_mip_height = 270;
            app.world
                .insert_one(cam, PostProcess::default().with_bloom(bloom))
                .unwrap();
        }
        let timings = gpu_timings(&mut app, 48);
        let ms = timings
            .iter()
            .find(|(l, _)| *l == "post.bloom")
            .map_or(f64::NAN, |(_, ms)| *ms);
        let s = app.resources.expect::<RenderStats>();
        println!(
            "  {label:<14} post.bloom {ms:.3} ms, {} render passes, target_bytes {:.1} MB",
            s.render_passes,
            s.target_bytes as f64 / 1e6
        );
    }
}

criterion_group!(benches, frame);

fn main() {
    benches();
    frame_ab();
    Criterion::default().configure_from_args().final_summary();
}
