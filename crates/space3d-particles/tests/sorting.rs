//! Per-particle back-to-front sorting (`SortMode::ByDistance`, graphics spec §5.2 PA4): every
//! view draws the particles of a sorted layer farthest first — on the CPU for direct draws, with
//! the GPU backend's bitonic sort for indirect ones — so overlapping alpha particles composite in
//! depth order, per view, deterministically (ties by instance order).

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_particles::prelude::*;
use space3d_particles::testing::{SlotState, read_pool};
use space3d_particles::{
    EmitShape, FloatRange, Gradient, ParticleBackend, ParticleBlend, SortMode, SpriteRender, SpriteShape,
};
use space3d_render::*;

const W: u32 = 512;
const H: u32 = 256;
/// Pixels per world unit (orthographic height 10 over 256 px).
const PPU: f32 = 25.6;
const RADIUS: f32 = 0.7;

/// `n` opaque alpha discs of `radius` in a box around the origin, born over half a second
/// (random delays) and colored by age (red when born, toward blue), drawn with `sort`; two views
/// side by side: from +Z (left half) and from −Z (right half).
fn scene_of(app: &mut App, sort: SortMode, n: u32, radius: f32) {
    let layer = ParticleLayer {
        lod: false,
        shape: EmitShape::Box {
            half_extents: Vec3::new(2.6, 2.6, 2.6),
        },
        particle_delay: FloatRange::new(0.0, 0.5),
        size: FloatRange::constant(2.0 * radius),
        color_over_life: Gradient::linear(Vec4::new(1.0, 0.0, 0.0, 1.0), Vec4::new(0.0, 0.0, 1.0, 1.0)),
        render: SpriteRender {
            shape: SpriteShape::Glow {
                core: 0.0,
                hardness: 1.0,
            },
            blend: ParticleBlend::Alpha,
            sort,
            ..SpriteRender::default()
        },
        ..ParticleLayer::burst(n, 1.0)
    };
    let fx = effect(app, layer);
    app.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO).seed(9).keep());
    for (i, (z, yaw)) in [(10.0, 0.0), (-10.0, std::f32::consts::PI)].into_iter().enumerate() {
        app.world.spawn((
            Transform::from_xyz(0.0, 0.0, z).with_rotation(Quat::from_rotation_y(yaw)),
            Camera3d {
                viewport: Some([0.5 * i as f32, 0.0, 0.5, 1.0]),
                ..Camera3d::default()
                    .with_clear_color(Vec4::new(0.0, 0.0, 0.0, 1.0))
                    .with_projection(Projection::Orthographic {
                        height: 10.0,
                        near: 0.1,
                        far: 100.0,
                    })
            },
        ));
    }
}

/// [`scene_of`] with 80 discs of [`RADIUS`].
fn scene(app: &mut App, sort: SortMode) {
    scene_of(app, sort, 80, RADIUS);
}

/// Pixels (sampled every 2 px, away from the top disc's rim) whose color is not the nearest
/// covering particle's of `radius`, and the pixels checked. The discs' colors follow their age.
fn misordered(img: &ImageData, particles: &[SlotState], radius: f32) -> (u32, u32) {
    let (mut bad, mut checked) = (0, 0);
    for view in 0..2 {
        for py in (0..H).step_by(2) {
            for px in (0..W / 2).step_by(2) {
                let sx = (px as f32 + 0.5 - 128.0) / PPU;
                let x = if view == 0 { sx } else { -sx };
                let y = (128.0 - py as f32 - 0.5) / PPU;
                // The nearest covering particle (largest z seen from +Z, smallest from −Z).
                let top = particles
                    .iter()
                    .filter(|p| p.age >= 0.0 && Vec2::new(p.pos.x - x, p.pos.y - y).length() < radius)
                    .max_by(|a, b| {
                        let (za, zb) = if view == 0 {
                            (a.pos.z, b.pos.z)
                        } else {
                            (-a.pos.z, -b.pos.z)
                        };
                        za.total_cmp(&zb)
                    });
                let Some(top) = top else { continue };
                let d = Vec2::new(top.pos.x - x, top.pos.y - y).length();
                if (radius - d) * PPU < 1.5 {
                    continue;
                }
                let t = top.age / top.life;
                let want = [srgb(1.0 - t), 0, srgb(t)];
                let got = img.get(px + view * W / 2, py);
                checked += 1;
                if (0..3).any(|c| got[c].abs_diff(want[c]) > 6) {
                    bad += 1;
                }
            }
        }
    }
    (bad, checked)
}

