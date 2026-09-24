//! {{title}} — a 3D physics game built with [Space3d](https://github.com/SpaceCorps/Space3d).
//!
//! Roll the ball with the arrow keys / WASD (Space jumps) and knock the crates off the arena.
//! Physics (rapier) runs on the deterministic fixed tick, so the same inputs always give the
//! same result — see `tests/input.rs`.
//!
//! * [`build`] — engine plugins + [`GamePlugin`] (used by `src/main.rs` and `mobile/`).
//! * [`GamePlugin`] — only the gameplay, so tests and stories can run it headless.

use space3d::prelude::*;

pub mod stories;

/// The ball the player rolls.
pub struct Ball;

/// A crate that scores a point when it falls off the arena.
pub struct Crate;

/// Follows the ball.
pub struct FollowCamera {
    /// Offset from the ball.
    pub offset: Vec3,
}

/// Game state.
#[derive(Default, Debug)]
pub struct Score {
    /// Crates knocked off the arena.
    pub crates: u32,
    /// Times the ball fell off.
    pub falls: u32,
}

/// Jump requested this frame (set in `Update`, consumed on the next fixed tick).
#[derive(Default)]
struct JumpRequest(bool);

/// Top speed on the ground (m/s).
pub const MAX_SPEED: f32 = 6.0;
/// How quickly the ball reaches `MAX_SPEED` (1/s).
pub const ACCELERATION: f32 = 8.0;
/// Upward speed of a jump (m/s).
pub const JUMP_SPEED: f32 = 5.5;
/// Half size of the square arena.
pub const ARENA: f32 = 8.0;
/// Where the ball starts (and respawns).
pub const BALL_START: Vec3 = Vec3::new(0.0, 0.5, 5.0);

/// Names of the input actions (bound in [`bind_controls`]).
pub mod actions {
    /// Roll towards −x.
    pub const LEFT: &str = "left";
    /// Roll towards +x.
    pub const RIGHT: &str = "right";
    /// Roll away from the camera.
    pub const FORWARD: &str = "forward";
    /// Roll towards the camera.
    pub const BACK: &str = "back";
    /// Jump.
    pub const JUMP: &str = "jump";
    /// Quit.
    pub const QUIT: &str = "quit";
}

/// Configure the complete game: engine plugins + [`GamePlugin`].
pub fn build(app: &mut App) {
    app.add_plugin(DefaultPlugins::new().window(WindowPlugin {
        title: "{{title}}".into(),
        canvas_id: Some("space3d".into()),
        ..WindowPlugin::default()
    }));
    app.add_plugin(GamePlugin);
}

/// What `space3d editor` (`src/bin/editor.rs`) opens: the game's scene, paused, ready to edit.
pub fn editor_scene(app: &mut App) {
    app.add_plugin(GamePlugin);
}

/// The gameplay.
pub struct GamePlugin;

impl Plugin for GamePlugin {
    fn build(&self, app: &mut App) {
        app.add_plugin(InputPlugin);
        app.add_plugin(PhysicsPlugin::default());
        bind_controls(app.resources.expect_mut::<ActionMap>());
        app.init_resource::<Score>();
        app.init_resource::<JumpRequest>();
        app.add_system(Stage::Startup, setup_scene);
        app.add_system(Stage::Update, read_jump);
        app.add_system(Stage::FixedUpdate, roll_ball);
        app.add_system(Stage::FixedUpdate, fall_off);
        app.add_system(Stage::Update, follow_camera);
        app.add_system(Stage::Update, quit_on_escape);
    }
}

