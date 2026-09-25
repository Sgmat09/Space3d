//! Particle stories (`Engine/Particles/*`): the built-in effects, the particle features and a
//! 100k-particle stress scene (graphics spec §5.2 PA1, PA2). They run on the default backend (the
//! GPU backend where compute exists; `Features/CPU Backend` forces the CPU one).
//!
//! Owner: P-PART (graphics spec §3.6). Compiled with the `particles` feature (part of
//! `engine`). Effects are driven by the fixed tick only, so every story is deterministic for its
//! tick count and doubles as a golden screenshot.

use super::{ambient, mesh, sun};
use crate::Story;
use space3d_core::prelude::*;
use space3d_particles::prelude::*;
use space3d_particles::{
    Curve, EmitDirection, EmitShape, Facing, FadeCurve, FloatRange, Force, Gradient, ParticleBackend, ParticleBlend,
    SimulationSpace, Spawn, SpriteRender, SpriteShape,
};
use space3d_render::{
    Camera3d, DownlevelOverride, FrameSettings, Material, Mesh3d, PostProcess, Projection, RenderAssets, RenderTarget,
    RenderTargetDesc, Tonemapper,
};

/// This area's stories, in registry order.
pub(crate) fn stories() -> Vec<Story> {
    vec![
        explosion(),
        hit_spark(),
        engine_exhaust(),
        shield_ripple(),
        warp_in(),
        warp_out(),
        laser_trail(),
        blend_modes(),
        soft_particles(),
        shapes(),
        curves(),
        facing(),
        local_vs_world(),
        cpu_backend(),
        stress_100k(),
    ]
}

/// Deep-space backdrop of the particle stories (linear).
const SPACE: Vec4 = Vec4::new(0.006, 0.007, 0.014, 1.0);

fn plugin(app: &mut App, settings: ParticleSettings) {
    app.add_plugin(ParticlesPlugin::with_settings(settings));
}

/// A sun and a little ambient light (for the hulls; the effects are emissive).
fn lights(app: &mut App) {
    sun(app, Vec3::new(-0.4, -1.0, -0.5), 2.5);
    ambient(app, 0.12);
}

/// A camera looking down at the flight plane from `distance` (the game's top-down view).
fn top_camera(app: &mut App, distance: f32) -> Entity {
    app.world.spawn((
        Name("camera".into()),
        Transform::from_xyz(0.0, distance * 0.78, distance * 0.62).looking_at(Vec3::ZERO, Vec3::Y),
        Camera3d::default().with_clear_color(SPACE),
    ))
}

/// An orthographic camera looking down −Z, `height` world units tall.
fn front_camera(app: &mut App, height: f32) -> Entity {
    app.world.spawn((
        Name("camera".into()),
        Transform::from_xyz(0.0, 0.0, 200.0),
        Camera3d::default()
            .with_clear_color(SPACE)
            .with_projection(Projection::Orthographic {
                height,
                near: 1.0,
                far: 400.0,
            }),
    ))
}

/// A ship hull (40 units long, facing −Z) at `t`.
fn hull(app: &mut App, t: Transform) -> Entity {
    let cube = mesh(app, MeshData::cube(1.0));
    let ship = app.world.spawn((Name("ship".into()), t));
    for (offset, scale, color) in [
        (Vec3::ZERO, Vec3::new(12.0, 6.0, 40.0), Vec3::new(0.22, 0.24, 0.28)),
        (
            Vec3::new(0.0, 0.0, 8.0),
            Vec3::new(36.0, 2.0, 12.0),
            Vec3::new(0.16, 0.17, 0.2),
        ),
        (
            Vec3::new(0.0, 3.5, -6.0),
            Vec3::new(5.0, 3.0, 9.0),
            Vec3::new(0.1, 0.25, 0.35),
        ),
    ] {
        app.world.spawn((
            Transform::from_xyz(offset.x, offset.y, offset.z).with_scale(scale),
            Mesh3d(cube),
            Material::srgb(color.x, color.y, color.z).with_metallic_roughness(0.6, 0.45),
            Parent(ship),
        ));
    }
    ship
}

/// Heading (local −Z) along `dir` in the XZ plane.
fn heading(dir: Vec3) -> Quat {
    Quat::from_rotation_arc(Vec3::NEG_Z, dir.normalize_or(Vec3::NEG_Z))
}

