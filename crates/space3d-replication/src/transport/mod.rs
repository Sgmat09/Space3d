//! Transports: how packets move between a client and a room.
//!
//! A transport offers two lanes. **Reliability always comes from the transport** — this crate
//! never retransmits:
//!
//! | lane | WebSocket (v1, all targets) | QUIC / WebTransport (deferred) | memory (tests) |
//! |---|---|---|---|
//! | [`Lane::Reliable`] | the TCP stream | one ordered stream | ordered; loss = head-of-line stall |
//! | [`Lane::Unreliable`] | same stream; the *sender* skips optional packets when backlogged | datagrams (≤ ~1150 B) | loss, jitter, reorder, duplicates |
//!
//! Protocol sequence numbers/acks exist only on true-datagram transports and only to advance
//! per-entity baselines (see `protocol`).
//!
//! * Client side: a [`Connector`] opens fresh [`ClientTransport`]s (never auto-reconnecting —
//!   `space3d_net::WsConfig::reconnect` would replay stale buffered inputs before the resume
//!   `Hello`; `NetClient` runs its own reconnect loop).
//! * Server side: every transport turns into one stream of [`LinkEvent`]s consumed by the
//!   gateway (`space3d-server`) or the harness; each link is written through a [`LinkTx`].
//!
//! All methods are non-blocking. `ClientTransport` and `LinkTx` are `Send + Sync` on every
//! target (the browser socket lives inside a driver future, as in `space3d-net`), so they can
//! be stored in `Resources`.

pub mod conditioner;
pub mod memory;
pub mod ws;

use crate::LinkId;
use std::net::SocketAddr;
use std::sync::Arc;

/// Delivery lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Lane {
    /// Reliable, ordered.
    Reliable,
    /// Unreliable, sequenced (stale packets are dropped by the receiver).
    Unreliable,
}

/// What a transport can do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransportCaps {
    /// `true` for datagram transports: `Unreliable` may be lost/reordered and packets carry
    /// seq/acks. `false` for streams (WebSocket): `Unreliable` rides the reliable stream.
    pub true_datagrams: bool,
    /// Largest packet on the unreliable lane.
    pub max_unreliable: usize,
    /// Largest packet on the reliable lane.
    pub max_reliable: usize,
}

impl TransportCaps {
    /// WebSocket-like stream with a single size cap.
    pub const fn stream(max_packet: usize) -> Self {
        Self {
            true_datagrams: false,
            max_unreliable: max_packet,
            max_reliable: max_packet,
        }
    }

    /// Datagram transport (unreliable ≤ `max_datagram`, reliable ≤ `max_reliable`).
    pub const fn datagram(max_datagram: usize, max_reliable: usize) -> Self {
        Self {
            true_datagrams: true,
            max_unreliable: max_datagram,
            max_reliable,
        }
    }
}

/// Lifecycle of one link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkState {
    /// Opening.
    Connecting,
    /// Usable.
    Open,
    /// Closed, with a reason (`"closed (1009): message too big"`, ...).
    Closed(String),
}

/// Transport failures.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    /// The link is closed.
    #[error("link closed")]
    Closed,
    /// The packet exceeds the lane's maximum size.
    #[error("packet of {len} bytes exceeds {max}")]
    TooLarge {
        /// Packet size.
        len: usize,
        /// Lane limit.
        max: usize,
    },
    /// The outbound backlog is full (reliable) — the caller should disconnect the peer.
    #[error("backpressure")]
    Backpressure,
    /// Could not open the link.
    #[error("connect failed: {0}")]
    Connect(String),
    /// Not available on this platform.
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
}

/// WebSocket-compatible close codes used by the protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CloseCode(pub u16);

impl CloseCode {
    /// 1000 normal.
    pub const NORMAL: CloseCode = CloseCode(1000);
    /// 1001 going away (server shutdown).
    pub const GOING_AWAY: CloseCode = CloseCode(1001);
    /// 1008 policy violation (rate limits, strikes, slow consumer).
    pub const POLICY: CloseCode = CloseCode(1008);
    /// 1009 message too big.
    pub const TOO_BIG: CloseCode = CloseCode(1009);
    /// 1011 internal error.
    pub const INTERNAL: CloseCode = CloseCode(1011);
}

