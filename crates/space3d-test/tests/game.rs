//! How a game built with Space3d tests itself.
//!
//! `MiniGame` stands in for a game crate: a player cube moved by named actions (arrow keys,
//! WASD, gamepad stick, right-half-of-screen touch) in `FixedUpdate`. The tests drive it with
//! simulated input for an exact number of ticks, then assert on the world, on a golden
//! screenshot (`tests/__screenshots__/mini_game/*.png`), on determinism and on budgets.

use space3d_core::prelude::*;
use space3d_platform::{ActionMap, ActionState, Binding, GamepadAxis, InputPlugin, KeyCode, TouchPhase};
use space3d_render::{Camera3d, DirectionalLight, Material, Mesh3d, RenderAssets, RenderPlugin};
use space3d_test::prelude::*;
use space3d_test::{InputEvent, world_hash};
use std::time::Duration;

// ---- the game ------------------------------------------------------------------------------

/// The controllable cube.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Player {
    /// Units per second.
    speed: f32,
}

/// Arena half size: the player is clamped to `-ARENA..=ARENA` on x and z.
const ARENA: f32 = 5.0;

struct MiniGame;

impl Plugin for MiniGame {
    fn build(&self, app: &mut App) {
        app.add_plugin(InputPlugin).add_plugin(RenderPlugin::default());
        app.resources
            .expect_mut::<ActionMap>()
            .bind("left", Binding::Key(KeyCode::ArrowLeft))
            .bind("left", Binding::Key(KeyCode::KeyA))
            .bind("right", Binding::Key(KeyCode::ArrowRight))
            .bind("right", Binding::Key(KeyCode::KeyD))
            .bind(
                "right",
                Binding::GamepadAxis {
                    axis: GamepadAxis::LeftStickX,
                    threshold: 0.2,
                },
            )
            .bind(
                "right",
                Binding::TouchRegion {
                    min: Vec2::new(0.5, 0.0),
                    max: Vec2::new(1.0, 1.0),
                },
            )
            .bind("up", Binding::Key(KeyCode::ArrowUp))
            .bind("down", Binding::Key(KeyCode::ArrowDown));

        let assets = app.resources.expect_mut::<RenderAssets>();
        let cube = assets.add_mesh(MeshData::cube(1.0));
        let floor = assets.add_mesh(MeshData::plane(ARENA * 2.0 + 1.0));
        app.world.spawn((
            Name("player".into()),
            Transform::from_xyz(0.0, 0.5, 0.0),
            Player { speed: 3.0 },
            Mesh3d(cube),
            Material::srgb(0.95, 0.55, 0.1),
        ));
        app.world
            .spawn((Transform::IDENTITY, Mesh3d(floor), Material::srgb(0.25, 0.3, 0.35)));
        app.world.spawn((
            Transform::from_xyz(0.0, 7.0, 7.0).looking_at(Vec3::ZERO, Vec3::Y),
            Camera3d::default().with_clear_color(Vec4::new(0.02, 0.02, 0.05, 1.0)),
        ));
        app.world.spawn((
            Transform::IDENTITY.looking_at(Vec3::new(-0.4, -1.0, -0.3), Vec3::Y),
            DirectionalLight {
                color: Vec3::ONE,
                intensity: 2.5,
            },
        ));
        app.add_system_named(Stage::FixedUpdate, "mini_game::move_player", move_player);
    }
}

fn move_player(ctx: &mut SystemCtx) {
    let dt = ctx.time().fixed_delta_secs();
    let actions = ctx.resources.expect::<ActionState>();
    let dir = Vec3::new(actions.axis("left", "right"), 0.0, actions.axis("up", "down"));
    let dir = dir.clamp_length_max(1.0);
    for (t, p) in ctx.world.query_mut::<(&mut Transform, &Player)>() {
        t.translation += dir * p.speed * dt;
        t.translation.x = t.translation.x.clamp(-ARENA, ARENA);
        t.translation.z = t.translation.z.clamp(-ARENA, ARENA);
    }
}

// ---- helpers --------------------------------------------------------------------------------

/// Distance the player covers in `ticks` fixed ticks at 60 Hz.
fn dist(ticks: u32) -> f32 {
    let dt = TickRate::default().step().as_secs_f32();
    ticks as f32 * 3.0 * dt
}

fn player_pos(app: &TestApp) -> Vec3 {
    let (player, _) = app.single::<Player>();
    app.translation(player)
}

fn assert_near(actual: Vec3, expected: Vec3) {
    assert!(
        actual.abs_diff_eq(expected, 1e-4),
        "player at {actual}, expected {expected}"
    );
}

/// The scripted session used by several tests: right for 30 ticks, then up for 20 while
/// still moving right for the first 10 of them (diagonal, normalized).
fn session() -> InputScript {
    InputScript::new()
        .hold_key(0, KeyCode::ArrowRight, 40)
        .hold_key(30, KeyCode::ArrowUp, 20)
}

fn expected_after_session() -> Vec3 {
    let diag = std::f32::consts::FRAC_1_SQRT_2;
    Vec3::new(dist(30) + dist(10) * diag, 0.5, -(dist(10) * diag + dist(10)))
}

