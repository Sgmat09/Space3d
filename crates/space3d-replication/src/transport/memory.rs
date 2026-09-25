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
//!
//! Link timeline: `connect` at `t` → the server sees `Opened` at `t + up.latency`, the client
//! sees `Open` at `t + up.latency + down.latency`. Closing flushes: the peer sees the close
//! after every packet sent before it.

use super::conditioner::{ConditionedQueue, LinkProfile, Semantics};
use super::{
    ClientTransport, CloseCode, Connector, Lane, LinkEvent, LinkState, LinkTx, PeerInfo, TransportCaps, TransportError,
    TransportStats,
};
use crate::LinkId;
use crate::time::NetClockHandle;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

/// Default memory-link packet cap client → server (matches the WS gateway's inbound cap).
pub const DEFAULT_MEMORY_MAX_PACKET: usize = 16 * 1024;

/// Default memory-link packet cap server → client (matches the WS gateway's outbound cap and
/// the client's 1 MiB inbound cap).
pub const DEFAULT_MEMORY_MAX_DOWN_PACKET: usize = 1 << 20;

/// Unreliable packet cap of memory links with [`Semantics::Datagram`] (a QUIC datagram).
pub const MEMORY_MAX_DATAGRAM: usize = 1200;

struct LinkShared {
    id: LinkId,
    up: ConditionedQueue,
    down: ConditionedQueue,
    to_client: VecDeque<Vec<u8>>,
    client_state: LinkState,
    server_open_at: Duration,
    server_opened: bool,
    client_open_at: Duration,
    /// Server closed its end (no more server sends); the client sees `Closed` once the down
    /// queue drained and the time passed.
    server_closed: Option<String>,
    client_close_at: Option<(Duration, String)>,
    /// Client closed its end; the server sees `LinkEvent::Closed` once the up queue drained.
    client_closed: bool,
    server_close_at: Option<(Duration, String)>,
    server_close_emitted: bool,
    partitioned_until: Option<Duration>,
    /// Client-side (client → server) caps.
    client_caps: TransportCaps,
    /// Server-side (server → client) caps.
    server_caps: TransportCaps,
    client_stats: TransportStats,
    server_stats: TransportStats,
    tx: Option<Arc<dyn LinkTx>>,
}

impl LinkShared {
    fn sever(&mut self, now: Duration, reason: &str) {
        self.up.clear();
        self.down.clear();
        if !matches!(self.client_state, LinkState::Closed(_)) {
            self.client_state = LinkState::Closed(reason.to_owned());
        }
        self.client_closed = true;
        self.client_close_at = None;
        if self.server_closed.is_none() {
            self.server_closed = Some(reason.to_owned());
        }
        if !self.server_close_emitted && self.server_close_at.is_none() {
            self.server_close_at = Some((now, reason.to_owned()));
        }
        self.partitioned_until = None;
    }
}

struct NetShared {
    clock: NetClockHandle,
    semantics: Semantics,
    seed: u64,
    next_link: u64,
    links: Vec<Arc<Mutex<LinkShared>>>,
    server_events: VecDeque<LinkEvent>,
    digest: u64,
    delivered: u64,
}

/// The whole in-memory network (clone = same network).
#[derive(Clone)]
pub struct MemoryNetwork {
    shared: Arc<Mutex<NetShared>>,
}

impl std::fmt::Debug for MemoryNetwork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.shared.lock();
        f.debug_struct("MemoryNetwork")
            .field("semantics", &s.semantics)
            .field("links", &s.links.len())
            .field("delivered", &s.delivered)
            .finish()
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

