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
//!
//! The scene fills the frame: a lit ground plane, 400 lit cubes (emissive up to 6) and a sun.
//! `hdr_minimal` is a post camera without bloom (the composite only: exposure, AgX, dither).

use criterion::{Criterion, criterion_group, criterion_main};
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
                    app.run_stage(Stage::Render);
                    total += t.elapsed();
                    wait(&app);
                }
                total
            })
        });
        group.bench_function(format!("gpu/{name}"), |b| {
            b.iter(|| {
                app.run_stage(Stage::Render);
                wait(&app);
            })
        });
        // Frames recorded back to back (waiting every 8th): the time per frame is the larger
        // of the CPU and the GPU time, here the GPU's.
        group.bench_function(format!("throughput/{name}"), |b| {
            b.iter_custom(|iters| {
                let t = Instant::now();
                for i in 0..iters {
                    app.run_stage(Stage::Render);
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

criterion_group!(benches, frame);
criterion_main!(benches);
