//! Web (wasm32) support: canvas attachment and logging setup.
//!
//! ```ignore
//! #[wasm_bindgen::prelude::wasm_bindgen(start)]
//! pub fn start() {
//!     space3d_platform::init_web_logging();
//!     let mut app = space3d_core::App::new();
//!     app.add_plugin(space3d_platform::PlatformPlugin {
//!         window: space3d_platform::WindowPlugin { canvas_id: Some("space3d".into()), ..Default::default() },
//!     });
//!     app.run(); // returns immediately; the browser drives the frame loop
//! }
//! ```
//! The renderer is created asynchronously (WebGPU when available, WebGL2 otherwise) and
//! inserted as soon as it is ready; frames before that run simulation only.

use wasm_bindgen::JsCast;
use web_sys::HtmlCanvasElement;
use winit::platform::web::{WindowAttributesExtWebSys, WindowExtWebSys};
use winit::window::{Window, WindowAttributes};

/// Route panics to `console.error` and `tracing` to the browser console. Safe to call twice.
pub fn init_web_logging() {
    console_error_panic_hook::set_once();
    let _ = tracing_wasm::try_set_as_global_default();
}

/// Find `<canvas id=...>` in the document.
pub fn find_canvas(id: &str) -> Option<HtmlCanvasElement> {
    web_sys::window()?
        .document()?
        .get_element_by_id(id)?
        .dyn_into::<HtmlCanvasElement>()
        .ok()
}

/// Attach to an existing canvas (by id) or create one and append it to `<body>`.
pub(crate) fn window_attributes(attrs: WindowAttributes, canvas_id: Option<&str>) -> WindowAttributes {
    let canvas = canvas_id.and_then(|id| {
        let c = find_canvas(id);
        if c.is_none() {
            tracing::warn!("canvas #{id} not found; creating one");
        }
        c
    });
    let append = canvas.is_none();
    attrs
        .with_canvas(canvas)
        .with_append(append)
        .with_focusable(true)
        .with_prevent_default(true)
}

/// The canvas backing a winit window.
pub(crate) fn canvas_of(window: &Window) -> Option<HtmlCanvasElement> {
    window.canvas()
}
