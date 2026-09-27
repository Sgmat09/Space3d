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
    /// Initial logical width (iOS windows always fill the screen).
    pub width: u32,
    /// Initial logical height (iOS windows always fill the screen).
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
        app.init_resource::<SafeAreaInsets>();
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

/// Edges of the window covered by system UI, in physical pixels (resource): on iOS the status
/// bar / notch / Dynamic Island, the home indicator and rounded corners, kept current by the
/// window runner (they change with the orientation). Zero elsewhere. Render the scene into the
/// whole window but keep text and touch controls inside these insets; the egui integration
/// (`space3d-ui`) keeps its windows inside them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SafeAreaInsets {
    /// Top inset.
    pub top: f32,
    /// Right inset.
    pub right: f32,
    /// Bottom inset.
    pub bottom: f32,
    /// Left inset.
    pub left: f32,
}

impl SafeAreaInsets {
    /// Insets of a safe area at `inner_pos` with `inner_size` inside a window at `outer_pos`
    /// with `outer_size` (all physical pixels, screen coordinates).
    pub fn from_rects(outer_pos: glam::IVec2, outer_size: UVec2, inner_pos: glam::IVec2, inner_size: UVec2) -> Self {
        let left = (inner_pos.x - outer_pos.x).max(0) as f32;
        let top = (inner_pos.y - outer_pos.y).max(0) as f32;
        let right = (outer_size.x as f32 - inner_size.x as f32 - left).max(0.0);
        let bottom = (outer_size.y as f32 - inner_size.y as f32 - top).max(0.0);
        Self {
            top,
            right,
            bottom,
            left,
        }
    }

    /// `true` when nothing is covered.
    pub fn is_zero(&self) -> bool {
        *self == Self::default()
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use glam::IVec2;

    #[test]
    fn safe_area_from_rects() {
        // iPhone 17 portrait: 402×874 pt at 3×, safe area 62 pt below the top, 34 pt above the bottom.
        let insets = SafeAreaInsets::from_rects(
            IVec2::ZERO,
            UVec2::new(1206, 2622),
            IVec2::new(0, 186),
            UVec2::new(1206, 2622 - 186 - 102),
        );
        assert_eq!(
            insets,
            SafeAreaInsets {
                top: 186.0,
                right: 0.0,
                bottom: 102.0,
                left: 0.0
            }
        );
        assert!(!insets.is_zero());
        let full = SafeAreaInsets::from_rects(
            IVec2::new(5, 5),
            UVec2::new(10, 10),
            IVec2::new(5, 5),
            UVec2::new(10, 10),
        );
        assert!(full.is_zero());
    }
}
