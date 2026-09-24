//! {{title}} — a [Space3d](https://github.com/SpaceCorps/Space3d) game.
//!
//! * [`build`] configures the whole app: engine plugins (window, renderer, input, audio, …)
//!   plus [`GamePlugin`]. `space3d::main!` in `src/main.rs` and `mobile/` call it.
//! * [`GamePlugin`] is only the gameplay, so tests and stories can run it headless.

use space3d::prelude::*;

pub mod stories;

/// The cube the player moves.
pub struct Player;

/// Decorative cube spinning at this many radians per second.
pub struct Spin(pub f32);

/// Player speed in units per second.
pub const PLAYER_SPEED: f32 = 3.0;

/// The player stays within `±ARENA` on the x and z axes.
pub const ARENA: f32 = 4.0;

/// Names of the input actions (bound in [`bind_controls`]).
pub mod actions {
    /// Move towards −x.
    pub const LEFT: &str = "left";
    /// Move towards +x.
    pub const RIGHT: &str = "right";
    /// Move away from the camera (−z).
    pub const FORWARD: &str = "forward";
    /// Move towards the camera (+z).
    pub const BACK: &str = "back";
    /// Quit the game.
    pub const QUIT: &str = "quit";
}

/// Configure the complete game: engine plugins + [`GamePlugin`].
pub fn build(app: &mut App) {
    app.add_plugin(DefaultPlugins::new().window(WindowPlugin {
        title: "{{title}}".into(),
        // The web build renders into <canvas id="space3d"> (web/index.html).
        canvas_id: Some("space3d".into()),
        ..WindowPlugin::default()
    }));
    app.add_plugin(GamePlugin);
}

/// What `space3d editor` (`src/bin/editor.rs`) opens: the game's scene, paused, ready to edit.
pub fn editor_scene(app: &mut App) {
    app.add_plugin(GamePlugin);
}

/// The gameplay: controls, scene and systems.
pub struct GamePlugin;

impl Plugin for GamePlugin {
    fn build(&self, app: &mut App) {
        app.add_plugin(InputPlugin); // no-op when the engine plugins already added it
        bind_controls(app.resources.expect_mut::<ActionMap>());
        app.add_system(Stage::Startup, setup_scene);
        // Gameplay runs on the deterministic fixed tick (60 Hz by default).
        app.add_system(Stage::FixedUpdate, move_player);
        app.add_system(Stage::FixedUpdate, spin);
        app.add_system(Stage::Update, quit_on_escape);
    }
}

/// Keyboard + touch bindings. Touching the left/right/top/bottom edge of the screen acts as a
/// virtual d-pad on phones.
pub fn bind_controls(map: &mut ActionMap) {
    let edge = |min: (f32, f32), max: (f32, f32)| Binding::TouchRegion {
        min: Vec2::new(min.0, min.1),
        max: Vec2::new(max.0, max.1),
    };
    map.bind(actions::LEFT, Binding::Key(KeyCode::ArrowLeft))
        .bind(actions::LEFT, Binding::Key(KeyCode::KeyA))
        .bind(actions::LEFT, edge((0.0, 0.25), (0.25, 0.75)))
        .bind(actions::RIGHT, Binding::Key(KeyCode::ArrowRight))
        .bind(actions::RIGHT, Binding::Key(KeyCode::KeyD))
        .bind(actions::RIGHT, edge((0.75, 0.25), (1.0, 0.75)))
        .bind(actions::FORWARD, Binding::Key(KeyCode::ArrowUp))
        .bind(actions::FORWARD, Binding::Key(KeyCode::KeyW))
        .bind(actions::FORWARD, edge((0.25, 0.0), (0.75, 0.25)))
        .bind(actions::BACK, Binding::Key(KeyCode::ArrowDown))
        .bind(actions::BACK, Binding::Key(KeyCode::KeyS))
        .bind(actions::BACK, edge((0.25, 0.75), (0.75, 1.0)))
        .bind(actions::QUIT, Binding::Key(KeyCode::Escape));
}

/// Ground, player, props, lights and camera.
pub fn setup_scene(ctx: &mut SystemCtx) {
    let (cube, ground) = {
        let assets = ctx.resources.expect_mut::<RenderAssets>();
        (
            assets.add_mesh(MeshData::cube(1.0)),
            assets.add_mesh(MeshData::plane(2.0 * ARENA + 2.0)),
        )
    };
    ctx.world.spawn((
        Name("ground".into()),
        Transform::default(),
        Mesh3d(ground),
        Material::srgb(0.32, 0.36, 0.42).with_metallic_roughness(0.0, 0.9),
    ));
    ctx.world.spawn((
        Name("player".into()),
        Player,
        Transform::from_xyz(0.0, 0.5, 0.0),
        Mesh3d(cube),
        Material::srgb(0.95, 0.45, 0.2).with_metallic_roughness(0.1, 0.35),
    ));
    for (i, (x, z)) in [(-3.0, -3.0), (3.0, -3.0), (0.0, -4.5)].into_iter().enumerate() {
        ctx.world.spawn((
            Name(format!("prop {i}")),
            Transform::from_xyz(x, 0.75, z).with_scale(Vec3::splat(0.5)),
            Mesh3d(cube),
            Material::srgb(0.25, 0.55 + 0.15 * i as f32, 0.95).with_metallic_roughness(0.6, 0.3),
            Spin(0.8 + 0.4 * i as f32),
        ));
    }
    ctx.world.spawn((
        Transform::default().looking_at(Vec3::new(-0.4, -1.0, -0.3), Vec3::Y),
        DirectionalLight {
            color: Vec3::new(1.0, 0.96, 0.9),
            intensity: 2.5,
        },
    ));
    ctx.resources.insert(AmbientLight {
        color: Vec3::new(0.7, 0.8, 1.0),
        intensity: 0.2,
    });
    ctx.world.spawn((
        Name("camera".into()),
        Transform::from_xyz(0.0, 6.0, 8.0).looking_at(Vec3::new(0.0, 0.0, -0.5), Vec3::Y),
        Camera3d::default().with_clear_color(Vec4::new(0.05, 0.06, 0.09, 1.0)),
    ));
}

/// Move the player from the input actions (fixed tick → deterministic).
pub fn move_player(ctx: &mut SystemCtx) {
    let dt = ctx.time().fixed_delta.as_secs_f32();
    let actions = ctx.resources.expect::<ActionState>();
    let dir = Vec3::new(
        actions.axis(actions::LEFT, actions::RIGHT),
        0.0,
        actions.axis(actions::FORWARD, actions::BACK),
    );
    if dir == Vec3::ZERO {
        return;
    }
    let step = dir.normalize() * PLAYER_SPEED * dt;
    for (transform, _) in ctx.world.query_mut::<(&mut Transform, &Player)>() {
        let p = transform.translation + step;
        transform.translation = Vec3::new(p.x.clamp(-ARENA, ARENA), p.y, p.z.clamp(-ARENA, ARENA));
    }
}

/// Rotate the props.
pub fn spin(ctx: &mut SystemCtx) {
    let dt = ctx.time().fixed_delta.as_secs_f32();
    for (transform, spin) in ctx.world.query_mut::<(&mut Transform, &Spin)>() {
        transform.rotation = (Quat::from_rotation_y(spin.0 * dt) * transform.rotation).normalize();
    }
}

/// Esc (or the Back button) quits.
pub fn quit_on_escape(ctx: &mut SystemCtx) {
    if ctx.resources.expect::<ActionState>().just_pressed(actions::QUIT) {
        ctx.send(AppExit);
    }
}