/// A ship flying a circle of `radius` at `speed` rad per tick, with two engine exhausts.
fn circling_ship(app: &mut App, radius: f32, speed: f32, throttle: f32) -> Entity {
    let ship = hull(app, Transform::from_xyz(radius, 0.0, 0.0));
    for x in [-6.0, 6.0] {
        let e = app.spawn_effect(
            builtin::ENGINE_EXHAUST,
            EffectSpawn::from_transform(Transform::from_xyz(x, 0.0, 21.0))
                .follow(ship)
                .keep(),
        );
        if let Ok(mut em) = app.world.get::<&mut ParticleEmitter>(e) {
            em.rate_scale = throttle;
        }
    }
    app.add_system(Stage::FixedUpdate, move |ctx: &mut SystemCtx| {
        let a = ctx.time().tick as f32 * speed;
        if let Ok(mut t) = ctx.world.get::<&mut Transform>(ship) {
            t.translation = Vec3::new(a.cos() * radius, 0.0, a.sin() * radius);
            t.rotation = heading(Vec3::new(-a.sin(), 0.0, a.cos()));
        }
    });
    ship
}

fn explosion() -> Story {
    Story::new("Engine/Particles/Builtin/Explosion", |app, k| {
        plugin(
            app,
            ParticleSettings {
                quality: k.f32("quality"),
                ..Default::default()
            },
        );
        lights(app);
        let cam = top_camera(app, 200.0);
        if k.bool("hdr") {
            let post = PostProcess::default().with_tonemapper(Tonemapper::AgX);
            let _ = app.world.insert_one(cam, post);
        }
        app.spawn_effect(
            builtin::EXPLOSION,
            EffectSpawn::at(Vec3::ZERO).scale(k.f32("scale")).seed(1),
        );
    })
    .description(
        "`builtin::EXPLOSION`, one call: `spawn_effect(builtin::EXPLOSION, EffectSpawn::at(p).scale(s))`. \
         A white-hot flash and afterglow, fireball billows (premultiplied smoke puffs whose light \
         burns out while their coverage holds), a hot core, sparks stretched along their velocity, \
         flickering embers, two shockwave rings on the flight plane and dark smoke rolling out \
         (0.3 s in). `quality` scales the spawn counts of the LOD layers; `hdr` renders through \
         the HDR scene texture and AgX instead of Direct mode's soft clip.",
    )
    .knob_f32("scale", 1.0, 0.25..=3.0)
    .knob_f32("quality", 1.0, 0.0..=1.0)
    .knob_bool("hdr", false)
    .tag("particles")
    .size(320, 180)
    .ticks(18)
}

fn hit_spark() -> Story {
    Story::new("Engine/Particles/Builtin/Hit Spark", |app, k| {
        plugin(app, ParticleSettings::default());
        lights(app);
        top_camera(app, 110.0);
        hull(app, Transform::IDENTITY.with_rotation(Quat::from_rotation_y(0.6)));
        let tint = Vec4::from_array(k.color("color"));
        // Bolts from the left hit the hull's side: sparks fly back toward the shooter.
        for (i, (p, from)) in [
            (Vec3::new(-6.0, 2.0, -8.0), Vec3::new(-1.0, 0.0, -0.3)),
            (Vec3::new(-5.0, 2.0, 10.0), Vec3::new(-1.0, 0.0, 0.4)),
            (Vec3::new(4.0, 3.0, -16.0), Vec3::new(0.2, 0.0, -1.0)),
        ]
        .into_iter()
        .enumerate()
        {
            app.spawn_effect(
                builtin::HIT_SPARK,
                EffectSpawn::at(p).facing(from).tint(tint).seed(i as u32),
            );
        }
    })
    .description(
        "`builtin::HIT_SPARK`, spawned with `.facing(toward_the_shooter).tint(laser_color)`: a \
         flash, sparks thrown back toward the shooter (velocity-stretched streaks), a small ring \
         on the emitter's plane and two puffs of smoke; three hits on a hull, 0.1 s in.",
    )
    .knob_color("color", [1.0, 0.35, 0.25, 1.0])
    .tag("particles")
    .size(320, 180)
    .ticks(5)
}

fn engine_exhaust() -> Story {
    Story::new("Engine/Particles/Builtin/Engine Exhaust", |app, k| {
        plugin(app, ParticleSettings::default());
        lights(app);
        top_camera(app, 190.0);
        circling_ship(app, 55.0, 0.03, k.f32("throttle"));
    })
    .description(
        "`builtin::ENGINE_EXHAUST` on two nozzles of a ship flying a circle (the emitters are \
         children of the hull, `EffectSpawn::follow(ship)`): a blue plume inheriting part of the \
         ship's velocity, sparks, and a local-space glow riding the nozzle. Emission is spread \
         over each tick's motion, so the plume never clumps; `throttle` drives \
         `ParticleEmitter::rate_scale` (0..2).",
    )
    .knob_f32("throttle", 1.0, 0.0..=2.0)
    .tag("particles")
    .size(320, 180)
    .ticks(90)
}

