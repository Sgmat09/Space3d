//! Ribbons and beams (graphics spec §5.2 PA3): ribbons are watertight at bends (no gaps, no
//! overlaps), shorten smoothly to exactly their lifetime, and reach their emitter's render pose;
//! beams span their end points (round ends, end glows) and follow an end entity; both keep a
//! minimum on-screen width.

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_particles::prelude::*;
use space3d_particles::{Curve, Gradient, ParticleBlend, RibbonFacing, RibbonProfile};

const W: u32 = 320;
const H: u32 = 180;

/// sRGB byte → linear.
fn linear(v: u8) -> f32 {
    let c = v as f32 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Pixel at world `(x, y)` under an ortho camera of height 10 at 320×180 (18 px per unit).
fn px(img: &ImageData, x: f32, y: f32) -> [u8; 4] {
    at(img, 0.5 + x * 18.0 / W as f32, 0.5 - y * 18.0 / H as f32)
}

/// A hard-edged, flat ribbon of half brightness (overlaps would double it).
fn flat_ribbon(width: f32, lifetime: f32) -> RibbonLayer {
    RibbonLayer {
        min_distance: 0.2,
        max_points: 256,
        lifetime,
        width,
        color_over_life: Gradient::constant(Vec4::new(0.5, 0.5, 0.5, 1.0)),
        profile: RibbonProfile {
            core: 0.0,
            hardness: 1.0,
        },
        facing: RibbonFacing::Plane { normal: Vec3::Z },
        ..RibbonLayer::default()
    }
}

fn add(app: &mut App, effect: ParticleEffect) -> EffectHandle {
    app.resources.expect_mut::<ParticleEffects>().add(effect).unwrap()
}

/// A ribbon following an emitter around three quarters of a circle (radius 3, a point every
/// 0.2 units, sharp enough bends at 18 px per unit): every pixel along its center line is lit
/// with the ribbon's own brightness — no gap between segments, no doubled overlap at a bend —
/// and the strip stays within its width of the path.
#[test]
fn ribbons_are_watertight_at_bends() {
    let Some(mut app) = app_with(W, H, settings()) else {
        return;
    };
    ortho_camera(&mut app, 10.0, Vec4::new(0.0, 0.0, 0.0, 1.0));
    let fx = add(
        &mut app,
        ParticleEffect {
            looping: true,
            ..ParticleEffect::new("ring").ribbon(flat_ribbon(0.6, 100.0))
        },
    );
    let e = app.spawn_effect(fx, EffectSpawn::at(Vec3::new(3.0, 0.0, 0.0)).keep());
    let turn = |t: u32| t as f32 * 0.035;
    for t in 1..=135u32 {
        let a = turn(t);
        app.world.get::<&mut Transform>(e).unwrap().translation = Vec3::new(3.0 * a.cos(), 3.0 * a.sin(), 0.0);
        app.step_ticks(1);
    }
    let img = render(&mut app);
    dump(&img, "ribbon_watertight");
    let mut lo = f32::MAX;
    let mut hi = 0.0f32;
    // Along the path, away from the round head.
    for k in 0..400 {
        let a = 0.05 + (turn(135) - 0.3) * k as f32 / 399.0;
        let v = linear(px(&img, 3.0 * a.cos(), 3.0 * a.sin())[0]);
        lo = lo.min(v);
        hi = hi.max(v);
    }
    eprintln!("center line: {lo:.3}..{hi:.3}");
    assert!(
        lo > 0.45 && hi < 0.55,
        "center line {lo}..{hi} (want 0.5: no gaps, no overlaps)"
    );
    // Off the path by more than the half width (0.3 + a pixel): dark.
    for k in 0..100 {
        let a = 0.1 + 3.5 * k as f32 / 99.0;
        for r in [2.55, 3.45] {
            let v = linear(px(&img, r * a.cos(), r * a.sin())[0]);
            assert!(v < 0.02, "outside the ribbon at angle {a}, radius {r}: {v}");
        }
    }
    assert!(stats(&app).ribbon_segments > 60, "{:?}", stats(&app));
    assert_eq!(gpu_errors(&app), 0);
}

/// A ribbon on an emitter moving +X at 30 units/s is `speed × lifetime` long behind its head,
/// shortening smoothly (the oldest segment is clipped to where its age reaches the lifetime,
/// not dropped a point at a time), and ends at the emitter's render pose even when the
/// emitter moved after the tick (the head).
#[test]
fn ribbon_length_is_its_lifetime_and_its_head_follows_the_emitter() {
    let lit_extent = |img: &ImageData| {
        let (mut min, mut max) = (f32::MAX, f32::MIN);
        for x in 0..W {
            if img.get(x, H / 2)[0] > 40 {
                let wx = (x as f32 + 0.5 - W as f32 / 2.0) / 18.0;
                min = min.min(wx);
                max = max.max(wx);
            }
        }
        (min, max)
    };
    let mut lengths = Vec::new();
    for extra in [0u32, 1, 2, 3] {
        let Some(mut app) = app_with(W, H, settings()) else {
            return;
        };
        ortho_camera(&mut app, 10.0, Vec4::new(0.0, 0.0, 0.0, 1.0));
        let fx = add(
            &mut app,
            ParticleEffect {
                looping: true,
                ..ParticleEffect::new("line").ribbon(RibbonLayer {
                    min_distance: 1.0,
                    ..flat_ribbon(0.5, 0.2)
                })
            },
        );
        let e = app.spawn_effect(fx, EffectSpawn::at(Vec3::new(-6.0, 0.0, 0.0)).keep());
        for t in 1..=(20 + extra) {
            app.world.get::<&mut Transform>(e).unwrap().translation.x = -6.0 + 0.5 * t as f32;
            app.step_ticks(1);
        }
        let img = render(&mut app);
        let (min, max) = lit_extent(&img);
        let head = -6.0 + 0.5 * (20 + extra) as f32;
        // The head's round end reaches a quarter unit past the emitter.
        assert!((max - (head + 0.25)).abs() < 0.12, "head at {max}, emitter at {head}");
        lengths.push(head - min);
        // Gameplay moves the emitter after the tick: the head follows its render pose.
        let moved = head + 0.4;
        app.world.get::<&mut Transform>(e).unwrap().translation.x = moved;
        app.world.get::<&mut GlobalTransform>(e).unwrap().0 = Mat4::from_translation(Vec3::new(moved, 0.0, 0.0));
        let img = render(&mut app);
        let (_, max2) = lit_extent(&img);
        assert!(
            (max2 - (moved + 0.25)).abs() < 0.12,
            "head at {max2}, emitter moved to {moved}"
        );
        assert_eq!(gpu_errors(&app), 0);
    }
    eprintln!("lengths {lengths:?}");
    // 30 units/s × 0.2 s = 6 units (+ the soft tail's last pixels), whatever the phase of the
    // points (a point per 2 ticks): the length changes smoothly, not by a point's spacing.
    for l in &lengths {
        assert!((l - 6.0).abs() < 0.2, "length {l}");
    }
    let spread = lengths.iter().fold(0.0f32, |m, l| m.max((l - lengths[0]).abs()));
    assert!(spread < 0.12, "{lengths:?}");
}

/// A beam spans its emitter and its `ParticleBeam` end with round ends, an end glow sits on
/// the end point, and `BeamEnd::Entity` follows the entity; a far-away thin beam keeps a
/// minimum on-screen width.
#[test]
fn beams_span_their_end_points() {
    let Some(mut app) = app_with(W, H, settings()) else {
        return;
    };
    ortho_camera(&mut app, 10.0, Vec4::new(0.0, 0.0, 0.0, 1.0));
    let beam = |width: f32, end_glow: f32| BeamLayer {
        width,
        segments: 4,
        end_glow,
        color: Gradient::constant(Vec4::new(0.5, 0.5, 0.5, 1.0)),
        profile: RibbonProfile {
            core: 0.0,
            hardness: 1.0,
        },
        ..BeamLayer::default()
    };
    let fx = add(
        &mut app,
        ParticleEffect {
            looping: true,
            ..ParticleEffect::new("beam").beam(beam(0.5, 0.0))
        },
    );
    let glow = add(
        &mut app,
        ParticleEffect {
            looping: true,
            ..ParticleEffect::new("beam glow").beam(beam(0.2, 6.0))
        },
    );
    app.spawn_effect(
        fx,
        EffectSpawn::at(Vec3::new(-6.0, 2.0, 0.0))
            .beam_to(BeamEnd::World(Vec3::new(5.0, 2.0, 0.0)))
            .keep(),
    );
    let target = app.world.spawn((Transform::from_xyz(4.0, -2.0, 0.0),));
    app.spawn_effect(
        glow,
        EffectSpawn::at(Vec3::new(-6.0, -2.0, 0.0))
            .beam_to(BeamEnd::Entity(target))
            .keep(),
    );
    app.step_ticks(2);
    let img = render(&mut app);
    dump(&img, "beams");
    let lit = |x: f32, y: f32| px(&img, x, y)[0] > 100;
    // Top beam: −6..5 with round ends of half the width (0.25).
    for x in [-6.1, -3.0, 0.0, 4.0, 5.1] {
        assert!(lit(x, 2.0), "beam at x = {x}");
    }
    for x in [-6.45, 5.45] {
        assert!(!lit(x, 2.0), "past the round end at x = {x}");
    }
    assert!(!lit(0.0, 2.45), "outside the width");
    // Bottom beam: to the target entity, with a glow of 6 widths (1.2 units) there.
    assert!(lit(0.0, -2.0) && lit(4.0, -2.0));
    assert!(lit(4.0, -2.5) && lit(4.5, -2.0), "the end glow");
    assert!(!lit(-6.0, -2.5), "no glow at the start");
    // Move the target: the beam follows (its GlobalTransform, the render pose).
    app.world.get::<&mut Transform>(target).unwrap().translation = Vec3::new(2.0, -3.5, 0.0);
    app.step_ticks(1);
    let img = render(&mut app);
    let lit = |x: f32, y: f32| px(&img, x, y)[0] > 100;
    assert!(lit(2.0, -3.5) && !lit(4.5, -2.0), "the end moved with the entity");
    assert_eq!(gpu_errors(&app), 0);
}

/// A beam much thinner than a pixel is drawn at least 3 px wide, dimmed so its light per
/// unit of length stays that of its true width: its light across the beam varies by < 5 %
/// over sub-pixel offsets and matches a 3 px wide beam of a third of the brightness.
#[test]
fn thin_beams_keep_a_minimum_width_and_their_light() {
    let Some(mut app) = app_with(W, H, settings()) else {
        return;
    };
    // One unit per pixel.
    ortho_camera(&mut app, H as f32, Vec4::new(0.0, 0.0, 0.0, 1.0));
    let mut beam = |width: f32, c: f32| {
        add(
            &mut app,
            ParticleEffect {
                looping: true,
                ..ParticleEffect::new("beam").beam(BeamLayer {
                    width,
                    color: Gradient::constant(Vec4::new(c, c, c, 1.0)),
                    profile: RibbonProfile {
                        core: 0.0,
                        hardness: 0.0,
                    },
                    ..BeamLayer::default()
                })
            },
        )
    };
    let thin = beam(0.3, 1.0);
    let wide = beam(3.0, 0.1);
    // Horizontal beams at sub-pixel heights.
    for k in 0..8 {
        let y = 60.0 - 12.0 * k as f32 + k as f32 / 8.0;
        app.spawn_effect(
            thin,
            EffectSpawn::at(Vec3::new(-150.0, y, 0.0))
                .beam_to(BeamEnd::World(Vec3::new(-10.0, y, 0.0)))
                .keep(),
        );
        app.spawn_effect(
            wide,
            EffectSpawn::at(Vec3::new(10.0, y, 0.0))
                .beam_to(BeamEnd::World(Vec3::new(150.0, y, 0.0)))
                .keep(),
        );
    }
    app.step_ticks(1);
    let img = render(&mut app);
    dump(&img, "thin_beams");
    let column = |x: u32, y0: f32| -> f32 {
        let cy = 90.0 - y0;
        ((cy as i32 - 5).max(0)..(cy as i32 + 6).min(H as i32))
            .map(|y| linear(img.get(x, y as u32)[0]))
            .sum()
    };
    let mut thin_light = Vec::new();
    let mut wide_light = Vec::new();
    for k in 0..8 {
        let y = 60.0 - 12.0 * k as f32 + k as f32 / 8.0;
        thin_light.push(column(80, y));
        wide_light.push(column(240, y));
    }
    let spread = |v: &[f32]| {
        let (lo, hi) = v.iter().fold((f32::MAX, 0.0f32), |(a, b), &x| (a.min(x), b.max(x)));
        (hi - lo) / (v.iter().sum::<f32>() / v.len() as f32)
    };
    let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len() as f32;
    eprintln!(
        "thin {thin_light:?} ({:.1} %), wide {wide_light:?}",
        spread(&thin_light) * 100.0
    );
    assert!(spread(&thin_light) < 0.05, "{thin_light:?}");
    // 0.3 units at 1.0 against 3 units at 0.1: the same light per unit of length.
    let ratio = mean(&thin_light) / mean(&wide_light);
    assert!((ratio - 1.0).abs() < 0.12, "light ratio {ratio}");
    assert_eq!(gpu_errors(&app), 0);
}

/// A ribbon of an emitter that stops drains: no head, the tail shortens, the effect finishes
/// when the newest point has lived its lifetime (and the strip then draws nothing).
#[test]
fn stopped_ribbons_drain_and_finish() {
    let Some(mut app) = app_with(W, H, settings()) else {
        return;
    };
    ortho_camera(&mut app, 10.0, Vec4::new(0.0, 0.0, 0.0, 1.0));
    let fx = add(
        &mut app,
        ParticleEffect {
            looping: true,
            ..ParticleEffect::new("line").ribbon(RibbonLayer {
                width_over_life: Curve::Linear(1.0, 0.5),
                ..flat_ribbon(0.5, 0.25)
            })
        },
    );
    let e = app.spawn_effect(fx, EffectSpawn::at(Vec3::new(-4.0, 0.0, 0.0)));
    for t in 1..=10 {
        app.world.get::<&mut Transform>(e).unwrap().translation.x = -4.0 + 0.5 * t as f32;
        app.step_ticks(1);
    }
    app.stop_effect(e);
    let lit = |img: &ImageData| img.pixels.chunks(4).filter(|p| p[0] > 40).count();
    let mut last = lit(&render(&mut app));
    assert!(last > 100);
    for _ in 0..20 {
        app.step_ticks(1);
        if !app.world.contains(e) {
            break;
        }
        let now = lit(&render(&mut app));
        assert!(now <= last, "the drained trail only shrinks");
        last = now;
    }
    assert!(!app.world.contains(e), "finished and despawned");
    assert_eq!(lit(&render(&mut app)), 0);
    assert_eq!(stats(&app).ribbon_segments, 0);
    assert_eq!(gpu_errors(&app), 0);
    let _ = ParticleBlend::Additive;
}
