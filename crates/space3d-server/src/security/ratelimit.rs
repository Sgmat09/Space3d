//! Rate limiting with injected time (deterministic in tests).

pub use space3d_replication::server::receive::TokenBucket;
use std::hash::Hash;
use std::net::IpAddr;
use std::time::Duration;

/// Token buckets per key (IP, username, route), bounded LRU so a key flood cannot grow memory.
#[derive(Debug)]
pub struct KeyedLimiter<K: Hash + Eq + Clone> {
    rate: f64,
    burst: f64,
    capacity: usize,
    buckets: std::collections::HashMap<K, (TokenBucket, Duration)>,
}

impl<K: Hash + Eq + Clone> KeyedLimiter<K> {
    /// `rate`/s with `burst`, tracking at most `capacity` keys.
    pub fn new(rate: f64, burst: f64, capacity: usize) -> Self {
        Self {
            rate,
            burst,
            capacity,
            buckets: Default::default(),
        }
    }

    /// Take one token for `key` at `now`.
    pub fn check(&mut self, key: &K, now: Duration) -> bool {
        todo!()
    }
}

/// Per-IP concurrent connection counter + handshake rate (gateway).
#[derive(Debug)]
pub struct ConnectionLimiter {
    max_per_ip: u32,
    open: std::collections::HashMap<IpAddr, u32>,
    handshakes: KeyedLimiter<IpAddr>,
}

impl ConnectionLimiter {
    /// Limits from config.
    pub fn new(max_per_ip: u32, handshakes_per_min: u32) -> Self {
        Self {
            max_per_ip,
            open: Default::default(),
            handshakes: KeyedLimiter::new(
                f64::from(handshakes_per_min) / 60.0,
                f64::from(handshakes_per_min),
                100_000,
            ),
        }
    }

    /// A connection from `ip` opened; `false` if over the per-IP cap or handshake rate.
    pub fn on_open(&mut self, ip: IpAddr, now: Duration) -> bool {
        todo!()
    }

    /// A connection from `ip` closed.
    pub fn on_close(&mut self, ip: IpAddr) {
        todo!()
    }
}