/// Traffic counters of a link or transport.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransportStats {
    /// Bytes sent.
    pub bytes_out: u64,
    /// Bytes received.
    pub bytes_in: u64,
    /// Packets sent.
    pub packets_out: u64,
    /// Packets received.
    pub packets_in: u64,
    /// Unreliable packets dropped by the sender because the link was backlogged.
    pub skipped_out: u64,
}

/// Client end of one link. See the module docs.
pub trait ClientTransport: Send + Sync + 'static {
    /// Capabilities.
    fn caps(&self) -> TransportCaps;
    /// Current state.
    fn state(&self) -> LinkState;
    /// Queue a packet. Never blocks.
    fn send(&mut self, lane: Lane, packet: Vec<u8>) -> Result<(), TransportError>;
    /// Next received packet. Never blocks.
    fn recv(&mut self) -> Option<Vec<u8>>;
    /// Bytes queued but not yet written to the network.
    fn backlog_bytes(&self) -> usize;
    /// Close the link.
    fn close(&mut self, code: CloseCode, reason: &str);
    /// Counters.
    fn stats(&self) -> TransportStats;
}

/// Opens client links. `NetClient` calls it once per (re)connect attempt.
pub trait Connector: Send + Sync + 'static {
    /// Start opening a link to `url` (the returned transport starts in `Connecting`).
    fn connect(&self, url: &str) -> Result<Box<dyn ClientTransport>, TransportError>;
}

/// What the server knows about a link's peer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeerInfo {
    /// Socket address (a proxy's when behind one).
    pub addr: Option<SocketAddr>,
    /// Client IP after trusted-proxy `X-Forwarded-For` resolution (set by the gateway).
    pub client_ip: Option<std::net::IpAddr>,
    /// Browser `Origin`, if any.
    pub origin: Option<String>,
    /// `User-Agent`, if any.
    pub user_agent: Option<String>,
    /// Transport name (`"ws"`, `"memory"`, `"quic"`).
    pub transport: &'static str,
}

/// Server-side sender of one link (cheap to clone behind `Arc`). Never blocks.
pub trait LinkTx: Send + Sync + 'static {
    /// Capabilities.
    fn caps(&self) -> TransportCaps;
    /// Queue a packet. `Err(Backpressure)` when the outbound backlog cap is reached.
    fn send(&self, lane: Lane, packet: Vec<u8>) -> Result<(), TransportError>;
    /// Bytes queued but not yet written (flow control: skip snapshots while this grows).
    fn backlog_bytes(&self) -> usize;
    /// Close with a code (after flushing queued packets).
    fn close(&self, code: CloseCode, reason: &str);
    /// Whether the link is still open.
    fn is_open(&self) -> bool;
}

/// Server-side link lifecycle, produced by a transport and consumed by the gateway / harness.
#[derive(Clone)]
pub enum LinkEvent {
    /// A peer connected (not authenticated yet).
    Opened {
        /// Link id (unique per process).
        link: LinkId,
        /// Peer metadata.
        peer: PeerInfo,
        /// Sender for this link.
        tx: Arc<dyn LinkTx>,
    },
    /// A packet arrived (already size-capped by the transport).
    Packet {
        /// Link.
        link: LinkId,
        /// Lane it arrived on (always `Reliable` for streams).
        lane: Lane,
        /// Bytes.
        data: Vec<u8>,
    },
    /// The link closed.
    Closed {
        /// Link.
        link: LinkId,
        /// Reason.
        reason: String,
    },
}

impl std::fmt::Debug for LinkEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkEvent::Opened { link, peer, .. } => f
                .debug_struct("Opened")
                .field("link", link)
                .field("peer", peer)
                .finish_non_exhaustive(),
            LinkEvent::Packet { link, lane, data } => f
                .debug_struct("Packet")
                .field("link", link)
                .field("lane", lane)
                .field("len", &data.len())
                .finish(),
            LinkEvent::Closed { link, reason } => f
                .debug_struct("Closed")
                .field("link", link)
                .field("reason", reason)
                .finish(),
        }
    }
}
