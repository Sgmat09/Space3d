//! Rate limiting with injected time (deterministic in tests).

pub use space3d_replication::server::receive::TokenBucket;
use std::collections::HashMap;
use std::hash::Hash;
use std::net::IpAddr;
use std::time::Duration;

/// Token buckets per key (IP, username, route), bounded so a key flood cannot grow memory:
/// when `capacity` keys are tracked, buckets idle long enough to have refilled completely
/// (indistinguishable from a fresh bucket) are dropped first, then the least recently used.
#[derive(Debug)]
pub struct KeyedLimiter<K: Hash + Eq + Clone> {
    rate: f64,
    burst: f64,
    capacity: usize,
    buckets: HashMap<K, (TokenBucket, Duration)>,
}

impl<K: Hash + Eq + Clone> KeyedLimiter<K> {
    /// `rate`/s with `burst`, tracking at most `capacity` keys.
    pub fn new(rate: f64, burst: f64, capacity: usize) -> Self {
        Self {
            rate,
            burst,
            capacity: capacity.max(1),
            buckets: Default::default(),
        }
    }

    /// Take one token for `key` at `now`.
    pub fn check(&mut self, key: &K, now: Duration) -> bool {
        self.check_n(key, now, 1.0)
    }

    /// Take `n` tokens for `key` at `now`.
    pub fn check_n(&mut self, key: &K, now: Duration, n: f64) -> bool {
        if !self.buckets.contains_key(key) && self.buckets.len() >= self.capacity {
            self.evict(now);
        }
        let (rate, burst) = (self.rate, self.burst);
        let (bucket, last) = self.buckets.entry(key.clone()).or_insert_with(|| {
            let mut b = TokenBucket::new(rate, burst);
            // Anchor the bucket's clock at `now` (it starts full).
            b.take(now, 0.0);
            (b, now)
        });
        *last = now;
        bucket.take(now, n)
    }

    /// Seconds after which an idle bucket is full again.
    fn refill_time(&self) -> Duration {
        if self.rate <= 0.0 {
            return Duration::MAX;
        }
        Duration::try_from_secs_f64(self.burst / self.rate).unwrap_or(Duration::MAX)
    }

    fn evict(&mut self, now: Duration) {
        let refill = self.refill_time();
        self.buckets.retain(|_, (_, last)| now.saturating_sub(*last) < refill);
        if self.buckets.len() >= self.capacity {
            let oldest = self
                .buckets
                .iter()
                .min_by_key(|(_, (_, last))| *last)
                .map(|(k, _)| k.clone());
            if let Some(k) = oldest {
                self.buckets.remove(&k);
            }
        }
    }

    /// Keys currently tracked.
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    /// Whether no key is tracked.
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }
}

/// Per-IP concurrent connection counter + handshake rate (gateway).
#[derive(Debug)]
pub struct ConnectionLimiter {
    max_per_ip: u32,
    open: HashMap<IpAddr, u32>,
    handshakes: KeyedLimiter<IpAddr>,
}

/// Why [`ConnectionLimiter::on_open`] refused a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnRefusal {
    /// Too many concurrent connections from the IP.
    TooManyConnections,
    /// Too many new connections per minute from the IP.
    HandshakeRate,
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

    /// A connection from `ip` opened; `false` if over the per-IP cap or handshake rate (the
    /// connection is then **not** counted: do not call [`Self::on_close`] for it).
    pub fn on_open(&mut self, ip: IpAddr, now: Duration) -> bool {
        self.try_open(ip, now).is_ok()
    }

    /// Like [`Self::on_open`] with the reason.
    pub fn try_open(&mut self, ip: IpAddr, now: Duration) -> Result<(), ConnRefusal> {
        let open = self.open.get(&ip).copied().unwrap_or(0);
        if open >= self.max_per_ip {
            return Err(ConnRefusal::TooManyConnections);
        }
        if !self.handshakes.check(&ip, now) {
            return Err(ConnRefusal::HandshakeRate);
        }
        *self.open.entry(ip).or_insert(0) += 1;
        Ok(())
    }

    /// A counted connection from `ip` closed.
    pub fn on_close(&mut self, ip: IpAddr) {
        if let Some(n) = self.open.get_mut(&ip) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.open.remove(&ip);
            }
        }
    }

    /// Open connections from `ip`.
    pub fn open_from(&self, ip: IpAddr) -> u32 {
        self.open.get(&ip).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn keyed_buckets_refill_over_injected_time() {
        let mut l = KeyedLimiter::new(1.0, 2.0, 16);
        let k = "1.2.3.4".to_owned();
        assert!(l.check(&k, MS(0)));
        assert!(l.check(&k, MS(0)));
        assert!(!l.check(&k, MS(0)), "burst exhausted");
        assert!(!l.check(&k, MS(500)));
        assert!(l.check(&k, MS(1_600)), "refilled one token");
        assert!(l.check(&"other".to_owned(), MS(1_600)), "keys are independent");
    }

    #[test]
    fn keyed_limiter_is_bounded() {
        let mut l = KeyedLimiter::new(1.0, 1.0, 100);
        for i in 0..10_000u32 {
            l.check(&i, MS(u64::from(i)));
            assert!(l.len() <= 100);
        }
        // A hot key keeps its (empty) bucket while cold keys are evicted.
        let mut l = KeyedLimiter::new(0.001, 1.0, 2);
        assert!(l.check(&0u32, MS(0)));
        assert!(!l.check(&0u32, MS(1)));
        assert!(l.check(&1u32, MS(2)));
        assert!(l.check(&2u32, MS(3)), "evicts the LRU key");
        assert_eq!(l.len(), 2);
    }

    #[test]
    fn connection_limits() {
        let ip: IpAddr = "203.0.113.1".parse().unwrap();
        let mut c = ConnectionLimiter::new(2, 3);
        assert!(c.on_open(ip, MS(0)));
        assert!(c.on_open(ip, MS(0)));
        assert_eq!(c.try_open(ip, MS(0)), Err(ConnRefusal::TooManyConnections));
        c.on_close(ip);
        assert!(c.on_open(ip, MS(0)), "3rd handshake within the minute");
        c.on_close(ip);
        assert_eq!(c.try_open(ip, MS(10)), Err(ConnRefusal::HandshakeRate));
        assert!(c.on_open(ip, MS(20_100)), "one token back after 20 s");
        assert_eq!(c.open_from(ip), 2);
        c.on_close(ip);
        c.on_close(ip);
        c.on_close(ip);
        assert_eq!(c.open_from(ip), 0);
    }
}
