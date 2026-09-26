//! Material benchmarks (graphics spec §4.4 gate "ECS 100k static PBR entities, extraction ≤ 2.5
//! ms CPU"). Skipped (with a message) when no GPU adapter exists.
//!
//! * `materials/ecs_pbr/100k`: 100k static entities, 4 meshes × 8 PBR materials with ORM,
//!   normal and emissive maps (and a UV transform), spawned round-robin (interleaved batch
//!   keys): the render stage of an unchanged scene — material interning of every entity,
//!   batching, texture-set resolution — with the camera looking away (no draw cost).
//! * `materials/ecs_plain/100k`: the same with map-less materials (compare with
//!   `extract_upload/ecs_static_mixed/100k` of `benches/instancing.rs`).
//! * `materials/transparent_sort/4096`: one 4096-instance alpha-blended batch sorted back to
//!   front per frame (`TransparentSort::PerInstance`), camera looking at it.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use space3d_core::prelude::*;
use space3d_render::*;

fn app(looking_away: bool) -> Option<App> {
    let r = match pollster::block_on(Renderer::new_headless(64, 64)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping material benches: {e}");
            return None;
        }
    };
    let mut app = App::new();
    app.add_plugin(RenderPlugin::default());
    app.insert_resource(r);
    app.resources.expect_mut::<RenderSettings>().draw_path = Some(DrawPath::CpuDirect);
    let target = if looking_away {
        Vec3::new(0.0, 0.0, 100.0)
    } else {
        Vec3::ZERO
    };
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 10.0).looking_at(target, Vec3::Y),
        Camera3d::default(),
    ));
    Some(app)
}

fn wait(app: &App) {
    let _ = app
        .resources
        .expect::<Renderer>()
        .device()
        .poll(wgpu::PollType::wait_indefinitely());
}

fn ecs(c: &mut Criterion, name: &str, maps: bool) {
    let Some(mut app) = app(true) else { return };
    let n = 100_000;
    let a = app.resources.expect_mut::<RenderAssets>();
    let meshes: Vec<_> = (0..4)
        .map(|k| a.add_mesh(MeshData::cube(0.1 + 0.02 * k as f32)))
        .collect();
    let orm = a.add_image_linear(ImageData::filled(8, 8, [200, 128, 255, 255]));
    let normal = a.add_image_linear(ImageData::filled(8, 8, [128, 128, 255, 255]));
    let glow = a.add_image(ImageData::filled(8, 8, [255, 128, 0, 255]));
    let materials: Vec<Material> = (0..8)
        .map(|k| {
            let m = Material::srgb(0.1 * k as f32, 0.5, 0.8).with_metallic_roughness(1.0, 0.6);
            if maps {
                m.with_orm_map(orm)
                    .with_normal_map(normal, 1.0)
                    .with_emissive_map(glow, Vec3::splat(0.5))
                    .with_uv_transform(TextureTransform {
                        scale: Vec2::splat(2.0),
                        ..TextureTransform::IDENTITY
                    })
            } else {
                m
            }
        })
        .collect();
    for i in 0..n {
        app.world.spawn((
            Transform::from_xyz(i as f32 * 0.01, 0.0, 0.0),
            Mesh3d(meshes[i % 4]),
            materials[(i / 4) % 8].clone(),
        ));
    }
    app.step_ticks(1);
    wait(&app);
    let mut group = c.benchmark_group("materials");
    group.throughput(Throughput::Elements(n as u64));
    group.bench_function(name, |b| {
        b.iter(|| {
            app.run_stage(Stage::Render);
            wait(&app);
        })
    });
    group.finish();
}

fn ecs_pbr(c: &mut Criterion) {
    ecs(c, "ecs_pbr/100k", true);
}

fn ecs_plain(c: &mut Criterion) {
    ecs(c, "ecs_plain/100k", false);
}

fn transparent_sort(c: &mut Criterion) {
    let Some(mut app) = app(false) else { return };
    app.resources.expect_mut::<LightingSettings>().transparent_sort =
        TransparentSort::PerInstance { max_instances: 4096 };
    let quad = app
        .resources
        .expect_mut::<RenderAssets>()
        .add_mesh(MeshData::plane(0.05));
    let material = Material::unlit(Vec4::new(1.0, 0.5, 0.2, 0.5)).with_blend(BlendMode::AlphaBlend);
    for i in 0..4096u32 {
        // Scattered depths so the order changes as the camera moves.
        let z = ((i.wrapping_mul(2_654_435_761) >> 16) % 100) as f32 * -0.05;
        app.world.spawn((
            Transform::from_xyz((i % 64) as f32 * 0.06 - 2.0, (i / 64) as f32 * 0.06 - 2.0, z),
            Mesh3d(quad),
            material.clone(),
        ));
    }
    let camera = app
        .world
        .query::<(Entity, &Camera3d)>()
        .iter()
        .map(|(e, _)| e)
        .next()
        .unwrap();
    app.step_ticks(1);
    wait(&app);
    let mut frame = 0;
    let mut group = c.benchmark_group("materials");
    group.throughput(Throughput::Elements(4096));
    group.bench_function("transparent_sort/4096", |b| {
        b.iter(|| {
            frame += 1;
            let x = if frame % 2 == 0 { 0.5 } else { -0.5 };
            *app.world.get::<&mut Transform>(camera).unwrap() =
                Transform::from_xyz(x, 0.0, 10.0).looking_at(Vec3::ZERO, Vec3::Y);
            app.step_ticks(1);
            wait(&app);
        })
    });
    group.finish();
}

criterion_group!(benches, ecs_pbr, ecs_plain, transparent_sort);
criterion_main!(benches);
