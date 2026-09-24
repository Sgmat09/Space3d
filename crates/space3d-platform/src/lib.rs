#![warn(missing_docs)]
//! Space3d platform layer: the winit runner (desktop, web, Android, iOS) and input.
//!
//! * [`PlatformPlugin`] / [`WindowPlugin`] — open a window, create the renderer surface and
//!   run the frame loop (`App::run()`), handling resize, DPI, focus and mobile
//!   suspend/resume (surface recreation).
//! * [`Input`] — keyboard, mouse, multi-touch and (stub) gamepads, fully injectable for tests
//!   (`Input::inject_*`); [`ActionMap`] + [`ActionState`] map named actions to bindings.
//! * Web: `init_web_logging()` (wasm32 only), canvas attachment via [`WindowPlugin::canvas_id`].
//! * Mobile: `android_main_entry` (Android) and the iOS `start_app` pattern — see [`mobile`].
//!
//! ```no_run
//! use space3d_core::prelude::*;
//! use space3d_platform::*;
//! let mut app = App::new();
//! app.add_plugin(PlatformPlugin { window: WindowPlugin::titled("My game") });
//! app.add_system(Stage::Update, |ctx| {
//!     if ctx.resources.expect::<Input>().just_pressed(KeyCode::Escape) {
//!         ctx.send(AppExit);
//!     }
//! });
//! app.run();
//! ```

pub mod background;
mod input;
pub mod mobile;
mod runner;
#[cfg(target_arch = "wasm32")]
mod web;
mod window;

pub use background::{background_mode, set_background_mode};
pub use input::{
    ActionMap, ActionState, ActionValue, Binding, ButtonState, Gamepad, GamepadAxis, GamepadButton, Input, InputPlugin,
    KeyCode, MouseButton, Touch, TouchPhase,
};
#[cfg(target_os = "android")]
pub use mobile::{AndroidApp, android_main_entry};
#[cfg(target_arch = "wasm32")]
pub use web::{find_canvas, init_web_logging};
pub use window::{PlatformEvent, WindowPlugin, WindowState};

pub use winit;

use space3d_core::{App, Plugin};

/// Everything needed for a windowed app: [`WindowPlugin`] (which adds [`InputPlugin`] and
/// `space3d_render::RenderPlugin`) with the given window settings.
#[derive(Clone, Debug, Default)]
pub struct PlatformPlugin {
    /// Window settings.
    pub window: WindowPlugin,
}

impl Plugin for PlatformPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugin(self.window.clone());
    }
}

/// Common imports.
pub mod prelude {
    pub use crate::{
        ActionMap, ActionState, Binding, Input, InputPlugin, KeyCode, MouseButton, PlatformEvent, PlatformPlugin,
        WindowPlugin, WindowState,
    };
}
