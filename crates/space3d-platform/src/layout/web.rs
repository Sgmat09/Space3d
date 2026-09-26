//! Web: `navigator.keyboard.getLayoutMap()` (Keyboard Map API; Chromium-based browsers). The
//! promise resolves on a later frame; the runner applies the result with
//! [`super::KeyboardLayout::refresh_from_os`] / `LayoutWatch::poll`. Browsers without the API
//! (Firefox, Safari) or pages where it is blocked (cross-origin iframes without the
//! `keyboard-map` permission) use learned labels only.

use super::{OsLayout, normalize_label, w3c_code, writing_system_keys};
use std::cell::RefCell;
use wasm_bindgen::{JsCast, JsValue};

#[derive(Default)]
struct State {
    in_flight: bool,
    unsupported: bool,
    result: Option<OsLayout>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::default();
}

/// Start reading the layout map (no-op while a request runs or when unsupported).
pub(super) fn request() {
    let start = STATE.with_borrow_mut(|s| {
        let start = !s.in_flight && !s.unsupported;
        s.in_flight |= start;
        start
    });
    if !start {
        return;
    }
    wasm_bindgen_futures::spawn_local(async {
        let result = fetch().await;
        STATE.with_borrow_mut(|s| {
            s.in_flight = false;
            match result {
                Ok(Some(layout)) => s.result = Some(layout),
                Ok(None) => s.unsupported = true,
                Err(e) => {
                    tracing::debug!("keyboard layout map unavailable: {e:?}");
                    s.unsupported = true;
                }
            }
        });
    });
}

/// The finished result of the last [`request`], if any.
pub(super) fn take() -> Option<OsLayout> {
    STATE.with_borrow_mut(|s| s.result.take())
}

async fn fetch() -> Result<Option<OsLayout>, JsValue> {
    let Some(window) = web_sys::window() else {
        return Ok(None);
    };
    let keyboard = js_sys::Reflect::get(&window.navigator(), &JsValue::from_str("keyboard"))?;
    if keyboard.is_undefined() || keyboard.is_null() {
        return Ok(None);
    }
    let get_layout_map = js_sys::Reflect::get(&keyboard, &JsValue::from_str("getLayoutMap"))?;
    let Some(get_layout_map) = get_layout_map.dyn_ref::<js_sys::Function>() else {
        return Ok(None);
    };
    let promise: js_sys::Promise = get_layout_map.call0(&keyboard)?.dyn_into()?;
    let map = wasm_bindgen_futures::JsFuture::from(promise).await?;
    let get: js_sys::Function = js_sys::Reflect::get(&map, &JsValue::from_str("get"))?.dyn_into()?;
    let mut keys = Vec::new();
    for code in writing_system_keys() {
        let Some(name) = w3c_code(code) else { continue };
        if let Some(text) = get.call1(&map, &JsValue::from_str(name))?.as_string()
            && let Some(text) = normalize_label(&text)
        {
            keys.push((code, text, None));
        }
    }
    Ok(Some(OsLayout { id: None, keys }))
}
