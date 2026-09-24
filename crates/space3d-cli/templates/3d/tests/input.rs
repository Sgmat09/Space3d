//! Gameplay tests driven by simulated input. Every step is exactly one fixed tick (1/60 s),
//! so physics results are reproducible; no window or GPU needed.

use {{crate_name}}::{BALL_START, Ball, GamePlugin, Score};
use space3d::prelude::*;
use space3d::test::prelude::*;

fn game() -> TestApp {
    let mut app = TestApp::no_render();
    app.add_plugin(GamePlugin);
    app.step_ticks(30); // start-up + let the crates settle
    app
}

fn ball(app: &TestApp) -> Vec3 {
    app.translation(app.entities_with::<Ball>()[0])
}

#[test]
fn forward_rolls_the_ball_away_from_the_camera() {
    let mut app = game();
    let start = ball(&app);
    app.hold_key(KeyCode::ArrowUp, 60);
    let end = ball(&app);
    assert!(end.z < start.z - 2.0, "{start} -> {end}");
}

#[test]
fn space_jumps_only_from_the_ground() {
    let mut app = game();
    let rest = ball(&app);
    app.tap_key(KeyCode::Space);
    app.step_ticks(8);
    let air = ball(&app);
    assert!(air.y > rest.y + 0.5, "no jump: {rest} -> {air}");
    // A second press mid-air does nothing.
    app.tap_key(KeyCode::Space);
    app.step_ticks(20);
    let later = ball(&app);
    assert!(later.y < air.y + 1.0, "double jump: {air} -> {later}");
}

#[test]
fn falling_off_respawns_the_ball() {
    let mut app = game();
    app.press_key(KeyCode::ArrowRight); // roll off the right edge…
    app.run_until(|app| app.resource::<Score>().falls >= 1, 600)
        .expect("the ball never fell off");
    app.release_key(KeyCode::ArrowRight);
    app.step_ticks(30);
    let p = ball(&app);
    assert!(p.distance(BALL_START) < 1.5, "not respawned at the start: {p}");
}

#[test]
fn physics_is_deterministic() {
    let script = InputScript::new()
        .hold_key(0, KeyCode::ArrowUp, 50)
        .hold_key(20, KeyCode::ArrowLeft, 25)
        .hold_key(40, KeyCode::Space, 1);
    let run = || {
        let mut app = game();
        app.play(&script);
        app.step_ticks(60);
        let crates: Vec<Vec3> = app
            .entities_with::<{{crate_name}}::Crate>()
            .into_iter()
            .map(|e| app.translation(e))
            .collect();
        (ball(&app), crates)
    };
    assert_eq!(run(), run());
}
