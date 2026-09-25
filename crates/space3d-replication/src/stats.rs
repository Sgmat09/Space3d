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

/// Turns the cumulative byte counters of a [`NetStats`] into the 1 s window rates
/// (`bytes_in_per_sec`, `bytes_out_per_sec`).
#[derive(Clone, Copy, Debug, Default)]
pub struct RateWindow {
    start: Option<Duration>,
    bytes_in: u64,
    bytes_out: u64,
}

impl RateWindow {
    /// Call regularly with the current time; updates the rates once a second.
    pub fn update(&mut self, stats: &mut NetStats, now: Duration) {
        let Some(start) = self.start else {
            *self = Self {
                start: Some(now),
                bytes_in: stats.bytes_in,
                bytes_out: stats.bytes_out,
            };
            return;
        };
        let elapsed = now.saturating_sub(start);
        if elapsed >= Duration::from_secs(1) {
            let secs = elapsed.as_secs_f64();
            let rate = |d: u64| (d as f64 / secs).round().min(f64::from(u32::MAX)) as u32;
            stats.bytes_in_per_sec = rate(stats.bytes_in.saturating_sub(self.bytes_in));
            stats.bytes_out_per_sec = rate(stats.bytes_out.saturating_sub(self.bytes_out));
            *self = Self {
                start: Some(now),
                bytes_in: stats.bytes_in,
                bytes_out: stats.bytes_out,
            };
        }
    }
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
    /// Inbound packets dropped by the per-link packet / byte rate limits.
    pub rate_limited_packets: u64,
    /// Client messages dropped by the message rate limits.
    pub rate_limited_messages: u64,
    /// Links the room ended with a `Disconnect`, indexed by `DisconnectCode as usize`
    /// (see `DisconnectCode::ALL` / `label`); client `Leave`s are counted under `Leave`.
    pub disconnects: [u64; 16],
    /// Links that closed underneath the room (transport close, no `Disconnect`).
    pub links_lost: u64,
    /// Snapshot blocks dropped because one entity record exceeded the link's packet cap.
    pub oversize_dropped: u64,
}

impl RoomNetStats {
    /// Count a disconnect with `code`.
    pub fn count_disconnect(&mut self, code: crate::protocol::handshake::DisconnectCode) {
        if let Some(n) = self.disconnects.get_mut(code as usize) {
            *n += 1;
        }
    }

    /// Disconnects counted with `code`.
    pub fn disconnects_with(&self, code: crate::protocol::handshake::DisconnectCode) -> u64 {
        self.disconnects.get(code as usize).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_window_reports_per_second() {
        let mut stats = NetStats::default();
        let mut w = RateWindow::default();
        w.update(&mut stats, Duration::from_millis(100));
        stats.bytes_in += 3000;
        stats.bytes_out += 500;
        w.update(&mut stats, Duration::from_millis(600));
        assert_eq!(stats.bytes_in_per_sec, 0);
        w.update(&mut stats, Duration::from_millis(2100));
        assert_eq!((stats.bytes_in_per_sec, stats.bytes_out_per_sec), (1500, 250));
    }
}
