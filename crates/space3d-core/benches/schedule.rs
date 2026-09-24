use criterion::{Criterion, criterion_group, criterion_main};
use space3d_core::prelude::*;

#[derive(Clone, Copy)]
struct Vel(Vec3);

fn bench(c: &mut Criterion) {
    let mut app = App::new();
    for i in 0..100_000 {
        app.world.spawn((Transform::from_xyz(i as f32, 0.0, 0.0), Vel(Vec3::X)));
    }
    app.add_system(Stage::FixedUpdate, |ctx| {
        let dt = ctx.resources.expect::<Time>().fixed_delta_secs();
        for (t, v) in ctx.world.query_mut::<(&mut Transform, &Vel)>() {
            t.translation += v.0 * dt;
        }
    });
    c.bench_function("100k entities, 1 tick + transform propagation", |b| b.iter(|| app.step_ticks(1)));
}

criterion_group!(benches, bench);
criterion_main!(benches);
