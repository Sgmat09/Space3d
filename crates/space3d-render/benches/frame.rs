//! Frame-mode benchmarks at 1080p (graphics spec §4.4 gate "Hdr minimal over Direct").
//! Skipped (with a message) when no GPU adapter exists.
//!
//! * `frame/cpu/{direct,hdr_minimal}`: the render stage on the CPU (extraction, planning,
//!   recording and submission; the GPU is waited for outside the measurement).
//! * `frame/gpu/{direct,hdr_minimal}`: the render stage and the wait for the GPU — one frame's
//!   CPU + GPU latency. The difference between the two modes bounds the HDR path's added GPU
//!   time (scene texture and composite) from above.
//! * `frame/throughput/{direct,hdr_minimal}`: frames recorded back to back (a wait every 8th):
//!   the time per frame is the GPU's when it is the bottleneck.
//! * `frame/ab` (printed after the criterion groups; run alone with `-- frame/ab`): the two
//!   modes interleaved round by round, so load changes hit both alike, reporting the median of
//!   the paired deltas (Hdr minus Direct) for the CPU time, the throughput and the latency; and
//!   the latency of isolated frames 2 ms apart, where the GPU idles between frames and drops
//!   to its lowest clocks. The §4.4 gate is measured on the loaded GPU (throughput); the
//!   isolated figure is reported for reference (every pass costs about 3× more there).
//!
//! The scene fills the frame: a lit ground plane, 400 lit cubes (emissive up to 6) and a sun.
//! `hdr_minimal` is a post camera without bloom (the composite only: exposure, AgX, dither).

use criterion::{Criterion, criterion_group};
use space3d_core::prelude::*;
use space3d_render::*;
use std::time::{Duration, Instant};

fn app(post: bool) -> Option<App> {
    let renderer = match pollster::block_on(Renderer::new_headless(1920, 1080)) {
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
    if post {
        app.world
            .insert_one(cam, PostProcess::default().without_bloom())
            .unwrap();
    }
    for _ in 0..5 {
        app.step_ticks(1);
        wait(&app);
    }
    let s = app.resources.expect::<RenderStats>();
    assert_eq!(s.intermediate_groups, u32::from(post));
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
    for (name, post) in [("direct", false), ("hdr_minimal", true)] {
        let Some(mut app) = app(post) else { return };
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

/// `frame/ab`: Direct and Hdr minimal interleaved (the order alternating per round); medians of
/// the per-round values and of the paired deltas.
fn frame_ab() {
    let filter = std::env::args().skip(1).find(|a| !a.starts_with('-'));
    if filter.is_some_and(|f| !"frame/ab".contains(f.as_str())) {
        return;
    }
    let (Some(mut direct), Some(mut hdr)) = (app(false), app(true)) else {
        return;
    };
    let rounds = 24;
    let mut values: [[Vec<f64>; 4]; 2] = Default::default();
    let mut deltas: [Vec<f64>; 4] = Default::default();
    let mut record = |m: usize, pair: [f64; 2]| {
        values[0][m].push(pair[0]);
        values[1][m].push(pair[1]);
        deltas[m].push(pair[1] - pair[0]);
    };
    // Loaded GPU, then (separately, so the clocks can drop) isolated frames.
    for r in 0..rounds {
        let mut round = [[0.0; 3]; 2];
        for k in if r % 2 == 0 { [0, 1] } else { [1, 0] } {
            round[k] = ab_round(if k == 0 { &mut direct } else { &mut hdr });
        }
        for (m, (d, h)) in round[0].into_iter().zip(round[1]).enumerate() {
            record(m, [d, h]);
        }
    }
    for r in 0..rounds / 2 {
        let mut pair = [0.0; 2];
        for k in if r % 2 == 0 { [0, 1] } else { [1, 0] } {
            pair[k] = idle_round(if k == 0 { &mut direct } else { &mut hdr }, Duration::from_millis(2));
        }
        record(3, pair);
    }
    println!("frame/ab (interleaved rounds, medians in µs): Direct | Hdr minimal | paired delta");
    let labels = ["cpu", "throughput", "latency", "latency, frames 2 ms apart"];
    for (m, label) in labels.iter().enumerate() {
        println!(
            "  {label:<28} {:8.1} | {:8.1} | {:+8.1}",
            median(&mut values[0][m]),
            median(&mut values[1][m]),
            median(&mut deltas[m])
        );
    }
}

criterion_group!(benches, frame);

fn main() {
    benches();
    frame_ab();
    Criterion::default().configure_from_args().final_summary();
}
