//! WebSocket client transport (native + web) over `space3d_net::WsConnection`.
//!
//! Binary messages only; one WS message = one packet; both lanes share the stream
//! (`TransportCaps::stream`). No `WsConfig::reconnect` (see the module docs of `transport`).
//! The inbound size cap is set with `WsConfig::max_message_size` (default 1 MiB).

use super::{ClientTransport, CloseCode, Connector, Lane, LinkState, TransportCaps, TransportError, TransportStats};
use space3d_net::{Message, WsClient, WsConfig, WsConnection, WsState};
use std::collections::VecDeque;
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
        if !(url.starts_with("ws://") || url.starts_with("wss://")) {
            return Err(TransportError::Connect(format!("not a WebSocket URL: {url}")));
        }
        let conn = WsClient::connect_with(url, self.ws_config());
        Ok(Box::new(WsClientTransport::new(conn, self.max_message_size)))
    }
}

/// Unreliable packets are skipped (not queued) while this many bytes wait to be written.
pub const UNRELIABLE_SKIP_BACKLOG: usize = 64 * 1024;

/// One WebSocket link.
#[derive(Debug)]
pub struct WsClientTransport {
    conn: WsConnection,
    max_packet: usize,
    stats: TransportStats,
    /// Sizes of recently queued messages (the newest `queued_outgoing()` of them are the backlog).
    sent_sizes: VecDeque<usize>,
    closed: Option<String>,
}

impl WsClientTransport {
    /// Wrap an open (or opening) connection.
    pub fn new(conn: WsConnection, max_packet: usize) -> Self {
        Self {
            conn,
            max_packet,
            stats: TransportStats::default(),
            sent_sizes: VecDeque::new(),
            closed: None,
        }
    }
}

impl ClientTransport for WsClientTransport {
    fn caps(&self) -> TransportCaps {
        TransportCaps::stream(self.max_packet)
    }

    fn state(&self) -> LinkState {
        if let Some(reason) = &self.closed {
            return LinkState::Closed(reason.clone());
        }
        match self.conn.state() {
            WsState::Connecting => LinkState::Connecting,
            WsState::Open => LinkState::Open,
            WsState::Closed(r) => LinkState::Closed(r),
        }
    }

    fn send(&mut self, lane: Lane, packet: Vec<u8>) -> Result<(), TransportError> {
        if self.closed.is_some() || self.conn.is_closed() {
            return Err(TransportError::Closed);
        }
        if packet.len() > self.max_packet {
            return Err(TransportError::TooLarge {
                len: packet.len(),
                max: self.max_packet,
            });
        }
        if lane == Lane::Unreliable && self.backlog_bytes() > UNRELIABLE_SKIP_BACKLOG {
            self.stats.skipped_out += 1;
            return Ok(());
        }
        let len = packet.len();
        self.conn
            .send(Message::Binary(packet))
            .map_err(|_| TransportError::Closed)?;
        self.sent_sizes.push_back(len);
        let keep = self.conn.queued_outgoing().max(1);
        while self.sent_sizes.len() > keep {
            self.sent_sizes.pop_front();
        }
        self.stats.packets_out += 1;
        self.stats.bytes_out += len as u64;
        Ok(())
    }

    fn recv(&mut self) -> Option<Vec<u8>> {
        if self.closed.is_some() {
            return None;
        }
        match self.conn.try_recv()? {
            Message::Binary(b) => {
                self.stats.packets_in += 1;
                self.stats.bytes_in += b.len() as u64;
                Some(b)
            }
            Message::Text(_) => {
                // The protocol is binary only: a text frame is a protocol violation.
                self.close(CloseCode(1003), "text frames are not accepted");
                None
            }
        }
    }

    fn backlog_bytes(&self) -> usize {
        let queued = self.conn.queued_outgoing();
        self.sent_sizes.iter().rev().take(queued).sum()
    }

    fn close(&mut self, code: CloseCode, reason: &str) {
        if self.closed.is_none() {
            // `space3d_net` closes with 1000 after flushing queued messages; the code is local
            // information (the protocol's `Disconnect` packet carries the real reason).
            self.closed = Some(format!("closed by client ({}): {reason}", code.0));
            self.conn.close();
        }
    }

    fn stats(&self) -> TransportStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_never_reconnects_and_caps_messages() {
        let c = WsConnector::default().ws_config();
        assert_eq!(c.reconnect, None);
        assert_eq!(c.max_message_size, Some(DEFAULT_CLIENT_MAX_MESSAGE));
        assert!(WsConnector::default().connect("http://example.com").is_err());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn refused_connection_closes() {
        let mut t = WsConnector::default().connect("ws://127.0.0.1:1/play").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !matches!(t.state(), LinkState::Closed(_)) {
            assert!(std::time::Instant::now() < deadline, "never closed");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(t.send(Lane::Reliable, vec![1]), Err(TransportError::Closed));
        assert_eq!(t.recv(), None);
    }
}
