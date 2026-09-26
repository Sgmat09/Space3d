//! The particle library in a small space battle, seen from above through the recommended battle
//! look (`PostProcess` with AgX punchy, half a stop up, bloom): two wings of ships circling with
//! engine exhausts (and afterburners on the fast ones), hitscan laser shots with hit sparks and
//! shield ripples, a held laser beam, a repair beam, missiles with smoke trails that end in
//! explosions and debris, an EMP, and a ship warping in and out. Every effect is one
//! `spawn_effect` call or one `ParticleEmitter`; everything is driven by the fixed tick.
//!
//! ```text
//! cargo run -p particles
//! SPACE3D_EXIT_AFTER_FRAMES=300 cargo run -p particles   # smoke test: quit after 300 frames
//! SPACE3D_HEADLESS=1 SPACE3D_EXIT_AFTER_FRAMES=60 cargo run -p particles   # no window (CI)
//! ```
//! Controls: `Space` drops an explosion at the center, `E` fires an EMP, `Esc` quits. The
//! particle stats are printed every two seconds.

use space3d::particles::prelude::*;
use space3d::prelude::*;

/// A ship flying a circle: `radius`, angular `speed` (rad per tick), phase.
struct Orbit {
    radius: f32,
    speed: f32,
    phase: f32,
    center: Vec3,
}

/// A missile flying toward `target` at `speed` units per second.
struct Missile {
    target: Entity,
    speed: f32,
}

/// The battle's ships (the two wings) and the held beams' entities.
#[derive(Default)]
struct Battle {
    blue: Vec<Entity>,
    red: Vec<Entity>,
    report_at: f64,
}

const BLUE: Vec4 = Vec4::new(0.2, 0.55, 1.0, 1.0);
const RED: Vec4 = Vec4::new(1.0, 0.12, 0.06, 1.0);

fn build(app: &mut App) {
    app.add_plugin(DefaultPlugins::new().title("Space3d — particles"));
    add_game(app);
}

/// The scene and its systems, without the engine plugins (the tests add them to a headless app).
fn add_game(app: &mut App) {
    app.add_plugin(ParticlesPlugin::default());
    app.init_resource::<Battle>();
    app.add_system(Stage::Startup, setup);
    app.add_system(Stage::FixedUpdate, fly);
    app.add_system(Stage::FixedUpdate, fight);
    app.add_system(Stage::Update, controls);
    app.add_system(Stage::Update, report);
}

/// A 40-unit hull (facing −Z) of three boxes.
fn hull(ctx: &mut SystemCtx, cube: MeshHandle, t: Transform, paint: Vec3) -> Entity {
    let ship = ctx.world.spawn((t,));
    for (offset, scale, color) in [
        (Vec3::ZERO, Vec3::new(12.0, 6.0, 40.0), paint),
        (Vec3::new(0.0, 0.0, 8.0), Vec3::new(36.0, 2.0, 12.0), paint * 0.7),
        (
            Vec3::new(0.0, 3.5, -6.0),
            Vec3::new(5.0, 3.0, 9.0),
            Vec3::new(0.1, 0.25, 0.35),
        ),
    ] {
        ctx.world.spawn((
            Transform::from_xyz(offset.x, offset.y, offset.z).with_scale(scale),
            Mesh3d(cube),
            Material::srgb(color.x, color.y, color.z).with_metallic_roughness(0.6, 0.45),
            Parent(ship),
        ));
    }
    ship
}

/// Engines on both nozzles of `ship`: the exhaust, and an afterburner when `boost` > 0.
fn engines(ctx: &mut SystemCtx, ship: Entity, color: Vec4, boost: f32) {
    let flame = color * Vec4::new(2.0, 2.0, 2.0, 1.0);
    for x in [-6.0, 6.0] {
        let at = Transform::from_xyz(x, 0.0, 21.0);
        ctx.spawn_effect(
            builtin::ENGINE_EXHAUST,
            EffectSpawn::from_transform(at).follow(ship).tint(flame).keep(),
        );
        if boost > 0.0 {
            let e = ctx.spawn_effect(
                builtin::ENGINE_AFTERBURNER,
                EffectSpawn::from_transform(at).follow(ship).tint(flame).keep(),
            );
            if let Ok(mut em) = ctx.world.get::<&mut ParticleEmitter>(e) {
                em.rate_scale = boost;
            }
        }
    }
}

