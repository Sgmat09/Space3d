//! WebSocket server: accept clients, per-client ids, broadcast, inbound event queue.

use crate::{Message, NetError};
use axum::extract::ws::{CloseFrame, Message as AxMessage, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{ConnectInfo, State};
use axum::response::{IntoResponse, Response};
use axum::routing::MethodRouter;
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Server-assigned id of a connected WebSocket client (unique per [`WsServer`], never reused).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClientId(pub u64);

/// Inbound traffic of a [`WsServer`], polled by engine systems
/// ([`WsServer::try_recv`] / [`WsServer::drain`], or `Events<ServerEvent>` when the
/// server is inserted as a resource alongside [`crate::NetPlugin`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerEvent {
    /// A client finished the handshake. It can be sent to immediately.
    /// `Connected` events are delivered in [`ClientId`] order.
    Connected {
        /// The new client.
        client: ClientId,
        /// Its remote address.
        addr: SocketAddr,
    },
    /// A client sent a message.
    Message {
        /// Sender.
        client: ClientId,
        /// Payload.
        message: Message,
    },
    /// A client left (closed, errored, kicked, or server shutdown).
    Disconnected {
        /// The client that left.
        client: ClientId,
        /// Human-readable reason.
        reason: String,
    },
}

impl ServerEvent {
    /// The client this event is about.
    pub fn client(&self) -> ClientId {
        match self {
            ServerEvent::Connected { client, .. }
            | ServerEvent::Message { client, .. }
            | ServerEvent::Disconnected { client, .. } => *client,
        }
    }
}

enum Outgoing {
    Message(Message),
    Close(String),
}

struct Slot {
    tx: mpsc::UnboundedSender<Outgoing>,
    addr: SocketAddr,
}

struct Inner {
    clients: Mutex<BTreeMap<ClientId, Slot>>,
    events_tx: async_channel::Sender<ServerEvent>,
    events_rx: async_channel::Receiver<ServerEvent>,
    next_id: AtomicU64,
}

/// WebSocket server state, shareable between the network runtime and engine systems.
///
/// Mount it on an [`crate::HttpServer`] with [`crate::HttpServer::ws`], or run it
/// standalone with [`WsServer::listen`]. Cloning shares the same clients and inbox;
/// it can also be inserted as a resource (see [`ServerEvent`]).
///
/// All methods are non-blocking. Inbound events queue until polled.
#[derive(Clone)]
pub struct WsServer {
    inner: Arc<Inner>,
}

impl Default for WsServer {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for WsServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsServer")
            .field("clients", &self.client_count())
            .field("queued_events", &self.inner.events_rx.len())
            .finish()
    }
}

impl WsServer {
    /// A server with no clients yet.
    pub fn new() -> Self {
        let (events_tx, events_rx) = async_channel::unbounded();
        Self {
            inner: Arc::new(Inner {
                clients: Mutex::new(BTreeMap::new()),
                events_tx,
                events_rx,
                next_id: AtomicU64::new(1),
            }),
        }
    }

    /// Serve this WebSocket server on `addr` (any path). Port `0` picks a free port.
    pub fn listen(&self, addr: impl Into<SocketAddr>) -> Result<super::ServerHandle, NetError> {
        super::HttpServer::new()
            .bind(addr)
            .ws("/", self)
            .ws("/{*path}", self)
            .start()
    }

    /// Queue `msg` for one client.
    pub fn send(&self, client: ClientId, msg: impl Into<Message>) -> Result<(), NetError> {
        let clients = self.inner.clients.lock();
        let slot = clients.get(&client).ok_or(NetError::UnknownPeer(client.0))?;
        slot.tx
            .send(Outgoing::Message(msg.into()))
            .map_err(|_| NetError::Closed)
    }

    /// Queue `msg` for every connected client; returns how many it was queued for.
    pub fn broadcast(&self, msg: impl Into<Message>) -> usize {
        self.broadcast_filtered(msg.into(), |_| true)
    }

    /// Queue `msg` for every client except `except`.
    pub fn broadcast_except(&self, except: ClientId, msg: impl Into<Message>) -> usize {
        self.broadcast_filtered(msg.into(), |id| id != except)
    }

    fn broadcast_filtered(&self, msg: Message, keep: impl Fn(ClientId) -> bool) -> usize {
        let clients = self.inner.clients.lock();
        clients
            .iter()
            .filter(|(id, _)| keep(**id))
            .filter(|(_, slot)| slot.tx.send(Outgoing::Message(msg.clone())).is_ok())
            .count()
    }

    /// Close one client's connection (after flushing its queued messages).
    pub fn disconnect(&self, client: ClientId, reason: impl Into<String>) {
        if let Some(slot) = self.inner.clients.lock().get(&client) {
            let _ = slot.tx.send(Outgoing::Close(reason.into()));
        }
    }

