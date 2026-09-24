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
///
/// Keys: `latency`, `jitter`, `loss`, `duplicate`/`dup`, `reorder`, `reorder_delay`,
/// `bw`/`bandwidth` (bytes/s; `B`, `KB`, `MB`, `KiB`, `MiB` suffixes), `queue`
/// (bytes), `rto`, `burst` (`p_enter/p_exit/loss_in_burst`), and presets as a bare word or
/// `preset=…` (`perfect`, `lan`, `broadband`, `mobile_4g`, `bad_wifi`, `terrible`) which
/// later keys override. Durations: `ns`, `us`, `ms`, `s`. Probabilities: `2%` or `0.02`.
pub fn parse_netsim(spec: &str) -> Result<LinkConditions, String> {
    fn preset(name: &str) -> Option<LinkConditions> {
        Some(match name {
            "perfect" | "none" => LinkConditions::perfect(),
            "lan" => LinkConditions::lan(),
            "broadband" => LinkConditions::broadband(),
            "mobile_4g" | "4g" | "mobile" => LinkConditions::mobile_4g(),
            "bad_wifi" | "wifi" => LinkConditions::bad_wifi(),
            "terrible" => LinkConditions::terrible(),
            _ => return None,
        })
    }
    fn duration(v: &str) -> Result<Duration, String> {
        let v = v.trim();
        let split = v.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(v.len());
        let (num, unit) = v.split_at(split);
        let n: f64 = num.parse().map_err(|_| format!("bad duration {v:?}"))?;
        let secs = match unit.trim() {
            "ns" => n / 1e9,
            "us" | "µs" => n / 1e6,
            "ms" | "" => n / 1e3,
            "s" => n,
            other => return Err(format!("bad duration unit {other:?}")),
        };
        Duration::try_from_secs_f64(secs).map_err(|e| e.to_string())
    }
    fn prob(v: &str) -> Result<f32, String> {
        let v = v.trim();
        let p = if let Some(pct) = v.strip_suffix('%') {
            pct.trim().parse::<f32>().map_err(|_| format!("bad percentage {v:?}"))? / 100.0
        } else {
            v.parse::<f32>().map_err(|_| format!("bad probability {v:?}"))?
        };
        if !(0.0..=1.0).contains(&p) {
            return Err(format!("probability out of range: {v:?}"));
        }
        Ok(p)
    }
    fn bytes(v: &str) -> Result<u64, String> {
        let v = v.trim().trim_end_matches("/s");
        let split = v.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(v.len());
        let (num, unit) = v.split_at(split);
        let n: f64 = num.parse().map_err(|_| format!("bad size {v:?}"))?;
        let mult = match unit.trim().to_ascii_lowercase().as_str() {
            "" | "b" => 1.0,
            "kb" | "k" => 1e3,
            "mb" | "m" => 1e6,
            "kib" => 1024.0,
            "mib" => 1024.0 * 1024.0,
            other => return Err(format!("bad size unit {other:?}")),
        };
        Ok((n * mult).round() as u64)
    }
    let mut c = LinkConditions::perfect();
    for part in spec.split([',', ' ']).map(str::trim).filter(|p| !p.is_empty()) {
        let Some((key, value)) = part.split_once('=') else {
            c = preset(part).ok_or_else(|| format!("unknown preset {part:?}"))?;
            continue;
        };
        match key.trim() {
            "preset" => c = preset(value.trim()).ok_or_else(|| format!("unknown preset {value:?}"))?,
            "latency" | "lat" | "delay" => c.latency = duration(value)?,
            "jitter" => c.jitter = duration(value)?,
            "loss" => c.loss = prob(value)?,
            "duplicate" | "dup" => c.duplicate = prob(value)?,
            "reorder" => c.reorder = prob(value)?,
            "reorder_delay" => c.reorder_delay = duration(value)?,
            "bw" | "bandwidth" => c.bandwidth = Some(bytes(value)?.max(1)),
            "queue" | "queue_limit" => c.queue_limit = bytes(value)? as usize,
            "rto" => c.rto = duration(value)?,
            "burst" => {
                let parts: Vec<&str> = value.split('/').collect();
                if parts.len() != 3 {
                    return Err("burst=p_enter/p_exit/loss_in_burst".into());
                }
                c.burst = Some(BurstLoss {
                    p_enter: prob(parts[0])?,
                    p_exit: prob(parts[1])?,
                    loss_in_burst: prob(parts[2])?,
                });
            }
            other => return Err(format!("unknown netsim key {other:?}")),
        }
    }
    Ok(c)
}

#[derive(Debug)]
struct InFlight {
    deliver_at: Duration,
    order: u64,
    lane: Lane,
    bytes: Vec<u8>,
    tx_done: Duration,
}