/// Keyboard + touch bindings (screen edges act as a d-pad, the centre jumps).
pub fn bind_controls(map: &mut ActionMap) {
    let region = |min: (f32, f32), max: (f32, f32)| Binding::TouchRegion {
        min: Vec2::new(min.0, min.1),
        max: Vec2::new(max.0, max.1),
    };
    map.bind(actions::LEFT, Binding::Key(KeyCode::ArrowLeft))
        .bind(actions::LEFT, Binding::Key(KeyCode::KeyA))
        .bind(actions::LEFT, region((0.0, 0.25), (0.25, 0.75)))
        .bind(actions::RIGHT, Binding::Key(KeyCode::ArrowRight))
        .bind(actions::RIGHT, Binding::Key(KeyCode::KeyD))
        .bind(actions::RIGHT, region((0.75, 0.25), (1.0, 0.75)))
        .bind(actions::FORWARD, Binding::Key(KeyCode::ArrowUp))
        .bind(actions::FORWARD, Binding::Key(KeyCode::KeyW))
        .bind(actions::FORWARD, region((0.25, 0.0), (0.75, 0.25)))
        .bind(actions::BACK, Binding::Key(KeyCode::ArrowDown))
        .bind(actions::BACK, Binding::Key(KeyCode::KeyS))
        .bind(actions::BACK, region((0.25, 0.75), (0.75, 1.0)))
        .bind(actions::JUMP, Binding::Key(KeyCode::Space))
        .bind(actions::JUMP, region((0.25, 0.25), (0.75, 0.75)))
        .bind(actions::QUIT, Binding::Key(KeyCode::Escape));
}

/// Positions of the crate stacks: a 3×3 block, two layers high.
pub fn crate_positions() -> Vec<Vec3> {
    let mut out = Vec::new();
    for layer in 0..2 {
        for i in 0..3 {
            for j in 0..3 {
                out.push(Vec3::new(i as f32 - 1.0, 0.5 + layer as f32, j as f32 - 2.0));
            }
        }
    }
    out
}

/// A transform at `p`.
fn at(p: Vec3) -> Transform {
    Transform::from_xyz(p.x, p.y, p.z)
}

/// Arena, ball, crates, lights and camera.
pub fn setup_scene(ctx: &mut SystemCtx) {
    let (cube, sphere) = {
        let assets = ctx.resources.expect_mut::<RenderAssets>();
        (
            assets.add_mesh(MeshData::cube(1.0)),
            assets.add_mesh(MeshData::sphere(0.5, 32, 20)),
        )
    };
    // Arena: a fixed slab whose top is at y = 0. Scale is baked into the collider.
    ctx.world.spawn((
        Name("arena".into()),
        Transform::from_xyz(0.0, -0.5, 0.0).with_scale(Vec3::new(2.0 * ARENA, 1.0, 2.0 * ARENA)),
        Mesh3d(cube),
        Material::srgb(0.3, 0.34, 0.4).with_metallic_roughness(0.0, 0.85),
        RigidBody::Fixed,
        Collider::cuboid(Vec3::ONE),
    ));
    ctx.world.spawn((
        Name("ball".into()),
        Ball,
        at(BALL_START),
        Mesh3d(sphere),
        Material::srgb(0.2, 0.55, 1.0).with_metallic_roughness(0.7, 0.25),
        RigidBody::Dynamic,
        Collider::ball(0.5),
        Velocity::default(),
        Friction(1.0),
    ));
    for (i, p) in crate_positions().into_iter().enumerate() {
        let shade = 0.75 + 0.05 * (i % 4) as f32;
        ctx.world.spawn((
            Crate,
            at(p).with_scale(Vec3::splat(0.95)),
            Mesh3d(cube),
            Material::srgb(0.85 * shade, 0.6 * shade, 0.3 * shade).with_metallic_roughness(0.0, 0.7),
            RigidBody::Dynamic,
            Collider::cuboid(Vec3::ONE),
        ));
    }
    ctx.world.spawn((
        Transform::default().looking_at(Vec3::new(-0.4, -1.0, -0.25), Vec3::Y),
        DirectionalLight {
            color: Vec3::new(1.0, 0.96, 0.9),
            intensity: 2.6,
        },
    ));
    ctx.world.spawn((
        Transform::from_xyz(0.0, 3.0, -2.0),
        PointLight {
            color: Vec3::new(1.0, 0.6, 0.3),
            intensity: 8.0,
            range: 10.0,
        },
    ));
    ctx.resources.insert(AmbientLight {
        color: Vec3::new(0.65, 0.75, 1.0),
        intensity: 0.2,
    });
    let offset = Vec3::new(0.0, 6.0, 9.0);
    ctx.world.spawn((
        Name("camera".into()),
        at(BALL_START + offset).looking_at(BALL_START, Vec3::Y),
        Camera3d::default().with_clear_color(Vec4::new(0.05, 0.06, 0.1, 1.0)),
        FollowCamera { offset },
    ));
}

