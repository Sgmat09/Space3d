//! Gameplay tests driven by simulated input. They run headless and deterministically: every
//! step is exactly one fixed tick (1/60 s), no window and no GPU needed.

use {{crate_name}}::{ARENA, GamePlugin, PLAYER_SPEED};
use space3d::prelude::*;
use space3d::test::prelude::*;

fn game() -> TestApp {
    let mut app = TestApp::no_render();
    app.add_plugin(GamePlugin);
    app.step(); // runs Startup: the scene exists from here on
    app
}

fn player_position(app: &TestApp) -> Vec3 {
    app.translation(app.find_by_name("player").expect("the player is spawned at startup"))
}

#[test]
fn holding_right_moves_the_player_at_player_speed() {
    let mut app = game();
    let start = player_position(&app);
    app.hold_key(KeyCode::ArrowRight, 30); // half a second
    let moved = player_position(&app) - start;
    assert!((moved.x - PLAYER_SPEED * 0.5).abs() < 1e-3, "moved {moved:?}");
    assert!(moved.z.abs() < 1e-6);
}

#[test]
fn wasd_works_too() {
    let mut app = game();
    app.hold_key(KeyCode::KeyW, 12);
    assert!(player_position(&app).z < -0.5);
}

#[test]
fn the_player_cannot_leave_the_arena() {
    let mut app = game();
    app.hold_key(KeyCode::ArrowLeft, 600);
    assert_eq!(player_position(&app).x, -ARENA);
}

#[test]
fn touching_the_right_edge_moves_right() {
    let mut app = game(); // 256×256 virtual screen
    let finger = Vec2::new(250.0, 128.0);
    app.input().inject_touch(1, TouchPhase::Started, finger);
    app.step_ticks(20);
    app.input().inject_touch(1, TouchPhase::Ended, finger);
    app.step();
    assert!(player_position(&app).x > 0.5, "{:?}", player_position(&app));
}

#[test]
fn a_scripted_run_is_reproducible() {
    let script = InputScript::new()
        .hold_key(0, KeyCode::ArrowUp, 20)
        .hold_key(10, KeyCode::ArrowLeft, 30);
    let run = || {
        let mut app = game();
        app.play(&script);
        player_position(&app)
    };
    assert_eq!(run(), run());
}

#[test]
fn escape_quits() {
    let mut app = game();
    assert!(!app.should_exit());
    app.tap_key(KeyCode::Escape);
    assert!(app.should_exit());
}