fn shield_ripple() -> Story {
    Story::new("Engine/Particles/Builtin/Shield Ripple", |app, k| {
        plugin(app, ParticleSettings::default());
        lights(app);
        top_camera(app, 150.0);
        let ship = hull(app, Transform::IDENTITY.with_rotation(Quat::from_rotation_y(0.3)));
        let radius = k.f32("radius");
        // A bolt from the left (travelling +X) hits the shield: the arc faces the shooter.
        app.spawn_effect(
            builtin::SHIELD_RIPPLE,
            EffectSpawn::at(Vec3::ZERO)
                .facing(Vec3::new(-1.0, 0.0, 0.25))
                .scale(radius / 20.0)
                .follow(ship)
                .seed(4),
        );
    })
    .description(
        "`builtin::SHIELD_RIPPLE` around a ship, spawned at the shield center with \
         `.facing(-bolt_direction).scale(radius / 20).follow(ship)`: an arc brightest toward the \
         shooter, a faint full bubble, a flash on the shield surface and tangent streaks \
         (0.07 s in). The rings are local-space: they stay on a moving ship.",
    )
    .knob_f32("radius", 30.0, 10.0..=60.0)
    .tag("particles")
    .size(320, 180)
    .ticks(4)
}

fn warp_in() -> Story {
    Story::new("Engine/Particles/Builtin/Warp In", |app, _| {
        plugin(app, ParticleSettings::default());
        lights(app);
        top_camera(app, 380.0);
        app.spawn_effect(builtin::WARP_IN, EffectSpawn::at(Vec3::ZERO).seed(5));
    })
    .description(
        "`builtin::WARP_IN`: a vortex collapsing inward while streaks rush to the center, then a \
         flash and two expanding rings as the ship appears (0.35 s in).",
    )
    .tag("particles")
    .size(320, 180)
    .ticks(21)
}

fn warp_out() -> Story {
    Story::new("Engine/Particles/Builtin/Warp Out", |app, _| {
        plugin(app, ParticleSettings::default());
        lights(app);
        top_camera(app, 380.0);
        app.spawn_effect(builtin::WARP_OUT, EffectSpawn::at(Vec3::ZERO).seed(6));
    })
    .description(
        "`builtin::WARP_OUT`: a flash, a vortex opening outward, two expanding rings and streaks \
         rushing out of the center (0.2 s in).",
    )
    .tag("particles")
    .size(320, 180)
    .ticks(12)
}

fn laser_trail() -> Story {
    Story::new("Engine/Particles/Builtin/Laser Trail", |app, k| {
        plugin(app, ParticleSettings::default());
        lights(app);
        top_camera(app, 240.0);
        let tint = Vec4::from_array(k.color("color"));
        // Two bolts at 1500 units/s, the trail following each (it drains when a bolt is gone).
        for (i, (start, dir)) in [
            (Vec3::new(-300.0, 0.0, -30.0), Vec3::new(1.0, 0.0, 0.1)),
            (Vec3::new(280.0, 0.0, 50.0), Vec3::new(-1.0, 0.0, -0.25)),
        ]
        .into_iter()
        .enumerate()
        {
            let dir = dir.normalize();
            let bolt = app
                .world
                .spawn((Name("bolt".into()), Transform::from_xyz(start.x, start.y, start.z)));
            app.spawn_effect(
                builtin::LASER_TRAIL,
                EffectSpawn::at(Vec3::ZERO).follow(bolt).tint(tint).seed(i as u32),
            );
            app.add_system(Stage::FixedUpdate, move |ctx: &mut SystemCtx| {
                let t = ctx.time().tick as f32 + 1.0;
                if let Ok(mut tr) = ctx.world.get::<&mut Transform>(bolt) {
                    tr.translation = start + dir * (1500.0 / 60.0) * t;
                }
            });
        }
    })
    .description(
        "`builtin::LASER_TRAIL` following two bolts at 1500 units/s (`EffectSpawn::at(muzzle)\
         .follow(bolt).tint(color)`): a head glow riding the bolt and glows laid along its path \
         per unit of distance, spread over each tick's motion so the trail stays continuous.",
    )
    .knob_color("color", [1.0, 0.3, 0.2, 1.0])
    .tag("particles")
    .size(320, 180)
    .ticks(13)
}

/// A static cluster of seven sprites spread along a 90-unit line at `center`.
fn cluster(color: Vec4, blend: ParticleBlend, center: Vec3, coverage: f32) -> ParticleEffect {
    ParticleEffect::new("cluster").layer(ParticleLayer {
        lod: false,
        offset: center,
        shape: EmitShape::Line { length: 90.0 },
        size: FloatRange::new(22.0, 30.0),
        color_over_life: Gradient::constant(color),
        coverage: FloatRange::constant(coverage),
        render: SpriteRender {
            shape: SpriteShape::Glow {
                core: 0.0,
                hardness: 0.35,
            },
            blend,
            ..SpriteRender::default()
        },
        ..ParticleLayer::burst(7, 100.0)
    })
}

