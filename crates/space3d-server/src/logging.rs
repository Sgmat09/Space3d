//! Structured logging: `tracing-subscriber` with JSON (production) or pretty (dev) output and
//! an `EnvFilter` (`[log].filter`, overridable by `RUST_LOG`).
//!
//! Spans: the process runs inside a `server` span (`server_id`, `role`; entered by
//! `ServerMain` on the supervisor thread and inherited by the listeners' connection tasks, the
//! ingress task, the fleet link and the shard threads), and every room tick inside a fresh
//! `room` span ([`room_tick_span`]: `server_id`, `room_id`, `epoch`, `tick`), so replication
//! warnings from a room carry its id. JSON output includes the current span's fields. Both
//! are `ERROR`-level spans on purpose: context spans must stay enabled under any filter (a
//! `warn` filter would otherwise drop them and with them the ids on every warning). Other
//! fields: `client`, `player_id`, `link`.
//! Security events log at `warn` with an `event = "..."` field (bad_ticket, strike, kick, ban,
//! admin_action); the `space3d::audit` target records every admin action.

use crate::config::{LogConfig, LogFormat};
use tracing_subscriber::EnvFilter;

/// Tracing target of admin audit records.
pub const AUDIT_TARGET: &str = "space3d::audit";

/// Resource: the process span (see the module docs).
#[derive(Clone, Debug)]
pub struct ProcessSpan(pub tracing::Span);

/// The `server` span of a process.
pub fn process_span(config: &crate::ServerConfig) -> tracing::Span {
    tracing::error_span!("server", server_id = %config.server_id, role = ?config.role)
}

/// The `room` span one tick of a room runs in (`server_id`, `room_id`, `epoch`, `tick`).
///
/// A fresh span per tick, never one long-lived span with the tick `record`ed on it: the text
/// formatter appends every recorded value to the span's stored fields (` tick=1 tick=2 …`), so
/// a room's span — and every line logged in it — would grow without bound.
pub fn room_tick_span(app: &space3d_core::App, server_id: &str, room: space3d_replication::RoomId) -> tracing::Span {
    let (epoch, tick) = app
        .resources
        .get::<space3d_replication::server::NetServer>()
        .map_or((0, 0), |n| (n.config().room_epoch, n.tick().0));
    tracing::error_span!("room", server_id = %server_id, room_id = %room, epoch, tick)
}

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
