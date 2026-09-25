//! Particle rendering (graphics spec §5.2 PA1): blend modes composite exactly (CPU-computed
//! pixels, in Direct, Hdr and the Ldr fallback), soft fade against scene depth in every
//! `DepthAccess` mode, facings, and local vs world simulation space.

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_particles::prelude::*;
use space3d_particles::{Curve, EmitDirection, Facing, FloatRange, ParticleBlend, SimulationSpace, SpriteRender};
use space3d_render::*;

const W: u32 = 320;
const H: u32 = 180;

/// Additive, alpha and premultiplied particles over a solid background give the CPU-computed
/// pixels (`dst + c`, `c·a + dst·(1 − a)`, `c + dst·(1 − coverage)`), in Direct mode, through
/// the HDR scene texture, and in the Ldr fallback.
#[test]
fn blend_modes_composite_exactly() {
    for mode in ["direct", "hdr", "ldr"] {
        let Some(mut app) = app_with(W, H, settings()) else {
            return;
        };
        let bg = 0.2;
        let cam = ortho_camera(&mut app, 10.0, Vec4::new(bg, bg, bg, 1.0));
        if mode != "direct" {
            app.world
                .insert_one(cam, PostProcess::passthrough().with_tonemapper(Tonemapper::None))
                .unwrap();
            app.resources.expect_mut::<FrameSettings>().downlevel.no_float_targets = mode == "ldr";
        }
        let expected = blend_scene(&mut app);
        app.step_ticks(2);
        let img = render(&mut app);
        dump(&img, &format!("blend_{mode}"));
        // Ortho height 10 over 180 px: x = −3, 0, 3 at 0.5 ± 3·18/320.
        for ((x, linear), what) in expected.into_iter().zip(["additive", "alpha", "premultiplied"]) {
            let got = at(&img, 0.5 + x * 18.0 / W as f32, 0.5);
            assert_rgb(got, linear, 2, &format!("{mode} {what}"));
        }
        assert_rgb(at(&img, 0.05, 0.05), Vec3::splat(bg), 1, &format!("{mode} background"));
        assert_eq!(gpu_errors(&app), 0);
    }
}

/// Soft particles fade against scene depth in every `DepthAccess` mode of graphics spec §1.6:
/// the read-only attachment (1×, and MSAA through the depth resolve), sampled without an
/// attachment (GL, forced here), and the hard test only (soft particles off, and the MSAA
/// in-pass fallback of several views). Particles behind geometry are hidden in all of them.
#[test]
fn soft_fade_in_every_depth_access_mode() {
    let wall = 0.1;
    for mode in ["rods", "gl", "msaa", "hard", "msaa-multi-view"] {
        let s = ParticleSettings {
            soft_particles: mode != "hard",
            ..settings()
        };
        let Some(mut app) = app_with(W, H, s) else { return };
        ortho_camera(&mut app, 10.0, Vec4::new(0.0, 0.0, 0.0, 1.0));
        match mode {
            "gl" => app.resources.expect_mut::<FrameSettings>().downlevel.no_read_only_depth = true,
            "msaa" => app.resources.expect_mut::<Renderer>().set_msaa_samples(4),
            "msaa-multi-view" => {
                app.resources.expect_mut::<Renderer>().set_msaa_samples(4);
                // A second view (a corner inset) sharing the multisampled target.
                app.world.spawn((
                    Transform::from_xyz(0.0, 0.0, 10.0),
                    Camera3d {
                        order: 1,
                        viewport: Some([0.9, 0.9, 0.1, 0.1]),
                        ..Default::default()
                    },
                ));
            }
            _ => {}
        }
        let img = soft_scene(&mut app);
        dump(&img, &format!("soft_{mode}"));
        let caps = app.resources.expect::<Renderer>().graphics_caps().clone();
        let (front, open, behind) = soft_probe(&img);
        let faded = match mode {
            "hard" | "msaa-multi-view" => false,
            "msaa" => caps.msaa_depth_resolve,
            _ => true,
        };
        // In front of the wall by 0.25 with a soft distance of 1: a quarter of the light.
        let expected = if faded { wall + 0.25 } else { 1.0 };
        assert_rgb(
            front,
            Vec3::splat(expected),
            3,
            &format!("{mode}: in front of the wall"),
        );
        assert_rgb(open, Vec3::ONE, 1, &format!("{mode}: over the open background"));
        assert_rgb(behind, Vec3::splat(wall), 1, &format!("{mode}: behind the wall"));
        assert_eq!(gpu_errors(&app), 0);
    }
}

