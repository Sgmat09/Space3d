//! Particle stories (`Engine/Particles/*`): the built-in effects, the particle features and a
//! 100k-particle stress scene (graphics spec §5.2 PA1–PA3). They run on the default backend (the
//! GPU backend where compute exists; `Features/CPU Backend` forces the CPU one).
//!
//! Owner: P-PART (graphics spec §3.6). Compiled with the `particles` feature (part of
//! `engine`). Effects are driven by the fixed tick only, so every story is deterministic for its
//! tick count and doubles as a golden screenshot.

use super::{ambient, mesh, screen_target, sun};
use crate::{KnobValues, Story};
use space3d_core::prelude::*;
use space3d_particles::prelude::*;
use space3d_particles::{
    BuiltinTexture, ColorRange, Curve, EmitDirection, EmitShape, Facing, FadeCurve, Flicker, Flipbook, FlipbookMode,
    FloatRange, Force, Gradient, LightFlash, ParticleBackend, ParticleBlend, RibbonFacing, RibbonProfile, RibbonUv,
    SimulationSpace, SortMode, Spawn, SpriteRender, SpriteShape, SpriteTexture,
};
use space3d_render::{
    Camera3d, Distortion, DownlevelOverride, FrameSettings, GraphicsQuality, Material, Mesh3d, PostProcess,
    Projection, RenderAssets, RenderTarget, RenderTargetDesc, TextureSource, Tonemapper,
};

/// This area's stories, in registry order.
pub(crate) fn stories() -> Vec<Story> {
    let builtins = |hdr| {
        [
            explosion(hdr),
            hit_spark(hdr),
            engine_exhaust(hdr),
            shield_ripple(hdr),
            warp_in(hdr),
            warp_out(hdr),
            laser_trail(hdr),
            laser_beam(hdr),
            laser_shot(hdr),
            missile_trail(hdr),
            debris_sparks(hdr),
            engine_afterburner(hdr),
            emp(hdr),
            repair_beam(hdr),
        ]
    };
    let mut stories: Vec<Story> = builtins(false).into_iter().chain(builtins(true)).collect();
    stories.extend([
        blend_modes(),
        soft_particles(),
        shapes(),
        curves(),
        facing(),
        local_vs_world(),
        ribbons(),
        beams(),
        flipbook(),
        light_flashes(),
        distortion(),
        sorting(),
        hdr_bloom(),
        cpu_backend(),
        stress_100k(),
        stress_battle(),
    ]);
    stories
}

/// Tonemappers of the HDR variants' `tonemapper` knob.
const LOOKS: [&str; 3] = ["AgX (default)", "AgX (punchy)", "ACES (fitted)"];

/// A built-in effect's story: `Engine/Particles/Builtin/<name>` in Direct mode (no post: the
/// particles soft-clip themselves), or with `hdr` `Engine/Particles/HDR/<name>`, the same scene
/// through `PostProcess::default()` — the HDR scene texture, AgX and bloom: what a game with
/// post shows.
fn builtin_story(
    name: &str,
    hdr: bool,
    setup: impl Fn(&mut App, &KnobValues) -> Entity + Send + Sync + 'static,
) -> Story {
    let area = if hdr { "HDR" } else { "Builtin" };
    let story = Story::new(format!("Engine/Particles/{area}/{name}"), move |app, k| {
        let cam = setup(app, k);
        if hdr {
            let tonemapper = match k.choice("tonemapper") {
                1 => Tonemapper::AgXPunchy,
                2 => Tonemapper::AcesFitted,
                _ => Tonemapper::AgX,
            };
            let mut post = PostProcess::default().with_tonemapper(tonemapper);
            if !k.bool("bloom") {
                post = post.without_bloom();
            }
            let _ = app.world.insert_one(cam, post);
        }
    });
    if hdr {
        story.knob_choice("tonemapper", &LOOKS, 0).knob_bool("bloom", true)
    } else {
        story
    }
}