fn blend_modes() -> Story {
    Story::new("Engine/Particles/Features/Blend Modes", |app, _| {
        plugin(app, ParticleSettings::default());
        front_camera(app, 120.0);
        // Colored bars behind the sprites.
        let bar = mesh(app, MeshData::cube(1.0));
        for (i, c) in [
            Vec3::new(0.6, 0.08, 0.05),
            Vec3::new(0.05, 0.4, 0.1),
            Vec3::new(0.05, 0.1, 0.6),
        ]
        .into_iter()
        .enumerate()
        {
            app.world.spawn((
                Transform::from_xyz(-60.0 + i as f32 * 60.0, 0.0, -50.0).with_scale(Vec3::new(30.0, 130.0, 1.0)),
                Mesh3d(bar),
                Material::unlit(c.extend(1.0)),
            ));
        }
        let effects = app.resources.expect_mut::<ParticleEffects>();
        let rows = [
            cluster(
                Vec4::new(0.25, 0.5, 1.3, 1.0),
                ParticleBlend::Additive,
                Vec3::new(0.0, 35.0, 0.0),
                1.0,
            ),
            cluster(Vec4::new(1.0, 0.55, 0.15, 0.55), ParticleBlend::Alpha, Vec3::ZERO, 1.0),
            cluster(
                Vec4::new(1.2, 0.45, 0.1, 1.0),
                ParticleBlend::Premultiplied,
                Vec3::new(0.0, -35.0, 0.0),
                0.6,
            ),
        ]
        .map(|e| effects.add(e).expect("valid"));
        for (i, fx) in rows.into_iter().enumerate() {
            app.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO).seed(i as u32).keep());
        }
    })
    .description(
        "The three particle blends over colored bars, seven overlapping sprites each. Top: \
         additive (overlaps brighten toward white, the background shows through). Middle: alpha \
         (each sprite covers what is behind it by its opacity). Bottom: premultiplied (adds its \
         light and covers by a separate coverage: fire that is also smoke). One premultiplied \
         equation (`One, OneMinusSrcAlpha`) serves all three.",
    )
    .tag("particles")
    .size(320, 180)
    .ticks(2)
}

fn soft_particles() -> Story {
    Story::new("Engine/Particles/Features/Soft Particles", |app, k| {
        plugin(
            app,
            ParticleSettings {
                soft_particles: k.bool("soft"),
                ..Default::default()
            },
        );
        // The scene renders into a render target with the knob's sample count (a story can
        // only choose MSAA per render target), shown full-screen by the primary camera.
        let samples = if k.choice("msaa") == 1 { 4 } else { 1 };
        let (w, h) = (320, 180);
        let rt = app.resources.init::<RenderAssets>().add_render_target_with(
            w,
            h,
            RenderTargetDesc::default().with_samples(samples),
        );
        app.insert_resource(FrameSettings::default().with_downlevel(DownlevelOverride {
            no_read_only_depth: k.bool("no_read_only_depth"),
            ..Default::default()
        }));
        let ground = mesh(app, MeshData::plane(400.0));
        app.world.spawn((
            Transform::IDENTITY,
            Mesh3d(ground),
            Material::srgb(0.12, 0.13, 0.16).with_metallic_roughness(0.0, 0.85),
        ));
        let pillar = mesh(app, MeshData::cube(1.0));
        app.world.spawn((
            Transform::from_xyz(20.0, 15.0, -30.0).with_scale(Vec3::new(12.0, 30.0, 12.0)),
            Mesh3d(pillar),
            Material::srgb(0.55, 0.5, 0.45),
        ));
        sun(app, Vec3::new(-0.3, -1.0, -0.4), 2.2);
        ambient(app, 0.1);
        // Smoke puffs sitting on the ground (half below it) and glows against the pillar.
        let soft = k.f32("soft_distance");
        let puffs = ParticleLayer {
            lod: false,
            shape: EmitShape::Box {
                half_extents: Vec3::new(55.0, 0.0, 25.0),
            },
            size: FloatRange::new(26.0, 34.0),
            rotation: FloatRange::new(0.0, std::f32::consts::TAU),
            color_over_life: Gradient::constant(Vec4::new(0.55, 0.45, 0.35, 1.0)),
            coverage: FloatRange::constant(0.85),
            render: SpriteRender {
                shape: SpriteShape::Smoke {
                    breakup: 0.5,
                    hot_core: 0.0,
                },
                blend: ParticleBlend::Premultiplied,
                soft_distance: soft,
                ..SpriteRender::default()
            },
            ..ParticleLayer::burst(9, 100.0)
        };
        let glows = ParticleLayer {
            lod: false,
            offset: Vec3::new(20.0, 12.0, -22.0),
            shape: EmitShape::Line { length: 50.0 },
            size: FloatRange::constant(18.0),
            color_over_life: Gradient::constant(Vec4::new(0.4, 1.0, 2.0, 1.0)),
            render: SpriteRender {
                soft_distance: soft,
                ..SpriteRender::additive(SpriteShape::Glow {
                    core: 0.3,
                    hardness: 0.0,
                })
            },
            ..ParticleLayer::burst(4, 100.0)
        };
        let fx = app
            .resources
            .expect_mut::<ParticleEffects>()
            .add(ParticleEffect::new("puffs").layer(puffs).layer(glows))
            .expect("valid");
        app.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO).seed(2).keep());
        app.world.spawn((
            Name("scene camera".into()),
            Transform::from_xyz(0.0, 60.0, 110.0).looking_at(Vec3::new(0.0, 0.0, -10.0), Vec3::Y),
            Camera3d {
                order: -1,
                target: RenderTarget::Texture(rt),
                ..Camera3d::default().with_clear_color(SPACE)
            },
        ));
        // The render target on a full-screen quad, far from the scene.
        let screen = mesh(app, MeshData::plane(1.0));
        app.world.spawn((
            Transform::from_xyz(0.0, -2000.0, 0.0)
                .with_rotation(Quat::from_rotation_x(std::f32::consts::FRAC_PI_2))
                .with_scale(Vec3::new(2.0 * w as f32 / h as f32, 1.0, 2.0)),
            Mesh3d(screen),
            Material::unlit(Vec4::ONE).with_texture(rt),
        ));
        app.world.spawn((
            Name("camera".into()),
            Transform::from_xyz(0.0, -2000.0, 5.0),
            Camera3d::default()
                .with_clear_color(SPACE)
                .with_projection(Projection::Orthographic {
                    height: 2.0,
                    near: 0.1,
                    far: 10.0,
                }),
        ));
    })
    .description(
        "Smoke puffs sitting on the ground (half below it) and glows against a pillar. With `soft` \
         they fade within `soft_distance` of the scene depth instead of cutting hard lines into \
         it. The particles' slot reads scene depth: a read-only depth attachment at 1×; with \
         `msaa` 4× the depth is resolved first; with `no_read_only_depth` (WebGL2) depth is \
         sampled without an attachment and tested in the shader; with both, particles draw \
         inside the multisampled pass with the hard test only.",
    )
    .knob_bool("soft", true)
    .knob_f32("soft_distance", 12.0, 0.0..=40.0)
    .knob_choice("msaa", &["1×", "4×"], 0)
    .knob_bool("no_read_only_depth", false)
    .tag("particles")
    .size(320, 180)
    .ticks(2)
}