fn setup(ctx: &mut SystemCtx) {
    let cube = ctx.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.0));
    ctx.world.spawn((
        Transform::from_xyz(0.0, 520.0, 300.0).looking_at(Vec3::ZERO, Vec3::Y),
        Camera3d::default().with_clear_color(Vec4::new(0.004, 0.005, 0.012, 1.0)),
        PostProcess::default()
            .with_tonemapper(Tonemapper::AgXPunchy)
            .with_exposure(0.5),
    ));
    ctx.world.spawn((
        Transform::IDENTITY.looking_at(Vec3::new(-0.4, -1.0, -0.5), Vec3::Y),
        DirectionalLight {
            color: Vec3::new(1.0, 0.96, 0.9),
            intensity: 2.0,
        },
    ));
    ctx.resources.insert(AmbientLight {
        color: Vec3::new(0.6, 0.7, 1.0),
        intensity: 0.1,
    });
    let mut battle = Battle::default();
    for i in 0..4 {
        let orbit = Orbit {
            radius: 170.0 + 30.0 * i as f32,
            speed: 0.012 - 0.002 * i as f32,
            phase: i as f32 * 1.4,
            center: Vec3::new(-120.0, 0.0, 0.0),
        };
        let ship = hull(ctx, cube, Transform::IDENTITY, Vec3::new(0.25, 0.3, 0.4));
        let _ = ctx.world.insert_one(ship, orbit);
        engines(ctx, ship, BLUE, if i == 0 { 1.2 } else { 0.0 });
        battle.blue.push(ship);
    }
    for i in 0..4 {
        let orbit = Orbit {
            radius: 150.0 + 35.0 * i as f32,
            speed: -0.011 + 0.0015 * i as f32,
            phase: 0.8 + i as f32 * 1.6,
            center: Vec3::new(120.0, 0.0, 0.0),
        };
        let ship = hull(ctx, cube, Transform::IDENTITY, Vec3::new(0.4, 0.28, 0.24));
        let _ = ctx.world.insert_one(ship, orbit);
        engines(ctx, ship, RED, if i == 1 { 1.0 } else { 0.0 });
        battle.red.push(ship);
    }
    // A held laser (red wing's flagship on the blue flagship) and a repair beam in the blue wing.
    ctx.spawn_effect(
        builtin::LASER_BEAM,
        EffectSpawn::from_transform(Transform::from_xyz(0.0, 3.0, -21.0))
            .follow(battle.red[0])
            .beam_to(BeamEnd::Entity(battle.blue[0]))
            .tint(RED)
            .keep(),
    );
    ctx.spawn_effect(
        builtin::REPAIR_BEAM,
        EffectSpawn::from_transform(Transform::from_xyz(0.0, 4.0, -12.0))
            .follow(battle.blue[2])
            .beam_to(BeamEnd::Entity(battle.blue[0]))
            .keep(),
    );
    ctx.resources.insert(battle);
}

/// Ships fly their circles; missiles home in on their targets and explode on arrival.
fn fly(ctx: &mut SystemCtx) {
    let tick = ctx.time().tick as f32;
    for (t, o) in ctx.world.query_mut::<(&mut Transform, &Orbit)>() {
        let a = o.phase + o.speed * tick;
        t.translation = o.center + Vec3::new(a.cos(), 0.0, a.sin()) * o.radius;
        let dir = Vec3::new(-a.sin(), 0.0, a.cos()) * o.speed.signum();
        t.rotation = Quat::from_rotation_arc(Vec3::NEG_Z, dir);
    }
    let dt = ctx.time().fixed_delta_secs();
    let mut hits = Vec::new();
    for (e, t, m) in ctx.world.query::<(Entity, &Transform, &Missile)>().iter() {
        let Ok(target) = ctx.world.get::<&Transform>(m.target).map(|t| t.translation) else {
            hits.push((e, t.translation, None));
            continue;
        };
        let to = target - t.translation;
        if to.length() < m.speed * dt * 1.5 {
            hits.push((e, target, Some(to)));
        } else {
            hits.push((e, t.translation + to.normalize() * m.speed * dt, None));
        }
    }
    for (e, p, hit) in hits {
        match hit {
            Some(dir) => {
                let _ = ctx.world.despawn(e);
                ctx.spawn_effect(builtin::EXPLOSION, EffectSpawn::at(p).scale(0.7));
                ctx.spawn_effect(builtin::DEBRIS_SPARKS, EffectSpawn::at(p).facing(-dir));
            }
            None => {
                if let Ok(mut t) = ctx.world.get::<&mut Transform>(e) {
                    let dir = p - t.translation;
                    t.translation = p;
                    if let Some(d) = dir.try_normalize() {
                        t.rotation = Quat::from_rotation_arc(Vec3::NEG_Z, d);
                    }
                }
            }
        }
    }
}

