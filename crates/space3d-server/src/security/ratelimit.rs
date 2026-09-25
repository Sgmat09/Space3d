//! Rate limiting with injected time (deterministic in tests).

use super::netaddr::rate_key;
pub use space3d_replication::server::receive::TokenBucket;
use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
use std::net::IpAddr;
use std::time::Duration;

/// Token buckets per key (IP, username, route), bounded so a key flood cannot grow memory:
/// when `capacity` keys are tracked, the least recently used key is dropped (O(log n); an idle
/// bucket that refilled completely is indistinguishable from a fresh one, and LRU order drops
/// those first).
#[derive(Debug)]
pub struct KeyedLimiter<K: Hash + Eq + Clone> {
    rate: f64,
    burst: f64,
    capacity: usize,
    buckets: HashMap<K, (TokenBucket, u64)>,
    /// Recency order: sequence number of the last use → key.
    order: BTreeMap<u64, K>,
    seq: u64,
}

impl<K: Hash + Eq + Clone> KeyedLimiter<K> {
    /// `rate`/s with `burst`, tracking at most `capacity` keys.
    pub fn new(rate: f64, burst: f64, capacity: usize) -> Self {
        Self {
            rate,
            burst,
            capacity: capacity.max(1),
            buckets: Default::default(),
            order: BTreeMap::new(),
            seq: 0,
        }
    }

    /// Take one token for `key` at `now`.
    pub fn check(&mut self, key: &K, now: Duration) -> bool {
        self.check_n(key, now, 1.0)
    }

    /// Take `n` tokens for `key` at `now`.
    pub fn check_n(&mut self, key: &K, now: Duration, n: f64) -> bool {
        self.seq += 1;
        let seq = self.seq;
        if let Some((bucket, last)) = self.buckets.get_mut(key) {
            self.order.remove(last);
            *last = seq;
            self.order.insert(seq, key.clone());
            return bucket.take(now, n);
        }
        while self.buckets.len() >= self.capacity {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            self.buckets.remove(&oldest);
        }
        let mut bucket = TokenBucket::new(self.rate, self.burst);
        // Anchor the bucket's clock at `now` (it starts full).
        bucket.take(now, 0.0);
        let ok = bucket.take(now, n);
        self.buckets.insert(key.clone(), (bucket, seq));
        self.order.insert(seq, key.clone());
        ok
    }

    /// Whether `key` has a whole token at `now`, **without** taking it (unknown keys start
    /// full). Pair with [`Self::check`] to charge only some outcomes (e.g. failed logins).
    pub fn available(&mut self, key: &K, now: Duration) -> bool {
        match self.buckets.get_mut(key) {
            Some((bucket, _)) => {
                bucket.take(now, 0.0);
                bucket.tokens() >= 1.0
            }
            None => self.burst >= 1.0,
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

    /// Like [`Self::on_open`] with the reason. IPv6 addresses count per /64 ([`rate_key`]).
    pub fn try_open(&mut self, ip: IpAddr, now: Duration) -> Result<(), ConnRefusal> {
        let ip = rate_key(ip);
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
        let ip = rate_key(ip);
        if let Some(n) = self.open.get_mut(&ip) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.open.remove(&ip);
            }
        }
    }

    /// Open connections from `ip`.
    pub fn open_from(&self, ip: IpAddr) -> u32 {
        self.open.get(&rate_key(ip)).copied().unwrap_or(0)
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
    fn keyed_limiter_evicts_least_recently_used() {
        let mut l = KeyedLimiter::new(0.001, 1.0, 3);
        for k in 0..3u32 {
            assert!(l.check(&k, MS(u64::from(k))));
        }
        // Touch 0: 1 is now the least recently used.
        assert!(!l.check(&0u32, MS(10)), "0 is empty");
        assert!(l.check(&3u32, MS(11)));
        assert_eq!(l.len(), 3);
        assert!(!l.check(&0u32, MS(12)), "0 kept its (empty) bucket");
        assert!(l.check(&1u32, MS(13)), "1 was evicted: fresh bucket");
        // A full table stays cheap per new key (LRU pop, not a scan): churn through many keys.
        let mut big = KeyedLimiter::new(20.0 / 60.0, 20.0, 50_000);
        for i in 0..200_000u32 {
            big.check(&i, MS(u64::from(i)));
        }
        assert_eq!(big.len(), 50_000);
    }

    #[test]
    fn ipv6_connections_count_per_slash_64() {
        let mut c = ConnectionLimiter::new(8, 10_000);
        let mut admitted = 0;
        for i in 0..1000u32 {
            let ip: IpAddr = format!("2001:db8:1:2::{:x}", i + 1).parse().unwrap();
            if c.try_open(ip, MS(u64::from(i))).is_ok() {
                admitted += 1;
            }
        }
        assert_eq!(admitted, 8, "one /64 shares the per-IP cap");
        let other: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert!(c.try_open(other, MS(2_000)).is_ok());
        c.on_close("2001:db8:1:2::abcd".parse().unwrap());
        assert_eq!(c.open_from("2001:db8:1:2::1".parse().unwrap()), 7);
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