/// `ByDistance` draws each view's particles farthest first, on both backends: every checked
/// pixel shows the nearest particle covering it, seen from either side. Without it (spawn order,
/// `ByEmitter`) many pixels show a farther particle: the sort is what makes the difference.
#[test]
fn by_distance_draws_back_to_front_per_view() {
    for backend in [ParticleBackend::Gpu, ParticleBackend::Cpu] {
        for sort in [SortMode::ByDistance, SortMode::ByEmitter] {
            let Some(mut app) = app_with(W, H, ParticleSettings { backend, ..settings() }) else {
                return;
            };
            scene(&mut app, sort);
            app.step_ticks(36);
            let img = render(&mut app);
            dump(&img, &format!("sorting_{backend:?}_{sort:?}"));
            let (_, pool) = read_pool(&app).expect("pool");
            assert_eq!(pool.len(), 80);
            let (bad, checked) = misordered(&img, &pool, RADIUS);
            eprintln!("{backend:?} {sort:?}: {bad} of {checked} pixels misordered");
            assert!(checked > 2000, "{checked}");
            if sort == SortMode::ByDistance {
                assert!(
                    bad * 200 <= checked,
                    "{backend:?}: {bad} of {checked} pixels misordered"
                );
            } else {
                assert!(
                    bad * 10 > checked,
                    "{backend:?}: spawn order should misorder ({bad} of {checked})"
                );
            }
            if backend == ParticleBackend::Gpu && sort == SortMode::ByDistance {
                // A frame with a tick: simulate, mark, scatter, and a sort dispatch per view
                // (128 keys each: one block).
                app.step_ticks(1);
                let s = stats(&app);
                assert_eq!(s.dispatches, 3 + 2, "{s:?}");
            }
            assert_eq!(gpu_errors(&app), 0);
        }
    }
}

/// Sorting is deterministic and the frame without changes sorts nothing again: rendering twice
/// gives identical images, the second without sort dispatches or uploads (GPU backend), and the
/// two backends agree.
#[test]
fn sorting_is_deterministic_and_idle_when_nothing_changes() {
    let mut images = Vec::new();
    for backend in [ParticleBackend::Gpu, ParticleBackend::Cpu] {
        let Some(mut app) = app_with(W, H, ParticleSettings { backend, ..settings() }) else {
            return;
        };
        scene(&mut app, SortMode::ByDistance);
        app.step_ticks(40);
        let a = render(&mut app);
        let b = render(&mut app);
        assert!(a.pixels == b.pixels, "{backend:?}: identical renders");
        let b = render(&mut app);
        let s = stats(&app);
        assert!(a.pixels == b.pixels);
        assert_eq!((s.dispatches, s.bytes_uploaded), (0, 0), "{backend:?}: an idle frame");
        assert_eq!(s.cpu_reallocations, 0, "{backend:?}");
        images.push(a);
    }
    let diff = images[0]
        .pixels
        .iter()
        .zip(&images[1].pixels)
        .filter(|(a, b)| a.abs_diff(**b) > 8)
        .count();
    assert!(diff < 40, "the backends agree ({diff} channels differ)");
}

/// Groups larger than a workgroup's block (512 keys) sort through the global steps and the
/// local merges: 3000 discs (4096 keys: 10 dispatches per view) are drawn in depth order by the
/// GPU sort, like the CPU sort draws them.
#[test]
fn gpu_sort_of_large_groups() {
    let mut images = Vec::new();
    for backend in [ParticleBackend::Gpu, ParticleBackend::Cpu] {
        let Some(mut app) = app_with(W, H, ParticleSettings { backend, ..settings() }) else {
            return;
        };
        scene_of(&mut app, SortMode::ByDistance, 3000, 0.25);
        app.step_ticks(35);
        if backend == ParticleBackend::Gpu {
            // Simulate, mark, scatter; two views of 4096 keys: 1 + (1 + 1) + (2 + 1) + (3 + 1)
            // dispatches each.
            assert_eq!(stats(&app).dispatches, 3 + 2 * 10, "{:?}", stats(&app));
        }
        app.step_ticks(1);
        let img = render(&mut app);
        dump(&img, &format!("sorting_large_{backend:?}"));
        let (_, pool) = read_pool(&app).expect("pool");
        assert_eq!(pool.len(), 3000);
        let (bad, checked) = misordered(&img, &pool, 0.25);
        eprintln!("{backend:?}: {bad} of {checked} pixels misordered");
        assert!(
            checked > 2000 && bad * 100 <= checked,
            "{backend:?}: {bad} of {checked}"
        );
        assert_eq!(gpu_errors(&app), 0);
        images.push(img);
    }
    let diff = images[0]
        .pixels
        .chunks(4)
        .zip(images[1].pixels.chunks(4))
        .filter(|(a, b)| a.iter().zip(b.iter()).any(|(x, y)| x.abs_diff(*y) > 8))
        .count();
    assert!(
        diff * 200 < (W * H) as usize,
        "the backends agree ({diff} pixels differ)"
    );
}