/// One sprite of `shape` at `x`, moving along +Y at `speed` (for velocity facing).
fn sprite(shape: SpriteShape, facing: Facing, x: f32, color: Vec4, speed: f32) -> ParticleLayer {
    ParticleLayer {
        lod: false,
        offset: Vec3::new(x, 0.0, 0.0),
        direction: EmitDirection::Fixed(Vec3::Y),
        speed: FloatRange::constant(speed),
        size: FloatRange::constant(7.5),
        color_over_life: Gradient::constant(color),
        render: SpriteRender {
            facing,
            ..SpriteRender::additive(shape)
        },
        ..ParticleLayer::burst(1, 100.0)
    }
}

fn shapes() -> Story {
    Story::new("Engine/Particles/Features/Shapes", |app, _| {
        plugin(app, ParticleSettings::default());
        front_camera(app, 20.0);
        let warm = Vec4::new(1.4, 0.6, 0.2, 1.0);
        let cool = Vec4::new(0.3, 0.7, 1.6, 1.0);
        let row = [
            (
                SpriteShape::Glow {
                    core: 0.0,
                    hardness: 0.0,
                },
                Facing::Camera,
                warm,
                0.0,
            ),
            (
                SpriteShape::Glow {
                    core: 0.8,
                    hardness: 0.6,
                },
                Facing::Camera,
                cool,
                0.0,
            ),
            (
                SpriteShape::Streak { core: 0.6, taper: 1.0 },
                Facing::VelocityStretched {
                    scale: 0.3,
                    min: 1.0,
                    max: 8.0,
                },
                warm,
                24.0,
            ),
            (
                SpriteShape::Ring {
                    thickness: 0.12,
                    focus: 0.0,
                    fill: 0.1,
                },
                Facing::Camera,
                cool,
                0.0,
            ),
            (
                SpriteShape::Ring {
                    thickness: 0.1,
                    focus: 4.0,
                    fill: 0.0,
                },
                Facing::Camera,
                warm,
                0.0,
            ),
            (
                SpriteShape::Smoke {
                    breakup: 0.5,
                    hot_core: 0.4,
                },
                Facing::Camera,
                warm,
                0.0,
            ),
            (SpriteShape::Vortex { arms: 4.0, twist: 1.4 }, Facing::Camera, cool, 0.0),
        ];
        let mut effect = ParticleEffect::new("shapes");
        for (i, (shape, facing, color, speed)) in row.into_iter().enumerate() {
            effect = effect.layer(sprite(shape, facing, -27.0 + i as f32 * 9.0, color, speed));
        }
        let fx = app
            .resources
            .expect_mut::<ParticleEffects>()
            .add(effect)
            .expect("valid");
        app.spawn_effect(fx, EffectSpawn::at(Vec3::new(0.0, -0.6, 0.0)).seed(3).keep());
    })
    .description(
        "The analytic sprite shapes (no textures; crisp at any size, the game's `additive.rs` \
         kinds as data): soft glow, hard glow with a white-hot core, a streak stretched along \
         its velocity, a filled ring, a ring focused toward its +X, a smoke puff with a hot \
         core, and a vortex.",
    )
    .tag("particles")
    .size(320, 90)
    .ticks(3)
}