/// The battle's schedule: shots, missiles, a warp and an EMP, deterministic from the tick.
fn fight(ctx: &mut SystemCtx) {
    let tick = ctx.time().tick;
    let battle = ctx.resources.expect::<Battle>();
    let (blue, red) = (battle.blue.clone(), battle.red.clone());
    let pos = |ctx: &SystemCtx, e: Entity| ctx.world.get::<&Transform>(e).map(|t| t.translation).ok();
    // A hitscan shot every 7 ticks, alternating sides: hull or shield hits.
    if tick.is_multiple_of(7) {
        let k = (tick / 7) as usize;
        let (from, to, tint) = if k.is_multiple_of(2) {
            (blue[k % 4], red[(k / 2) % 4], BLUE)
        } else {
            (red[k % 4], blue[(k / 2) % 4], RED)
        };
        if let (Some(a), Some(b)) = (pos(ctx, from), pos(ctx, to)) {
            let dir = (b - a).normalize_or(Vec3::X);
            let muzzle = a + dir * 22.0;
            if k.is_multiple_of(3) {
                // Stopped by the shield.
                let hit = b - dir * 30.0;
                ctx.spawn_effect(
                    builtin::LASER_SHOT,
                    EffectSpawn::at(muzzle).beam_to(BeamEnd::World(hit)).tint(tint),
                );
                ctx.spawn_effect(
                    builtin::SHIELD_RIPPLE,
                    EffectSpawn::at(Vec3::ZERO).facing(-dir).scale(1.5).follow(to),
                );
            } else {
                let hit = b - dir * 8.0;
                ctx.spawn_effect(
                    builtin::LASER_SHOT,
                    EffectSpawn::at(muzzle).beam_to(BeamEnd::World(hit)).tint(tint),
                );
                ctx.spawn_effect(builtin::HIT_SPARK, EffectSpawn::at(hit).facing(-dir).tint(tint));
            }
        }
    }
    // A missile every 90 ticks from the blue wing's last ship at a red ship.
    if tick % 90 == 45 {
        let target = red[(tick / 90) as usize % 4];
        if let Some(a) = pos(ctx, blue[3]) {
            let m = ctx
                .world
                .spawn((Transform::from_xyz(a.x, a.y, a.z), Missile { target, speed: 380.0 }));
            ctx.spawn_effect(builtin::MISSILE_TRAIL, EffectSpawn::at(Vec3::ZERO).follow(m));
        }
    }
    // A ship warps out and back in, and an EMP goes off, every 6 seconds.
    if tick % 360 == 200 {
        ctx.spawn_effect(builtin::WARP_OUT, EffectSpawn::at(Vec3::new(0.0, 0.0, -230.0)));
    }
    if tick % 360 == 290 {
        ctx.spawn_effect(builtin::WARP_IN, EffectSpawn::at(Vec3::new(0.0, 0.0, 230.0)));
    }
    if tick % 360 == 120 {
        ctx.spawn_effect(builtin::EMP, EffectSpawn::at(Vec3::new(0.0, 0.0, 0.0)).scale(1.2));
    }
}

fn controls(ctx: &mut SystemCtx) {
    let input = ctx.resources.expect::<Input>();
    let (boom, emp, quit) = (
        input.just_pressed(KeyCode::Space),
        input.just_pressed(KeyCode::KeyE),
        input.just_pressed(KeyCode::Escape),
    );
    if boom {
        ctx.spawn_effect(builtin::EXPLOSION, EffectSpawn::at(Vec3::ZERO).scale(1.5));
    }
    if emp {
        ctx.spawn_effect(builtin::EMP, EffectSpawn::at(Vec3::ZERO));
    }
    if quit {
        ctx.send(AppExit);
    }
}

/// Print the particle stats every two seconds.
fn report(ctx: &mut SystemCtx) {
    let now = ctx.time().elapsed_secs();
    let battle = ctx.resources.expect_mut::<Battle>();
    if now < battle.report_at {
        return;
    }
    battle.report_at = now + 2.0;
    if let Some(s) = ctx.resources.get::<ParticleStats>() {
        println!(
            "particles: {} effects, {} alive ({:?} backend), {} ribbon/beam segments, {} draws, {} dispatches",
            s.effects, s.alive, s.backend, s.ribbon_segments, s.draw_calls, s.dispatches
        );
    }
}

space3d::main!(build);

#[cfg(test)]
mod tests {
    use super::*;

    fn headless() -> App {
        let mut app = App::new();
        app.add_plugin(DefaultPlugins::headless(64, 64));
        add_game(&mut app);
        app
    }

    /// The battle runs: engines, beams and shots play; a missile reaches its target and turns
    /// into an explosion; one-shots finish and despawn; `Space` and `Esc` work.
    #[test]
    fn the_battle_plays() {
        let mut app = headless();
        app.step_ticks(240);
        let emitters = app.world.query::<&ParticleEmitter>().iter().count();
        assert!(emitters >= 20, "{emitters} emitters");
        let missiles = app.world.query::<&Missile>().iter().count();
        assert!(missiles <= 2, "missiles arrive and explode ({missiles} in flight)");
        app.resources.expect_mut::<Input>().inject_key_tap(KeyCode::Space);
        let before = app.world.query::<&ParticleEmitter>().iter().count();
        app.step_ticks(1);
        assert!(
            app.world.query::<&ParticleEmitter>().iter().count() > before,
            "Space: an explosion"
        );
        assert!(!app.should_exit());
        app.resources.expect_mut::<Input>().inject_key_tap(KeyCode::Escape);
        app.step_ticks(1);
        assert!(app.should_exit());
    }
}
