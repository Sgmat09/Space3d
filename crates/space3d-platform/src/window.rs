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
    /// Phones: the first finger on the screen is also the mouse (its position, and the left
    /// button while it is down), for a game whose controls read the mouse. The finger still
    /// arrives as a touch. The position stays where the finger was lifted. Default `false`.
    pub touch_as_mouse: bool,
    /// Window icon (title bar, Alt-Tab) and, on Windows, the taskbar icon. `None` uses the
    /// platform default. Ignored on macOS (the Dock shows the app bundle's icon), the web and
    /// mobile.
    pub icon: Option<WindowIcon>,
}

/// An RGBA8 window icon (see [`WindowPlugin::icon`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowIcon {
    /// Row-major RGBA8 pixels, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl WindowIcon {
    /// An icon from an RGBA8 image.
    pub fn from_image(image: &space3d_core::ImageData) -> Self {
        Self {
            rgba: image.pixels.clone(),
            width: image.width,
            height: image.height,
        }
    }
}

pub use winit::window::CursorIcon;

/// The pointer shape shown over the window (resource, inserted by [`WindowPlugin`]).
///
/// `space3d-ui` writes egui's cursor here whenever egui asks for a different one (pointing hand
/// over buttons, I-beam over text fields, resize arrows...); a game may write it too, and the
/// last write wins. After every frame the runner applies it to the OS window with winit's
/// `Window::set_cursor`, which Windows re-applies on `WM_SETCURSOR` and macOS keeps through its
/// cursor rects. The pointer is visible only while [`Self::icon`] is `Some` and
/// [`WindowState::cursor_visible`] is `true`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowCursor {
    /// Requested shape; `None` hides the pointer (egui's `CursorIcon::None`).
    pub icon: Option<CursorIcon>,
    applied: Option<AppliedCursor>,
}

impl Default for WindowCursor {
    fn default() -> Self {
        Self {
            icon: Some(CursorIcon::Default),
            applied: None,
        }
    }
}

impl WindowCursor {
    /// What the runner last applied to a real OS window; `None` before the first frame with a
    /// window, and always without one (headless apps, tests without a window).
    pub fn applied(&self) -> Option<AppliedCursor> {
        self.applied
    }

    pub(crate) fn set_applied(&mut self, applied: AppliedCursor) {
        self.applied = Some(applied);
    }
}

/// A cursor the runner applied to the OS window (see [`WindowCursor::applied`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppliedCursor {
    /// Shape set with `Window::set_cursor` (kept while hidden).
    pub icon: CursorIcon,
    /// Whether the pointer is shown.
    pub visible: bool,
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
            touch_as_mouse: false,
            icon: None,
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
        app.init_resource::<WindowCursor>();
        app.init_resource::<SafeArea>();
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

/// The edges of the window the system keeps for itself, in physical pixels (resource): a
/// phone's notch or camera island, rounded corners and the home bar. The picture covers them;
/// what must stay readable and tappable keeps out of them (`space3d-ui` lays its UI out inside).
/// Filled on iOS; zero everywhere else (Android's cutouts are not read yet).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SafeArea {
    /// Kept at the left edge.
    pub left: u32,
    /// Kept at the top edge.
    pub top: u32,
    /// Kept at the right edge.
    pub right: u32,
    /// Kept at the bottom edge.
    pub bottom: u32,
}

/// Whether the game wants typed text now (resource). `space3d-ui` sets `wanted` while one of
/// its text fields has the focus; on Android and iOS the runner shows the on-screen keyboard
/// while it is set and hides it after. A game with text fields of its own writes it itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextInput {
    /// A text field has the focus.
    pub wanted: bool,
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
