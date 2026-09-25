//! Priority accumulation + per-client byte budget.
//!
//! Per client per send tick: `budget = min(client_bandwidth, cap) / send_rate` (token bucket,
//! 2× burst; 4× during initial sync). Always first: despawns, removals, reliable messages,
//! the client's controlled (predicted) entities. Then every relevant entity with pending
//! changes (the client's other own entities with a 4× boost) accumulates
//! `acc += rule_priority × NetPriority × distance_falloff × Δticks`; the top entries by `acc`
//! are written while the budget allows (trying up to 8 smaller ones after the first miss);
//! written entities reset `acc = 0`, the rest keep growing. Nothing starves: a full bucket
//! lets the top entry through even when it does not fit (the budget goes into debt), and a
//! top entry deferred several snapshots in a row holds lower priorities back until it goes.

use std::time::Duration;

/// Token bucket in bytes.
#[derive(Clone, Debug)]
pub struct ByteBudget {
    rate: u32,
    burst: u32,
    tokens: f64,
}

impl ByteBudget {
    /// `rate` bytes/s, `burst` bytes.
    pub fn new(rate: u32, burst: u32) -> Self {
        Self {
            rate,
            burst,
            tokens: f64::from(burst),
        }
    }

    /// Refill for `dt`.
    pub fn refill(&mut self, dt: Duration) {
        self.tokens = (self.tokens + f64::from(self.rate) * dt.as_secs_f64()).min(f64::from(self.burst));
    }

    /// Bytes available now.
    pub fn available(&self) -> usize {
        self.tokens.max(0.0) as usize
    }

    /// Whether the bucket is full (nothing was spent for a whole burst's worth of refills).
    pub fn is_full(&self) -> bool {
        self.tokens >= f64::from(self.burst)
    }

    /// Consume `n` bytes (may go negative once; the next refill pays it back).
    pub fn consume(&mut self, n: usize) {
        self.tokens -= n as f64;
    }

    /// Change the rate (SetRate / caps).
    pub fn set_rate(&mut self, rate: u32, burst: u32) {
        self.rate = rate;
        self.burst = burst;
    }
}

/// Distance falloff `1 / (1 + d / reference)`.
pub fn distance_falloff(d: f32, reference: f32) -> f32 {
    1.0 / (1.0 + d.max(0.0) / reference.max(f32::EPSILON))
}
