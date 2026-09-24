//! Keyboard controls (feature `client`): WASD and the arrow keys through named actions, so they
//! can be rebound and driven in tests with `Input::inject_*`.
//!
//! ```ignore
//! app.add_plugin(space3d_platform::InputPlugin);          // PlatformPlugin adds it for you
//! multiplayer_arena::controls::install(&mut app);
//! ```

use crate::CubeInput;
use space3d_core::prelude::*;
use space3d_platform::{ActionMap, ActionState, Binding, KeyCode};
use space3d_replication::AppReplicationExt;

/// Action names.
pub const LEFT: &str = "left";
/// See [`LEFT`].
pub const RIGHT: &str = "right";
/// See [`LEFT`].
pub const FORWARD: &str = "forward";
/// See [`LEFT`].
pub const BACK: &str = "back";

/// Bind WASD and the arrow keys.
pub fn bind(map: &mut ActionMap) {
    for (action, keys) in [
        (LEFT, [KeyCode::KeyA, KeyCode::ArrowLeft]),
        (RIGHT, [KeyCode::KeyD, KeyCode::ArrowRight]),
        (FORWARD, [KeyCode::KeyW, KeyCode::ArrowUp]),
        (BACK, [KeyCode::KeyS, KeyCode::ArrowDown]),
    ] {
        for key in keys {
            map.bind(action, Binding::Key(key));
        }
    }
}

/// The input sampler: runs once per predicted tick (FixedUpdate) and reads the action state
/// the input plugin computed in PreUpdate.
pub fn sample(ctx: &mut SystemCtx) -> CubeInput {
    let actions = ctx.resources.expect::<ActionState>();
    CubeInput::from_axes(actions.axis(LEFT, RIGHT), actions.axis(FORWARD, BACK))
}

/// Bind the keys and install [`sample`] as the input sampler (needs `InputPlugin`, which
/// `PlatformPlugin` adds, and the replication client plugin).
pub fn install(app: &mut App) {
    bind(app.resources.expect_mut::<ActionMap>());
    app.set_input_sampler(sample);
}
