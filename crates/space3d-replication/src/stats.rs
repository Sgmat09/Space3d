//! Connection and room statistics (read by games, the admin API and Prometheus export).

use std::time::Duration;

/// Per-connection statistics (client side: `NetClient::stats`; server side: `NetServer::stats`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NetStats {
    /// Smoothed round-trip time.
    pub rtt: Duration,
    /// RTT variation.
    pub jitter: Duration,
    /// Estimated packet loss 0..1 (datagram transports; 0 on streams).
    pub loss: f32,
    /// Received bytes/s (1 s window).
    pub bytes_in_per_sec: u32,
    /// Sent bytes/s (1 s window).
    pub bytes_out_per_sec: u32,
    /// Total bytes received.
    pub bytes_in: u64,
    /// Total bytes sent.
    pub bytes_out: u64,
    /// Current interpolation delay in ticks (client).
    pub interp_delay_ticks: f32,
    /// Last server-reported input slack in ticks (client) / average slack (server).
    pub input_slack: f32,
    /// Current tick-rate scale from the pacer (client).
    pub tick_scale: f32,
    /// Rollbacks performed (client).
    pub rollbacks: u64,
    /// Ticks re-simulated (client).
    pub resim_ticks: u64,
    /// Ticks for which the server had no input from this client.
    pub input_missing: u64,
    /// Inputs that arrived after their tick was simulated.
    pub input_late: u64,
    /// Snapshots skipped by flow control (server) / dropped as stale (client).
    pub snapshots_skipped: u64,
    /// Entities that wanted to be sent but did not fit the budget last snapshot (server).
    pub starved_entities: u32,
    /// Fraction of the per-snapshot byte budget used last snapshot (server).
    pub budget_utilization: f32,
}

/// Cumulative per-room counters (server). The server crate converts deltas into Prometheus
/// metrics once per second; counters never reset during the room's lifetime.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoomNetStats {
    /// Bytes received from clients.
    pub bytes_in: u64,
    /// Bytes sent to clients.
    pub bytes_out: u64,
    /// Packets received.
    pub packets_in: u64,
    /// Packets sent.
    pub packets_out: u64,
    /// Snapshots sent.
    pub snapshots_sent: u64,
    /// Snapshots skipped by flow control.
    pub snapshots_skipped: u64,
    /// Sum of snapshot sizes (for an average).
    pub snapshot_bytes: u64,
    /// Inputs applied on time.
    pub inputs_ok: u64,
    /// Inputs that arrived too late.
    pub inputs_late: u64,
    /// Inputs rejected (outside the tick window or failed `sanitize`).
    pub inputs_invalid: u64,
    /// Ticks simulated with a missing input (per client).
    pub inputs_missing: u64,
    /// Strikes recorded.
    pub strikes: u64,
    /// Joins accepted.
    pub joins: u64,
    /// Joins rejected by the room (full, schema, ...).
    pub rejects: u64,
    /// Resumes accepted.
    pub resumes: u64,
    /// Clients kicked.
    pub kicks: u64,
}
