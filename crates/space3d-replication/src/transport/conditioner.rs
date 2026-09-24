//! Deterministic network conditioner: latency, jitter, loss (incl. bursts), duplication,
//! reordering, bandwidth cap and queue limit, per direction, driven by a seeded RNG and a
//! virtual clock. Used by [`super::memory::MemoryNetwork`]; [`Conditioned`] can also wrap a
//! real client transport (wall clock) for manual testing (`--netsim latency=80ms,loss=2%`).
//!
//! Two delivery semantics:
//! * [`Semantics::Stream`] emulates TCP/WebSocket: order is preserved; a "lost" packet is
//!   delivered after an extra retransmission timeout and holds back everything behind it
//!   (head-of-line blocking) — exactly what the WS path must survive.
//! * [`Semantics::Datagram`]: unreliable-lane packets are really dropped/duplicated/reordered;
//!   reliable-lane packets behave like a QUIC stream (stall, never lost).

use super::{ClientTransport, CloseCode, Lane, LinkState, TransportCaps, TransportError, TransportStats};
use std::time::Duration;

/// SplitMix64: tiny, fast, seedable, wasm-clean. The same seed gives the same run.
#[derive(Clone, Debug)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    /// Seeded generator.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// Next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    pub fn next_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Bernoulli trial.
    pub fn chance(&mut self, p: f32) -> bool {
        p > 0.0 && self.next_f32() < p
    }
}

/// Delivery semantics of a conditioned link.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Semantics {
    /// TCP-like (WebSocket).
    #[default]
    Stream,
    /// Datagrams + reliable stream (QUIC-like).
    Datagram,
}

/// Gilbert–Elliott burst-loss model.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BurstLoss {
    /// P(good → bad) per packet.
    pub p_enter: f32,
    /// P(bad → good) per packet.
    pub p_exit: f32,
    /// Loss probability while bad.
    pub loss_in_burst: f32,
}

/// Conditions of one direction of a link.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkConditions {
    /// One-way base latency.
    pub latency: Duration,
    /// Uniform ± jitter.
    pub jitter: Duration,
    /// Loss probability (datagram: drop; stream: +`rto` stall).
    pub loss: f32,
    /// Optional burst loss on top of `loss`.
    pub burst: Option<BurstLoss>,
    /// Duplicate probability (datagram unreliable only).
    pub duplicate: f32,
    /// Probability of an extra `reorder_delay` (datagram unreliable only).
    pub reorder: f32,
    /// Extra delay for reordered packets.
    pub reorder_delay: Duration,
    /// Serialization rate in bytes/s (`None` = unlimited); queueing delay emerges.
    pub bandwidth: Option<u64>,
    /// Bytes in flight before tail drop (unreliable) / `Backpressure` (reliable).
    pub queue_limit: usize,
    /// Stream retransmission timeout used for "lost" stream packets (≥ 200 ms like TCP).
    pub rto: Duration,
}

impl LinkConditions {
    /// No latency, no loss.
    pub fn perfect() -> Self {
        Self {
            latency: Duration::ZERO,
            jitter: Duration::ZERO,
            loss: 0.0,
            burst: None,
            duplicate: 0.0,
            reorder: 0.0,
            reorder_delay: Duration::ZERO,
            bandwidth: None,
            queue_limit: usize::MAX,
            rto: Duration::from_millis(200),
        }
    }

    /// LAN: 1 ms ± 0.5 ms.
    pub fn lan() -> Self {
        Self {
            latency: Duration::from_millis(1),
            jitter: Duration::from_micros(500),
            ..Self::perfect()
        }
    }

    /// Broadband: 20 ms ± 3 ms, 0.1 % loss.
    pub fn broadband() -> Self {
        Self {
            latency: Duration::from_millis(20),
            jitter: Duration::from_millis(3),
            loss: 0.001,
            ..Self::perfect()
        }
    }

    /// Mobile 4G: 45 ms ± 15 ms, 1 % loss, 2 % reorder, 1 MB/s.
    pub fn mobile_4g() -> Self {
        Self {
            latency: Duration::from_millis(45),
            jitter: Duration::from_millis(15),
            loss: 0.01,
            reorder: 0.02,
            reorder_delay: Duration::from_millis(20),
            bandwidth: Some(1_000_000),
            queue_limit: 256 * 1024,
            ..Self::perfect()
        }
    }

    /// Bad Wi-Fi: 60 ms ± 30 ms, 3 % loss with bursts, 5 % reorder.
    pub fn bad_wifi() -> Self {
        Self {
            latency: Duration::from_millis(60),
            jitter: Duration::from_millis(30),
            loss: 0.03,
            burst: Some(BurstLoss {
                p_enter: 0.01,
                p_exit: 0.3,
                loss_in_burst: 0.5,
            }),
            reorder: 0.05,
            reorder_delay: Duration::from_millis(30),
            bandwidth: Some(500_000),
            queue_limit: 128 * 1024,
            ..Self::perfect()
        }
    }

