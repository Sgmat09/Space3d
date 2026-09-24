//! Window configuration ([`WindowPlugin`]), runtime window state and platform events.

use crate::input::InputPlugin;
use glam::UVec2;
use space3d_core::{App, Plugin};
use space3d_render::RenderPlugin;

/// Opens a window, creates the renderer's surface and installs the winit runner.
///
/// Adds [`InputPlugin`] and [`RenderPlugin`]. `App::run()` then hands control to winit:
/// every redraw calls `app.update(dt)`; the loop ends on window close (when
/// `exit_on_close`) or when an `AppExit` event is sent.
#[derive(Clone, Debug)]
pub struct WindowPlugin {
    /// Window title (also settable at runtime via [`WindowState::title`]).
    pub title: String,
    /// Initial logical width.
    pub width: u32,
    /// Initial logical height.
    pub height: u32,
    /// Present with vsync.
    pub vsync: bool,
    /// User-resizable window.
    pub resizable: bool,
    /// MSAA samples (1 or 4).
    pub msaa_samples: u32,
    /// Web: id of an existing `<canvas>` to render into. `None` creates one and appends it to
    /// `<body>`.
    pub canvas_id: Option<String>,
    /// Exit when the window is closed (otherwise only [`PlatformEvent::CloseRequested`] is sent).
    pub exit_on_close: bool,
}

impl Default for WindowPlugin {
    fn default() -> Self {
        Self {
            title: "Space3d".into(),
            width: 1280,
            height: 720,
            vsync: true,
            resizable: true,
            msaa_samples: 4,
            canvas_id: None,
            exit_on_close: true,
        }
    }
}

impl WindowPlugin {
    /// Settings with a title.
    pub fn titled(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Default::default()
        }
    }
}

impl Plugin for WindowPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugin(InputPlugin);
        app.add_plugin(RenderPlugin::default());
        app.add_event::<PlatformEvent>();
        app.insert_resource(WindowState {
            physical_size: UVec2::new(self.width, self.height),
            scale_factor: 1.0,
            focused: true,
            suspended: false,
            title: self.title.clone(),
            cursor_visible: true,
            cursor_grab: false,
        });
        let settings = self.clone();
        app.set_runner(move |app| crate::runner::run(app, settings));
    }
}

/// Live window state (resource). `title`, `cursor_visible` and `cursor_grab` are writable:
/// the runner applies changes after each frame.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowState {
    /// Framebuffer size in physical pixels.
    pub physical_size: UVec2,
    /// Physical pixels per logical pixel.
    pub scale_factor: f64,
    /// Has keyboard focus.
    pub focused: bool,
    /// Suspended by the OS (mobile background): no surface, no frames.
    pub suspended: bool,
    /// Window title.
    pub title: String,
    /// Show the OS cursor.
    pub cursor_visible: bool,
    /// Grab/lock the cursor (FPS-style camera control).
    pub cursor_grab: bool,
}

/// Window/application lifecycle events (`Events<PlatformEvent>`).
#[derive(Clone, Debug, PartialEq)]
pub enum PlatformEvent {
    /// Framebuffer resized (physical pixels).
    Resized(UVec2),
    /// DPI changed.
    ScaleFactorChanged(f64),
    /// Focus gained/lost.
    Focused(bool),
    /// User asked to close the window.
    CloseRequested,
    /// App went to background (mobile) — surface destroyed.
    Suspended,
    /// App returned to foreground — surface recreated.
    Resumed,
}
