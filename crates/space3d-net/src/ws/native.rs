//! Native WebSocket transport (tokio-tungstenite + rustls).

use super::{Incoming, Message};
use crate::error::error_chain;
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as TMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub(crate) struct Socket {
    inner: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
}

impl Socket {
    pub async fn connect(
        url: &str,
        max_message_size: Option<usize>,
        headers: &[(String, String)],
    ) -> Result<Self, String> {
        crate::tls::ensure_crypto_provider();
        let config = max_message_size.map(|n| {
            WebSocketConfig::default()
                .max_message_size(Some(n))
                .max_frame_size(Some(n))
        });
        let mut request = url.into_client_request().map_err(|e| error_chain(&e))?;
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| format!("header {name}: {e}"))?;
            let value = HeaderValue::from_str(value).map_err(|e| format!("header {name}: {e}"))?;
            request.headers_mut().insert(name, value);
        }
        // Nagle off: game traffic is many small latency-sensitive frames.
        let (inner, _response) = tokio_tungstenite::connect_async_with_config(request, config, true)
            .await
            .map_err(|e| error_chain(&e))?;
        Ok(Self { inner })
    }

    pub async fn send(&mut self, msg: Message) -> Result<(), String> {
        let msg = match msg {
            Message::Text(s) => TMessage::text(s),
            Message::Binary(b) => TMessage::binary(b),
        };
        self.inner.send(msg).await.map_err(|e| error_chain(&e))
    }

    /// Cancel-safe: only awaits `StreamExt::next`.
    pub async fn recv(&mut self) -> Incoming {
        loop {
            match self.inner.next().await {
                Some(Ok(TMessage::Text(t))) => return Incoming::Message(Message::Text(t.as_str().to_owned())),
                Some(Ok(TMessage::Binary(b))) => return Incoming::Message(Message::Binary(b.to_vec())),
                // Pings are answered by tungstenite automatically.
                Some(Ok(TMessage::Ping(_) | TMessage::Pong(_) | TMessage::Frame(_))) => continue,
                Some(Ok(TMessage::Close(frame))) => {
                    return Incoming::Closed(match frame {
                        Some(f) if !f.reason.is_empty() => {
                            format!("closed by peer ({}): {}", u16::from(f.code), f.reason)
                        }
                        Some(f) => format!("closed by peer ({})", u16::from(f.code)),
                        None => "closed by peer".into(),
                    });
                }
                Some(Err(e)) => return Incoming::Closed(error_chain(&e)),
                None => return Incoming::Closed("connection closed".into()),
            }
        }
    }

    pub async fn close(&mut self) {
        let _ = self.inner.close(None).await;
    }
}
