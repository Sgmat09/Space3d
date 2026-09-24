//! 1000 dynamic boxes falling onto a ground plane, 60 fixed ticks per iteration.
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use space3d_core::prelude::*;
use space3d_physics::{Collider, PhysicsPlugin, RigidBody};

fn setup() -> App {
    let mut app = App::new();
    app.add_plugin(PhysicsPlugin::default());
    app.world.spawn((
        Transform::from_xyz(0.0, -0.5, 0.0),
        RigidBody::Fixed,
        Collider::cuboid(Vec3::new(200.0, 1.0, 200.0)),
    ));
    for i in 0..1000u32 {
        let (x, y, z) = (i % 10, (i / 100), (i / 10) % 10);
        app.world.spawn((
            Transform::from_xyz(x as f32 * 1.2 - 6.0, 1.0 + y as f32 * 1.2, z as f32 * 1.2 - 6.0),
            RigidBody::Dynamic,
            Collider::cuboid(Vec3::ONE),
        ));
    }
    app
}

fn falling_boxes(c: &mut Criterion) {
    c.bench_function("physics/1000_falling_boxes_60_ticks", |b| {
        b.iter_batched(setup, |mut app| app.step_ticks(60), BatchSize::LargeInput)
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10);
    targets = falling_boxes
}
criterion_main!(benches);
