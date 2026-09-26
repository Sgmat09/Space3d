//! A followed emitter's world-space trail reaches its local-space layers whatever stage moves
//! the emitter and whatever the frame rate: a trail on a bolt moved at 1500 units/s in
//! `Stage::FixedUpdate`, `PreUpdate` or `Update` (SpaceCorps2027 moves bolts and avatars in
//! `Update`), at 60 Hz and at 120 Hz (frames with and without a tick), on both backends — the
//! ribbon of `builtin::LASER_TRAIL` (its head is drawn at the render pose) and a trail of sprite
//! glows emitted per distance (emission is decided on the tick; between the pose the last tick
//! read and the render pose, the render node lays the next tick's emission: the render-time
//! bridge). Before the bridge, an `Update` mover left a gap of one frame of motion (25 units at
//! 60 Hz) between the trail and the head.

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_particles::ParticleBackend;
use space3d_particles::prelude::*;
use space3d_render::{Camera3d, Projection};

const W: u32 = 640;
const H: u32 = 360;
/// Pixels per world unit of the top-down orthographic camera.
const PPU: f32 = 2.0;
const START: f32 = -150.0;
/// Bolt speed in units per 60 Hz tick (1500 units/s).
const PER_TICK: f32 = 25.0;

/// The bolt row after `frames` frames of `1 / (60 · sub)` s, the bolt moved in `stage` (by
/// `PER_TICK / sub` per frame, or per tick in `FixedUpdate`): its x in pixels, and the image.
fn run(backend: ParticleBackend, sprites: bool, stage: Stage, sub: f32, frames: u32) -> Option<(f32, ImageData)> {
    let settings = ParticleSettings {
        backend,
        ..ParticleSettings::default()
    };
    let mut app = app_with(W, H, settings)?;
    app.world.spawn((
        Transform::from_xyz(0.0, 200.0, 0.0).looking_at(Vec3::ZERO, Vec3::NEG_Z),
        Camera3d::default()
            .with_clear_color(Vec4::new(0.0, 0.0, 0.0, 1.0))
            .with_projection(Projection::Orthographic {
                height: H as f32 / PPU,
                near: 1.0,
                far: 400.0,
            }),
    ));
    let bolt = app.world.spawn((Transform::from_xyz(START, 0.0, 0.0),));
    let fx = if sprites {
        sprite_trail(&mut app)
    } else {
        builtin::LASER_TRAIL
    };
    app.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO).follow(bolt).seed(1));
    let fixed = stage == Stage::FixedUpdate;
    let mut frame = 0u32;
    app.add_system(stage, move |ctx: &mut SystemCtx| {
        frame += 1;
        let x = if fixed {
            START + PER_TICK * (ctx.time().tick + 1) as f32
        } else {
            START + PER_TICK / sub * frame as f32
        };
        if let Ok(mut t) = ctx.world.get::<&mut Transform>(bolt) {
            t.translation.x = x;
        }
    });
    let step = app.resources.expect::<TickRate>().step();
    for _ in 0..frames {
        app.update(step.div_f32(sub));
    }
    let x = app.world.get::<&Transform>(bolt).unwrap().translation.x;
    let img = render(&mut app);
    assert_eq!(gpu_errors(&app), 0);
    Some((W as f32 / 2.0 + x * PPU, img))
}

fn luma(p: [u8; 4]) -> u32 {
    (p[0] as u32 * 3 + p[1] as u32 * 6 + p[2] as u32) / 10
}

/// Lit columns of the bolt row: first, last, and the dark columns between them.
fn scan(img: &ImageData) -> (u32, u32, u32) {
    let lit: Vec<bool> = (0..W).map(|x| luma(img.get(x, H / 2)) > 24).collect();
    let first = lit.iter().position(|&l| l).unwrap_or(0) as u32;
    let last = lit.iter().rposition(|&l| l).unwrap_or(0) as u32;
    let dark = (first..=last).filter(|&x| !lit[x as usize]).count() as u32;
    (first, last, dark)
}

#[test]
fn laser_trail_reaches_its_head_in_every_stage_and_frame_rate() {
    for (backend, sprites) in [
        (ParticleBackend::Cpu, false),
        (ParticleBackend::Gpu, false),
        (ParticleBackend::Cpu, true),
        (ParticleBackend::Gpu, true),
    ] {
        for (name, stage) in [
            ("fixed", Stage::FixedUpdate),
            ("pre", Stage::PreUpdate),
            ("update", Stage::Update),
        ] {
            // 60 Hz; 120 Hz ending on a frame with a tick and on one without; 30 Hz (two
            // ticks a frame: game#2).
            for (sub, frames) in [(1.0, 9), (2.0, 17), (2.0, 18), (0.5, 5)] {
                let Some((bolt_px, img)) = run(backend, sprites, stage, sub, frames) else {
                    return;
                };
                let kind = if sprites { "sprites" } else { "ribbon" };
                let what = format!("{kind} {backend:?} {name} {}Hz, {frames} frames", 60.0 * sub);
                dump(&img, &format!("follow_{kind}_{backend:?}_{name}_{sub}_{frames}"));
                let (first, last, dark) = scan(&img);
                assert_eq!(
                    dark, 0,
                    "{what}: {dark} dark columns between the trail and the head ({first}..{last})"
                );
                // The head (size 8: 16 px across, glow edge a little inside) is centered on the
                // bolt, and the trail is long (0.15 s at 1500 units/s ≈ 450 px, from the start).
                assert!(
                    (last as f32 - bolt_px).abs() <= 8.0,
                    "{what}: lit to {last}, the bolt is at {bolt_px}"
                );
                assert!(last - first > 200, "{what}: trail {first}..{last}");
            }
        }
    }
}
