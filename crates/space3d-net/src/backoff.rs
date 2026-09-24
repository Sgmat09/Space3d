//! Exponential backoff shared by HTTP retries and WebSocket reconnects.

use std::time::Duration;

/// Exponential backoff: `initial * multiplier^attempt`, capped at `max`.
///
/// Deterministic (no jitter) so tests and replays behave identically.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Backoff {
    /// Delay before the first retry.
    pub initial: Duration,
    /// Upper bound for any single delay.
    pub max: Duration,
    /// Growth factor per attempt (values below 1.0 are treated as 1.0).
    pub multiplier: f64,
}

impl Backoff {
    /// Doubling backoff from `initial` up to `max`.
    pub const fn new(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max,
            multiplier: 2.0,
        }
    }

    /// Constant delay between attempts.
    pub const fn constant(delay: Duration) -> Self {
        Self {
            initial: delay,
            max: delay,
            multiplier: 1.0,
        }
    }

    /// Delay before retry number `attempt` (0-based).
    pub fn delay(&self, attempt: u32) -> Duration {
        let factor = self.multiplier.max(1.0).powi(attempt.min(64) as i32);
        let secs = (self.initial.as_secs_f64() * factor).min(self.max.as_secs_f64());
        Duration::try_from_secs_f64(secs).unwrap_or(self.max)
    }
}

impl Default for Backoff {
    /// 200 ms doubling up to 10 s.
    fn default() -> Self {
        Self::new(Duration::from_millis(200), Duration::from_secs(10))
    }
}

/// When and how often an HTTP request is retried.
///
/// Retries happen on [`crate::NetError::is_retryable`] errors and, if
/// `retry_on_status` is set, on `408`, `429` and `5xx` responses. Retries are
/// opt-in: be careful enabling them for non-idempotent requests (`POST`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetryPolicy {
    /// Extra attempts after the first one (0 = no retries).
    pub max_retries: u32,
    /// Delay schedule between attempts.
    pub backoff: Backoff,
    /// Also retry on `408`, `429` and `5xx` status codes.
    pub retry_on_status: bool,
}

impl RetryPolicy {
    /// Never retry.
    pub const fn none() -> Self {
        Self {
            max_retries: 0,
            backoff: Backoff::new(Duration::from_millis(100), Duration::from_secs(5)),
            retry_on_status: true,
        }
    }

    /// Retry up to `max_retries` times with 100 ms → 5 s doubling backoff.
    pub const fn exponential(max_retries: u32) -> Self {
        Self {
            max_retries,
            ..Self::none()
        }
    }

    /// Replace the backoff schedule.
    pub const fn with_backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }

    /// Whether a response status should trigger a retry under this policy.
    pub fn should_retry_status(&self, status: u16) -> bool {
        self.retry_on_status && (status == 408 || status == 429 || (500..600).contains(&status))
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::none()
    }
}

/// Automatic WebSocket reconnection (see [`crate::WsConfig::reconnect`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReconnectPolicy {
    /// Delay schedule between reconnect attempts; resets after a successful connect.
    pub backoff: Backoff,
    /// Give up after this many consecutive failed attempts (`None` = retry forever).
    pub max_attempts: Option<u32>,
}

impl ReconnectPolicy {
    /// Reconnect forever using `backoff`.
    pub const fn new(backoff: Backoff) -> Self {
        Self {
            backoff,
            max_attempts: None,
        }
    }

    /// Give up after `n` consecutive failed attempts.
    pub const fn max_attempts(mut self, n: u32) -> Self {
        self.max_attempts = Some(n);
        self
    }

    pub(crate) fn allows(&self, attempt: u32) -> bool {
        self.max_attempts.is_none_or(|m| attempt < m)
    }
}

impl Default for ReconnectPolicy {
    /// Forever, 250 ms doubling up to 10 s.
    fn default() -> Self {
        Self::new(Backoff::new(Duration::from_millis(250), Duration::from_secs(10)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let b = Backoff::new(Duration::from_millis(100), Duration::from_millis(500));
        assert_eq!(b.delay(0), Duration::from_millis(100));
        assert_eq!(b.delay(1), Duration::from_millis(200));
        assert_eq!(b.delay(2), Duration::from_millis(400));
        assert_eq!(b.delay(3), Duration::from_millis(500));
        assert_eq!(b.delay(u32::MAX), Duration::from_millis(500));
        let c = Backoff::constant(Duration::from_millis(7));
        assert_eq!(c.delay(10), Duration::from_millis(7));
    }

    #[test]
    fn retry_statuses() {
        let p = RetryPolicy::exponential(2);
        assert!(p.should_retry_status(503));
        assert!(p.should_retry_status(429));
        assert!(!p.should_retry_status(404));
        assert!(!p.should_retry_status(200));
    }

    #[test]
    fn reconnect_attempt_limit() {
        let p = ReconnectPolicy::default().max_attempts(2);
        assert!(p.allows(0) && p.allows(1) && !p.allows(2));
        assert!(ReconnectPolicy::default().allows(1_000_000));
    }
}
