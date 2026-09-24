//! Room directory (`RoomManager`, clone = shared): the only way the supervisor, ingress,
//! admin API and fleet link reach rooms. Never touches a room `World`: it sends
//! [`RoomCommand`]s (bounded channel, applied by `RoomPlugin` in PreUpdate) and reads the
//! [`RoomStatus`] each room publishes at 1 Hz.

use super::checkpoint::Checkpoint;
use crate::game::RoomSpec;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use space3d_replication::protocol::control::NoticeLevel;
use space3d_replication::server::RoomSender;
use space3d_replication::{PlayerId, RoomId};
use std::sync::Arc;
use std::time::Duration;

/// Room lifecycle phase (published in `RoomStatus`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoomPhase {
    /// Created, waiting for players.
    #[default]
    Waiting,
    /// Playing.
    Running,
    /// Match over / draining; no new joins.
    Ending,
    /// Closed (about to be removed).
    Closed,
    /// Panicked; being restored from its checkpoint.
    Crashed,
}

/// Commands to a room (applied on its shard thread in PreUpdate).
#[derive(Debug)]
pub enum RoomCommand {
    /// Kick a player.
    Kick {
        /// Player.
        player: PlayerId,
        /// Reason.
        reason: String,
    },
    /// Broadcast a notice.
    Broadcast {
        /// Text.
        text: String,
        /// Level.
        level: NoticeLevel,
    },
    /// Close the room (disconnect everyone, final checkpoint/results, remove).
    Close {
        /// Reason.
        reason: String,
    },
    /// Server drain: warn clients; match rooms end by `deadline`, persistent rooms checkpoint now.
    Drain {
        /// Time left.
        deadline: Duration,
    },
    /// Checkpoint now.
    Checkpoint,
    /// Accept/refuse new joins.
    SetJoinable(bool),
    /// Request a detailed snapshot for the admin API.
    Inspect(futures::channel::oneshot::Sender<RoomDetail>),
}

/// Room summary, published at 1 Hz (admin API, fleet heartbeat, metrics).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RoomStatus {
    /// Room.
    pub room_id: RoomId,
    /// Mode.
    pub mode: String,
    /// Phase.
    pub phase: RoomPhase,
    /// Connected players.
    pub players: u16,
    /// Capacity.
    pub max_players: u16,
    /// Accepting joins.
    pub joinable: bool,
    /// Current tick.
    pub tick: u32,
    /// Tick time p99 over the last second (µs).
    pub tick_p99_us: u32,
    /// Downstream bytes/s.
    pub bytes_out_per_sec: u64,
    /// Created (unix ms).
    pub created_ms: i64,
}

/// Detailed room view (admin `GET /admin/v1/rooms/{id}`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RoomDetail {
    /// Summary.
    pub status: RoomStatus,
    /// `(player, name, rtt_ms, down_bytes_per_sec, state)` per slot.
    pub players: Vec<(PlayerId, String, f32, u32, String)>,
    /// Replicated entities.
    pub entities: u32,
}

/// Why a room could not be created.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RoomError {
    /// `max_rooms` reached.
    #[error("room capacity reached")]
    Capacity,
    /// Draining.
    #[error("server draining")]
    Draining,
    /// Spec rejected by the game.
    #[error("invalid spec: {0}")]
    InvalidSpec(String),
    /// The room is gone.
    #[error("room gone")]
    Gone,
}

/// Handle to one room.
#[derive(Clone)]
pub struct RoomHandle {
    /// Id.
    pub id: RoomId,
    /// Inbox (ingress → room).
    pub sender: RoomSender,
    /// Commands.
    pub commands: crossbeam_channel::Sender<RoomCommand>,
    /// Latest published status.
    pub status: Arc<Mutex<RoomStatus>>,
}

impl RoomHandle {
    /// Send a command without blocking.
    pub fn command(&self, cmd: RoomCommand) -> Result<(), RoomError> {
        self.commands.try_send(cmd).map_err(|_| RoomError::Gone)
    }
}

/// Room directory (clone = shared).
#[derive(Clone)]
pub struct RoomManager {
    inner: Arc<Mutex<Directory>>,
}

struct Directory {
    rooms: std::collections::BTreeMap<RoomId, RoomHandle>,
    shards: super::shard::ShardPool,
    draining: bool,
    max_rooms: usize,
}

impl RoomManager {
    /// Directory over a shard pool.
    pub fn new(shards: super::shard::ShardPool, max_rooms: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Directory {
                rooms: Default::default(),
                shards,
                draining: false,
                max_rooms,
            })),
        }
    }

    /// Existing room.
    pub fn get(&self, id: RoomId) -> Option<RoomHandle> {
        self.inner.lock().rooms.get(&id).cloned()
    }

    /// Existing room, or create it from `spec` (non-blocking: the inbox exists immediately; the
    /// `App` is built on the least-loaded shard).
    pub fn get_or_create(&self, spec: &RoomSpec) -> Result<RoomHandle, RoomError> {
        todo!()
    }

    /// Create a room, optionally restoring a checkpoint.
    pub fn create(&self, spec: RoomSpec, restore: Option<Checkpoint>) -> Result<RoomHandle, RoomError> {
        todo!()
    }

    /// All room statuses.
    pub fn list(&self) -> Vec<RoomStatus> {
        todo!()
    }

    /// Room currently hosting `player`, if any.
    pub fn find_player(&self, player: PlayerId) -> Option<RoomId> {
        todo!()
    }

    /// Send a command to every room.
    pub fn broadcast(&self, make: impl Fn() -> RoomCommand) {
        todo!()
    }

    /// Stop creating rooms.
    pub fn set_draining(&self, yes: bool) {
        self.inner.lock().draining = yes;
    }

    /// Remove a closed room (called by the shard when the room reports `Closed`).
    pub fn remove(&self, id: RoomId) {
        self.inner.lock().rooms.remove(&id);
    }
}