fn curves() -> Story {
    Story::new("Engine/Particles/Features/Curves", |app, _| {
        plugin(app, ParticleSettings::default());
        front_camera(app, 120.0);
        let presets = [
            FadeCurve::Linear,
            FadeCurve::Out,
            FadeCurve::Flash,
            FadeCurve::InOut,
            FadeCurve::Linger,
            FadeCurve::Burn,
            FadeCurve::Hold,
        ];
        let mut effect = ParticleEffect {
            looping: true,
            ..ParticleEffect::new("fountains")
        };
        for (i, fade) in presets.into_iter().enumerate() {
            effect = effect.layer(ParticleLayer {
                lod: false,
                offset: Vec3::new(-72.0 + i as f32 * 24.0, -55.0, 0.0),
                spawn: Spawn {
                    rate: 40.0,
                    ..Spawn::default()
                },
                direction: EmitDirection::Fixed(Vec3::Y),
                speed: FloatRange::constant(70.0),
                lifetime: FloatRange::constant(1.5),
                size: FloatRange::constant(8.0),
                size_over_life: Curve::Smooth(vec![(0.0, 0.4), (0.3, 1.4), (1.0, 2.0)]),
                color_over_life: Gradient {
                    keys: vec![
                        (0.0, [2.0, 1.8, 1.4, 1.0]),
                        (0.35, [1.6, 0.6, 0.15, 1.0]),
                        (1.0, [0.5, 0.05, 0.25, 1.0]),
                    ],
                },
                alpha_over_life: Curve::Preset(fade),
                render: SpriteRender::additive(SpriteShape::Glow {
                    core: 0.0,
                    hardness: 0.0,
                }),
                ..ParticleLayer::default()
            });
        }
        let fx = app
            .resources
            .expect_mut::<ParticleEffects>()
            .add(effect)
            .expect("valid");
        app.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO).seed(8).keep());
    })
    .description(
        "Curves over a particle's life, baked into the LUT the vertex shader samples: seven \
         fountains rising for 1.5 s with a Catmull-Rom size curve, a color gradient \
         (white-hot → orange → purple) and, left to right, the alpha fade presets Linear, Out, \
         Flash, InOut, Linger, Burn and Hold (the game's fades).",
    )
    .tag("particles")
    .size(320, 180)
    .ticks(100)
}

/// A ring sprite in the emitter's XZ plane, focused toward the emitter's −Z.
fn emitter_ring(color: Vec4) -> ParticleLayer {
    ParticleLayer {
        lod: false,
        size: FloatRange::constant(40.0),
        rotation: FloatRange::constant(std::f32::consts::FRAC_PI_2),
        color_over_life: Gradient::constant(color),
        render: SpriteRender {
            facing: Facing::EmitterPlane,
            ..SpriteRender::additive(SpriteShape::Ring {
                thickness: 0.1,
                focus: 3.0,
                fill: 0.05,
            })
        },
        ..ParticleLayer::burst(1, 100.0)
    }
}

