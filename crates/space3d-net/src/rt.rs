//! Tiny cross-platform async helpers (timer + timeout) used by the clients.

use crate::NetError;
use std::future::Future;
use std::time::Duration;

/// Sleep without blocking (tokio timer on native, `setTimeout` on web).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) async fn sleep(d: Duration) {
    tokio::time::sleep(d).await
}

#[cfg(target_arch = "wasm32")]
mod web_timer {
    use wasm_bindgen::prelude::*;
    #[wasm_bindgen]
    extern "C" {
        // Global in both window and worker scopes.
        #[wasm_bindgen(js_name = setTimeout)]
        pub fn set_timeout(handler: &js_sys::Function, timeout: i32) -> JsValue;
    }
}

/// Sleep without blocking (tokio timer on native, `setTimeout` on web).
#[cfg(target_arch = "wasm32")]
pub(crate) async fn sleep(d: Duration) {
    let ms = d.as_millis().min(i32::MAX as u128) as i32;
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        web_timer::set_timeout(&resolve, ms);
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

/// Run `fut` with an optional deadline; `None` means no timeout.
pub(crate) async fn timeout<F: Future>(limit: Option<Duration>, fut: F) -> Result<F::Output, NetError> {
    let Some(limit) = limit else {
        return Ok(fut.await);
    };
    let fut = std::pin::pin!(fut);
    let timer = std::pin::pin!(sleep(limit));
    match futures::future::select(fut, timer).await {
        futures::future::Either::Left((out, _)) => Ok(out),
        futures::future::Either::Right(_) => Err(NetError::Timeout),
    }
}

/// Result of racing some work against a stop signal.
pub(crate) enum Race<T> {
    Done(T),
    Stopped,
}

/// Await `fut`, but bail out as soon as `stop` completes.
pub(crate) async fn race<F: Future, S: Future>(fut: F, stop: S) -> Race<F::Output> {
    let fut = std::pin::pin!(fut);
    let stop = std::pin::pin!(stop);
    match futures::future::select(stop, fut).await {
        futures::future::Either::Left(_) => Race::Stopped,
        futures::future::Either::Right((out, _)) => Race::Done(out),
    }
}