    /// Close every client connection. The server keeps accepting new ones.
    pub fn disconnect_all(&self, reason: impl Into<String>) {
        let reason = reason.into();
        for slot in self.inner.clients.lock().values() {
            let _ = slot.tx.send(Outgoing::Close(reason.clone()));
        }
    }

    /// Currently connected clients, in id order.
    pub fn clients(&self) -> Vec<ClientId> {
        self.inner.clients.lock().keys().copied().collect()
    }

    /// Number of connected clients.
    pub fn client_count(&self) -> usize {
        self.inner.clients.lock().len()
    }

    /// Whether `client` is connected.
    pub fn is_connected(&self, client: ClientId) -> bool {
        self.inner.clients.lock().contains_key(&client)
    }

    /// Remote address of a connected client.
    pub fn client_addr(&self, client: ClientId) -> Option<SocketAddr> {
        self.inner.clients.lock().get(&client).map(|s| s.addr)
    }

    /// Next inbound event, if any. Never blocks.
    pub fn try_recv(&self) -> Option<ServerEvent> {
        self.inner.events_rx.try_recv().ok()
    }

    /// All queued inbound events, oldest first.
    pub fn drain(&self) -> Vec<ServerEvent> {
        std::iter::from_fn(|| self.try_recv()).collect()
    }

    /// axum route that upgrades to a WebSocket and runs the client until it
    /// leaves or `shutdown` (the owning HTTP server's token) fires.
    pub(crate) fn upgrade_route(&self, shutdown: CancellationToken) -> MethodRouter {
        axum::routing::get(upgrade).with_state((self.clone(), shutdown))
    }

    async fn run_client(self, socket: WebSocket, addr: SocketAddr, shutdown: CancellationToken) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let id = {
            // Allocate, register and announce atomically: `Connected` events arrive in
            // id order, and the client is sendable by the time anyone sees the event.
            let mut clients = self.inner.clients.lock();
            let id = ClientId(self.inner.next_id.fetch_add(1, Ordering::Relaxed));
            clients.insert(id, Slot { tx, addr });
            let _ = self
                .inner
                .events_tx
                .try_send(ServerEvent::Connected { client: id, addr });
            id
        };
        tracing::debug!(client = id.0, %addr, "websocket client connected");

        let (mut sink, mut stream) = socket.split();
        let reason = loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    let frame = CloseFrame { code: close_code::AWAY, reason: "server shutting down".into() };
                    let _ = sink.send(AxMessage::Close(Some(frame))).await;
                    break "server shutting down".to_owned();
                }
                out = rx.recv() => match out {
                    Some(Outgoing::Message(m)) => {
                        let m = match m {
                            Message::Text(s) => AxMessage::Text(s.into()),
                            Message::Binary(b) => AxMessage::Binary(b.into()),
                        };
                        if let Err(e) = sink.send(m).await {
                            break format!("send failed: {e}");
                        }
                    }
                    Some(Outgoing::Close(reason)) => {
                        let frame = CloseFrame { code: close_code::NORMAL, reason: reason.clone().into() };
                        let _ = sink.send(AxMessage::Close(Some(frame))).await;
                        break reason;
                    }
                    None => break "server dropped".to_owned(),
                },
                inc = stream.next() => match inc {
                    Some(Ok(AxMessage::Text(t))) => {
                        let message = Message::Text(t.as_str().to_owned());
                        let _ = self.inner.events_tx.try_send(ServerEvent::Message { client: id, message });
                    }
                    Some(Ok(AxMessage::Binary(b))) => {
                        let message = Message::Binary(b.to_vec());
                        let _ = self.inner.events_tx.try_send(ServerEvent::Message { client: id, message });
                    }
                    Some(Ok(AxMessage::Ping(_) | AxMessage::Pong(_))) => {}
                    Some(Ok(AxMessage::Close(frame))) => {
                        break match frame {
                            Some(f) if !f.reason.is_empty() => format!("closed by client ({}): {}", f.code, f.reason),
                            Some(f) => format!("closed by client ({})", f.code),
                            None => "closed by client".to_owned(),
                        };
                    }
                    Some(Err(e)) => break format!("connection error: {e}"),
                    None => break "connection closed".to_owned(),
                },
            }
        };
        tracing::debug!(client = id.0, %reason, "websocket client disconnected");
        let mut clients = self.inner.clients.lock();
        clients.remove(&id);
        let _ = self
            .inner
            .events_tx
            .try_send(ServerEvent::Disconnected { client: id, reason });
    }
}

async fn upgrade(
    State((server, shutdown)): State<(WsServer, CancellationToken)>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    upgrade: WebSocketUpgrade,
) -> Response {
    if shutdown.is_cancelled() {
        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    upgrade.on_upgrade(move |socket| server.run_client(socket, addr, shutdown))
}
