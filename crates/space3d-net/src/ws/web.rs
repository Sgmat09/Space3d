//! Browser WebSocket transport (`web_sys::WebSocket`, text + arraybuffer binary).

use super::{Incoming, Message};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::{BinaryType, CloseEvent, Event, MessageEvent, WebSocket};

enum JsEvent {
    Open,
    Message(Message),
    Close(String),
    Error,
}

pub(crate) struct Socket {
    ws: WebSocket,
    rx: async_channel::Receiver<JsEvent>,
    _onopen: Closure<dyn FnMut(Event)>,
    _onmessage: Closure<dyn FnMut(MessageEvent)>,
    _onclose: Closure<dyn FnMut(CloseEvent)>,
    _onerror: Closure<dyn FnMut(Event)>,
}

fn js_err(v: wasm_bindgen::JsValue) -> String {
    v.as_string()
        .or_else(|| js_sys::Reflect::get(&v, &"message".into()).ok()?.as_string())
        .unwrap_or_else(|| format!("{v:?}"))
}

/// Resolve relative URLs (`/ws`) against the page and map http(s) → ws(s).
fn resolve(url: &str) -> String {
    let Some(base) = crate::client::platform_base_url() else {
        return url.to_owned();
    };
    let Ok(mut u) = url::Url::parse(&base).and_then(|b| b.join(url)) else {
        return url.to_owned();
    };
    let scheme = match u.scheme() {
        "http" => Some("ws"),
        "https" => Some("wss"),
        _ => None,
    };
    if let Some(s) = scheme {
        let _ = u.set_scheme(s);
    }
    u.into()
}

impl Socket {
    pub async fn connect(url: &str) -> Result<Self, String> {
        let ws = WebSocket::new(&resolve(url)).map_err(js_err)?;
        ws.set_binary_type(BinaryType::Arraybuffer);
        let (tx, rx) = async_channel::unbounded();

        let t = tx.clone();
        let onopen = Closure::<dyn FnMut(Event)>::new(move |_: Event| {
            let _ = t.try_send(JsEvent::Open);
        });
        let t = tx.clone();
        let onmessage = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
            let data = e.data();
            let msg = if let Some(s) = data.as_string() {
                Message::Text(s)
            } else if let Ok(buf) = data.dyn_into::<js_sys::ArrayBuffer>() {
                Message::Binary(js_sys::Uint8Array::new(&buf).to_vec())
            } else {
                return;
            };
            let _ = t.try_send(JsEvent::Message(msg));
        });
        let t = tx.clone();
        let onclose = Closure::<dyn FnMut(CloseEvent)>::new(move |e: CloseEvent| {
            let reason = e.reason();
            let text = if reason.is_empty() {
                format!("closed ({})", e.code())
            } else {
                format!("closed ({}): {reason}", e.code())
            };
            let _ = t.try_send(JsEvent::Close(text));
        });
        let t = tx;
        let onerror = Closure::<dyn FnMut(Event)>::new(move |_: Event| {
            let _ = t.try_send(JsEvent::Error);
        });
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));
        ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));

        let sock = Self {
            ws,
            rx,
            _onopen: onopen,
            _onmessage: onmessage,
            _onclose: onclose,
            _onerror: onerror,
        };
        loop {
            match sock.rx.recv().await {
                Ok(JsEvent::Open) => return Ok(sock),
                Ok(JsEvent::Close(reason)) => return Err(reason),
                // `error` is always followed by `close`, which carries the code.
                Ok(JsEvent::Error | JsEvent::Message(_)) => continue,
                Err(_) => return Err("websocket dropped".into()),
            }
        }
    }

    pub async fn send(&mut self, msg: Message) -> Result<(), String> {
        match msg {
            Message::Text(s) => self.ws.send_with_str(&s),
            Message::Binary(b) => self.ws.send_with_u8_array(&b),
        }
        .map_err(js_err)
    }

    /// Cancel-safe: only awaits the channel.
    pub async fn recv(&mut self) -> Incoming {
        loop {
            match self.rx.recv().await {
                Ok(JsEvent::Message(m)) => return Incoming::Message(m),
                Ok(JsEvent::Close(reason)) => return Incoming::Closed(reason),
                Ok(JsEvent::Open | JsEvent::Error) => continue,
                Err(_) => return Incoming::Closed("websocket dropped".into()),
            }
        }
    }

    pub async fn close(&mut self) {
        let _ = self.ws.close_with_code(1000);
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        self.ws.set_onopen(None);
        self.ws.set_onmessage(None);
        self.ws.set_onclose(None);
        self.ws.set_onerror(None);
        if self.ws.ready_state() <= WebSocket::OPEN {
            let _ = self.ws.close();
        }
    }
}