enum ServerSide {
    Opened(LinkId, Arc<dyn LinkTx>),
    Packet(LinkId, Lane, Vec<u8>),
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
                digest: FNV_OFFSET,
                delivered: 0,
            })),
        }
    }

    /// The clock driving the network.
    pub fn clock(&self) -> NetClockHandle {
        self.shared.lock().clock.clone()
    }

    /// Delivery semantics.
    pub fn semantics(&self) -> Semantics {
        self.shared.lock().semantics
    }

    fn now(&self) -> Duration {
        self.shared.lock().clock.now()
    }

    fn find(&self, link: LinkId) -> Option<Arc<Mutex<LinkShared>>> {
        let s = self.shared.lock();
        s.links.iter().find(|l| l.lock().id == link).cloned()
    }

    /// Deliver every packet due at the clock's current time (both directions, all links),
    /// in deterministic `(deliver_at, link, order)` order.
    pub fn pump(&self) {
        let (now, links) = {
            let s = self.shared.lock();
            (s.clock.now(), s.links.clone())
        };
        // (time, link index, sub-order, event)
        let mut server_side: Vec<(Duration, usize, u64, ServerSide)> = Vec::new();
        let mut digest_items: Vec<(Duration, usize, u8, u64, Vec<u8>)> = Vec::new();
        let mut closes: Vec<LinkEvent> = Vec::new();
        for (idx, link) in links.iter().enumerate() {
            let mut l = link.lock();
            if let Some(until) = l.partitioned_until {
                l.up.clear();
                l.down.clear();
                if now >= until {
                    l.sever(now, "timed out (network partition)");
                }
            }
            if !l.server_opened && now >= l.server_open_at && l.server_close_at.is_none() && !l.client_closed {
                l.server_opened = true;
                if let Some(tx) = l.tx.clone() {
                    server_side.push((l.server_open_at, idx, 0, ServerSide::Opened(l.id, tx)));
                }
            }
            if l.client_state == LinkState::Connecting && now >= l.client_open_at {
                l.client_state = LinkState::Open;
            }
            // Up: client → server.
            if l.server_opened {
                let mut due = Vec::new();
                l.up.pop_due_timed(now, &mut due);
                for (i, (at, lane, bytes)) in due.into_iter().enumerate() {
                    if l.server_closed.is_some() {
                        continue;
                    }
                    l.server_stats.packets_in += 1;
                    l.server_stats.bytes_in += bytes.len() as u64;
                    let at = at.max(l.server_open_at);
                    digest_items.push((at, idx, 0, i as u64, bytes.clone()));
                    server_side.push((at, idx, 1 + i as u64, ServerSide::Packet(l.id, lane, bytes)));
                }
            }
            // Down: server → client.
            let mut due = Vec::new();
            l.down.pop_due_timed(now, &mut due);
            for (i, (at, _lane, bytes)) in due.into_iter().enumerate() {
                if l.client_closed {
                    continue;
                }
                l.client_stats.packets_in += 1;
                l.client_stats.bytes_in += bytes.len() as u64;
                digest_items.push((at, idx, 1, i as u64, bytes.clone()));
                l.to_client.push_back(bytes);
            }
            // Deferred closes (after the queue in that direction drained).
            if let Some((at, reason)) = l.client_close_at.clone()
                && now >= at
                && l.down.is_empty()
            {
                l.client_close_at = None;
                if !matches!(l.client_state, LinkState::Closed(_)) {
                    l.client_state = LinkState::Closed(reason);
                }
            }
            if let Some((at, reason)) = l.server_close_at.clone()
                && now >= at
                && (l.up.is_empty() || l.server_closed.is_some())
            {
                l.server_close_at = None;
                if !l.server_close_emitted {
                    l.server_close_emitted = true;
                    if l.server_opened {
                        closes.push(LinkEvent::Closed { link: l.id, reason });
                    }
                }
                l.up.clear();
            }
        }
        server_side.sort_by_key(|(t, idx, sub, _)| (*t, *idx, *sub));
        digest_items.sort_by_key(|(t, idx, dir, sub, _)| (*t, *idx, *dir, *sub));
        let mut s = self.shared.lock();
        for (_, idx, dir, _, bytes) in &digest_items {
            let mut h = s.digest;
            for b in (*idx as u64)
                .to_le_bytes()
                .iter()
                .chain(std::iter::once(dir))
                .chain((bytes.len() as u64).to_le_bytes().iter())
                .chain(bytes.iter())
            {
                h ^= u64::from(*b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
            s.digest = h;
            s.delivered += 1;
        }
        for (_, _, _, ev) in server_side {
            s.server_events.push_back(match ev {
                ServerSide::Opened(link, tx) => LinkEvent::Opened {
                    link,
                    peer: PeerInfo {
                        transport: "memory",
                        ..Default::default()
                    },
                    tx,
                },
                ServerSide::Packet(link, lane, data) => LinkEvent::Packet { link, lane, data },
            });
        }
        s.server_events.extend(closes);
    }

    /// Earliest pending delivery time over all links.
    pub fn next_delivery(&self) -> Option<Duration> {
        let links = self.shared.lock().links.clone();
        let mut best: Option<Duration> = None;
        let mut consider = |t: Option<Duration>| {
            if let Some(t) = t {
                best = Some(best.map_or(t, |b| b.min(t)));
            }
        };
        for link in links {
            let l = link.lock();
            consider(l.up.next_delivery());
            consider(l.down.next_delivery());
            if !l.server_opened && !l.client_closed {
                consider(Some(l.server_open_at));
            }
            if l.client_state == LinkState::Connecting {
                consider(Some(l.client_open_at));
            }
            consider(l.partitioned_until);
            consider(l.client_close_at.as_ref().map(|c| c.0));
            consider(l.server_close_at.as_ref().map(|c| c.0));
        }
        best
    }

    /// Change a link's conditions.
    pub fn set_profile(&self, link: LinkId, profile: LinkProfile) {
        if let Some(l) = self.find(link) {
            let mut l = l.lock();
            l.up.set_conditions(profile.up);
            l.down.set_conditions(profile.down);
        }
    }

    /// Drop everything on `link` in both directions for `duration` (then the link closes as a
    /// timeout would; used for resume tests).
    pub fn partition(&self, link: LinkId, duration: Duration) {
        let now = self.now();
        if let Some(l) = self.find(link) {
            let mut l = l.lock();
            l.up.clear();
            l.down.clear();
            l.partitioned_until = Some(now + duration);
        }
    }

    /// Close a link from the network side (both ends see `Closed`).
    pub fn sever(&self, link: LinkId, reason: &str) {
        let now = self.now();
        if let Some(l) = self.find(link) {
            l.lock().sever(now, reason);
        }
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
            max_down_packet: DEFAULT_MEMORY_MAX_DOWN_PACKET,
        }
    }

    /// Ids of all links ever opened, in order.
    pub fn links(&self) -> Vec<LinkId> {
        self.shared.lock().links.iter().map(|l| l.lock().id).collect()
    }

    /// Hash of every packet delivered so far (determinism checks).
    pub fn wire_digest(&self) -> u64 {
        self.shared.lock().digest
    }

    /// Number of packets delivered so far.
    pub fn delivered(&self) -> u64 {
        self.shared.lock().delivered
    }

    /// Server-side traffic counters of a link.
    pub fn server_stats(&self, link: LinkId) -> Option<TransportStats> {
        self.find(link).map(|l| l.lock().server_stats)
    }

    /// Server → client bytes queued on `link` and not yet delivered (what the server's
    /// `LinkTx::backlog_bytes` reports).
    pub fn down_backlog(&self, link: LinkId) -> Option<usize> {
        let now = self.now();
        self.find(link).map(|l| l.lock().down.backlog_bytes(now))
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
    max_down_packet: usize,
}

impl MemoryConnector {
    /// Override the client → server packet cap.
    pub fn max_packet(mut self, bytes: usize) -> Self {
        self.max_packet = bytes;
        self
    }

    /// Override the server → client packet cap.
    pub fn max_down_packet(mut self, bytes: usize) -> Self {
        self.max_down_packet = bytes;
        self
    }

    /// Open a link and return the concrete transport (tests that need [`MemoryClientTransport::link_id`]).
    pub fn connect_memory(&self) -> MemoryClientTransport {
        let mut s = self.net.shared.lock();
        let id = LinkId(s.next_link);
        s.next_link += 1;
        let now = s.clock.now();
        let seed = s.seed ^ id.0.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let semantics = s.semantics;
        let caps_of = |max: usize| match semantics {
            Semantics::Stream => TransportCaps::stream(max),
            Semantics::Datagram => TransportCaps::datagram(MEMORY_MAX_DATAGRAM.min(max), max),
        };
        let (client_caps, server_caps) = (caps_of(self.max_packet), caps_of(self.max_down_packet));
        let server_open_at = now + self.profile.up.latency;
        let link = Arc::new(Mutex::new(LinkShared {
            id,
            up: ConditionedQueue::new(self.profile.up.clone(), semantics, seed ^ 0x7570),
            down: ConditionedQueue::new(self.profile.down.clone(), semantics, seed ^ 0x646f_776e),
            to_client: VecDeque::new(),
            client_state: LinkState::Connecting,
            server_open_at,
            server_opened: false,
            client_open_at: server_open_at + self.profile.down.latency,
            server_closed: None,
            client_close_at: None,
            client_closed: false,
            server_close_at: None,
            server_close_emitted: false,
            partitioned_until: None,
            client_caps,
            server_caps,
            client_stats: TransportStats::default(),
            server_stats: TransportStats::default(),
            tx: None,
        }));
        let tx: Arc<dyn LinkTx> = Arc::new(MemoryLinkTx {
            link: link.clone(),
            net: self.net.clone(),
        });
        link.lock().tx = Some(tx);
        s.links.push(link.clone());
        MemoryClientTransport {
            link,
            net: self.net.clone(),
        }
    }
}

impl Connector for MemoryConnector {
    fn connect(&self, _url: &str) -> Result<Box<dyn ClientTransport>, TransportError> {
        Ok(Box::new(self.connect_memory()))
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
        self.link.lock().client_caps
    }

    fn state(&self) -> LinkState {
        self.link.lock().client_state.clone()
    }

    fn send(&mut self, lane: Lane, packet: Vec<u8>) -> Result<(), TransportError> {
        let now = self.net.now();
        let mut l = self.link.lock();
        if l.client_closed || matches!(l.client_state, LinkState::Closed(_)) {
            return Err(TransportError::Closed);
        }
        let max = match lane {
            Lane::Reliable => l.client_caps.max_reliable,
            Lane::Unreliable => l.client_caps.max_unreliable,
        };
        if packet.len() > max {
            return Err(TransportError::TooLarge { len: packet.len(), max });
        }
        l.client_stats.packets_out += 1;
        l.client_stats.bytes_out += packet.len() as u64;
        if l.partitioned_until.is_some() {
            return Ok(());
        }
        l.up.push(now, lane, packet)
    }

    fn recv(&mut self) -> Option<Vec<u8>> {
        self.link.lock().to_client.pop_front()
    }

    fn backlog_bytes(&self) -> usize {
        let now = self.net.now();
        self.link.lock().up.backlog_bytes(now)
    }

    fn close(&mut self, code: CloseCode, reason: &str) {
        let now = self.net.now();
        let mut l = self.link.lock();
        if l.client_closed {
            return;
        }
        l.client_closed = true;
        l.client_state = LinkState::Closed(format!("closed by client ({}): {reason}", code.0));
        l.to_client.clear();
        if !l.server_close_emitted {
            let at = now + l.up.conditions().latency;
            l.server_close_at = Some((at, format!("closed by peer ({}): {reason}", code.0)));
        }
    }

    fn stats(&self) -> TransportStats {
        self.link.lock().client_stats
    }
}

impl Drop for MemoryClientTransport {
    fn drop(&mut self) {
        self.close(CloseCode::NORMAL, "dropped");
    }
}

/// Server end of a memory link (handed out in `LinkEvent::Opened`).
pub struct MemoryLinkTx {
    link: Arc<Mutex<LinkShared>>,
    net: MemoryNetwork,
}

impl LinkTx for MemoryLinkTx {
    fn caps(&self) -> TransportCaps {
        self.link.lock().server_caps
    }

    fn send(&self, lane: Lane, packet: Vec<u8>) -> Result<(), TransportError> {
        let now = self.net.now();
        let mut l = self.link.lock();
        if l.server_closed.is_some() {
            return Err(TransportError::Closed);
        }
        let max = match lane {
            Lane::Reliable => l.server_caps.max_reliable,
            Lane::Unreliable => l.server_caps.max_unreliable,
        };
        if packet.len() > max {
            return Err(TransportError::TooLarge { len: packet.len(), max });
        }
        l.server_stats.packets_out += 1;
        l.server_stats.bytes_out += packet.len() as u64;
        if l.partitioned_until.is_some() {
            return Ok(());
        }
        l.down.push(now, lane, packet)
    }

    fn backlog_bytes(&self) -> usize {
        let now = self.net.now();
        self.link.lock().down.backlog_bytes(now)
    }

    fn close(&self, code: CloseCode, reason: &str) {
        let now = self.net.now();
        let mut l = self.link.lock();
        if l.server_closed.is_some() {
            return;
        }
        let text = format!("closed by peer ({}): {reason}", code.0);
        l.server_closed = Some(text.clone());
        let at = now + l.down.conditions().latency;
        l.client_close_at = Some((at, text.clone()));
        if !l.server_close_emitted {
            l.server_close_at = Some((now, text));
        }
    }

    fn is_open(&self) -> bool {
        self.link.lock().server_closed.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::ManualClock;
    use crate::transport::conditioner::LinkConditions;

    fn net(sem: Semantics) -> (ManualClock, MemoryNetwork) {
        let clock = ManualClock::new();
        let n = MemoryNetwork::new(1, NetClockHandle(Arc::new(clock.clone())), sem);
        (clock, n)
    }

    fn opened(events: &[LinkEvent]) -> Arc<dyn LinkTx> {
        events
            .iter()
            .find_map(|e| match e {
                LinkEvent::Opened { tx, .. } => Some(tx.clone()),
                _ => None,
            })
            .expect("opened")
    }

    #[test]
    fn link_lifecycle_with_latency() {
        let (clock, n) = net(Semantics::Stream);
        let ep = n.server_endpoint();
        let lat = LinkConditions {
            latency: Duration::from_millis(10),
            ..LinkConditions::perfect()
        };
        let mut c = n.connector(LinkProfile::symmetric(lat)).connect_memory();
        assert_eq!(c.state(), LinkState::Connecting);
        c.send(Lane::Reliable, vec![1]).unwrap();
        n.pump();
        assert!(ep.drain().is_empty());
        clock.advance(Duration::from_millis(10));
        n.pump();
        let ev = ep.drain();
        assert!(matches!(ev[0], LinkEvent::Opened { .. }));
        assert!(matches!(&ev[1], LinkEvent::Packet { data, .. } if data == &vec![1]));
        let tx = opened(&ev);
        assert_eq!(c.state(), LinkState::Connecting);
        clock.advance(Duration::from_millis(10));
        n.pump();
        assert_eq!(c.state(), LinkState::Open);
        tx.send(Lane::Unreliable, vec![2]).unwrap();
        tx.close(CloseCode::POLICY, "bye");
        assert!(!tx.is_open());
        assert_eq!(tx.send(Lane::Reliable, vec![3]), Err(TransportError::Closed));
        n.pump();
        // The server sees its own close as an event (gateway bookkeeping).
        assert!(matches!(ep.drain().as_slice(), [LinkEvent::Closed { .. }]));
        clock.advance(Duration::from_millis(10));
        n.pump();
        // Flushed before close.
        assert_eq!(c.recv(), Some(vec![2]));
        assert!(matches!(c.state(), LinkState::Closed(r) if r.contains("1008")));
        assert_eq!(c.send(Lane::Reliable, vec![1]), Err(TransportError::Closed));
    }

    #[test]
    fn client_close_reaches_server_after_packets() {
        let (clock, n) = net(Semantics::Stream);
        let ep = n.server_endpoint();
        let mut c = n.connector(LinkProfile::default()).connect_memory();
        n.pump();
        let _ = ep.drain();
        c.send(Lane::Reliable, vec![7]).unwrap();
        c.close(CloseCode::NORMAL, "leaving");
        clock.advance(Duration::from_millis(1));
        n.pump();
        let ev = ep.drain();
        assert!(matches!(&ev[0], LinkEvent::Packet { data, .. } if data == &vec![7]));
        assert!(matches!(&ev[1], LinkEvent::Closed { reason, .. } if reason.contains("leaving")));
    }

    #[test]
    fn partition_then_timeout_close() {
        let (clock, n) = net(Semantics::Stream);
        let ep = n.server_endpoint();
        let mut c = n.connector(LinkProfile::default()).connect_memory();
        n.pump();
        let tx = opened(&ep.drain());
        n.partition(c.link_id(), Duration::from_secs(1));
        c.send(Lane::Reliable, vec![1]).unwrap();
        tx.send(Lane::Reliable, vec![2]).unwrap();
        clock.advance(Duration::from_millis(500));
        n.pump();
        assert!(ep.drain().is_empty());
        assert_eq!(c.recv(), None);
        clock.advance(Duration::from_millis(500));
        n.pump();
        assert!(matches!(ep.drain().as_slice(), [LinkEvent::Closed { .. }]));
        assert!(matches!(c.state(), LinkState::Closed(_)));
    }

    #[test]
    fn digest_is_deterministic() {
        let run = |seed: u64| {
            let clock = ManualClock::new();
            let n = MemoryNetwork::new(seed, NetClockHandle(Arc::new(clock.clone())), Semantics::Datagram);
            let ep = n.server_endpoint();
            let mut c = n
                .connector(LinkProfile::symmetric(LinkConditions::bad_wifi()))
                .connect_memory();
            let mut tx = None;
            for i in 0..300u32 {
                clock.advance(Duration::from_millis(5));
                n.pump();
                for e in ep.drain() {
                    if let LinkEvent::Opened { tx: t, .. } = e {
                        tx = Some(t);
                    }
                }
                let _ = c.send(Lane::Unreliable, i.to_le_bytes().to_vec());
                if let Some(tx) = &tx {
                    let _ = tx.send(Lane::Unreliable, (i * 3).to_le_bytes().to_vec());
                }
                while c.recv().is_some() {}
            }
            (n.wire_digest(), n.delivered())
        };
        assert_eq!(run(5), run(5));
        assert_ne!(run(5).0, run(6).0);
    }

    #[test]
    fn size_caps() {
        let (_clock, n) = net(Semantics::Datagram);
        let mut c = n.connector(LinkProfile::default()).connect_memory();
        assert_eq!(c.caps().max_unreliable, MEMORY_MAX_DATAGRAM);
        assert!(matches!(
            c.send(Lane::Unreliable, vec![0; MEMORY_MAX_DATAGRAM + 1]),
            Err(TransportError::TooLarge { .. })
        ));
        c.send(Lane::Reliable, vec![0; MEMORY_MAX_DATAGRAM + 1]).unwrap();
    }
}