fn read_jump(ctx: &mut SystemCtx) {
    if ctx.resources.expect::<ActionState>().just_pressed(actions::JUMP) {
        ctx.resources.expect_mut::<JumpRequest>().0 = true;
    }
}

/// Steer the ball by changing its velocity; physics does the rest.
pub fn roll_ball(ctx: &mut SystemCtx) {
    let dt = ctx.time().fixed_delta.as_secs_f32();
    let (x, z) = {
        let a = ctx.resources.expect::<ActionState>();
        (
            a.axis(actions::LEFT, actions::RIGHT),
            a.axis(actions::FORWARD, actions::BACK),
        )
    };
    let jump = std::mem::take(&mut ctx.resources.expect_mut::<JumpRequest>().0);
    let target = Vec3::new(x, 0.0, z).normalize_or_zero() * MAX_SPEED;
    let mut ball = None;
    for (entity, transform, velocity, _) in ctx.world.query_mut::<(Entity, &Transform, &mut Velocity, &Ball)>() {
        let horizontal = Vec3::new(velocity.linear.x, 0.0, velocity.linear.z);
        let blend = (ACCELERATION * dt).min(1.0);
        let new = horizontal.lerp(target, blend);
        velocity.linear.x = new.x;
        velocity.linear.z = new.z;
        ball = Some((entity, transform.translation));
    }
    if jump && let Some((entity, position)) = ball {
        let physics = ctx.resources.expect::<Physics>();
        let filter = RayFilter {
            exclude: Some(entity),
            ..Default::default()
        };
        let grounded = physics.raycast_with(position, Vec3::NEG_Y, 0.6, filter).is_some();
        if grounded && let Ok(mut velocity) = ctx.world.get::<&mut Velocity>(entity) {
            velocity.linear.y = JUMP_SPEED;
        }
    }
}

/// Count crates that fell off; put the ball back when it falls.
pub fn fall_off(ctx: &mut SystemCtx) {
    let fallen: Vec<Entity> = ctx
        .world
        .query::<(Entity, &Transform, &Crate)>()
        .iter()
        .filter(|(_, t, _)| t.translation.y < -5.0)
        .map(|(e, _, _)| e)
        .collect();
    for e in &fallen {
        let _ = ctx.world.despawn(*e);
    }
    let mut ball_fell = false;
    for (transform, velocity, _) in ctx.world.query_mut::<(&mut Transform, &mut Velocity, &Ball)>() {
        if transform.translation.y < -10.0 {
            *transform = at(BALL_START);
            *velocity = Velocity::default();
            ball_fell = true;
        }
    }
    let score = ctx.resources.expect_mut::<Score>();
    score.crates += fallen.len() as u32;
    score.falls += u32::from(ball_fell);
}

/// Smoothly follow the ball.
pub fn follow_camera(ctx: &mut SystemCtx) {
    let dt = ctx.time().delta_secs();
    let Some(ball) = ctx
        .world
        .query::<(&Transform, &Ball)>()
        .iter()
        .next()
        .map(|(t, _)| t.translation)
    else {
        return;
    };
    for (transform, follow) in ctx.world.query_mut::<(&mut Transform, &FollowCamera)>() {
        let wanted = ball + follow.offset;
        let eye = transform.translation.lerp(wanted, (4.0 * dt).min(1.0));
        *transform = at(eye).looking_at(ball, Vec3::Y);
    }
}

/// Esc quits.
pub fn quit_on_escape(ctx: &mut SystemCtx) {
    if ctx.resources.expect::<ActionState>().just_pressed(actions::QUIT) {
        ctx.send(AppExit);
    }
}
