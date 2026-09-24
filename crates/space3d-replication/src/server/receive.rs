//! `replication::server_receive` (PreUpdate): drain the room inbox.
//!
//! * `Join` → schema/game/capacity/joinable checks (`Reject` + close on failure), resume
//!   verification, supersede, slot allocation, `Welcome` (+ fresh resume token), events.
//! * `Packet` → per-link token buckets (packets/s, bytes/s, messages/s) → `decode_packet`
//!   (strike on error) → acks (datagram) → Time (Pong queued) → Input (buffer) → Control →
//!   messages (size cap, per-type rate limit, `NetMessage::validate`, `Events<FromClient<M>>`).
//! * `Closed` → slot `Disconnected { until }` + `PlayerDisconnected`.
//!
//! Strikes: `strikes.max` within `strikes.window` → `Disconnect(ProtocolViolation)`.

use std::time::Duration;

/// Deterministic token bucket (time injected by the caller).
#[derive(Clone, Debug)]
pub struct TokenBucket {
    rate: f64,
    burst: f64,
    tokens: f64,
    last: Duration,
}

impl TokenBucket {
    /// `rate` per second with `burst` capacity (starts full).
    pub fn new(rate: f64, burst: f64) -> Self {
        Self {
            rate,
            burst,
            tokens: burst,
            last: Duration::ZERO,
        }
    }

    /// Take `n` tokens at `now`; `false` if not enough.
    pub fn take(&mut self, now: Duration, n: f64) -> bool {
        let dt = now.saturating_sub(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + dt * self.rate).min(self.burst);
        if self.tokens >= n {
            self.tokens -= n;
            true
        } else {
            false
        }
    }
}

/// Per-link inbound limits (internal to the replication builder).
#[derive(Clone, Debug)]
pub struct LinkLimiter {
    /// Packets per second.
    pub packets: TokenBucket,
    /// Bytes per second.
    pub bytes: TokenBucket,
    /// Messages per second (all types).
    pub messages: TokenBucket,
    /// Strike timestamps within the window.
    pub strikes: Vec<Duration>,
}
