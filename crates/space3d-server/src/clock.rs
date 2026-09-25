//! Wall-clock (unix time) source for tokens, leases, bans and rate limits. Injected
//! everywhere so tests run on [`ManualUnixClock`]. Tick timing uses monotonic time instead
//! (`space3d_replication::time::NetClock`).

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

/// Unix time in milliseconds.
pub trait UnixClock: Send + Sync + 'static {
    /// Milliseconds since the unix epoch.
    fn now_ms(&self) -> i64;
}

/// The system clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemUnixClock;

impl UnixClock for SystemUnixClock {
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// A settable clock (clones share the time).
#[derive(Clone, Debug, Default)]
pub struct ManualUnixClock(Arc<AtomicI64>);

impl ManualUnixClock {
    /// Clock at `ms`.
    pub fn at(ms: i64) -> Self {
        Self(Arc::new(AtomicI64::new(ms)))
    }

    /// Advance by `ms`.
    pub fn advance_ms(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }

    /// Set to `ms`.
    pub fn set_ms(&self, ms: i64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

impl UnixClock for ManualUnixClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}
