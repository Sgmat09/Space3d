//! WebSocket client transport (native + web) over `space3d_net::WsConnection`.
//!
//! Binary messages only; one WS message = one packet; both lanes share the stream
//! (`TransportCaps::stream`). No `WsConfig::reconnect` (see the module docs of `transport`).
//! The inbound size cap is set with `WsConfig::max_message_size` (default 1 MiB).

use super::{ClientTransport, CloseCode, Connector, Lane, LinkState, TransportCaps, TransportError, TransportStats};
use space3d_net::{WsConfig, WsConnection};
use std::time::Duration;

/// Default inbound cap for clients (server packets are ≤ 64 KiB unreliable, ≤ 1 MiB reliable).
pub const DEFAULT_CLIENT_MAX_MESSAGE: usize = 1 << 20;

/// Opens [`WsClientTransport`]s.
#[derive(Clone, Debug)]
pub struct WsConnector {
    /// Inbound message cap.
    pub max_message_size: usize,
    /// Connect + WS handshake timeout.
    pub connect_timeout: Duration,
}

impl Default for WsConnector {
    fn default() -> Self {
        Self {
            max_message_size: DEFAULT_CLIENT_MAX_MESSAGE,
            connect_timeout: Duration::from_secs(10),
        }
    }
}

impl WsConnector {
    /// The `WsConfig` used for every attempt (no auto-reconnect).
    pub fn ws_config(&self) -> WsConfig {
        WsConfig::default()
            .connect_timeout(self.connect_timeout)
            .max_message_size(self.max_message_size)
    }
}

impl Connector for WsConnector {
    fn connect(&self, url: &str) -> Result<Box<dyn ClientTransport>, TransportError> {
        todo!("WsClient::connect_with(url, self.ws_config()) → WsClientTransport")
    }
}

/// One WebSocket link.
#[derive(Debug)]
pub struct WsClientTransport {
    conn: WsConnection,
    max_packet: usize,
    stats: TransportStats,
}

impl WsClientTransport {
    /// Wrap an open (or opening) connection.
    pub fn new(conn: WsConnection, max_packet: usize) -> Self {
        Self {
            conn,
            max_packet,
            stats: TransportStats::default(),
        }
    }
}

impl ClientTransport for WsClientTransport {
    fn caps(&self) -> TransportCaps {
        TransportCaps::stream(self.max_packet)
    }

    fn state(&self) -> LinkState {
        todo!()
    }

    fn send(&mut self, lane: Lane, packet: Vec<u8>) -> Result<(), TransportError> {
        todo!("unreliable: skip (Ok, count skipped_out) while queued_outgoing() is high")
    }

    fn recv(&mut self) -> Option<Vec<u8>> {
        todo!("binary only; a text frame closes the link")
    }

    fn backlog_bytes(&self) -> usize {
        todo!()
    }

    fn close(&mut self, code: CloseCode, reason: &str) {
        todo!()
    }

    fn stats(&self) -> TransportStats {
        self.stats
    }
}