/// The description of a built-in's story (`what`), with the HDR variant's note.
fn builtin_description(what: &str, hdr: bool) -> String {
    if hdr {
        format!(
            "{what} Rendered through `PostProcess::default()` (HDR scene texture, AgX, bloom): the \
             look a game with post gets (`Builtin/*` shows Direct mode's soft clip); `tonemapper` \
             switches to AgX's punchy look or ACES, `bloom` turns the glow off for comparison."
        )
    } else {
        what.to_string()
    }
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

/// The engine color of the stories' ship: an ion-blue flame (`ENGINE_EXHAUST` takes its hue
/// from the tint; [`circling_ship`] doubles it).
const ION_BLUE: [f32; 4] = [0.15, 0.45, 1.0, 1.0];

/// A ship flying a circle of `radius` at `speed` rad per tick, with two engine exhausts tinted
/// `2 × color` (a color knob stays within 0..1; the game's engine colors reach about 2).
fn circling_ship(app: &mut App, radius: f32, speed: f32, throttle: f32, color: [f32; 4]) -> Entity {
    let ship = hull(app, Transform::from_xyz(radius, 0.0, 0.0));
    for x in [-6.0, 6.0] {
        let e = app.spawn_effect(
            builtin::ENGINE_EXHAUST,
            EffectSpawn::from_transform(Transform::from_xyz(x, 0.0, 21.0))
                .follow(ship)
                .tint(Vec4::from_array(color) * Vec4::new(2.0, 2.0, 2.0, 1.0))
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

fn explosion(hdr: bool) -> Story {
    builtin_story("Explosion", hdr, |app, k| {
        plugin(
            app,
            ParticleSettings {
                quality: k.f32("quality"),
                ..Default::default()
            },
        );
        lights(app);
        let cam = top_camera(app, 200.0);
        app.spawn_effect(
            builtin::EXPLOSION,
            EffectSpawn::at(Vec3::ZERO).scale(k.f32("scale")).seed(1),
        );
        cam
    })
    .description(builtin_description(
        "`builtin::EXPLOSION`, one call: `spawn_effect(builtin::EXPLOSION, EffectSpawn::at(p).scale(s))`. \
         A white-hot flash and afterglow, fireball billows (premultiplied smoke puffs whose light \
         burns out while their coverage holds), a hot core, sparks stretched along their velocity, \
         flickering embers, two shockwave rings on the flight plane and dark smoke rolling out \
         (0.3 s in). `quality` scales the spawn counts of the LOD layers.",
        hdr,
    ))
    .knob_f32("scale", 1.0, 0.25..=3.0)
    .knob_f32("quality", 1.0, 0.0..=1.0)
    .tag("particles")
    .size(320, 180)
    .ticks(18)
}

fn hit_spark(hdr: bool) -> Story {
    builtin_story("Hit Spark", hdr, |app, k| {
        plugin(app, ParticleSettings::default());
        lights(app);
        let cam = top_camera(app, 110.0);
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
        cam
    })
    .description(builtin_description(
        "`builtin::HIT_SPARK`, spawned with `.facing(toward_the_shooter).tint(laser_color)`: a \
         flash, sparks thrown back toward the shooter (velocity-stretched streaks), a small ring \
         on the emitter's plane and two puffs of smoke; three hits on a hull, 0.1 s in.",
        hdr,
    ))
    .knob_color("color", [1.0, 0.35, 0.25, 1.0])
    .tag("particles")
    .size(320, 180)
    .ticks(5)
}

fn engine_exhaust(hdr: bool) -> Story {
    builtin_story("Engine Exhaust", hdr, |app, k| {
        plugin(
            app,
            ParticleSettings {
                quality: k.f32("quality"),
                ..Default::default()
            },
        );
        lights(app);
        let cam = top_camera(app, 190.0);
        circling_ship(app, 55.0, 0.03, k.f32("throttle"), k.color("color"));
        cam
    })
    .description(builtin_description(
        "`builtin::ENGINE_EXHAUST` on two nozzles of a ship flying a circle (the emitters are \
         children of the hull, `EffectSpawn::follow(ship)`): a white-hot plume taking its hue \
         from the tint (`color` × 2, any faction color), inheriting part of the ship's velocity, \
         sparks, and a local-space glow riding the nozzle. Emission is spread over each tick's \
         motion, so the plume never clumps; `throttle` drives `ParticleEmitter::rate_scale` \
         (0..2); `quality` is `ParticleSettings::quality` (`for_quality`: 0.25 on Low, 0.125 on \
         the web).",
        hdr,
    ))
    .knob_f32("throttle", 1.0, 0.0..=2.0)
    .knob_f32("quality", 1.0, 0.0..=1.0)
    .knob_color("color", ION_BLUE)
    .tag("particles")
    .size(320, 180)
    .ticks(90)
}

fn shield_ripple(hdr: bool) -> Story {
    builtin_story("Shield Ripple", hdr, |app, k| {
        plugin(app, ParticleSettings::default());
        lights(app);
        let cam = top_camera(app, 150.0);
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
        cam
    })
    .description(builtin_description(
        "`builtin::SHIELD_RIPPLE` around a ship, spawned at the shield center with \
         `.facing(-bolt_direction).scale(radius / 20).follow(ship)`: an arc brightest toward the \
         shooter, a faint full bubble, a flash on the shield surface and tangent streaks \
         (0.07 s in). The rings are local-space: they stay on a moving ship.",
        hdr,
    ))
    .knob_f32("radius", 30.0, 10.0..=60.0)
    .tag("particles")
    .size(320, 180)
    .ticks(4)
}

fn warp_in(hdr: bool) -> Story {
    builtin_story("Warp In", hdr, |app, _| {
        plugin(app, ParticleSettings::default());
        lights(app);
        let cam = top_camera(app, 380.0);
        app.spawn_effect(builtin::WARP_IN, EffectSpawn::at(Vec3::ZERO).seed(5));
        cam
    })
    .description(builtin_description(
        "`builtin::WARP_IN`: a vortex collapsing inward while streaks rush to the center, then a \
         flash and two expanding rings as the ship appears (0.35 s in).",
        hdr,
    ))
    .tag("particles")
    .size(320, 180)
    .ticks(21)
}

fn warp_out(hdr: bool) -> Story {
    builtin_story("Warp Out", hdr, |app, _| {
        plugin(app, ParticleSettings::default());
        lights(app);
        let cam = top_camera(app, 380.0);
        app.spawn_effect(builtin::WARP_OUT, EffectSpawn::at(Vec3::ZERO).seed(6));
        cam
    })
    .description(builtin_description(
        "`builtin::WARP_OUT`: a flash, a vortex opening outward, two expanding rings and streaks \
         rushing out of the center (0.2 s in).",
        hdr,
    ))
    .tag("particles")
    .size(320, 180)
    .ticks(12)
}

fn laser_trail(hdr: bool) -> Story {
    builtin_story("Laser Trail", hdr, |app, k| {
        plugin(app, ParticleSettings::default());
        lights(app);
        let cam = top_camera(app, 240.0);
        let tint = Vec4::from_array(k.color("color"));
        // Two bolts at 1500 units/s, the trail following each (it drains when a bolt is gone).
        // Their movement is a gameplay system added after `ParticlesPlugin`: the particle tick
        // still runs after it (`ParticlesPlugin::TICK_ORDER`), so the trail ends at the head.
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
        cam
    })
    .description(builtin_description(
        "`builtin::LASER_TRAIL` following two bolts at 1500 units/s (`EffectSpawn::at(muzzle)\
         .follow(bolt).tint(color)`): a white-hot head riding the bolt and a continuous beam of \
         glows laid along its path per unit of distance, spread over each tick's motion. The \
         bolts move in a gameplay system; the particle tick runs after it, so the trail ends at \
         the head.",
        hdr,
    ))
    .knob_color("color", [1.0, 0.1, 0.05, 1.0])
    .tag("particles")
    .size(320, 180)
    .ticks(13)
}

/// A ship firing a continuous `LASER_BEAM` at another: the emitter rides the gun (a child of
/// the shooter), its `ParticleBeam` ends at the target entity.
fn laser_beam(hdr: bool) -> Story {
    builtin_story("Laser Beam", hdr, |app, k| {
        plugin(app, ParticleSettings::default());
        lights(app);
        let cam = top_camera(app, 230.0);
        let shooter = hull(
            app,
            Transform::from_xyz(-90.0, 0.0, 25.0).with_rotation(heading(Vec3::new(1.0, 0.0, -0.3))),
        );
        let target = hull(
            app,
            Transform::from_xyz(85.0, 0.0, -25.0).with_rotation(heading(Vec3::new(-0.2, 0.0, 1.0))),
        );
        // The beam ends on the target's hull side (a point riding the target: an entity).
        let hit = app.world.spawn((Transform::from_xyz(-7.0, 2.0, 4.0), Parent(target)));
        let tint = Vec4::from_array(k.color("color"));
        app.spawn_effect(
            builtin::LASER_BEAM,
            EffectSpawn::from_transform(Transform::from_xyz(0.0, 2.0, -21.0))
                .follow(shooter)
                .beam_to(BeamEnd::Entity(hit))
                .tint(tint)
                .seed(20)
                .keep(),
        );
        cam
    })
    .description(builtin_description(
        "`builtin::LASER_BEAM` from a ship's gun to another ship's hull side (the emitter follows \
         the shooter, `EffectSpawn::beam_to(BeamEnd::Entity(hit_point))`): a wide glow of the \
         tint around a white-hot core, energy flowing along it, a slight flicker, a round glow \
         where it hits and a muzzle glow. Looping; the beam follows both ships as they move.",
        hdr,
    ))
    .knob_color("color", [1.0, 0.1, 0.05, 1.0])
    .tag("particles")
    .size(320, 180)
    .ticks(20)
}

/// Hitscan `LASER_SHOT`s fired every other tick at three targets: each is at another stage of
/// its 0.12 s fade.
fn laser_shot(hdr: bool) -> Story {
    builtin_story("Laser Shot", hdr, |app, k| {
        plugin(app, ParticleSettings::default());
        lights(app);
        let cam = top_camera(app, 230.0);
        hull(
            app,
            Transform::from_xyz(-100.0, 0.0, 0.0).with_rotation(heading(Vec3::X)),
        );
        let tint = Vec4::from_array(k.color("color"));
        let targets = [
            Vec3::new(90.0, 0.0, -60.0),
            Vec3::new(110.0, 0.0, 5.0),
            Vec3::new(80.0, 0.0, 60.0),
        ];
        app.add_system(Stage::FixedUpdate, move |ctx: &mut SystemCtx| {
            let tick = ctx.time().tick;
            if tick.is_multiple_of(2) && tick < 6 {
                let hit = targets[(tick / 2) as usize];
                ctx.spawn_effect(
                    builtin::LASER_SHOT,
                    EffectSpawn::at(Vec3::new(-78.0, 0.0, 0.0))
                        .beam_to(BeamEnd::World(hit))
                        .tint(tint)
                        .seed(tick as u32),
                );
                ctx.spawn_effect(
                    builtin::HIT_SPARK,
                    EffectSpawn::at(hit)
                        .facing(Vec3::new(-78.0, 0.0, 0.0) - hit)
                        .tint(tint)
                        .seed(tick as u32 + 10),
                );
            }
        });
        cam
    })
    .description(builtin_description(
        "`builtin::LASER_SHOT`, the hitscan shot: `spawn_effect(builtin::LASER_SHOT, \
         EffectSpawn::at(muzzle).beam_to(BeamEnd::World(hit)).tint(color))` draws the beam for \
         0.12 s, fading as it thins, with a muzzle flash and a hot glow on the hit (with a \
         `HIT_SPARK` there). Three shots fired two ticks apart, each at another stage.",
        hdr,
    ))
    .knob_color("color", [0.2, 0.55, 1.0, 1.0])
    .tag("particles")
    .size(320, 180)
    .ticks(6)
}

/// A missile curving across the view with `MISSILE_TRAIL`.
fn missile_trail(hdr: bool) -> Story {
    builtin_story("Missile Trail", hdr, |app, _| {
        plugin(app, ParticleSettings::default());
        lights(app);
        let cam = top_camera(app, 240.0);
        let body = mesh(app, MeshData::cube(1.0));
        let missile = app
            .world
            .spawn((Name("missile".into()), Transform::from_xyz(-200.0, 0.0, 60.0)));
        app.world.spawn((
            Transform::IDENTITY.with_scale(Vec3::new(1.2, 1.2, 5.0)),
            Mesh3d(body),
            Material::srgb(0.5, 0.5, 0.55).with_metallic_roughness(0.7, 0.4),
            Parent(missile),
        ));
        app.spawn_effect(
            builtin::MISSILE_TRAIL,
            EffectSpawn::at(Vec3::ZERO).follow(missile).seed(21).keep(),
        );
        // 420 units/s along an arc of radius 260 (bending right), a second of flight.
        app.add_system(Stage::FixedUpdate, move |ctx: &mut SystemCtx| {
            let theta = -0.9 + 420.0 / 260.0 * ctx.time().tick as f32 / 60.0;
            let pos = Vec3::new(-40.0 + 260.0 * theta.sin(), 0.0, 200.0 - 260.0 * theta.cos());
            if let Ok(mut tr) = ctx.world.get::<&mut Transform>(missile) {
                tr.translation = pos;
                tr.rotation = heading(Vec3::new(theta.cos(), 0.0, theta.sin()));
            }
        });
        cam
    })
    .description(builtin_description(
        "`builtin::MISSILE_TRAIL` following a missile on a curve at 420 units/s \
         (`EffectSpawn::at(Vec3::ZERO).follow(missile)`): a white-hot flame and a short glowing \
         wake riding the missile, sparks, and a smoke ribbon laid along its path that widens, \
         breaks up and fades over 1.6 s, with a few puffs for volume.",
        hdr,
    ))
    .tag("particles")
    .size(320, 180)
    .ticks(60)
}

/// `DEBRIS_SPARKS` breaking off a hull, lighting it (the light flash).
fn debris_sparks(hdr: bool) -> Story {
    builtin_story("Debris Sparks", hdr, |app, k| {
        plugin(app, ParticleSettings::default());
        sun(app, Vec3::new(-0.4, -1.0, -0.5), 0.8);
        ambient(app, 0.05);
        let cam = top_camera(app, 130.0);
        hull(app, Transform::IDENTITY.with_rotation(Quat::from_rotation_y(0.5)));
        app.spawn_effect(
            builtin::DEBRIS_SPARKS,
            EffectSpawn::at(Vec3::new(-7.0, 3.0, -6.0))
                .facing(Vec3::new(-1.0, 0.0, -0.3))
                .scale(k.f32("scale"))
                .seed(22),
        );
        cam
    })
    .description(builtin_description(
        "`builtin::DEBRIS_SPARKS` breaking off a hull (`.facing(toward_the_shooter)`, \
         `.scale(hull / 40)`): glowing chunks tumbling out and cooling from orange to dark \
         plating, sparks, flickering embers and a flash whose light (a `PointLight` while it \
         plays) catches the hull, 0.12 s in.",
        hdr,
    ))
    .knob_f32("scale", 1.0, 0.25..=3.0)
    .tag("particles")
    .size(320, 180)
    .ticks(7)
}

/// A ship boosting in a circle: `ENGINE_AFTERBURNER` over `ENGINE_EXHAUST` on both nozzles.
fn engine_afterburner(hdr: bool) -> Story {
    builtin_story("Engine Afterburner", hdr, |app, k| {
        plugin(app, ParticleSettings::default());
        lights(app);
        let cam = top_camera(app, 240.0);
        let boost = k.f32("boost");
        let color = Vec4::from_array(k.color("color")) * Vec4::new(2.0, 2.0, 2.0, 1.0);
        let ship = circling_ship(app, 70.0, 0.035, 1.0, k.color("color"));
        for x in [-6.0, 6.0] {
            let e = app.spawn_effect(
                builtin::ENGINE_AFTERBURNER,
                EffectSpawn::from_transform(Transform::from_xyz(x, 0.0, 21.0))
                    .follow(ship)
                    .tint(color)
                    .seed(23 + (x > 0.0) as u32)
                    .keep(),
            );
            if let Ok(mut em) = app.world.get::<&mut ParticleEmitter>(e) {
                em.rate_scale = boost;
            }
        }
        cam
    })
    .description(builtin_description(
        "`builtin::ENGINE_AFTERBURNER` layered over `ENGINE_EXHAUST` on both nozzles of a ship \
         flying a circle: a long white-hot plume taking its hue from the tint, three shock \
         diamonds riding the nozzle, a glowing ribbon trail along the ship's path and heat \
         sparks. `boost` drives `ParticleEmitter::rate_scale` (0..1.5): the plume keeps its \
         density and grows longer and brighter.",
        hdr,
    ))
    .knob_f32("boost", 1.0, 0.0..=1.5)
    .knob_color("color", ION_BLUE)
    .tag("particles")
    .size(320, 180)
    .ticks(90)
}

/// An `EMP` among a few ships, whose hulls catch its light.
fn emp(hdr: bool) -> Story {
    builtin_story("EMP", hdr, |app, k| {
        plugin(app, ParticleSettings::default());
        sun(app, Vec3::new(-0.4, -1.0, -0.5), 0.8);
        ambient(app, 0.05);
        let cam = top_camera(app, 260.0);
        for (p, dir) in [
            (Vec3::new(-75.0, 0.0, -25.0), Vec3::X),
            (Vec3::new(70.0, 0.0, 40.0), Vec3::new(-0.6, 0.0, -1.0)),
            (Vec3::new(30.0, 0.0, -70.0), Vec3::new(-1.0, 0.0, 0.3)),
        ] {
            hull(app, Transform::from_xyz(p.x, p.y, p.z).with_rotation(heading(dir)));
        }
        app.spawn_effect(
            builtin::EMP,
            EffectSpawn::at(Vec3::ZERO)
                .tint(Vec4::from_array(k.color("color")))
                .seed(25),
        );
        cam
    })
    .description(builtin_description(
        "`builtin::EMP` (`.scale(radius / 80)`): a white-cyan flash whose light catches the ships \
         around it, a fast shock ring on the flight plane with a fainter one behind, crackling \
         arcs thrown outward and flickering motes of charge, 0.13 s in.",
        hdr,
    ))
    .knob_color("color", [1.0, 1.0, 1.0, 1.0])
    .tag("particles")
    .size(320, 180)
    .ticks(8)
}

/// A support ship's `REPAIR_BEAM` on a damaged one.
fn repair_beam(hdr: bool) -> Story {
    builtin_story("Repair Beam", hdr, |app, _| {
        plugin(app, ParticleSettings::default());
        lights(app);
        let cam = top_camera(app, 230.0);
        let healer = hull(
            app,
            Transform::from_xyz(-95.0, 0.0, -30.0).with_rotation(heading(Vec3::new(1.0, 0.0, 0.4))),
        );
        let target = hull(
            app,
            Transform::from_xyz(80.0, 0.0, 30.0).with_rotation(heading(Vec3::new(0.3, 0.0, -1.0))),
        );
        let dock = app.world.spawn((Transform::from_xyz(-7.0, 3.0, 0.0), Parent(target)));
        app.spawn_effect(
            builtin::REPAIR_BEAM,
            EffectSpawn::from_transform(Transform::from_xyz(0.0, 4.0, -12.0))
                .follow(healer)
                .beam_to(BeamEnd::Entity(dock))
                .seed(26)
                .keep(),
        );
        cam
    })
    .description(builtin_description(
        "`builtin::REPAIR_BEAM` from a support ship to a damaged one (`BeamEnd::Entity`): a soft \
         green stream flowing toward the target, a bright filament wavering around it \
         (`jitter`), round glows at both ends and a gentle pulse.",
        hdr,
    ))
    .tag("particles")
    .size(320, 180)
    .ticks(30)
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
        let rt = screen_target(
            app,
            RenderTargetDesc::default().with_samples(samples),
            Vec3::new(0.0, -2000.0, 0.0),
            SPACE,
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
        let ship = circling_ship(app, 60.0, 0.03, 0.0, ION_BLUE);
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

/// An emitter flying a figure eight of `size` (period `period` ticks) from `phase`.
fn figure_eight(app: &mut App, e: Entity, center: Vec3, size: Vec2, period: f32, phase: f32) {
    app.add_system(Stage::FixedUpdate, move |ctx: &mut SystemCtx| {
        let a = std::f32::consts::TAU * (ctx.time().tick as f32 / period) + phase;
        let pos = center + Vec3::new(size.x * a.sin(), 0.0, size.y * (2.0 * a).sin() * 0.5);
        let vel = Vec3::new(size.x * a.cos(), 0.0, size.y * (2.0 * a).cos());
        if let Ok(mut t) = ctx.world.get::<&mut Transform>(e) {
            t.translation = pos;
            t.rotation = heading(vel);
        }
    });
}

fn ribbons() -> Story {
    Story::new("Engine/Particles/Features/Ribbons", |app, _| {
        plugin(app, ParticleSettings::default());
        lights(app);
        top_camera(app, 260.0);
        let glow = RibbonLayer {
            name: "glow".into(),
            min_distance: 2.0,
            max_points: 96,
            lifetime: 1.1,
            width: 7.0,
            width_over_life: Curve::Linear(1.0, 0.0),
            color_over_life: Gradient::linear(Vec4::new(0.6, 1.2, 3.0, 1.0), Vec4::new(0.8, 0.1, 1.2, 0.0)),
            profile: RibbonProfile {
                core: 0.5,
                hardness: 0.0,
            },
            ..RibbonLayer::default()
        };
        let smoke = RibbonLayer {
            name: "smoke".into(),
            min_distance: 3.0,
            max_points: 96,
            lifetime: 1.6,
            width: 6.0,
            width_over_life: Curve::Linear(0.6, 3.0),
            color_over_life: Gradient::linear(Vec4::new(0.35, 0.33, 0.32, 0.8), Vec4::new(0.12, 0.12, 0.13, 0.0)),
            profile: RibbonProfile {
                core: 0.0,
                hardness: 0.0,
            },
            uv: RibbonUv::Tile { length: 16.0 },
            noise: 0.75,
            blend: ParticleBlend::Alpha,
            facing: RibbonFacing::Plane { normal: Vec3::Y },
            ..RibbonLayer::default()
        };
        let plasma = RibbonLayer {
            name: "plasma".into(),
            min_distance: 2.0,
            max_points: 96,
            lifetime: 0.9,
            width: 9.0,
            width_over_life: Curve::Smooth(vec![(0.0, 0.3), (0.2, 1.0), (1.0, 0.6)]),
            color_over_life: Gradient::linear(Vec4::new(2.5, 0.4, 1.6, 1.0), Vec4::new(0.4, 0.05, 0.6, 0.0)),
            profile: RibbonProfile {
                core: 0.3,
                hardness: 0.3,
            },
            uv: RibbonUv::Tile { length: 10.0 },
            scroll: 3.0,
            noise: 0.9,
            ..RibbonLayer::default()
        };
        let effects = app.resources.expect_mut::<ParticleEffects>();
        let fx = [glow, smoke, plasma].map(|r| {
            effects
                .add(ParticleEffect {
                    looping: true,
                    ..ParticleEffect::new(r.name.clone()).ribbon(r)
                })
                .expect("valid")
        });
        for (i, fx) in fx.into_iter().enumerate() {
            let e = app.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO).seed(30 + i as u32).keep());
            figure_eight(
                app,
                e,
                Vec3::new(0.0, 0.0, -55.0 + 55.0 * i as f32),
                Vec2::new(150.0, 50.0),
                150.0,
                i as f32 * 0.6,
            );
        }
    })
    .description(
        "Ribbon layers (`RibbonLayer`) on three emitters flying figure eights: a history of the \
         emitter's positions drawn as one watertight strip up to its render position, its tail \
         shortening smoothly to the points' lifetime. Top: an additive camera-facing glow \
         narrowing over its life. Middle: alpha-blended smoke lying on the flight plane, \
         widening and broken up by noise fixed to the path (`uv: Tile`). Bottom: a plasma \
         trail whose noise scrolls toward the head (`scroll`).",
    )
    .tag("particles")
    .size(320, 180)
    .ticks(110)
}

fn beams() -> Story {
    Story::new("Engine/Particles/Features/Beams", |app, _| {
        plugin(app, ParticleSettings::default());
        front_camera(app, 120.0);
        let beam = |b: BeamLayer| ParticleEffect {
            looping: true,
            ..ParticleEffect::new(b.name.clone()).beam(b)
        };
        let rows = [
            // A clean laser: hard-edged core, glows at both ends.
            beam(BeamLayer {
                name: "laser".into(),
                width: 3.0,
                color: Gradient::constant(Vec4::new(3.0, 0.5, 0.3, 1.0)),
                profile: RibbonProfile {
                    core: 0.8,
                    hardness: 0.5,
                },
                start_glow: 2.0,
                end_glow: 3.5,
                ..BeamLayer::default()
            }),
            // Lightning: 32 segments jittered 20 times a second, flickering.
            beam(BeamLayer {
                name: "lightning".into(),
                width: 1.6,
                segments: 32,
                jitter: 5.0,
                jitter_hz: 20.0,
                color: Gradient::constant(Vec4::new(1.5, 2.5, 5.0, 1.0)),
                profile: RibbonProfile {
                    core: 0.8,
                    hardness: 0.0,
                },
                flicker: Some(Flicker {
                    amplitude: 0.4,
                    frequency: 25.0,
                }),
                end_glow: 4.0,
                ..BeamLayer::default()
            }),
            // An energy stream: noise scrolling toward the end, the color changing along it.
            beam(BeamLayer {
                name: "stream".into(),
                width: 8.0,
                segments: 8,
                color: Gradient::linear(Vec4::new(0.3, 2.0, 0.6, 1.0), Vec4::new(0.2, 0.8, 2.4, 1.0)),
                profile: RibbonProfile {
                    core: 0.2,
                    hardness: 0.0,
                },
                uv: RibbonUv::Tile { length: 16.0 },
                scroll: 2.0,
                noise: 0.8,
                ..BeamLayer::default()
            }),
            // A textured beam: the built-in sparkle tiled along it.
            beam(BeamLayer {
                name: "textured".into(),
                width: 7.0,
                color: Gradient::constant(Vec4::new(2.0, 1.6, 0.8, 1.0)),
                texture: Some(SpriteTexture::Builtin(BuiltinTexture::Sparkle)),
                uv: RibbonUv::Tile { length: 8.0 },
                ..BeamLayer::default()
            }),
        ];
        let effects = app.resources.expect_mut::<ParticleEffects>();
        let rows = rows.map(|e| effects.add(e).expect("valid"));
        for (i, fx) in rows.into_iter().enumerate() {
            let y = 40.0 - 26.0 * i as f32;
            app.spawn_effect(
                fx,
                EffectSpawn::at(Vec3::new(-85.0, y, 0.0))
                    .beam_to(BeamEnd::World(Vec3::new(85.0, y + 4.0, 0.0)))
                    .seed(40 + i as u32)
                    .keep(),
            );
        }
    })
    .description(
        "Beam layers (`BeamLayer`) between two points (`EffectSpawn::beam_to`), rebuilt every \
         frame from their end points: a laser with a hard core and round glows at both ends; \
         lightning (32 segments jittered 20 times a second, flickering); an energy stream whose \
         noise flows toward the end and whose color changes along it; a texture tiled along a \
         beam.",
    )
    .tag("particles")
    .size(320, 180)
    .ticks(10)
}

fn flipbook() -> Story {
    Story::new("Engine/Particles/Features/Flipbook", |app, _| {
        plugin(app, ParticleSettings::default());
        front_camera(app, 60.0);
        let book = |texture: BuiltinTexture, mode: FlipbookMode, blend: bool| SpriteRender {
            shape: SpriteShape::Texture(SpriteTexture::Builtin(texture)),
            flipbook: Some(Flipbook {
                frames: 16,
                mode,
                blend,
                ..Flipbook::default()
            }),
            ..SpriteRender::default()
        };
        // Fireballs rising over their life (a new one every 0.2 s: every frame is on screen).
        let fire = ParticleLayer {
            lod: false,
            offset: Vec3::new(-50.0, 6.0, 0.0),
            shape: EmitShape::Line { length: 0.0 },
            direction: EmitDirection::Fixed(Vec3::X),
            speed: FloatRange::constant(30.0),
            lifetime: FloatRange::constant(3.2),
            size: FloatRange::constant(9.0),
            color_over_life: Gradient::constant(Vec4::new(3.0, 3.0, 3.0, 1.0)),
            render: book(BuiltinTexture::Fireball16, FlipbookMode::OverLife { cycles: 1.0 }, true),
            ..ParticleLayer::continuous(5.0, 3.2)
        };
        // Smoke puffs on random frames (premultiplied: the smoke covers what is behind it).
        let smoke = ParticleLayer {
            lod: false,
            offset: Vec3::new(0.0, -14.0, 0.0),
            shape: EmitShape::Line { length: 100.0 },
            size: FloatRange::new(10.0, 14.0),
            rotation: FloatRange::new(0.0, std::f32::consts::TAU),
            color_over_life: Gradient::constant(Vec4::new(0.8, 0.8, 0.85, 1.0)),
            render: SpriteRender {
                blend: ParticleBlend::Premultiplied,
                ..book(BuiltinTexture::Smoke16, FlipbookMode::Random, false)
            },
            ..ParticleLayer::burst(9, 100.0)
        };
        let fx = app
            .resources
            .expect_mut::<ParticleEffects>()
            .add(ParticleEffect {
                looping: true,
                ..ParticleEffect::new("flipbooks").layer(fire).layer(smoke)
            })
            .expect("valid");
        app.spawn_effect(fx, EffectSpawn::at(Vec3::ZERO).seed(50).keep());
    })
    .description(
        "Flipbooks from texture arrays (the procedural built-ins, 16 frames of 64² each). Top: \
         `Fireball16` played over each particle's life, blending between frames, a new particle \
         every 0.2 s (left: young and white-hot; right: burnt out). Bottom: `Smoke16` puffs on \
         random frames, premultiplied.",
    )
    .tag("particles")
    .size(320, 180)
    .ticks(200)
}

fn light_flashes() -> Story {
    Story::new("Engine/Particles/Features/Light Flashes", |app, _| {
        plugin(app, ParticleSettings::default());
        sun(app, Vec3::new(-0.4, -1.0, -0.5), 0.6);
        ambient(app, 0.04);
        top_camera(app, 200.0);
        let ground = mesh(app, MeshData::plane(400.0));
        app.world.spawn((
            Transform::from_xyz(0.0, -6.0, 0.0),
            Mesh3d(ground),
            Material::srgb(0.2, 0.2, 0.22).with_metallic_roughness(0.1, 0.7),
        ));
        for x in [-80.0, 0.0, 80.0] {
            hull(
                app,
                Transform::from_xyz(x, 0.0, -25.0).with_rotation(heading(Vec3::new(0.3, 0.0, 1.0))),
            );
        }
        let flash = |color: Vec3| ParticleEffect {
            duration: 0.05,
            light: Some(LightFlash {
                duration: 0.5,
                color,
                peak: 2500.0,
                range: 120.0,
                offset: Vec3::new(0.0, 12.0, 0.0),
                ..LightFlash::default()
            }),
            ..ParticleEffect::new("flash").layer(ParticleLayer {
                lod: false,
                size: FloatRange::constant(16.0),
                color_over_life: Gradient::constant((color * 3.0).extend(1.0)),
                alpha_over_life: Curve::Preset(FadeCurve::Flash),
                render: SpriteRender::additive(SpriteShape::Glow {
                    core: 1.5,
                    hardness: 0.0,
                }),
                ..ParticleLayer::burst(1, 0.5)
            })
        };
        let effects = app.resources.expect_mut::<ParticleEffects>();
        let fx = [
            Vec3::new(1.0, 0.5, 0.15),
            Vec3::new(0.2, 0.7, 1.0),
            Vec3::new(1.0, 0.25, 0.8),
        ]
        .map(|c| effects.add(flash(c)).expect("valid"));
        for (i, fx) in fx.into_iter().enumerate() {
            app.spawn_effect(
                fx,
                EffectSpawn::at(Vec3::new(-80.0 + 80.0 * i as f32, 4.0, 5.0)).seed(60 + i as u32),
            );
        }
    })
    .description(
        "Light flashes (`ParticleEffect::light`): while a flash plays, an ordinary `PointLight` \
         child entity of its emitter lights the scene — here three colored flashes over the \
         hulls and the ground beside them, 0.07 s in. `peak` follows the `intensity` curve over \
         the flash, × the effect's scale²; the light is despawned when the flash ends, before \
         the effect finishes.",
    )
    .tag("particles")
    .size(320, 180)
    .ticks(4)
}

/// A grid far below the flight plane (`size` world units, a line every `cell`; below the reach
/// of the effects' big camera-facing glows, which it would cut), unlit and dim: a backdrop that
/// shows how distortion bends the image behind an effect.
fn grid_backdrop(app: &mut App, size: f32, cell: f32) {
    let n = 512u32;
    let cells = (size / cell).round().max(1.0) as u32;
    let step = n / cells;
    let mut image = ImageData::new(n, n);
    for y in 0..n {
        for x in 0..n {
            let line = x % step < 2 || y % step < 2;
            let c = if line { [70, 92, 130, 255] } else { [10, 12, 20, 255] };
            image.pixels[((y * n + x) * 4) as usize..][..4].copy_from_slice(&c);
        }
    }
    let texture = app
        .resources
        .init::<RenderAssets>()
        .add_texture(TextureSource::from_image(image));
    let plane = mesh(app, MeshData::plane(size));
    app.world.spawn((
        Name("grid".into()),
        Transform::from_xyz(0.0, -80.0, 0.0),
        Mesh3d(plane),
        Material::unlit(Vec4::ONE).with_texture(texture),
    ));
}

fn distortion() -> Story {
    Story::new("Engine/Particles/Features/Distortion", |app, k| {
        plugin(app, ParticleSettings::default());
        lights(app);
        grid_backdrop(app, 640.0, 20.0);
        let cam = top_camera(app, 230.0);
        let post = PostProcess::default().with_distortion(Distortion {
            enabled: k.bool("distortion"),
            strength: k.f32("strength"),
            ..Distortion::default()
        });
        let _ = app.world.insert_one(cam, post);
        // An explosion's shockwave and heat shimmer (left), a ship's engine heat haze (middle,
        // flying toward the camera's top) and a warp's twisting vortex (right).
        app.spawn_effect(
            builtin::EXPLOSION,
            EffectSpawn::at(Vec3::new(-75.0, 0.0, 5.0)).scale(0.7).seed(71),
        );
        let ship = hull(
            app,
            Transform::from_xyz(0.0, 0.0, -15.0).with_rotation(heading(Vec3::new(0.25, 0.0, -1.0))),
        );
        for x in [-6.0, 6.0] {
            let e = app.spawn_effect(
                builtin::ENGINE_EXHAUST,
                EffectSpawn::from_transform(Transform::from_xyz(x, 0.0, 21.0))
                    .follow(ship)
                    .tint(Vec4::new(0.3, 0.9, 2.0, 1.0))
                    .seed(72 + (x > 0.0) as u32)
                    .keep(),
            );
            if let Ok(mut em) = app.world.get::<&mut ParticleEmitter>(e) {
                em.rate_scale = 1.6;
            }
        }
        app.spawn_effect(
            builtin::WARP_IN,
            EffectSpawn::at(Vec3::new(80.0, 0.0, -10.0)).scale(0.35).seed(74),
        );
    })
    .description(
        "Distortion layers (`SpriteRender::distortion(strength)`, `SpriteTarget::Distortion`):          sprites drawn into the post group's distortion buffer instead of color, which the          composite applies (post's PO5). The built-ins carry them: `EXPLOSION`'s shockwave and          heat shimmer (left, 0.3 s in), `ENGINE_EXHAUST`'s heat haze behind a ship (middle) and          `WARP_IN`'s twisting vortex (right), over a grid on the flight plane. Ring shapes push          the image outward, vortices swirl it, glows shimmer it like heat; closer geometry is          never bent, and cameras without `PostProcess` draw nothing of them. `distortion`          toggles `PostProcess::distortion`, `strength` scales every offset.",
    )
    .knob_bool("distortion", true)
    .knob_f32("strength", 1.0, 0.0..=3.0)
    .tag("particles")
    .size(320, 180)
    .ticks(18)
}

/// Premultiplied smoke puffs of `color` sorted by `sort`: `n` of `size` in `shape`.
fn smoke_puffs(color: Vec4, sort: SortMode, shape: EmitShape, n: u32, size: (f32, f32)) -> ParticleEffect {
    ParticleEffect::new("sorting puffs").layer(ParticleLayer {
        lod: false,
        shape,
        size: FloatRange::new(size.0, size.1),
        rotation: FloatRange::new(0.0, std::f32::consts::TAU),
        color_over_life: Gradient::constant(color),
        start_color: ColorRange {
            a: [0.8, 0.8, 0.8, 1.0],
            b: [1.2, 1.2, 1.2, 1.0],
        },
        coverage: FloatRange::new(0.8, 0.95),
        render: SpriteRender {
            shape: SpriteShape::Smoke {
                breakup: 0.5,
                hot_core: 0.0,
            },
            blend: ParticleBlend::Premultiplied,
            sort,
            ..SpriteRender::default()
        },
        ..ParticleLayer::burst(n, 100.0)
    })
}

fn sorting() -> Story {
    Story::new("Engine/Particles/Features/Sorting", |app, _| {
        plugin(app, ParticleSettings::default());
        app.world.spawn((
            Name("camera".into()),
            Transform::from_xyz(0.0, 18.0, 88.0).looking_at(Vec3::ZERO, Vec3::Y),
            Camera3d::default().with_clear_color(Vec4::new(0.05, 0.06, 0.09, 1.0)),
        ));
        // A warm core inside a cool ring of smoke, twice: two layers sorted per emitter (left),
        // and one layer sorted per particle (right: both emitters in one sorted group).
        let warm = Vec4::new(0.6, 0.24, 0.06, 1.0);
        let cool = Vec4::new(0.14, 0.24, 0.4, 1.0);
        let ring = EmitShape::Ring {
            radius: 17.0,
            width: 3.0,
            arc: std::f32::consts::TAU,
        };
        let core = EmitShape::Sphere {
            radius: 8.0,
            inner: 0.0,
        };
        for (x, sort) in [(-28.0, SortMode::ByEmitter), (28.0, SortMode::ByDistance)] {
            let effects = app.resources.expect_mut::<ParticleEffects>();
            let (a, b) = (
                effects
                    .add(smoke_puffs(warm, sort, core, 80, (7.0, 10.0)))
                    .expect("valid"),
                effects
                    .add(smoke_puffs(cool, sort, ring, 140, (5.0, 7.0)))
                    .expect("valid"),
            );
            app.spawn_effect(a, EffectSpawn::at(Vec3::new(x, 0.0, 0.0)).seed(81).keep());
            app.spawn_effect(b, EffectSpawn::at(Vec3::new(x, 0.0, 0.0)).seed(82).keep());
        }
    })
    .description(
        "Per-particle sorting (`SortMode::ByDistance`): a warm core of premultiplied smoke inside \
         a cool ring of it, seen from above the ring's plane. Left, sorted per emitter \
         (`ByEmitter`, the default for alpha layers): each emitter draws as a whole, so the ring \
         covers the core also where it passes behind it. Right, `ByDistance`: every view draws \
         the layer's particles farthest first (on the CPU for direct draws, with a bitonic sort \
         on the GPU backend), so the ring passes in front of the core and behind it. The \
         built-ins sort their lingering smoke this way.",
    )
    .tag("particles")
    .size(320, 180)
    .ticks(2)
}

fn hdr_bloom() -> Story {
    Story::new("Engine/Particles/Features/HDR Bloom", |app, k| {
        plugin(app, ParticleSettings::default());
        let cam = front_camera(app, 100.0);
        let tonemapper = match k.choice("tonemapper") {
            1 => Tonemapper::AgXPunchy,
            2 => Tonemapper::AcesFitted,
            _ => Tonemapper::AgX,
        };
        let mut post = PostProcess::default()
            .with_tonemapper(tonemapper)
            .with_exposure(k.f32("exposure"));
        if !k.bool("bloom") {
            post = post.without_bloom();
        }
        let _ = app.world.insert_one(cam, post);
        // Rows of sprites whose peak radiance doubles from column to column: 0.25 … 16
        // (`Bloom::glow()` starts at 1 with a knee from 0.5).
        let row = |color: Vec4, size: f32, core: f32| ParticleLayer {
            lod: false,
            size: FloatRange::constant(size),
            color_over_life: Gradient::constant(color),
            render: SpriteRender::additive(SpriteShape::Glow { core, hardness: 0.0 }),
            ..ParticleLayer::burst(1, 100.0)
        };
        let rows = [
            (26.0, row(Vec4::new(1.0, 0.45, 0.12, 1.0), 3.0, 0.0)),
            (5.0, row(Vec4::new(0.15, 0.55, 1.0, 1.0), 14.0, 0.0)),
            (-22.0, row(Vec4::new(1.0, 0.5, 0.15, 1.0), 16.0, 2.0)),
        ];
        for (r, (y, layer)) in rows.into_iter().enumerate() {
            let fx = app
                .resources
                .expect_mut::<ParticleEffects>()
                .add(ParticleEffect::new("hdr row").layer(layer))
                .expect("valid");
            for i in 0..7 {
                let radiance = 0.25 * 2f32.powi(i);
                app.spawn_effect(
                    fx,
                    EffectSpawn::at(Vec3::new(-72.0 + 24.0 * i as f32, y, 0.0))
                        .tint(Vec4::new(radiance, radiance, radiance, 1.0))
                        .seed(90 + 10 * r as u32 + i as u32)
                        .keep(),
                );
            }
        }
    })
    .description(
        "HDR intensity calibration against `Bloom::glow()` (threshold 1, knee 0.5, additive \
         0.35) and AgX: sprites whose peak radiance doubles from column to column, 0.25, 0.5, 1, \
         2, 4, 8 and 16 — small sparks (top), wide glows (middle) and glows with a white-hot \
         core of 2× (bottom). Below the knee (0.5) nothing blooms; from 1 the glow grows with \
         the energy above the threshold whatever the sprite's size, and from about 4 a sprite \
         reads white-hot: the built-ins author bodies below 1, what should glow at 2–5 and \
         cores, sparks and flashes at 5–16. `bloom`, `tonemapper` and `exposure` to compare.",
    )
    .knob_choice("tonemapper", &LOOKS, 0)
    .knob_bool("bloom", true)
    .knob_f32("exposure", 0.0, -2.0..=2.0)
    .tag("particles")
    .size(320, 180)
    .ticks(2)
}

fn stress_battle() -> Story {
    Story::new("Engine/Particles/Stress/Battle", |app, k| {
        let quality = match k.choice("quality") {
            0 => GraphicsQuality::Low,
            1 => GraphicsQuality::Medium,
            _ => GraphicsQuality::High,
        };
        plugin(app, ParticleSettings::for_quality(quality));
        lights(app);
        let pitch = 70f32.to_radians();
        let cam = app.world.spawn((
            Name("camera".into()),
            Transform::from_xyz(0.0, 432.0 * pitch.sin(), 432.0 * pitch.cos()).looking_at(Vec3::ZERO, Vec3::Y),
            Camera3d::default().with_clear_color(SPACE),
        ));
        let _ = app.world.insert_one(
            cam,
            PostProcess::default()
                .with_tonemapper(Tonemapper::AgXPunchy)
                .with_exposure(0.5),
        );
        space3d_particles::testing::battle(app, space3d_particles::testing::Battle::default());
    })
    .description(
        "The particle load of the graphics spec's Battle reference scene (`testing::battle`), \
         2 s in, through the game's battle camera and the recommended battle look (AgX punchy, \
         +0.5 EV): 300 ships with three engine exhausts each (20 of them boosting), a laser bolt \
         with its trail every tick, missiles trailing smoke into explosions, held lasers and \
         repair beams, 30 explosions a second, hit sparks and shield ripples, debris, warps and \
         EMPs — 100k particles at `quality` High. Lower tiers \
         (`ParticleSettings::for_quality`) thin the off-screen emitters and the spawn counts to \
         their budgets (Low: the CPU backend's 16k) without dropping what is in view.",
    )
    .knob_choice("quality", &["Low", "Medium", "High"], 2)
    .tag("particles")
    .tag("perf")
    .size(320, 180)
    .ticks(120)
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
        circling_ship(app, 90.0, 0.03, 1.2, ION_BLUE);
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
