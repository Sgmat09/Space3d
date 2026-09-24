//! Texture op benchmarks on 1024² images: `cargo bench -p space3d-assets`.

use criterion::{Criterion, criterion_group, criterion_main};
use space3d_assets::edit::texture::{Filter, box_blur, gaussian_blur, resize, value_noise};
use std::hint::black_box;

fn texture_ops(c: &mut Criterion) {
    let img = value_noise(1024, 1024, 7, 64.0, 5);
    let mut g = c.benchmark_group("texture_1024");
    g.sample_size(20);
    g.bench_function("box_blur_r4", |b| b.iter(|| box_blur(black_box(&img), 4)));
    g.bench_function("box_blur_r32", |b| b.iter(|| box_blur(black_box(&img), 32)));
    g.bench_function("gaussian_blur_s2", |b| b.iter(|| gaussian_blur(black_box(&img), 2.0)));
    g.bench_function("gaussian_blur_s6", |b| b.iter(|| gaussian_blur(black_box(&img), 6.0)));
    g.bench_function("resize_bilinear_to_512", |b| {
        b.iter(|| resize(black_box(&img), 512, 512, Filter::Bilinear))
    });
    g.bench_function("resize_bilinear_to_2048", |b| {
        b.iter(|| resize(black_box(&img), 2048, 2048, Filter::Bilinear))
    });
    g.bench_function("resize_nearest_to_512", |b| {
        b.iter(|| resize(black_box(&img), 512, 512, Filter::Nearest))
    });
    g.finish();
}

criterion_group!(benches, texture_ops);
criterion_main!(benches);
