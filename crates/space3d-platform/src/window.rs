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

/// Renderer backends asked for by a web page's URL query (`location.search`): `webgl2` →
/// WebGL2 only (`wgpu::Backends::GL`), `webgpu` → WebGPU only. `None` (neither key): WebGPU
/// when the browser has an adapter, WebGL2 otherwise. The window runner applies it on the web,
/// so `http://localhost:8080/?webgl2` shows a game's WebGL2 path in a browser with WebGPU.
pub fn backends_from_query(query: &str) -> Option<space3d_render::wgpu::Backends> {
    backend_override(query).map(|(_, backends)| backends)
}

/// The backend key in a page URL query (`webgl2`, `webgpu`) and the backends it allows.
fn backend_override(query: &str) -> Option<(&'static str, space3d_render::wgpu::Backends)> {
    use space3d_render::wgpu::Backends;
    query
        .trim_start_matches('?')
        .split('&')
        .find_map(|pair| match pair.split('=').next().unwrap_or_default() {
            "webgl2" => Some(("webgl2", Backends::GL)),
            "webgpu" => Some(("webgpu", Backends::BROWSER_WEBGPU)),
            _ => None,
        })
}

/// What to do when the renderer could not be created and the page URL query restricted the
/// backends ([`backends_from_query`]); `None` without a restriction.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn backend_override_hint(query: &str) -> Option<String> {
    let (key, _) = backend_override(query)?;
    let only = if key == "webgpu" { "WebGPU" } else { "WebGL2" };
    Some(format!(
        "`?{key}` in the page URL allows only {only}, which this browser could not provide: remove it to let the engine choose (WebGPU, else WebGL2)"
    ))
}

/// DOM event the web runner dispatches on the game's canvas once the renderer exists (it
/// bubbles to `document` and `window`). A page can show "loading…" until then: `init()` of the
/// wasm-bindgen module resolves as soon as the game starts, before the renderer is created
/// asynchronously.
pub const PAGE_READY_EVENT: &str = "space3d-ready";

/// DOM event (a `CustomEvent` whose `detail` is the message) the web runner dispatches on the
/// game's canvas when the game cannot render: the renderer could not be created (no WebGPU or
/// WebGL2, a `?webgpu`/`?webgl2` override the browser cannot satisfy), or the browser lost the
/// WebGL context or the WebGPU device (GPU reset, memory pressure, too many contexts). The
/// engine does not recover from a loss; the page can offer a reload. Also logged as an error.
pub const PAGE_ERROR_EVENT: &str = "space3d-error";

#[cfg(test)]
mod tests {
    use super::*;
    use space3d_render::wgpu::Backends;

    #[test]
    fn backend_query() {
        assert_eq!(backends_from_query(""), None);
        assert_eq!(backends_from_query("?level=3"), None);
        assert_eq!(backends_from_query("?webgl2"), Some(Backends::GL));
        assert_eq!(backends_from_query("?level=3&webgl2=1"), Some(Backends::GL));
        assert_eq!(backends_from_query("webgpu"), Some(Backends::BROWSER_WEBGPU));
        assert_eq!(backends_from_query("?webgl"), None);
    }

    #[test]
    fn backend_hint_names_the_override() {
        assert_eq!(backend_override_hint("?level=3"), None);
        let hint = backend_override_hint("?level=3&webgpu").unwrap();
        assert!(
            hint.starts_with("`?webgpu` in the page URL allows only WebGPU"),
            "{hint}"
        );
        assert!(backend_override_hint("?webgl2=1").unwrap().contains("only WebGL2"));
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
