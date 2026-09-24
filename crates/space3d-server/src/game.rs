//! How a game plugs into the server: [`GameDefinition`] and [`RoomSpec`].

use serde::{Deserialize, Serialize};
use space3d_core::App;
use space3d_replication::server::ServerNetConfig;
use space3d_replication::{MatchId, PlayerId, RoomId};
use std::collections::BTreeMap;

/// What to build for a room. Carried inside join tickets (so a game server can create the room
/// lazily on the first valid ticket, with no RPC on the match-start path) and in
/// `FleetDown::PrepareRoom` (pre-warm). Keep `params` small: tickets are ≤ 1 KiB.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomSpec {
    /// Room id (allocated by the player service; random, never 0).
    pub room_id: RoomId,
    /// Game mode (`"duel"`, `"zone"`).
    pub mode: String,
    /// Map / level.
    pub map: String,
    /// Deterministic seed for the room.
    pub seed: u64,
    /// Capacity.
    pub max_players: u16,
    /// Competitive match id (results are keyed by it), if any.
    pub match_id: Option<MatchId>,
    /// Team rosters from matchmaking (empty for drop-in rooms).
    pub teams: Vec<Vec<PlayerId>>,
    /// Persistent world room: never closes when empty, checkpoints on every interval.
    pub persistent: bool,
    /// Free-form game parameters.
    pub params: BTreeMap<String, String>,
}

/// What to do with a room whose server died.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoveryPolicy {
    /// Restore from the latest checkpoint on another server (default).
    #[default]
    Restore,
    /// Void the match; players return to matchmaking.
    Abandon,
}

/// A game's server-side definition. All methods are called from shard threads or the
/// supervisor, hence `Send + Sync`.
pub trait GameDefinition: Send + Sync + 'static {
    /// `Hello::game_id` this server accepts.
    fn game_id(&self) -> &str;

    /// Add the shared protocol plugin — the **same** function the client calls, so the schema
    /// hashes match. Also used once at startup (in a scratch `App`) to compute the schema hash
    /// reported to the fleet.
    fn protocol(&self, app: &mut App);

    /// Build a room (called on its shard thread after `protocol` and
    /// `ServerReplicationPlugin` were added): add server-only rules, spawn the level.
    fn build_room(&self, app: &mut App, spec: &RoomSpec);

    /// Per-room replication settings (default: the server-wide base from config).
    fn room_net_config(&self, spec: &RoomSpec, base: &ServerNetConfig) -> ServerNetConfig {
        let _ = spec;
        base.clone()
    }

    /// Reject specs this game cannot host (unknown mode/map).
    fn validate_spec(&self, spec: &RoomSpec) -> Result<(), String> {
        let _ = spec;
        Ok(())
    }

    /// Registry for **server-only** state included in checkpoints (AI, timers); replicated
    /// components are checkpointed by `space3d_replication::server::full_state` already.
    fn checkpoint_registry(&self) -> Option<space3d_scene::ComponentRegistry> {
        None
    }

    /// Recovery policy after a server loss.
    fn recovery_policy(&self, spec: &RoomSpec) -> RecoveryPolicy {
        let _ = spec;
        RecoveryPolicy::Restore
    }

    /// Room that `HelloAuth::Dev` joins are routed to (dev servers without a player service;
    /// `game.allow_dev_auth` on a loopback bind). Default: room 1, mode `"dev"`, 64 players.
    fn dev_room_spec(&self) -> RoomSpec {
        RoomSpec {
            room_id: RoomId(1),
            mode: "dev".into(),
            max_players: 64,
            ..Default::default()
        }
    }
}

/// Placeholder game for player-service-only processes (never builds rooms).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoGame;

impl GameDefinition for NoGame {
    fn game_id(&self) -> &str {
        ""
    }

    fn protocol(&self, _app: &mut App) {}

    fn build_room(&self, _app: &mut App, _spec: &RoomSpec) {
        unreachable!("NoGame never hosts rooms (role = \"player-service\")")
    }
}
