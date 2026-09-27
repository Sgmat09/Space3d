//! Web (wasm32) support: canvas attachment and the renderer backend override (logging:
//! [`crate::console_log`]).
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
//! inserted as soon as it is ready; frames before that run simulation only. The page hears
//! about it through DOM events on the canvas: [`crate::PAGE_READY_EVENT`], or
//! [`crate::PAGE_ERROR_EVENT`] when the renderer could not be created or the GPU context was
//! lost later.

use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::HtmlCanvasElement;
use winit::platform::web::{WindowAttributesExtWebSys, WindowExtWebSys};
use winit::window::{Window, WindowAttributes};

/// [`crate::window::backends_from_query`] for this page's URL.
pub(crate) fn backends_from_url() -> Option<space3d_render::wgpu::Backends> {
    let search = web_sys::window()?.location().search().ok()?;
    let backends = crate::window::backends_from_query(&search);
    if let Some(b) = backends {
        tracing::info!("renderer backends restricted to {b:?} by the page URL ({search})");
    }
    backends
}

/// [`crate::window::backend_override_hint`] for this page's URL.
pub(crate) fn backend_override_hint() -> Option<String> {
    let search = web_sys::window()?.location().search().ok()?;
    crate::window::backend_override_hint(&search)
}

/// Dispatch a bubbling DOM event on the game's canvas for the page ([`crate::PAGE_READY_EVENT`],
/// [`crate::PAGE_ERROR_EVENT`] with the message as `detail`).
pub(crate) fn notify_page(canvas: &HtmlCanvasElement, event: &str, detail: Option<&str>) {
    let init = web_sys::CustomEventInit::new();
    init.set_bubbles(true);
    if let Some(detail) = detail {
        init.set_detail(&detail.into());
    }
    match web_sys::CustomEvent::new_with_event_init_dict(event, &init) {
        Ok(ev) => {
            let _ = canvas.dispatch_event(&ev);
        }
        Err(e) => tracing::debug!("cannot create the {event} event: {e:?}"),
    }
}

/// Report a lost WebGL context (GPU reset, memory pressure, too many contexts, the tab in the
/// background on some mobile browsers): an error in the log and [`crate::PAGE_ERROR_EVENT`].
/// The engine does not restore the context (every GPU resource would have to be recreated), so
/// the canvas stays black; the page can offer a reload. Without this nothing tells.
pub(crate) fn watch_context_loss(canvas: &HtmlCanvasElement) {
    let target = canvas.clone();
    let listener = Closure::<dyn FnMut()>::new(move || {
        let msg = "the browser lost the WebGL context (GPU reset, memory pressure or too many contexts): \
                   nothing renders until the page is reloaded";
        tracing::error!("{msg}");
        notify_page(&target, crate::PAGE_ERROR_EVENT, Some(msg));
    });
    if canvas
        .add_event_listener_with_callback("webglcontextlost", listener.as_ref().unchecked_ref())
        .is_ok()
    {
        // Lives as long as the canvas (one per game).
        listener.forget();
    }
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
