//! Structured logging: `tracing-subscriber` with JSON (production) or pretty (dev) output and
//! an `EnvFilter` (`[log].filter`, overridable by `RUST_LOG`). Spans/fields used across the
//! crate: `server_id`, `role`, `room_id`, `client`, `player_id`, `link`, `tick`. Security
//! events log at `warn` with an `event = "..."` field (bad_ticket, strike, kick, ban,
//! admin_action); the `space3d::audit` target records every admin action.

use crate::config::{LogConfig, LogFormat};
use tracing_subscriber::EnvFilter;

/// Tracing target of admin audit records.
pub const AUDIT_TARGET: &str = "space3d::audit";

/// Install the global subscriber (idempotent; later calls are ignored).
pub fn init(config: &LogConfig) {
    let filter = std::env::var("RUST_LOG")
        .ok()
        .and_then(|v| EnvFilter::try_new(v).ok())
        .or_else(|| EnvFilter::try_new(&config.filter).ok())
        .unwrap_or_else(|| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    let result = match config.format {
        LogFormat::Json => builder.json().with_current_span(true).with_span_list(false).try_init(),
        LogFormat::Pretty => builder.with_target(true).try_init(),
    };
    // Already installed (tests, embedding): keep the existing subscriber.
    let _ = result;
}

#[cfg(test)]
mod tests {
    #[test]
    fn init_is_idempotent() {
        let cfg = crate::config::LogConfig::default();
        super::init(&cfg);
        super::init(&cfg);
        tracing::info!(target: super::AUDIT_TARGET, action = "test", "audit record");
    }
}
