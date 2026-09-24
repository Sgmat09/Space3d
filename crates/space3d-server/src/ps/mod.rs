//! Player-service role: accounts, sessions, profiles & player data, leaderboards, matchmaking,
//! lobbies, fleet registry, room checkpoints (split mode), bans and audit log.
//!
//! Threading: axum handlers run on the I/O runtime and may `await` `Db` tasks (`Task::wait`)
//! — they are not frame code. Argon2 runs on the compute pool. Matchmaking, lobby starts, fleet
//! lease sweeps and allocation run in the supervisor `App` ([`PlayerServicePlugin`] systems,
//! 10 Hz) against [`PlayerService`] state, so they are deterministic under a manual clock.

pub mod auth;
pub mod fleet;
pub mod lobbies;
pub mod matchmaker;
pub mod results;
pub mod routes;
pub mod schema;
pub mod store;

use crate::clock::UnixClock;
use crate::config::ServerConfig;
use crate::metrics::PlayerServiceMetrics;
use crate::security::bans::BanList;
use crate::security::token::KeyRing;
use parking_lot::Mutex;
use std::net::SocketAddr;
use std::sync::Arc;

/// Shared player-service state (behind `Arc`; used by HTTP handlers and supervisor systems).
pub struct PlayerService {
    /// Config.
    pub config: ServerConfig,
    /// Storage.
    pub store: Arc<dyn store::PlayerStore>,
    /// Token keys.
    pub keys: Arc<KeyRing>,
    /// Wall clock.
    pub clock: Arc<dyn UnixClock>,
    /// Matchmaking queues.
    pub matchmaker: Mutex<matchmaker::Matchmaker>,
    /// Game-server registry.
    pub fleet: Mutex<fleet::FleetRegistry>,
    /// Lobbies.
    pub lobbies: Mutex<lobbies::Lobbies>,
    /// Bans (mirrors the store).
    pub bans: BanList,
    /// Metrics.
    pub metrics: PlayerServiceMetrics,
}

impl PlayerService {
    /// Open the store (migrations), load bans; resolves when ready.
    pub fn open(
        config: ServerConfig,
        db: space3d_db::Db,
        keys: Arc<KeyRing>,
        clock: Arc<dyn UnixClock>,
        metrics: PlayerServiceMetrics,
    ) -> space3d_tasks::Task<Result<Arc<Self>, crate::ServerError>> {
        todo!()
    }

    /// Mint a join ticket for `player` into `spec`'s room on `server_id` (60 s, single use).
    pub fn issue_ticket(
        &self,
        player: &store::Account,
        server: &fleet::ServerRecord,
        spec: &crate::game::RoomSpec,
        team: Option<u16>,
    ) -> space3d_replication::api::Assignment {
        todo!()
    }
}

/// Supervisor plugin for the player-service role: binds the HTTP API (public `/v1/*`, internal
/// `/internal/v1/*` incl. the fleet WebSocket), runs matchmaking / lobby / fleet systems,
/// publishes [`PlayerServiceHandle`].
pub struct PlayerServicePlugin {
    /// Service state.
    pub service: Arc<PlayerService>,
    /// In-process fleet peers (all-in-one mode).
    pub local_fleet: Vec<crate::gs::fleet::InProcessFleetPeer>,
}

impl space3d_core::Plugin for PlayerServicePlugin {
    fn build(&self, app: &mut space3d_core::App) {
        todo!()
    }
}

/// Resource: a running player service.
#[derive(Clone)]
pub struct PlayerServiceHandle {
    /// Bound HTTP address.
    pub addr: SocketAddr,
    /// State.
    pub service: Arc<PlayerService>,
}