    /// Terrible: 150 ms ± 50 ms, 10 % loss, 64 KB/s.
    pub fn terrible() -> Self {
        Self {
            latency: Duration::from_millis(150),
            jitter: Duration::from_millis(50),
            loss: 0.1,
            duplicate: 0.01,
            reorder: 0.1,
            reorder_delay: Duration::from_millis(50),
            bandwidth: Some(64_000),
            queue_limit: 64 * 1024,
            ..Self::perfect()
        }
    }
}

impl Default for LinkConditions {
    fn default() -> Self {
        Self::perfect()
    }
}

/// Both directions of a link.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LinkProfile {
    /// Client → server.
    pub up: LinkConditions,
    /// Server → client.
    pub down: LinkConditions,
}

impl LinkProfile {
    /// Same conditions both ways.
    pub fn symmetric(c: LinkConditions) -> Self {
        Self { up: c.clone(), down: c }
    }
}

/// Parse `latency=80ms,jitter=10ms,loss=2%,bw=64KB` (debug menus, `--netsim`).
pub fn parse_netsim(spec: &str) -> Result<LinkConditions, String> {
    todo!()
}

/// One direction's in-flight queue: decides delivery time (or drop) of each packet.
#[derive(Debug)]
pub struct ConditionedQueue {
    conditions: LinkConditions,
    semantics: Semantics,
    rng: SplitMix64,
    in_bad_state: bool,
    /// (deliver_at, insertion order, lane, bytes)
    in_flight: Vec<(Duration, u64, Lane, Vec<u8>)>,
    next_order: u64,
    busy_until: Duration,
    last_stream_delivery: Duration,
}

impl ConditionedQueue {
    /// New queue.
    pub fn new(conditions: LinkConditions, semantics: Semantics, seed: u64) -> Self {
        Self {
            conditions,
            semantics,
            rng: SplitMix64::new(seed),
            in_bad_state: false,
            in_flight: Vec::new(),
            next_order: 0,
            busy_until: Duration::ZERO,
            last_stream_delivery: Duration::ZERO,
        }
    }

    /// Replace the conditions (in-flight packets keep their schedule).
    pub fn set_conditions(&mut self, conditions: LinkConditions) {
        self.conditions = conditions;
    }

    /// Enqueue a packet sent at `now`. `Err(Backpressure)` when a reliable packet exceeds the queue limit.
    pub fn push(&mut self, now: Duration, lane: Lane, bytes: Vec<u8>) -> Result<(), TransportError> {
        todo!()
    }

    /// Pop every packet due at or before `now`, in delivery order.
    pub fn pop_due(&mut self, now: Duration, out: &mut Vec<(Lane, Vec<u8>)>) {
        todo!()
    }

    /// Bytes currently in flight.
    pub fn in_flight_bytes(&self) -> usize {
        self.in_flight.iter().map(|p| p.3.len()).sum()
    }

    /// Earliest pending delivery time (for event-driven harness stepping).
    pub fn next_delivery(&self) -> Option<Duration> {
        self.in_flight.iter().map(|p| p.0).min()
    }
}

/// Wraps a real client transport with conditions on both directions (wall clock). For manual
/// testing only; tests use the memory network.
pub struct Conditioned<T: ClientTransport> {
    inner: T,
    up: ConditionedQueue,
    down: ConditionedQueue,
    clock: crate::time::NetClockHandle,
    ready: std::collections::VecDeque<Vec<u8>>,
}

impl<T: ClientTransport> Conditioned<T> {
    /// Wrap `inner`.
    pub fn new(
        inner: T,
        profile: LinkProfile,
        semantics: Semantics,
        seed: u64,
        clock: crate::time::NetClockHandle,
    ) -> Self {
        Self {
            inner,
            up: ConditionedQueue::new(profile.up, semantics, seed),
            down: ConditionedQueue::new(profile.down, semantics, seed ^ 0x5bd1_e995),
            clock,
            ready: Default::default(),
        }
    }
}

impl<T: ClientTransport> ClientTransport for Conditioned<T> {
    fn caps(&self) -> TransportCaps {
        self.inner.caps()
    }

    fn state(&self) -> LinkState {
        self.inner.state()
    }

    fn send(&mut self, lane: Lane, packet: Vec<u8>) -> Result<(), TransportError> {
        todo!()
    }

    fn recv(&mut self) -> Option<Vec<u8>> {
        todo!()
    }

    fn backlog_bytes(&self) -> usize {
        self.inner.backlog_bytes() + self.up.in_flight_bytes()
    }

    fn close(&mut self, code: CloseCode, reason: &str) {
        self.inner.close(code, reason);
    }

    fn stats(&self) -> TransportStats {
        self.inner.stats()
    }
}
