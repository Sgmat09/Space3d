//! In-process network with a virtual clock and per-link [`ConditionedQueue`]s: server and
//! clients in one process, fully deterministic for a given seed (AGENTS rule 4).
//!
//! ```ignore
//! let clock = ManualClock::new();
//! let net = MemoryNetwork::new(7, NetClockHandle(Arc::new(clock.clone())), Semantics::Stream);
//! let server = net.server_endpoint();            // LinkEvent source for the room / gateway
//! let connector = net.connector(LinkProfile::symmetric(LinkConditions::mobile_4g()));
//! let mut client = connector.connect("mem://room")?; // appears as LinkEvent::Opened on `server`
//! clock.advance(Duration::from_millis(16));
//! net.pump();                                      // deliver everything due
//! ```

use super::conditioner::{ConditionedQueue, LinkProfile, Semantics};
use super::{
    ClientTransport, CloseCode, Connector, Lane, LinkEvent, LinkState, LinkTx, TransportCaps, TransportError,
    TransportStats,
};
use crate::LinkId;
use crate::time::NetClockHandle;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

/// Default memory-link packet cap (matches the WS gateway's inbound cap).
pub const DEFAULT_MEMORY_MAX_PACKET: usize = 16 * 1024;

struct LinkShared {
    id: LinkId,
    up: ConditionedQueue,
    down: ConditionedQueue,
    to_client: VecDeque<Vec<u8>>,
    client_state: LinkState,
    server_closed: Option<String>,
    partitioned_until: Option<Duration>,
    caps: TransportCaps,
    client_stats: TransportStats,
}

struct NetShared {
    clock: NetClockHandle,
    semantics: Semantics,
    seed: u64,
    next_link: u64,
    links: Vec<Arc<Mutex<LinkShared>>>,
    server_events: VecDeque<LinkEvent>,
}

/// The whole in-memory network (clone = same network).
#[derive(Clone)]
pub struct MemoryNetwork {
    shared: Arc<Mutex<NetShared>>,
}

impl MemoryNetwork {
    /// New network. Every link gets a seed derived from `seed` and its id.
    pub fn new(seed: u64, clock: NetClockHandle, semantics: Semantics) -> Self {
        Self {
            shared: Arc::new(Mutex::new(NetShared {
                clock,
                semantics,
                seed,
                next_link: 1,
                links: Vec::new(),
                server_events: VecDeque::new(),
            })),
        }
    }

    /// Deliver every packet due at the clock's current time (both directions, all links),
    /// in deterministic `(deliver_at, link, order)` order.
    pub fn pump(&self) {
        todo!()
    }

    /// Earliest pending delivery time over all links.
    pub fn next_delivery(&self) -> Option<Duration> {
        todo!()
    }

    /// Change a link's conditions.
    pub fn set_profile(&self, link: LinkId, profile: LinkProfile) {
        todo!()
    }

    /// Drop everything on `link` in both directions for `duration` (then the link closes as a
    /// timeout would; used for resume tests).
    pub fn partition(&self, link: LinkId, duration: Duration) {
        todo!()
    }

    /// Close a link from the network side (both ends see `Closed`).
    pub fn sever(&self, link: LinkId, reason: &str) {
        todo!()
    }

    /// The server-side event source.
    pub fn server_endpoint(&self) -> MemoryServerEndpoint {
        MemoryServerEndpoint { net: self.clone() }
    }

    /// A connector whose links use `profile`.
    pub fn connector(&self, profile: LinkProfile) -> MemoryConnector {
        MemoryConnector {
            net: self.clone(),
            profile,
            max_packet: DEFAULT_MEMORY_MAX_PACKET,
        }
    }

    /// Ids of all links ever opened, in order.
    pub fn links(&self) -> Vec<LinkId> {
        self.shared.lock().links.iter().map(|l| l.lock().id).collect()
    }

    /// Hash of every packet delivered so far (determinism checks).
    pub fn wire_digest(&self) -> u64 {
        todo!()
    }
}

/// Server side of a [`MemoryNetwork`]: yields [`LinkEvent`]s (after [`MemoryNetwork::pump`]).
#[derive(Clone)]
pub struct MemoryServerEndpoint {
    net: MemoryNetwork,
}

impl MemoryServerEndpoint {
    /// Next event. Never blocks.
    pub fn poll(&self) -> Option<LinkEvent> {
        self.net.shared.lock().server_events.pop_front()
    }

    /// All pending events.
    pub fn drain(&self) -> Vec<LinkEvent> {
        self.net.shared.lock().server_events.drain(..).collect()
    }
}

/// Opens memory links.
#[derive(Clone)]
pub struct MemoryConnector {
    net: MemoryNetwork,
    profile: LinkProfile,
    max_packet: usize,
}

impl MemoryConnector {
    /// Override the packet cap.
    pub fn max_packet(mut self, bytes: usize) -> Self {
        self.max_packet = bytes;
        self
    }
}

impl Connector for MemoryConnector {
    fn connect(&self, url: &str) -> Result<Box<dyn ClientTransport>, TransportError> {
        todo!("create LinkShared, push LinkEvent::Opened (after the up-link latency), return MemoryClientTransport")
    }
}

/// Client end of a memory link.
pub struct MemoryClientTransport {
    link: Arc<Mutex<LinkShared>>,
    net: MemoryNetwork,
}

impl MemoryClientTransport {
    /// This link's id (as seen by the server).
    pub fn link_id(&self) -> LinkId {
        self.link.lock().id
    }
}

impl ClientTransport for MemoryClientTransport {
    fn caps(&self) -> TransportCaps {
        self.link.lock().caps
    }

    fn state(&self) -> LinkState {
        self.link.lock().client_state.clone()
    }

    fn send(&mut self, lane: Lane, packet: Vec<u8>) -> Result<(), TransportError> {
        todo!()
    }

    fn recv(&mut self) -> Option<Vec<u8>> {
        todo!()
    }

    fn backlog_bytes(&self) -> usize {
        self.link.lock().up.in_flight_bytes()
    }

    fn close(&mut self, code: CloseCode, reason: &str) {
        todo!()
    }

    fn stats(&self) -> TransportStats {
        self.link.lock().client_stats
    }
}

/// Server end of a memory link (handed out in `LinkEvent::Opened`).
pub struct MemoryLinkTx {
    link: Arc<Mutex<LinkShared>>,
    net: MemoryNetwork,
}

impl LinkTx for MemoryLinkTx {
    fn caps(&self) -> TransportCaps {
        self.link.lock().caps
    }

    fn send(&self, lane: Lane, packet: Vec<u8>) -> Result<(), TransportError> {
        todo!()
    }

    fn backlog_bytes(&self) -> usize {
        self.link.lock().down.in_flight_bytes()
    }

    fn close(&self, code: CloseCode, reason: &str) {
        todo!()
    }

    fn is_open(&self) -> bool {
        self.link.lock().server_closed.is_none()
    }
}
