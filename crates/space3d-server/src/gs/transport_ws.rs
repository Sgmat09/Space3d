//! WebSocket server transport: a hardened `space3d_net::WsServer` (16 KiB inbound cap, bounded
//! queues, text frames rejected, admission hook for per-IP limits / bans / Origin) turned into
//! a stream of `LinkEvent`s, with one [`WsLinkTx`] per client.

use space3d_net::{ClientId, WsServer, WsServerConfig};
use space3d_replication::LinkId;
use space3d_replication::transport::{CloseCode, Lane, LinkEvent, LinkTx, TransportCaps, TransportError};

/// Server-side WS transport.
#[derive(Clone, Debug)]
pub struct WsServerTransport {
    ws: WsServer,
    caps: TransportCaps,
}

impl WsServerTransport {
    /// Create with limits (`max_message_size` = `limits.max_message_bytes`, `accept_text = false`).
    pub fn new(config: WsServerConfig) -> Self {
        let caps = TransportCaps::stream(config.max_message_size);
        Self {
            ws: WsServer::with_config(config),
            caps,
        }
    }

    /// The underlying server (mount with `HttpServer::ws(path, transport.ws())`).
    pub fn ws(&self) -> &WsServer {
        &self.ws
    }

    /// `LinkId` of a WS client (`ClientId` → `LinkId(id)`).
    pub fn link_id(client: ClientId) -> LinkId {
        LinkId(client.0)
    }

    /// Await the next event (ingress task only; never from a system).
    pub async fn next_event(&self) -> Option<LinkEvent> {
        todo!("ws.recv().await → Opened (with WsLinkTx + ConnectMeta) / Packet (binary) / Closed")
    }
}

/// Sender for one WS client.
#[derive(Debug)]
pub struct WsLinkTx {
    ws: WsServer,
    client: ClientId,
    caps: TransportCaps,
}

impl LinkTx for WsLinkTx {
    fn caps(&self) -> TransportCaps {
        self.caps
    }

    fn send(&self, lane: Lane, packet: Vec<u8>) -> Result<(), TransportError> {
        todo!("ws.send(client, Message::Binary); NetError::Closed → Backpressure/Closed")
    }

    fn backlog_bytes(&self) -> usize {
        self.ws.backlog_bytes(self.client).unwrap_or(0)
    }

    fn close(&self, code: CloseCode, reason: &str) {
        self.ws.disconnect_with(self.client, code.0, reason);
    }

    fn is_open(&self) -> bool {
        self.ws.backlog_bytes(self.client).is_some()
    }
}