fn facing() -> Story {
    Story::new("Engine/Particles/Features/Facing", |app, _| {
        plugin(app, ParticleSettings::default());
        lights(app);
        top_camera(app, 140.0);
        let blue = Vec4::new(0.4, 0.9, 1.8, 1.0);
        let effect = ParticleEffect::new("facings")
            // Camera-facing glows.
            .layer(ParticleLayer {
                lod: false,
                offset: Vec3::new(-75.0, 0.0, 0.0),
                shape: EmitShape::Sphere {
                    radius: 14.0,
                    inner: 0.0,
                },
                size: FloatRange::new(6.0, 10.0),
                color_over_life: Gradient::constant(blue),
                render: SpriteRender::additive(SpriteShape::Glow {
                    core: 0.5,
                    hardness: 0.0,
                }),
                ..ParticleLayer::burst(12, 100.0)
            })
            // Streaks stretched along their velocity (a radial burst in the plane).
            .layer(ParticleLayer {
                lod: false,
                offset: Vec3::new(-25.0, 0.0, 0.0),
                direction: EmitDirection::Random,
                flatten: 1.0,
                speed: FloatRange::new(30.0, 60.0),
                drag: 1.0,
                size: FloatRange::constant(2.5),
                color_over_life: Gradient::constant(Vec4::new(2.0, 1.3, 0.5, 1.0)),
                render: SpriteRender::additive(SpriteShape::Streak { core: 0.5, taper: 1.0 }).stretched(0.25),
                ..ParticleLayer::burst(24, 100.0)
            })
            // A ring lying on the flight plane.
            .layer(ParticleLayer {
                lod: false,
                offset: Vec3::new(25.0, 0.0, 0.0),
                size: FloatRange::constant(40.0),
                rotation: FloatRange::constant(0.8),
                color_over_life: Gradient::constant(blue),
                render: SpriteRender {
                    facing: Facing::Plane { normal: Vec3::Y },
                    ..SpriteRender::additive(SpriteShape::Ring {
                        thickness: 0.1,
                        focus: 3.0,
                        fill: 0.05,
                    })
                },
                ..ParticleLayer::burst(1, 100.0)
            });
        let effects = app.resources.expect_mut::<ParticleEffects>();
        let fx = effects.add(effect).expect("valid");
        let ring = effects
            .add(ParticleEffect::new("emitter ring").layer(emitter_ring(Vec4::new(1.8, 0.7, 0.3, 1.0))))
            .expect("valid");
        app.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO).seed(9).keep());
        // The emitter-plane ring on a tilted emitter.
        app.spawn_effect(
            ring,
            EffectSpawn::from_transform(Transform::from_xyz(75.0, 0.0, 0.0).with_rotation(Quat::from_rotation_z(0.9)))
                .seed(10)
                .keep(),
        );
    })
    .description(
        "Sprite orientations, left to right: camera-facing glows; streaks stretched along their \
         velocity; a ring in a fixed plane (`Plane { normal: Y }`, the flight plane); a ring in \
         the emitter's XZ plane at spawn (`EmitterPlane`) on a tilted emitter, its focus toward \
         the emitter's −Z.",
    )
    .tag("particles")
    .size(320, 180)
    .ticks(12)
}

fn local_vs_world() -> Story {
    Story::new("Engine/Particles/Features/Local vs World", |app, _| {
        plugin(app, ParticleSettings::default());
        lights(app);
        top_camera(app, 240.0);
        let ship = circling_ship(app, 60.0, 0.03, 0.0);
        let glow = |space: SimulationSpace, color: Vec4| ParticleLayer {
            space,
            spawn: Spawn {
                rate: 60.0,
                ..Spawn::default()
            },
            shape: EmitShape::Ring {
                radius: 26.0,
                width: 0.0,
                arc: 0.0,
            },
            lifetime: FloatRange::constant(1.2),
            size: FloatRange::constant(4.0),
            color_over_life: Gradient::linear(color, color * 0.3),
            ..ParticleLayer::default()
        };
        let fx = app
            .resources
            .expect_mut::<ParticleEffects>()
            .add(ParticleEffect {
                looping: true,
                ..ParticleEffect::new("spaces")
                    .layer(glow(SimulationSpace::World, Vec4::new(1.6, 0.3, 0.15, 1.0)))
                    .layer(glow(SimulationSpace::Local, Vec4::new(0.3, 1.6, 0.4, 1.0)))
            })
            .expect("valid");
        app.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO).follow(ship).seed(12).keep());
    })
    .description(
        "One effect on a ship flying a circle, two layers emitting on a ring around it: \
         world-space particles (red) stay where they were emitted and draw the ship's path; \
         local-space particles (green) move with the ship and keep their ring.",
    )
    .tag("particles")
    .size(320, 180)
    .ticks(75)
}

fn cpu_backend() -> Story {
    Story::new("Engine/Particles/Features/CPU Backend", |app, _| {
        plugin(
            app,
            ParticleSettings {
                backend: ParticleBackend::Cpu,
                ..Default::default()
            },
        );
        app.insert_resource(FrameSettings::default().with_downlevel(DownlevelOverride::webgl2()));
        lights(app);
        top_camera(app, 300.0);
        circling_ship(app, 90.0, 0.03, 1.2);
        app.spawn_effect(
            builtin::EXPLOSION,
            EffectSpawn::at(Vec3::new(-30.0, 0.0, 10.0)).seed(13),
        );
        app.spawn_effect(
            builtin::WARP_OUT,
            EffectSpawn::at(Vec3::new(60.0, 0.0, -40.0)).seed(14).scale(0.4),
        );
    })
    .description(
        "The WebGL2 path on any device: `ParticleBackend::Cpu` (simulation in Rust, the same \
         ring slots, RNG and instance stream as the GPU backend) and \
         `DownlevelOverride::webgl2()` (no compute, no float targets, scene depth sampled \
         without a depth attachment). An explosion, a warp and a ship with engine exhausts.",
    )
    .tag("particles")
    .size(320, 180)
    .ticks(24)
}

