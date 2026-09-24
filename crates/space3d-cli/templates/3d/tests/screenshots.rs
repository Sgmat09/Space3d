//! Golden screenshot tests.
//!
//! The first run writes `tests/__screenshots__/**.png` — look at them and commit them. Later
//! runs compare against them and fail on visible differences (the diff image and an HTML
//! report are written by `space3d test`). Accept intended changes with
//! `space3d test --update-snapshots`. Without a GPU these tests skip themselves.

use {{crate_name}}::GamePlugin;
use space3d::prelude::*;
use space3d::test::prelude::*;

// One golden screenshot per story in `src/stories.rs`.
space3d::stories::storybook_tests!({{crate_name}}::stories::storybook);

#[test]
fn arena_at_start() {
    require_gpu!();
    let mut app = TestApp::headless(320, 240);
    app.add_plugin(GamePlugin);
    app.step_ticks(2);
    assert_screenshot!(&mut app, "arena_start");
}

#[test]
fn ball_rolled_into_the_crates() {
    require_gpu!();
    let mut app = TestApp::headless(320, 240);
    app.add_plugin(GamePlugin);
    app.hold_key(KeyCode::ArrowUp, 90);
    app.step_ticks(30);
    // Physics is deterministic per tick, so this frame is stable too.
    assert_screenshot!(&mut app, "ball_hits_crates");
}
