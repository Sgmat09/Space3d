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
    /// Histogram with ascending upper `bounds` (a `+Inf` bucket is implicit).
    pub fn new(bounds: &[f64]) -> Self {
        let mut bounds: Vec<f64> = bounds.iter().copied().filter(|b| b.is_finite()).collect();
        bounds.sort_by(|a, b| a.total_cmp(b));
        bounds.dedup();
        let counts = (0..=bounds.len()).map(|_| AtomicU64::new(0)).collect();
        Self {
            inner: Arc::new(HistogramInner {
                bounds,
                counts,
                sum_bits: AtomicU64::new(0f64.to_bits()),
                count: AtomicU64::new(0),
            }),
        }
    }

    /// Record a value.
    pub fn observe(&self, v: f64) {
        if v.is_nan() {
            return;
        }
        let i = self.inner.bounds.partition_point(|b| *b < v);
        self.inner.counts[i].fetch_add(1, Ordering::Relaxed);
        self.inner.count.fetch_add(1, Ordering::Relaxed);
        let mut cur = self.inner.sum_bits.load(Ordering::Relaxed);
        loop {
            let next = (f64::from_bits(cur) + v).to_bits();
            match self
                .inner
                .sum_bits
                .compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Record a duration in seconds.
    pub fn observe_duration(&self, d: Duration) {
        self.observe(d.as_secs_f64());
    }

    /// Number of observations.
    pub fn count(&self) -> u64 {
        self.inner.count.load(Ordering::Relaxed)
    }

    /// Sum of observations.
    pub fn sum(&self) -> f64 {
        f64::from_bits(self.inner.sum_bits.load(Ordering::Relaxed))
    }

    /// Per-bucket (non-cumulative) counts; the last entry is the `+Inf` bucket.
    pub fn bucket_counts(&self) -> Vec<u64> {
        self.inner.counts.iter().map(|c| c.load(Ordering::Relaxed)).collect()
    }

    /// Upper bounds.
    pub fn bounds(&self) -> &[f64] {
        &self.inner.bounds
    }

    /// Approximate quantile from buckets (admin API, heartbeats): the upper bound of the bucket
    /// holding the `q`-quantile (the largest finite bound for the `+Inf` bucket; 0 when empty).
    pub fn quantile(&self, q: f64) -> f64 {
        quantile_of(&self.inner.bounds, &self.bucket_counts(), q)
    }
}

/// Quantile over `(bounds, counts)` as produced by [`Histogram::bucket_counts`] (also used for
/// windowed deltas).
pub fn quantile_of(bounds: &[f64], counts: &[u64], q: f64) -> f64 {
    let total: u64 = counts.iter().sum();
    if total == 0 {
        return 0.0;
    }
    let rank = ((q.clamp(0.0, 1.0) * total as f64).ceil() as u64).max(1);
    let mut seen = 0;
    for (i, c) in counts.iter().enumerate() {
        seen += c;
        if seen >= rank {
            return bounds
                .get(i)
                .copied()
                .unwrap_or_else(|| bounds.last().copied().unwrap_or(0.0));
        }
    }
    bounds.last().copied().unwrap_or(0.0)
}

/// Tick-time buckets (seconds): 0.25 ms … 50 ms.
pub const TICK_BUCKETS: &[f64] = &[0.00025, 0.0005, 0.001, 0.002, 0.004, 0.008, 0.016, 0.033, 0.05];
/// Latency buckets (seconds) for RTT / HTTP.
pub const LATENCY_BUCKETS: &[f64] = &[0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5];
/// Size buckets (bytes) for snapshots.
pub const SIZE_BUCKETS: &[f64] = &[64.0, 128.0, 256.0, 512.0, 1024.0, 2048.0, 4096.0, 8192.0, 16384.0];
/// Wait buckets (seconds) for matchmaking.
pub const WAIT_BUCKETS: &[f64] = &[1.0, 5.0, 10.0, 20.0, 30.0, 60.0, 90.0, 120.0, 300.0];
/// Checkpoint size buckets (bytes).
pub const CHECKPOINT_BUCKETS: &[f64] = &[
    1024.0,
    8192.0,
    65536.0,
    262_144.0,
    1_048_576.0,
    4_194_304.0,
    16_777_216.0,
];

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

fn owned_labels(labels: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = labels.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
    v.sort();
    v
}

impl Metrics {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    fn series<T>(
        &self,
        name: &str,
        help: &str,
        kind: &'static str,
        labels: &[(&str, &str)],
        make: impl FnOnce() -> Series,
        pick: impl Fn(&Series) -> Option<T>,
    ) -> T {
        let labels = owned_labels(labels);
        let mut families = self.families.lock();
        let idx = match families.iter().position(|f| f.name == name) {
            Some(i) => i,
            None => {
                families.push(Family {
                    name: name.to_owned(),
                    help: help.to_owned(),
                    kind,
                    series: Vec::new(),
                });
                families.len() - 1
            }
        };
        let family = &mut families[idx];
        assert_eq!(family.kind, kind, "metric {name} registered with two types");
        if let Some((_, s)) = family.series.iter().find(|(l, _)| *l == labels) {
            return pick(s).expect("metric type mismatch");
        }
        let s = make();
        let out = pick(&s).expect("metric type mismatch");
        family.series.push((labels, s));
        out
    }

    /// Counter for `name{labels}` (created on first use; same handle afterwards).
    pub fn counter(&self, name: &str, help: &str, labels: &[(&str, &str)]) -> Counter {
        self.series(
            name,
            help,
            "counter",
            labels,
            || Series::Counter(Counter::default()),
            |s| match s {
                Series::Counter(c) => Some(c.clone()),
                _ => None,
            },
        )
    }

    /// Gauge for `name{labels}`.
    pub fn gauge(&self, name: &str, help: &str, labels: &[(&str, &str)]) -> Gauge {
        self.series(
            name,
            help,
            "gauge",
            labels,
            || Series::Gauge(Gauge::default()),
            |s| match s {
                Series::Gauge(g) => Some(g.clone()),
                _ => None,
            },
        )
    }

    /// Histogram for `name{labels}` with `buckets` (upper bounds, ascending).
    pub fn histogram(&self, name: &str, help: &str, buckets: &[f64], labels: &[(&str, &str)]) -> Histogram {
        self.series(
            name,
            help,
            "histogram",
            labels,
            || Series::Histogram(Histogram::new(buckets)),
            |s| match s {
                Series::Histogram(h) => Some(h.clone()),
                _ => None,
            },
        )
    }

    /// Prometheus text format (0.0.4).
    pub fn render(&self) -> String {
        use std::fmt::Write;
        let families = self.families.lock();
        let mut out = String::with_capacity(4096);
        let mut order: Vec<&Family> = families.iter().collect();
        order.sort_by(|a, b| a.name.cmp(&b.name));
        for f in order {
            let _ = writeln!(out, "# HELP {} {}", f.name, escape_help(&f.help));
            let _ = writeln!(out, "# TYPE {} {}", f.name, f.kind);
            let mut series: Vec<&(Vec<(String, String)>, Series)> = f.series.iter().collect();
            series.sort_by(|a, b| a.0.cmp(&b.0));
            for (labels, s) in series {
                match s {
                    Series::Counter(c) => {
                        let _ = writeln!(out, "{}{} {}", f.name, fmt_labels(labels, None), c.get());
                    }
                    Series::Gauge(g) => {
                        let _ = writeln!(out, "{}{} {}", f.name, fmt_labels(labels, None), fmt_f64(g.get()));
                    }
                    Series::Histogram(h) => {
                        let counts = h.bucket_counts();
                        let mut cumulative = 0;
                        for (i, c) in counts.iter().enumerate() {
                            cumulative += c;
                            let le = h.bounds().get(i).map_or_else(|| "+Inf".to_owned(), |b| fmt_f64(*b));
                            let _ = writeln!(out, "{}_bucket{} {cumulative}", f.name, fmt_labels(labels, Some(&le)));
                        }
                        let _ = writeln!(out, "{}_sum{} {}", f.name, fmt_labels(labels, None), fmt_f64(h.sum()));
                        let _ = writeln!(out, "{}_count{} {}", f.name, fmt_labels(labels, None), h.count());
                    }
                }
            }
        }
        out
    }
}

fn escape_help(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', "\\n")
}

fn escape_label(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

fn fmt_f64(v: f64) -> String {
    if v.is_infinite() {
        if v > 0.0 { "+Inf".into() } else { "-Inf".into() }
    } else if v.is_nan() {
        "NaN".into()
    } else {
        format!("{v}")
    }
}

fn fmt_labels(labels: &[(String, String)], le: Option<&str>) -> String {
    if labels.is_empty() && le.is_none() {
        return String::new();
    }
    let mut parts: Vec<String> = labels
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", escape_label(v)))
        .collect();
    if let Some(le) = le {
        parts.push(format!("le=\"{le}\""));
    }
    format!("{{{}}}", parts.join(","))
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
    /// Checkpoint capture + serialization time.
    pub checkpoint_seconds: Histogram,
    /// Checkpoint sizes.
    pub checkpoint_bytes: Histogram,
    /// Registry for everything else (labelled families).
    pub registry: Metrics,
}

impl GameServerMetrics {
    /// Register the standard families.
    pub fn register(registry: &Metrics) -> Self {
        let r = registry;
        for phase in ["waiting", "running", "ending", "closed", "crashed"] {
            r.gauge("space3d_rooms", "Rooms by phase.", &[("phase", phase)]);
        }
        for state in ["syncing", "connected", "disconnected"] {
            r.gauge("space3d_players", "Players by connection state.", &[("state", state)]);
        }
        for dir in ["in", "out"] {
            r.counter("space3d_packets_total", "Packets by direction.", &[("dir", dir)]);
            r.counter(
                "space3d_bytes_total",
                "Bytes by direction.",
                &[("dir", dir), ("channel", "all")],
            );
        }
        for result in ["ok", "late", "missing", "invalid"] {
            r.counter("space3d_input_total", "Inputs by result.", &[("result", result)]);
        }
        r.counter("space3d_strikes_total", "Protocol strikes.", &[]);
        Self {
            tick_seconds: r.histogram("space3d_tick_seconds", "Room tick duration.", TICK_BUCKETS, &[]),
            ticks_skipped: r.counter(
                "space3d_ticks_skipped_total",
                "Ticks skipped because a room fell behind.",
                &[],
            ),
            room_crashes: r.counter("space3d_room_crashes_total", "Rooms that panicked.", &[]),
            snapshot_bytes: r.histogram("space3d_snapshot_bytes", "Average snapshot size.", SIZE_BUCKETS, &[]),
            rtt_seconds: r.histogram("space3d_rtt_seconds", "Client round-trip time.", LATENCY_BUCKETS, &[]),
            outbox_pending: r.gauge("space3d_outbox_pending", "Outbox entries not yet acknowledged.", &[]),
            checkpoint_seconds: r.histogram(
                "space3d_checkpoint_seconds",
                "Checkpoint capture time.",
                TICK_BUCKETS,
                &[],
            ),
            checkpoint_bytes: r.histogram("space3d_checkpoint_bytes", "Checkpoint size.", CHECKPOINT_BUCKETS, &[]),
            registry: r.clone(),
        }
    }

    /// `space3d_handshakes_total{result}`.
    pub fn handshake(&self, result: &str) {
        self.registry
            .counter(
                "space3d_handshakes_total",
                "Gateway handshakes by result.",
                &[("result", result)],
            )
            .inc();
    }

    /// `space3d_rate_limited_total{kind}`.
    pub fn rate_limited(&self, kind: &str) {
        self.registry
            .counter("space3d_rate_limited_total", "Rate-limited events.", &[("kind", kind)])
            .inc();
    }

    /// `space3d_disconnects_total{reason}`.
    pub fn disconnect(&self, reason: &str) {
        self.registry
            .counter(
                "space3d_disconnects_total",
                "Disconnects by reason.",
                &[("reason", reason)],
            )
            .inc();
    }

    /// `space3d_snapshots_skipped_total{reason}`.
    pub fn snapshots_skipped(&self, reason: &str, n: u64) {
        self.registry
            .counter(
                "space3d_snapshots_skipped_total",
                "Snapshots skipped.",
                &[("reason", reason)],
            )
            .inc_by(n);
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
        registry.gauge("space3d_sessions_active", "Refresh families issued this process.", &[]);
        registry.gauge("space3d_db_queue_depth", "Store operations in flight.", &[]);
        Self {
            registry: registry.clone(),
        }
    }

    /// `space3d_logins_total{provider,result}`.
    pub fn login(&self, provider: &str, result: &str) {
        self.registry
            .counter(
                "space3d_logins_total",
                "Logins by provider and result.",
                &[("provider", provider), ("result", result)],
            )
            .inc();
    }

    /// `space3d_matches_total{queue}`.
    pub fn match_formed(&self, queue: &str) {
        self.registry
            .counter("space3d_matches_total", "Matches formed.", &[("queue", queue)])
            .inc();
    }

    /// `space3d_mm_wait_seconds{queue}`.
    pub fn mm_wait(&self, queue: &str, secs: f64) {
        self.registry
            .histogram(
                "space3d_mm_wait_seconds",
                "Matchmaking wait.",
                WAIT_BUCKETS,
                &[("queue", queue)],
            )
            .observe(secs);
    }

    /// `space3d_mm_queue_size{queue,region}`.
    pub fn queue_size(&self, queue: &str, region: &str, n: usize) {
        self.registry
            .gauge(
                "space3d_mm_queue_size",
                "Queued players.",
                &[("queue", queue), ("region", region)],
            )
            .set(n as f64);
    }

    /// `space3d_fleet_servers{region,state}`.
    pub fn fleet_servers(&self, region: &str, state: &str, n: usize) {
        self.registry
            .gauge(
                "space3d_fleet_servers",
                "Registered game servers.",
                &[("region", region), ("state", state)],
            )
            .set(n as f64);
    }

    /// `space3d_fleet_free_seats{region}`.
    pub fn free_seats(&self, region: &str, n: u64) {
        self.registry
            .gauge(
                "space3d_fleet_free_seats",
                "Free player seats (autoscaler input).",
                &[("region", region)],
            )
            .set(n as f64);
    }

    /// `space3d_http_requests_total{route,status}` + `space3d_http_seconds{route}`.
    pub fn http(&self, route: &str, status: u16, secs: f64) {
        let status = status.to_string();
        self.registry
            .counter(
                "space3d_http_requests_total",
                "HTTP requests.",
                &[("route", route), ("status", &status)],
            )
            .inc();
        self.registry
            .histogram(
                "space3d_http_seconds",
                "HTTP latency.",
                LATENCY_BUCKETS,
                &[("route", route)],
            )
            .observe(secs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_gauges_share_handles() {
        let m = Metrics::new();
        let a = m.counter("x_total", "x", &[("k", "v")]);
        let b = m.counter("x_total", "x", &[("k", "v")]);
        a.inc();
        b.inc_by(2);
        assert_eq!(a.get(), 3);
        let g = m.gauge("g", "g", &[]);
        g.set(1.5);
        assert_eq!(m.gauge("g", "g", &[]).get(), 1.5);
    }

    #[test]
    fn histogram_quantiles_and_render() {
        let m = Metrics::new();
        let h = m.histogram("lat_seconds", "latency", &[0.001, 0.01, 0.1], &[]);
        for _ in 0..98 {
            h.observe(0.0005);
        }
        h.observe(0.05);
        h.observe(5.0);
        assert_eq!(h.count(), 100);
        assert_eq!(h.quantile(0.5), 0.001);
        assert_eq!(h.quantile(0.99), 0.1);
        assert_eq!(h.quantile(1.0), 0.1);
        assert_eq!(Histogram::new(&[1.0]).quantile(0.99), 0.0);
        m.counter("b_total", "b \"quoted\"", &[("route", "/v1/\"x\"")]).inc();
        let text = m.render();
        assert!(text.contains("# TYPE lat_seconds histogram"), "{text}");
        assert!(text.contains("lat_seconds_bucket{le=\"0.001\"} 98"), "{text}");
        assert!(text.contains("lat_seconds_bucket{le=\"+Inf\"} 100"), "{text}");
        assert!(text.contains("lat_seconds_count 100"), "{text}");
        assert!(text.contains("b_total{route=\"/v1/\\\"x\\\"\"} 1"), "{text}");
        // Families are sorted: b_total before lat_seconds.
        assert!(text.find("b_total").unwrap() < text.find("lat_seconds").unwrap());
    }

    #[test]
    fn standard_families_register() {
        let m = Metrics::new();
        let gs = GameServerMetrics::register(&m);
        gs.tick_seconds.observe(0.0007);
        gs.handshake("ok");
        let ps = PlayerServiceMetrics::register(&m);
        ps.login("guest", "ok");
        ps.http("/v1/me", 200, 0.002);
        let text = m.render();
        for name in [
            "space3d_tick_seconds_bucket",
            "space3d_rooms{phase=\"running\"}",
            "space3d_handshakes_total{result=\"ok\"} 1",
            "space3d_logins_total{provider=\"guest\",result=\"ok\"} 1",
            "space3d_http_requests_total{route=\"/v1/me\",status=\"200\"} 1",
        ] {
            assert!(text.contains(name), "missing {name} in\n{text}");
        }
    }
}
