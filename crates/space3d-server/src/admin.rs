//! Admin HTTP API (separate listener, default `127.0.0.1:9090`; never public).
//! `Authorization: Bearer <admin token>` compared in constant time against the configured
//! tokens; every mutating call is audit-logged (`space3d::audit` target + player-service
//! `audit_log` table). `/metrics`, `/healthz`, `/readyz` need no token.
//!
//! ```text
//! GET  /admin/v1/status                     process, role, phase, rooms, players, tick p99
//! GET  /admin/v1/rooms                      RoomStatus list
//! GET  /admin/v1/rooms/{id}                 RoomDetail (players, per-client rtt/bandwidth)
//! POST /admin/v1/rooms/{id}/close           {reason}
//! POST /admin/v1/rooms/{id}/checkpoint      force a checkpoint
//! GET  /admin/v1/players                    connected players (game server) / search (player service)
//! POST /admin/v1/players/{id}/kick          {reason}
//! POST /admin/v1/bans                       {player?|cidr?, duration?, reason}  (player service: persisted + pushed)
//! DELETE /admin/v1/bans/{id}
//! POST /admin/v1/broadcast                  {text, room?, level}
//! POST /admin/v1/drain                      start graceful drain
//! GET  /admin/v1/config                     redacted effective config
//! GET  /admin/v1/schema                     ReplicationRegistry::manifest() of this game
//! POST /admin/v1/backup                     player service: VACUUM INTO data_dir/backup-<ts>.db
//! GET  /metrics
//! ```

use crate::gs::rooms::RoomManager;
use crate::health::HealthState;
use crate::lifecycle::LifecycleHandle;
use crate::metrics::Metrics;
use crate::security::bans::BanList;
use std::sync::Arc;

/// Everything the admin routes need (clone = shared).
#[derive(Clone)]
pub struct AdminState {
    /// SHA-256 of each accepted admin token.
    pub token_hashes: Arc<Vec<[u8; 32]>>,
    /// Rooms (game-server role).
    pub rooms: Option<RoomManager>,
    /// Player service (player-service role).
    pub players: Option<Arc<crate::ps::PlayerService>>,
    /// Bans.
    pub bans: BanList,
    /// Lifecycle control (drain).
    pub lifecycle: LifecycleHandle,
    /// Metrics registry.
    pub metrics: Metrics,
    /// Health.
    pub health: HealthState,
    /// Redacted config text.
    pub config_text: Arc<String>,
    /// Schema manifest lines.
    pub schema: Arc<Vec<String>>,
}

/// Build the admin router.
pub fn router(state: AdminState) -> space3d_net::server::axum::Router {
    todo!()
}