/// Camera-facing sprites stay round, plane-facing ones lie in their plane, velocity-stretched
/// ones stretch along the motion.
#[test]
fn facings() {
    let Some(mut app) = app_with(W, H, settings()) else {
        return;
    };
    // Orthographic, looking down 45° toward −Z.
    app.world.spawn((
        Transform::from_xyz(0.0, 10.0, 10.0).looking_at(Vec3::ZERO, Vec3::Y),
        Camera3d::default()
            .with_clear_color(Vec4::new(0.0, 0.0, 0.0, 1.0))
            .with_projection(Projection::Orthographic {
                height: 10.0,
                near: 0.1,
                far: 100.0,
            }),
    ));
    let layer = |facing: Facing| ParticleLayer {
        render: SpriteRender {
            facing,
            ..dot(Vec4::ONE, ParticleBlend::Additive, 2.0).render
        },
        ..dot(Vec4::ONE, ParticleBlend::Additive, 2.0)
    };
    let camera = effect(&mut app, layer(Facing::Camera));
    let plane = effect(&mut app, layer(Facing::Plane { normal: Vec3::Y }));
    let stretched = effect(
        &mut app,
        ParticleLayer {
            speed: FloatRange::constant(10.0),
            direction: EmitDirection::Fixed(Vec3::X),
            ..layer(Facing::VelocityStretched {
                scale: 0.3,
                min: 1.0,
                max: 8.0,
            })
        },
    );
    for (fx, x) in [(camera, -5.5), (plane, -1.0), (stretched, 3.5)] {
        app.spawn_effect(fx, EffectSpawn::at(Vec3::new(x, 0.0, 0.0)).keep());
    }
    app.step_ticks(1);
    let img = render(&mut app);
    dump(&img, "facings");
    // Lit extent (width, height) in pixels of the columns [x0, x1).
    let extent = |x0: f32, x1: f32| {
        let (mut minx, mut maxx, mut miny, mut maxy) = (u32::MAX, 0, u32::MAX, 0);
        for y in 0..H {
            for x in (x0 * W as f32) as u32..(x1 * W as f32) as u32 {
                if img.get(x, y)[0] > 128 {
                    (minx, maxx, miny, maxy) = (minx.min(x), maxx.max(x), miny.min(y), maxy.max(y));
                }
            }
        }
        assert!(maxx >= minx, "something lit in {x0}..{x1}");
        ((maxx - minx + 1) as f32, (maxy - miny + 1) as f32)
    };
    let (w, h) = extent(0.0, 0.29);
    assert!((w / h - 1.0).abs() < 0.1, "camera-facing: round ({w}×{h})");
    let (w, h) = extent(0.29, 0.52);
    let squash = std::f32::consts::FRAC_1_SQRT_2;
    assert!((h / w - squash).abs() < 0.08, "plane-facing, seen at 45°: {w}×{h}");
    let (w, h) = extent(0.52, 1.0);
    // 1 + 10 · 0.3 / 2 = 2.5 times longer than wide.
    assert!((w / h - 2.5).abs() < 0.25, "velocity-stretched: {w}×{h}");
    assert_eq!(gpu_errors(&app), 0);
}

