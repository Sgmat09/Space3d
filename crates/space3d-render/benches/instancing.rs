//! Instancing / upload benchmarks. Skipped (with a message) when no GPU adapter exists.
//!
//! * `extract_upload/explicit/{100k,1M}`: rewrite every instance of a dynamic buffer, then
//!   extract + upload + submit one frame (camera looks away, so no draw cost is measured).
//! * `extract_upload/ecs/100k`: 100k moving ECS entities batched and diff-uploaded.
//! * `mesh_arena/upload_1000`: register 1000 meshes and upload them into the arena.

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use space3d_core::prelude::*;
use space3d_render::*;

fn renderer() -> Option<Renderer> {
    match pollster::block_on(Renderer::new_headless(64, 64)) {
        Ok(r) => Some(r),
        Err(e) => {
            eprintln!("skipping render benches: {e}");
            None
        }
    }
}

fn app_with(renderer: Renderer) -> App {
    let mut app = App::new();
    app.add_plugin(RenderPlugin::default());
    app.insert_resource(renderer);
    app.resources.expect_mut::<RenderSettings>().draw_path = Some(DrawPath::CpuDirect);
    // Looking away from everything: measures extraction + upload, not rasterization.
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 10.0).looking_at(Vec3::new(0.0, 0.0, 100.0), Vec3::Y),
        Camera3d::default(),
    ));
    app
}

fn wait(app: &App) {
    let _ = app
        .resources
        .expect::<Renderer>()
        .device()
        .poll(wgpu::PollType::wait_indefinitely());
}

fn explicit(c: &mut Criterion) {
    let mut group = c.benchmark_group("extract_upload");
    group.sample_size(20);
    for n in [100_000usize, 1_000_000] {
        let Some(r) = renderer() else { return };
        let mut app = app_with(r);
        let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.1));
        let data = (0..n)
            .map(|i| InstanceCompact::new(Vec3::new(i as f32 * 0.01, 0.0, 0.0), Quat::IDENTITY, 1.0))
            .collect();
        let h = app
            .resources
            .expect_mut::<InstanceBuffers>()
            .insert(InstanceBuffer::from_vec(data, BufferUsage::Dynamic));
        app.world.spawn((InstancedMesh {
            mesh,
            material: Material::default(),
            instances: h,
        },));
        app.step_ticks(1);
        wait(&app);
        let mut t = 0.0f32;
        group.throughput(Throughput::Elements(n as u64));
        group.bench_function(format!("explicit/{}k", n / 1000), |b| {
            b.iter(|| {
                t += 0.01;
                let buf = app
                    .resources
                    .expect_mut::<InstanceBuffers>()
                    .get_mut::<InstanceCompact>(h)
                    .unwrap();
                for inst in buf.as_mut_slice() {
                    inst.translation[1] = t;
                }
                app.run_stage(Stage::Render);
                wait(&app);
            })
        });
    }

    let Some(r) = renderer() else { return };
    let mut app = app_with(r);
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.1));
    let n = 100_000;
    for i in 0..n {
        app.world.spawn((
            Transform::from_xyz(i as f32 * 0.01, 0.0, 0.0),
            Mesh3d(mesh),
            Material::default(),
        ));
    }
    app.step_ticks(1);
    wait(&app);
    group.throughput(Throughput::Elements(n as u64));
    group.bench_function("ecs/100k", |b| {
        b.iter(|| {
            for (_, t) in app.world.query_mut::<(Entity, &mut Transform)>() {
                t.translation.y += 0.01;
            }
            app.step_ticks(1);
            wait(&app);
        })
    });
    group.finish();
}

fn mesh_arena(c: &mut Criterion) {
    let Some(mut r) = renderer() else { return };
    let meshes: Vec<MeshData> = (0..1000)
        .map(|i| MeshData::sphere(1.0, 8 + i % 16, 4 + i % 8))
        .collect();
    let mut group = c.benchmark_group("mesh_arena");
    group.sample_size(20);
    group.throughput(Throughput::Elements(meshes.len() as u64));
    group.bench_function("upload_1000", |b| {
        b.iter_batched(
            || {
                let mut assets = RenderAssets::default();
                for m in &meshes {
                    assets.add_mesh(m.clone());
                }
                assets
            },
            |mut assets| {
                r.upload_assets(&mut assets);
                let _ = r.device().poll(wgpu::PollType::wait_indefinitely());
                assets
            },
            BatchSize::LargeInput,
        )
    });
    group.finish();
}

criterion_group!(benches, explicit, mesh_arena);
criterion_main!(benches);