/// Counters of one [`ConditionedQueue`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueStats {
    /// Packets accepted.
    pub sent: u64,
    /// Packets delivered.
    pub delivered: u64,
    /// Packets dropped (loss or tail drop).
    pub dropped: u64,
    /// Packets duplicated.
    pub duplicated: u64,
    /// Packets delayed by a simulated retransmission (stream loss).
    pub stalled: u64,
    /// Reliable packets refused with `Backpressure`.
    pub backpressured: u64,
}

/// One direction's in-flight queue: decides delivery time (or drop) of each packet.
#[derive(Debug)]
pub struct ConditionedQueue {
    conditions: LinkConditions,
    semantics: Semantics,
    rng: SplitMix64,
    in_bad_state: bool,
    in_flight: Vec<InFlight>,
    next_order: u64,
    busy_until: Duration,
    /// Last delivery time on the ordered stream (all packets for `Stream`, reliable lane for `Datagram`).
    last_stream_delivery: Duration,
    stats: QueueStats,
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
            stats: QueueStats::default(),
        }
    }

    /// Replace the conditions (in-flight packets keep their schedule).
    pub fn set_conditions(&mut self, conditions: LinkConditions) {
        self.conditions = conditions;
    }

    /// Current conditions.
    pub fn conditions(&self) -> &LinkConditions {
        &self.conditions
    }

    /// Counters.
    pub fn stats(&self) -> QueueStats {
        self.stats
    }

    fn roll_loss(&mut self) -> bool {
        let mut p = self.conditions.loss;
        if let Some(b) = self.conditions.burst {
            if self.in_bad_state {
                if self.rng.chance(b.p_exit) {
                    self.in_bad_state = false;
                }
            } else if self.rng.chance(b.p_enter) {
                self.in_bad_state = true;
            }
            if self.in_bad_state {
                p = p.max(b.loss_in_burst);
            }
        }
        self.rng.chance(p)
    }

    fn jitter(&mut self) -> Duration {
        let j = self.conditions.jitter.as_secs_f64();
        let base = self.conditions.latency.as_secs_f64();
        if j <= 0.0 {
            return self.conditions.latency;
        }
        let offset = (f64::from(self.rng.next_f32()) * 2.0 - 1.0) * j;
        Duration::from_secs_f64((base + offset).max(0.0))
    }

    /// Enqueue a packet sent at `now`. `Err(Backpressure)` when a reliable packet exceeds the queue limit.
    pub fn push(&mut self, now: Duration, lane: Lane, bytes: Vec<u8>) -> Result<(), TransportError> {
        let len = bytes.len();
        let ordered = self.semantics == Semantics::Stream || lane == Lane::Reliable;
        if self.backlog_bytes(now).saturating_add(len) > self.conditions.queue_limit {
            if ordered && lane == Lane::Reliable {
                self.stats.backpressured += 1;
                return Err(TransportError::Backpressure);
            }
            self.stats.dropped += 1;
            return Ok(());
        }
        self.stats.sent += 1;
        let start = now.max(self.busy_until);
        let tx = match self.conditions.bandwidth {
            Some(bw) if bw > 0 => Duration::from_secs_f64(len as f64 / bw as f64),
            _ => Duration::ZERO,
        };
        let tx_done = start + tx;
        self.busy_until = tx_done;
        let lost = self.roll_loss();
        let mut deliver_at = tx_done + self.jitter();
        if ordered {
            if lost {
                // TCP/QUIC stream: the packet is retransmitted after an RTO (maybe more than once)
                // and everything behind it waits (head-of-line blocking).
                self.stats.stalled += 1;
                let mut rto = self.conditions.rto;
                deliver_at += rto;
                for _ in 0..2 {
                    if !self.rng.chance(self.conditions.loss) {
                        break;
                    }
                    rto *= 2;
                    deliver_at += rto;
                }
            }
            deliver_at = deliver_at.max(self.last_stream_delivery);
            self.last_stream_delivery = deliver_at;
        } else {
            if lost {
                self.stats.dropped += 1;
                return Ok(());
            }
            if self.rng.chance(self.conditions.reorder) {
                deliver_at += self.conditions.reorder_delay;
            }
            if self.rng.chance(self.conditions.duplicate) {
                self.stats.duplicated += 1;
                let order = self.next_order;
                self.next_order += 1;
                self.in_flight.push(InFlight {
                    deliver_at: deliver_at + Duration::from_millis(1),
                    order,
                    lane,
                    bytes: bytes.clone(),
                    tx_done,
                });
            }
        }
        let order = self.next_order;
        self.next_order += 1;
        self.in_flight.push(InFlight {
            deliver_at,
            order,
            lane,
            bytes,
            tx_done,
        });
        Ok(())
    }

    /// Pop every packet due at or before `now`, in delivery order.
    pub fn pop_due(&mut self, now: Duration, out: &mut Vec<(Lane, Vec<u8>)>) {
        let mut timed = Vec::new();
        self.pop_due_timed(now, &mut timed);
        out.extend(timed.into_iter().map(|(_, lane, b)| (lane, b)));
    }

    /// Like [`Self::pop_due`], with each packet's delivery time.
    pub fn pop_due_timed(&mut self, now: Duration, out: &mut Vec<(Duration, Lane, Vec<u8>)>) {
        if !self.in_flight.iter().any(|p| p.deliver_at <= now) {
            return;
        }
        self.in_flight.sort_by_key(|p| (p.deliver_at, p.order));
        let n = self.in_flight.partition_point(|p| p.deliver_at <= now);
        for p in self.in_flight.drain(..n) {
            self.stats.delivered += 1;
            out.push((p.deliver_at, p.lane, p.bytes));
        }
    }

    /// Bytes currently in flight (queued, serializing or propagating).
    pub fn in_flight_bytes(&self) -> usize {
        self.in_flight.iter().map(|p| p.bytes.len()).sum()
    }

    /// Bytes not yet serialized onto the simulated wire at `now` (the "socket send buffer").
    pub fn backlog_bytes(&self, now: Duration) -> usize {
        self.in_flight
            .iter()
            .filter(|p| p.tx_done > now)
            .map(|p| p.bytes.len())
            .sum()
    }

    /// Earliest pending delivery time (for event-driven harness stepping).
    pub fn next_delivery(&self) -> Option<Duration> {
        self.in_flight.iter().map(|p| p.deliver_at).min()
    }

    /// Whether nothing is in flight.
    pub fn is_empty(&self) -> bool {
        self.in_flight.is_empty()
    }

    /// Drop everything in flight (partition / sever).
    pub fn clear(&mut self) {
        self.stats.dropped += self.in_flight.len() as u64;
        self.in_flight.clear();
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

    fn pump(&mut self) {
        let now = self.clock.now();
        let mut due = Vec::new();
        self.up.pop_due(now, &mut due);
        for (lane, bytes) in due.drain(..) {
            let _ = self.inner.send(lane, bytes);
        }
        let lane = if self.inner.caps().true_datagrams {
            Lane::Unreliable
        } else {
            Lane::Reliable
        };
        while let Some(p) = self.inner.recv() {
            let _ = self.down.push(now, lane, p);
        }
        self.down.pop_due(now, &mut due);
        self.ready.extend(due.into_iter().map(|(_, b)| b));
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
        if let LinkState::Closed(_) = self.inner.state() {
            return Err(TransportError::Closed);
        }
        let now = self.clock.now();
        self.up.push(now, lane, packet)?;
        self.pump();
        Ok(())
    }

    fn recv(&mut self) -> Option<Vec<u8>> {
        self.pump();
        self.ready.pop_front()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn trace(seed: u64, c: LinkConditions, sem: Semantics) -> Vec<(Duration, Vec<u8>)> {
        let mut q = ConditionedQueue::new(c, sem, seed);
        let mut out = Vec::new();
        for i in 0..500u32 {
            let now = Duration::from_millis(u64::from(i) * 5);
            let _ = q.push(now, Lane::Unreliable, i.to_le_bytes().to_vec());
            let mut due = Vec::new();
            q.pop_due_timed(now, &mut due);
            out.extend(due.into_iter().map(|(t, _, b)| (t, b)));
        }
        let mut due = Vec::new();
        q.pop_due_timed(Duration::from_secs(3600), &mut due);
        out.extend(due.into_iter().map(|(t, _, b)| (t, b)));
        out
    }

    #[test]
    fn same_seed_same_trace() {
        for sem in [Semantics::Stream, Semantics::Datagram] {
            let a = trace(42, LinkConditions::terrible(), sem);
            let b = trace(42, LinkConditions::terrible(), sem);
            assert_eq!(a, b);
            let c = trace(43, LinkConditions::terrible(), sem);
            assert_ne!(a, c);
        }
    }

    #[test]
    fn datagram_loss_rate_within_one_percent() {
        let c = LinkConditions {
            loss: 0.05,
            ..LinkConditions::perfect()
        };
        let mut q = ConditionedQueue::new(c, Semantics::Datagram, 7);
        let n = 100_000u32;
        let mut out = Vec::new();
        for i in 0..n {
            q.push(Duration::from_micros(u64::from(i)), Lane::Unreliable, vec![0; 8])
                .unwrap();
            q.pop_due(Duration::from_micros(u64::from(i)), &mut out);
        }
        let loss = 1.0 - out.len() as f64 / f64::from(n);
        assert!((loss - 0.05).abs() < 0.01, "loss {loss}");
    }

    #[test]
    fn stream_never_reorders_or_loses() {
        let out = trace(5, LinkConditions::terrible(), Semantics::Stream);
        // Terrible's queue limit may tail-drop unreliable packets, but order is preserved.
        let ids: Vec<u32> = out
            .iter()
            .map(|(_, b)| u32::from_le_bytes(b[..4].try_into().unwrap()))
            .collect();
        assert!(ids.windows(2).all(|w| w[0] < w[1]), "stream reordered");
        let times: Vec<Duration> = out.iter().map(|(t, _)| *t).collect();
        assert!(times.windows(2).all(|w| w[0] <= w[1]));
        // Without a queue limit nothing is lost, only stalled.
        let c = LinkConditions {
            loss: 0.2,
            ..LinkConditions::perfect()
        };
        let out = trace(5, c, Semantics::Stream);
        assert_eq!(out.len(), 500);
    }

    #[test]
    fn stream_loss_causes_head_of_line_stalls() {
        let c = LinkConditions {
            latency: Duration::from_millis(10),
            loss: 0.1,
            ..LinkConditions::perfect()
        };
        let out = trace(11, c, Semantics::Stream);
        let max_gap = out.windows(2).map(|w| w[1].0.saturating_sub(w[0].0)).max().unwrap();
        assert!(max_gap >= Duration::from_millis(150), "no stall observed: {max_gap:?}");
    }

    #[test]
    fn datagrams_reorder_and_duplicate() {
        let c = LinkConditions {
            latency: Duration::from_millis(20),
            jitter: Duration::from_millis(15),
            reorder: 0.1,
            reorder_delay: Duration::from_millis(30),
            duplicate: 0.05,
            ..LinkConditions::perfect()
        };
        let out = trace(3, c, Semantics::Datagram);
        let ids: Vec<u32> = out
            .iter()
            .map(|(_, b)| u32::from_le_bytes(b[..4].try_into().unwrap()))
            .collect();
        assert!(ids.windows(2).any(|w| w[0] > w[1]), "never reordered");
        assert!(ids.len() > 500, "never duplicated");
    }

    #[test]
    fn bandwidth_cap_is_honoured() {
        let c = LinkConditions {
            bandwidth: Some(100_000),
            ..LinkConditions::perfect()
        };
        let mut q = ConditionedQueue::new(c, Semantics::Stream, 1);
        // 1 MB offered at t=0 → the last byte leaves after 10 s.
        for _ in 0..1000 {
            q.push(Duration::ZERO, Lane::Reliable, vec![0; 1000]).unwrap();
        }
        let mut out = Vec::new();
        q.pop_due_timed(Duration::from_secs(5), &mut out);
        let delivered: usize = out.iter().map(|p| p.2.len()).sum();
        assert!((490_000..=510_000).contains(&delivered), "{delivered}");
        assert!(q.backlog_bytes(Duration::from_secs(5)) >= 490_000);
        q.pop_due_timed(Duration::from_secs(11), &mut out);
        assert_eq!(out.len(), 1000);
    }

    #[test]
    fn queue_limit_backpressures_reliable_and_drops_unreliable() {
        let c = LinkConditions {
            bandwidth: Some(1000),
            queue_limit: 2000,
            ..LinkConditions::perfect()
        };
        let mut q = ConditionedQueue::new(c, Semantics::Datagram, 1);
        q.push(Duration::ZERO, Lane::Reliable, vec![0; 1500]).unwrap();
        assert_eq!(
            q.push(Duration::ZERO, Lane::Reliable, vec![0; 1000]),
            Err(TransportError::Backpressure)
        );
        q.push(Duration::ZERO, Lane::Unreliable, vec![0; 1000]).unwrap();
        assert_eq!(q.stats().dropped, 1);
    }

    #[test]
    fn netsim_parsing() {
        let c = parse_netsim("latency=80ms,jitter=10ms,loss=2%,bw=64KB").unwrap();
        assert_eq!(c.latency, Duration::from_millis(80));
        assert_eq!(c.jitter, Duration::from_millis(10));
        assert!((c.loss - 0.02).abs() < 1e-6);
        assert_eq!(c.bandwidth, Some(64_000));
        let c = parse_netsim("mobile_4g, loss=0.5").unwrap();
        assert_eq!(c.latency, LinkConditions::mobile_4g().latency);
        assert_eq!(c.loss, 0.5);
        let c = parse_netsim("burst=1%/30%/50%,queue=64KiB,rto=250ms").unwrap();
        assert_eq!(c.queue_limit, 65_536);
        assert!(c.burst.is_some());
        assert!(parse_netsim("loss=150%").is_err());
        assert!(parse_netsim("bogus=1").is_err());
        assert!(parse_netsim("nope").is_err());
    }
}