/// World-space particles stay where they were emitted (a trail behind a moving emitter);
/// local-space ones move with it (and with its parent: an emitter following an entity).
#[test]
fn local_vs_world_space() {
    let Some(mut app) = app_with(W, H, settings()) else {
        return;
    };
    ortho_camera(&mut app, 10.0, Vec4::new(0.0, 0.0, 0.0, 1.0));
    let trail = |space: SimulationSpace, color: Vec4| ParticleLayer {
        space,
        spawn: space3d_particles::Spawn {
            rate: 60.0,
            ..Default::default()
        },
        lifetime: FloatRange::constant(10.0),
        alpha_over_life: Curve::Constant(1.0),
        ..dot(color, ParticleBlend::Additive, 0.4)
    };
    let fx = app
        .resources
        .expect_mut::<ParticleEffects>()
        .add(ParticleEffect {
            looping: true,
            ..ParticleEffect::new("trails")
                .layer(trail(SimulationSpace::World, Vec4::new(1.0, 0.0, 0.0, 1.0)))
                .layer(ParticleLayer {
                    offset: Vec3::new(0.0, -1.0, 0.0),
                    ..trail(SimulationSpace::Local, Vec4::new(0.0, 1.0, 0.0, 1.0))
                })
        })
        .unwrap();
    // The emitter follows a ship moving +X by 0.2 per tick from x = −3.
    let ship = app.world.spawn((Transform::from_xyz(-3.0, 1.0, 0.0),));
    app.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO).follow(ship).keep());
    for t in 0..30 {
        app.world.get::<&mut Transform>(ship).unwrap().translation.x = -3.0 + 0.2 * (t + 1) as f32;
        app.step_ticks(1);
    }
    let img = render(&mut app);
    dump(&img, "local_vs_world");
    // Emitter now at x = 3: red (world) particles span −3..3 at y = 1, green (local) ones all
    // sit at the emitter (x = 3, y = 0).
    let px = |x: f32, y: f32| at(&img, 0.5 + x * 18.0 / W as f32, 0.5 - y * 18.0 / H as f32);
    for x in [-2.0, 0.0, 2.0] {
        assert!(px(x, 1.0)[0] > 100, "world trail at x = {x}: {:?}", px(x, 1.0));
        assert!(px(x, 0.0)[1] < 10, "no local particle left behind at x = {x}");
    }
    assert!(px(3.0, 0.0)[1] > 100, "local particles ride the emitter");
    assert_eq!(gpu_errors(&app), 0);
}

/// `DownlevelOverride::webgl2()` (no compute, no float targets, no read-only depth: the
/// WebGL2 code paths on this device) renders the built-in effects like the default paths.
#[test]
fn webgl2_override_matches_default_paths() {
    let scene = |webgl2: bool| {
        let mut app = app(W, H)?;
        if webgl2 {
            app.insert_resource(FrameSettings::default().with_downlevel(DownlevelOverride::webgl2()));
        }
        camera(
            &mut app,
            Vec3::new(0.0, 150.0, 120.0),
            Vec3::ZERO,
            Vec4::new(0.01, 0.01, 0.02, 1.0),
        );
        let ground = app
            .resources
            .expect_mut::<RenderAssets>()
            .add_mesh(MeshData::plane(400.0));
        app.world.spawn((
            Transform::from_xyz(0.0, -6.0, 0.0),
            Mesh3d(ground),
            Material::unlit(Vec4::new(0.05, 0.05, 0.06, 1.0)),
        ));
        app.spawn_effect(builtin::EXPLOSION, EffectSpawn::at(Vec3::ZERO).seed(3));
        app.spawn_effect(
            builtin::WARP_OUT,
            EffectSpawn::at(Vec3::new(60.0, 0.0, -20.0)).scale(0.4).seed(4),
        );
        app.step_ticks(15);
        let img = render(&mut app);
        assert_eq!(gpu_errors(&app), 0);
        Some(img)
    };
    let Some(a) = scene(false) else { return };
    let b = scene(true).unwrap();
    dump(&b, "webgl2_override");
    let diff = space3d_test::compare_images(&a, &b, &space3d_test::CompareOptions::perceptual());
    assert!(diff.passed, "{}", diff.summary());
}