fn stress_100k() -> Story {
    Story::new("Engine/Particles/Stress/100k", |app, _| {
        plugin(app, ParticleSettings::default());
        app.world.spawn((
            Name("camera".into()),
            Transform::from_xyz(0.0, 150.0, 30.0).looking_at(Vec3::ZERO, Vec3::Y),
            Camera3d::default().with_clear_color(SPACE),
        ));
        // Two emitters sweeping round, 180° apart, each streaming 12 500 glows a second from an
        // arc into an outward spiral (a vortex turns them as they fly): 4 s lives, 100k alive.
        let arm = ParticleLayer {
            lod: false,
            shape: EmitShape::Ring {
                radius: 6.0,
                width: 4.0,
                arc: 0.9,
            },
            flatten: 1.0,
            spread: 0.15,
            speed: FloatRange::new(9.0, 16.0),
            forces: vec![Force::Vortex {
                axis: Vec3::Y,
                strength: 1.6,
                pull: 0.0,
            }],
            drag: 0.1,
            size: FloatRange::new(0.8, 1.6),
            color_over_life: Gradient::linear(Vec4::new(0.3, 0.16, 0.06, 1.0), Vec4::new(0.02, 0.05, 0.2, 1.0)),
            alpha_over_life: Curve::Preset(FadeCurve::InOut),
            render: SpriteRender::additive(SpriteShape::Glow {
                core: 0.2,
                hardness: 0.0,
            }),
            ..ParticleLayer::continuous(12_500.0, 4.0)
        };
        let fx = app
            .resources
            .expect_mut::<ParticleEffects>()
            .add(ParticleEffect {
                looping: true,
                ..ParticleEffect::new("stress 100k").layer(arm)
            })
            .expect("valid");
        let arms: Vec<Entity> = (0..2)
            .map(|i| app.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO).seed(15 + i).keep()))
            .collect();
        app.add_system(Stage::FixedUpdate, move |ctx: &mut SystemCtx| {
            let a = ctx.time().tick as f32 * 0.03;
            for (i, &e) in arms.iter().enumerate() {
                if let Ok(mut t) = ctx.world.get::<&mut Transform>(e) {
                    t.rotation = Quat::from_rotation_y(a + i as f32 * std::f32::consts::PI);
                }
            }
        });
    })
    .description(
        "100 000 particles on the GPU backend: two emitters sweeping round, each streaming 12 500 \
         additive glows a second into an outward spiral (a vortex turns them as they fly), 4 s \
         lives. The pool, the simulation (one `simulate` and one `emit` dispatch per tick) and \
         the compaction into the instance stream (`mark`, `scatter`) stay on the GPU, and the \
         frame draws them with one indirect draw. The CPU backend (WebGL2) has a 16k budget and \
         refuses most of it.",
    )
    .tag("particles")
    .tag("perf")
    .size(256, 256)
    .ticks(240)
}

#[cfg(test)]
mod tests {
    use crate::space3d_test::screenshot::GoldenDir;
    use crate::space3d_test::{CompareOptions, gpu_info};
    use crate::{StoryCheckOutcome, Storybook, StorybookTestOptions, check_storybook};

    /// The particle stories against their goldens (the storybook test runs them too; this one
    /// selects only them, and runs on the GL backend as well: with `WGPU_BACKEND=gl` and ANGLE
    /// it compares within `CompareOptions::perceptual()`).
    #[test]
    fn particle_stories_match_their_goldens() {
        let Ok(gpu) = gpu_info() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut book = Storybook::new();
        let gl = gpu.backend.eq_ignore_ascii_case("gl");
        for story in super::stories() {
            // The GL backend (WebGL2 class) runs particles on the CPU backend, whose budget
            // (16k) does not hold the stress scene.
            if gl && story.has_tag("perf") {
                continue;
            }
            book.add(story);
        }
        let golden = GoldenDir::for_crate(env!("CARGO_MANIFEST_DIR"), env!("CARGO_PKG_NAME"));
        let options = StorybookTestOptions {
            compare: if gl {
                CompareOptions::perceptual()
            } else {
                CompareOptions::default()
            },
            ..Default::default()
        };
        let failures: Vec<String> = check_storybook(&book, &golden, &options)
            .into_iter()
            .filter_map(|c| match c.outcome {
                StoryCheckOutcome::Failed(m) => Some(format!("[{}] {m}", c.id)),
                _ => None,
            })
            .collect();
        assert!(
            failures.is_empty(),
            "{} failed on {}:\n{}",
            failures.len(),
            gpu.backend,
            failures.join("\n")
        );
    }
}
