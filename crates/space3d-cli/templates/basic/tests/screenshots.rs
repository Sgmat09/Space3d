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
fn start_screen() {
    require_gpu!();
    let mut app = TestApp::headless(320, 240);
    app.add_plugin(GamePlugin);
    app.step_ticks(2);
    assert_screenshot!(&mut app, "start_screen");
}

#[test]
fn after_moving_right() {
    require_gpu!();
    let mut app = TestApp::headless(320, 240);
    app.add_plugin(GamePlugin);
    app.hold_key(KeyCode::ArrowRight, 60);
    assert_screenshot!(&mut app, "moved_right");
}
