//! Golden screenshot tests (see the template README): first run writes
//! `tests/__screenshots__/**.png`, later runs compare. Skipped without a GPU.

use {{crate_name}}::client::{ClientScenePlugin, apply_snapshot};
use {{crate_name}}::stories::demo_players;
use space3d::prelude::*;
use space3d::test::prelude::*;

// One golden screenshot per story in `src/stories.rs`.
space3d::stories::storybook_tests!({{crate_name}}::stories::storybook);

#[test]
fn three_players_me_highlighted() {
    require_gpu!();
    let mut app = TestApp::headless(320, 240);
    app.add_plugin(ClientScenePlugin);
    app.step(); // start-up creates the scene
    let players = demo_players(3);
    let app_ref: &mut App = &mut app;
    apply_snapshot(&mut app_ref.world, &app_ref.resources, &players, Some(2));
    app.step_ticks(30); // let the replicas ease into place
    assert_screenshot!(&mut app, "three_players");
}