// ---- tests ----------------------------------------------------------------------------------

#[test]
fn arrow_keys_move_the_player_on_exact_ticks() {
    let mut app = TestApp::builder().no_render().build();
    app.add_plugin(MiniGame);

    app.hold_key(KeyCode::ArrowRight, 30);
    app.step(); // release becomes visible
    assert_near(player_pos(&app), Vec3::new(dist(30), 0.5, 0.0));

    app.play(&InputScript::new().hold_key(0, KeyCode::ArrowUp, 20));
    assert_near(player_pos(&app), Vec3::new(dist(30), 0.5, -dist(20)));

    // run_until: walk left until the player crosses x = 0.
    app.press_key(KeyCode::KeyA);
    let ticks = app.run_until(|a| player_pos(a).x < 0.01, 120).expect("reaches x = 0");
    assert_eq!(ticks, 30);
    app.release_key(KeyCode::KeyA);

    // The arena clamps the player.
    app.hold_key(KeyCode::ArrowDown, 600);
    assert_eq!(player_pos(&app).z, ARENA);
}

#[test]
fn scripted_session_matches_golden_screenshot() {
    let mut app = TestApp::headless(160, 120);
    app.add_plugin(MiniGame);
    app.play(&session());
    assert_near(player_pos(&app), expected_after_session());

    // Skips (with a notice) when this machine has no GPU adapter.
    assert_screenshot!(&mut app, "mini_game/after_session");

    // Budgets: player + floor are two batches, one draw call each.
    RenderBudget::new()
        .max_draw_calls(2)
        .max_triangles(12 + 2)
        .min_instances(2)
        .assert_frame(&mut app);
}

#[test]
fn gamepad_and_touch_drive_the_same_action() {
    let mut app = TestApp::builder().size(160, 120).no_render().build();
    app.add_plugin(MiniGame);
    app.play(&InputScript::new().gamepad_axis_hold(0, 0, GamepadAxis::LeftStickX, 1.0, 10));
    assert_near(player_pos(&app), Vec3::new(dist(10), 0.5, 0.0));

    // A finger held on the right half of the 160x120 window for 10 ticks.
    let touch = InputScript::new()
        .touch(0, 1, TouchPhase::Started, Vec2::new(150.0, 60.0))
        .touch(10, 1, TouchPhase::Ended, Vec2::new(150.0, 60.0));
    app.play(&touch);
    assert_near(player_pos(&app), Vec3::new(dist(20), 0.5, 0.0));

    // The left half does nothing.
    app.touch_tap(Vec2::new(10.0, 60.0));
    assert_near(player_pos(&app), Vec3::new(dist(20), 0.5, 0.0));
}

#[test]
fn recorded_input_replays_identically() {
    let mut live = TestApp::no_render();
    live.add_plugin(MiniGame);
    live.start_recording();
    live.hold_key(KeyCode::ArrowRight, 12);
    live.step_ticks(3);
    live.hold_key(KeyCode::ArrowUp, 7);
    live.set_gamepad_axis(0, GamepadAxis::LeftStickX, 1.0);
    live.step_ticks(5);
    live.set_gamepad_axis(0, GamepadAxis::LeftStickX, 0.0);
    live.step();
    let script = live.stop_recording();

    // Round-trip through a RON file, as a game would store a bug-repro session.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.ron");
    script.save(&path).unwrap();
    let loaded = InputScript::load(&path).unwrap();
    assert_eq!(loaded, script);
    assert!(
        loaded
            .events()
            .iter()
            .any(|e| e.event == InputEvent::KeyDown(KeyCode::ArrowRight))
    );

    let mut replay = TestApp::no_render();
    replay.add_plugin(MiniGame);
    replay.play(&loaded);
    replay.step_ticks(live.tick() - replay.tick());
    assert_eq!(replay.tick(), live.tick());
    assert_eq!(player_pos(&replay), player_pos(&live));
    assert_eq!(world_hash(&replay.world), world_hash(&live.world));
}

#[test]
fn gameplay_is_deterministic() {
    // Two runs in lockstep with the same input; fingerprints compared every tick and the final
    // frames compared bit-exactly when a GPU is available.
    let report = DeterminismCheck::new(session().duration())
        .builder(TestApp::builder().size(96, 72))
        .fingerprinter(Fingerprinter::new().component::<Player>())
        .input(session())
        .name("mini_game")
        .assert(|app| {
            app.add_plugin(MiniGame);
        });
    assert_eq!(report.ticks, 51);
    assert_eq!(report.screenshots_compared, gpu_available());
}

#[test]
fn simulation_fits_the_frame_budget() {
    let mut app = TestApp::no_render();
    app.add_plugin(MiniGame);
    app.press_key(KeyCode::ArrowRight);
    // Generous: the tiny game ticks in microseconds; this guards against pathological regressions.
    let timings = assert_frame_budget(&mut app, 120, Duration::from_millis(4));
    assert_eq!(timings.ticks, 120);
}
