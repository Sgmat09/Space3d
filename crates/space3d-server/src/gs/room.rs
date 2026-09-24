//! Inside a room `App`: construction ([`build_room_app`]) and the server-side room plugin.
//!
//! Construction order (on the shard thread): `App::new().with_tickrate(tick_hz)` →
//! resources (`NetInbox`, `SecretSource` (getrandom), `NetClockHandle`, `Metrics`,
//! [`RoomContext`], [`RoomControl`]) → `game.protocol(app)` → `ServerReplicationPlugin`
//! (config from `game.room_net_config`) → [`RoomPlugin`] → `game.build_room(app, spec)` →
//! `full_state::restore` + server-only restore if a checkpoint is given (epoch + 1).
//!
//! `RoomPlugin` systems: PreUpdate `room::commands` (apply [`RoomCommand`]s: kick, broadcast,
//! close, drain, checkpoint, joinable, inspect); Last `room::ops` (1 Hz `RoomStatus` publish,
//! checkpoint cadence via `AsyncQueue::compute_then` serialization + `CheckpointStore::put`,
//! empty-room TTL, forwarding [`RoomControl`] writes to the outbox, player joined/left fleet
//! notifications).

use super::checkpoint::{Checkpoint, CheckpointStore};
use super::outbox::OutboxSender;
use super::rooms::{RoomCommand, RoomStatus};
use crate::game::{GameDefinition, RoomSpec};
use crate::metrics::Metrics;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use space3d_core::App;
use space3d_replication::server::{NetInbox, ServerNetConfig};
use space3d_replication::{MatchId, PlayerId, RoomId};
use std::sync::Arc;

/// Everything needed to build a room on its shard.
pub struct RoomBuild {
    /// Spec.
    pub spec: RoomSpec,
    /// Game.
    pub game: Arc<dyn GameDefinition>,
    /// Base replication config (server-wide).
    pub net: ServerNetConfig,
    /// Tick rate.
    pub tick_hz: f64,
    /// Inbox.
    pub inbox: NetInbox,
    /// Commands.
    pub commands: crossbeam_channel::Receiver<RoomCommand>,
    /// Status cell.
    pub status: Arc<Mutex<RoomStatus>>,
    /// Outbox for results / player data.
    pub outbox: OutboxSender,
    /// Checkpoint store.
    pub checkpoints: Arc<dyn CheckpointStore>,
    /// Checkpoint to restore.
    pub restore: Option<Checkpoint>,
    /// Metrics registry.
    pub metrics: Metrics,
    /// Server id.
    pub server_id: String,
    /// Creation time (unix ms).
    pub created_ms: i64,
    /// Checkpoint cadence.
    pub checkpoint_interval: std::time::Duration,
}

/// Build a room `App` (call on the thread that will run it).
pub fn build_room_app(build: RoomBuild) -> Result<App, super::rooms::RoomError> {
    todo!()
}

/// Resource: static facts about the room.
#[derive(Clone, Debug)]
pub struct RoomContext {
    /// Spec.
    pub spec: RoomSpec,
    /// Hosting server.
    pub server_id: String,
    /// Created (unix ms).
    pub created_ms: i64,
}

/// One player's outcome.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlayerResult {
    /// Player.
    pub player: PlayerId,
    /// Team.
    pub team: Option<u16>,
    /// Score.
    pub score: i64,
    /// Placement (1 = winner).
    pub placement: Option<u16>,
    /// Game stats.
    pub stats: serde_json::Value,
}

/// Result of a match (→ outbox → player service: match history, leaderboards, Elo).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MatchResult {
    /// Match.
    pub match_id: MatchId,
    /// Room.
    pub room_id: RoomId,
    /// Mode.
    pub mode: String,
    /// Start (unix ms).
    pub started_ms: i64,
    /// End (unix ms).
    pub ended_ms: i64,
    /// Winning team, if any.
    pub winner_team: Option<u16>,
    /// Players.
    pub players: Vec<PlayerResult>,
    /// Leaderboard submissions `(board, player, score)`.
    pub scores: Vec<(String, PlayerId, i64)>,
    /// Extra data.
    pub data: serde_json::Value,
}

/// Resource: room → outside world (all calls non-blocking; delivered at-least-once through the
/// durable outbox, deduplicated by op id on the player service).
#[derive(Default)]
pub struct RoomControl {
    pending: Vec<RoomRequest>,
}

/// Queued room request (internal).
#[derive(Debug)]
pub enum RoomRequest {
    /// Match finished.
    EndMatch(MatchResult),
    /// Player data write.
    WritePlayerData {
        /// Player.
        player: PlayerId,
        /// Key.
        key: String,
        /// Value.
        value: serde_json::Value,
    },
    /// Leaderboard score.
    SubmitScore {
        /// Board.
        board: String,
        /// Player.
        player: PlayerId,
        /// Score.
        score: i64,
    },
    /// Load player data (answer arrives as `Events<PlayerDataLoaded>`).
    LoadPlayerData {
        /// Player.
        player: PlayerId,
        /// Keys.
        keys: Vec<String>,
    },
    /// Checkpoint now.
    Checkpoint,
    /// Close the room.
    Close(String),
}

impl RoomControl {
    /// Report the match result.
    pub fn end_match(&mut self, result: MatchResult) {
        self.pending.push(RoomRequest::EndMatch(result));
    }

    /// Persist a player-data key (e.g. `"progress"`, `"inventory"`).
    pub fn write_player_data(&mut self, player: PlayerId, key: &str, value: serde_json::Value) {
        self.pending.push(RoomRequest::WritePlayerData {
            player,
            key: key.to_owned(),
            value,
        });
    }

    /// Submit a leaderboard score.
    pub fn submit_score(&mut self, board: &str, player: PlayerId, score: i64) {
        self.pending.push(RoomRequest::SubmitScore {
            board: board.to_owned(),
            player,
            score,
        });
    }

    /// Ask for player data (e.g. on `PlayerJoined`); answered via `Events<PlayerDataLoaded>`.
    pub fn load_player_data(&mut self, player: PlayerId, keys: &[&str]) {
        self.pending.push(RoomRequest::LoadPlayerData {
            player,
            keys: keys.iter().map(|k| (*k).to_owned()).collect(),
        });
    }

    /// Checkpoint at the end of this tick.
    pub fn request_checkpoint(&mut self) {
        self.pending.push(RoomRequest::Checkpoint);
    }

    /// Close the room after this tick.
    pub fn close(&mut self, reason: &str) {
        self.pending.push(RoomRequest::Close(reason.to_owned()));
    }

    /// Drain queued requests (room plugin).
    pub fn take(&mut self) -> Vec<RoomRequest> {
        std::mem::take(&mut self.pending)
    }
}

/// Event: player data requested with `RoomControl::load_player_data` arrived.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerDataLoaded {
    /// Player.
    pub player: PlayerId,
    /// Entries (missing keys are absent).
    pub entries: Vec<space3d_replication::api::PlayerDataEntry>,
}

/// The server-side room plugin (see the module docs).
pub struct RoomPlugin;

impl space3d_core::Plugin for RoomPlugin {
    fn build(&self, app: &mut App) {
        todo!()
    }
}
