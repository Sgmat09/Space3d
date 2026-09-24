//! Structured logging: `tracing-subscriber` with JSON (production) or pretty (dev) output and
//! an `EnvFilter` (`[log].filter`, overridable by `RUST_LOG`). Spans/fields used across the
//! crate: `server_id`, `role`, `room_id`, `client`, `player_id`, `link`, `tick`. Security
//! events log at `warn` with an `event = "..."` field (bad_ticket, strike, kick, ban,
//! admin_action); the `space3d::audit` target records every admin action.

use crate::config::LogConfig;

/// Install the global subscriber (idempotent; later calls are ignored).
pub fn init(config: &LogConfig) {
    todo!()
}
