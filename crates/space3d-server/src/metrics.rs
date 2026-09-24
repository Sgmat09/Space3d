//! In-house Prometheus metrics (text exposition 0.0.4): atomics, fixed buckets, no global
//! recorder (tests run several servers per process), bounded label cardinality (never room
//! ids, player ids or IPs — per-room detail is in the admin JSON API).

use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Monotonic counter.
#[derive(Clone, Debug, Default)]
pub struct Counter(Arc<AtomicU64>);

impl Counter {
    /// +1.
    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    /// +n.
    pub fn inc_by(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }
    /// Current value.
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Gauge (f64 stored as bits).
#[derive(Clone, Debug, Default)]
pub struct Gauge(Arc<AtomicU64>);

impl Gauge {
    /// Set.
    pub fn set(&self, v: f64) {
        self.0.store(v.to_bits(), Ordering::Relaxed);
    }
    /// Current value.
    pub fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }
}

/// Fixed-bucket histogram.
#[derive(Clone, Debug)]
pub struct Histogram {
    inner: Arc<HistogramInner>,
}

#[derive(Debug)]
struct HistogramInner {
    bounds: Vec<f64>,
    counts: Vec<AtomicU64>,
    sum_bits: AtomicU64,
    count: AtomicU64,
}

impl Histogram {
    /// Record a value.
    pub fn observe(&self, v: f64) {
        todo!()
    }
    /// Record a duration in seconds.
    pub fn observe_duration(&self, d: Duration) {
        self.observe(d.as_secs_f64());
    }
    /// Approximate quantile from buckets (admin API, heartbeats).
    pub fn quantile(&self, q: f64) -> f64 {
        todo!()
    }
}

/// Tick-time buckets (seconds): 0.25 ms … 50 ms.
pub const TICK_BUCKETS: &[f64] = &[0.00025, 0.0005, 0.001, 0.002, 0.004, 0.008, 0.016, 0.033, 0.05];
/// Latency buckets (seconds) for RTT / HTTP.
pub const LATENCY_BUCKETS: &[f64] = &[0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5];
/// Size buckets (bytes) for snapshots.
pub const SIZE_BUCKETS: &[f64] = &[64.0, 128.0, 256.0, 512.0, 1024.0, 2048.0, 4096.0, 8192.0, 16384.0];

/// A metrics registry (clone = shared). One per process, passed explicitly (and inserted as a
/// resource into every room / supervisor `App`).
#[derive(Clone, Debug, Default)]
pub struct Metrics {
    families: Arc<Mutex<Vec<Family>>>,
}

#[derive(Debug)]
struct Family {
    name: String,
    help: String,
    kind: &'static str,
    series: Vec<(Vec<(String, String)>, Series)>,
}

#[derive(Debug)]
enum Series {
    Counter(Counter),
    Gauge(Gauge),
    Histogram(Histogram),
}

impl Metrics {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Counter for `name{labels}` (created on first use; same handle afterwards).
    pub fn counter(&self, name: &str, help: &str, labels: &[(&str, &str)]) -> Counter {
        todo!()
    }

    /// Gauge for `name{labels}`.
    pub fn gauge(&self, name: &str, help: &str, labels: &[(&str, &str)]) -> Gauge {
        todo!()
    }

    /// Histogram for `name{labels}` with `buckets` (upper bounds, ascending).
    pub fn histogram(&self, name: &str, help: &str, buckets: &[f64], labels: &[(&str, &str)]) -> Histogram {
        todo!()
    }

    /// Prometheus text format.
    pub fn render(&self) -> String {
        todo!()
    }
}

/// The game server's standard metric handles (registered once at startup). Names:
///
/// `space3d_rooms{phase}`, `space3d_players{state}`, `space3d_tick_seconds` (hist),
/// `space3d_ticks_skipped_total`, `space3d_room_crashes_total`, `space3d_bytes_total{dir,channel}`,
/// `space3d_packets_total{dir}`, `space3d_snapshot_bytes` (hist), `space3d_snapshots_skipped_total{reason}`,
/// `space3d_rtt_seconds` (hist), `space3d_input_total{result=ok|late|missing|invalid}`,
/// `space3d_handshakes_total{result}`, `space3d_rate_limited_total{kind}`, `space3d_strikes_total`,
/// `space3d_disconnects_total{reason}`, `space3d_outbox_pending`, `space3d_checkpoint_seconds` (hist),
/// `space3d_checkpoint_bytes` (hist), `space3d_shard_load{shard}`.
#[derive(Clone, Debug)]
pub struct GameServerMetrics {
    /// Room tick duration.
    pub tick_seconds: Histogram,
    /// Ticks skipped because a room fell behind.
    pub ticks_skipped: Counter,
    /// Rooms that panicked.
    pub room_crashes: Counter,
    /// Snapshot sizes.
    pub snapshot_bytes: Histogram,
    /// RTT samples.
    pub rtt_seconds: Histogram,
    /// Outbox entries not yet acked.
    pub outbox_pending: Gauge,
    /// Registry for everything else (labelled families).
    pub registry: Metrics,
}

impl GameServerMetrics {
    /// Register the standard families.
    pub fn register(registry: &Metrics) -> Self {
        todo!()
    }
}

/// The player service's standard metric handles: `space3d_logins_total{provider,result}`,
/// `space3d_sessions_active`, `space3d_mm_queue_size{queue,region}`, `space3d_mm_wait_seconds{queue}`,
/// `space3d_matches_total{queue}`, `space3d_fleet_servers{region,state}`,
/// `space3d_fleet_free_seats{region}` (autoscaler input), `space3d_db_queue_depth`,
/// `space3d_http_requests_total{route,status}`, `space3d_http_seconds{route}`.
#[derive(Clone, Debug)]
pub struct PlayerServiceMetrics {
    /// Registry.
    pub registry: Metrics,
}

impl PlayerServiceMetrics {
    /// Register the standard families.
    pub fn register(registry: &Metrics) -> Self {
        todo!()
    }
}
