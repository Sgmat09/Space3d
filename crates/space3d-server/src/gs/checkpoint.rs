//! Room checkpoints for crash recovery and graceful restarts.
//!
//! A checkpoint = replicated state (`space3d_replication::server::full_state::FullState`,
//! codec bytes, fast) + optional server-only state (the game's `ComponentRegistry` →
//! `space3d_scene` JSON) + an optional game blob, written every `checkpoint_interval`, on phase
//! changes, on drain and on request. Serialization runs off the tick (`AsyncQueue::compute_then`);
//! at most one upload per room is in flight (a newer checkpoint replaces a queued one).

use crate::game::RoomSpec;
use serde::{Deserialize, Serialize};
use space3d_replication::RoomId;
use space3d_replication::server::full_state::FullState;
use space3d_tasks::Task;

/// Format version of [`Checkpoint`].
pub const CHECKPOINT_VERSION: u16 = 1;

/// A room checkpoint.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// [`CHECKPOINT_VERSION`].
    pub version: u16,
    /// Room.
    pub room_id: RoomId,
    /// Spec (to rebuild the room).
    pub spec: RoomSpec,
    /// Replicated state (tick, epoch, schema hash inside).
    pub state: FullState,
    /// Server-only components (scene JSON from `GameDefinition::checkpoint_registry`).
    pub server_only: Option<String>,
    /// Game-defined blob (resources).
    pub game_blob: Option<Vec<u8>>,
    /// Save time (unix ms).
    pub saved_ms: i64,
}

impl Checkpoint {
    /// postcard bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        todo!()
    }

    /// Inverse of [`Self::to_bytes`] (checks the version).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        todo!()
    }
}

/// Checkpoint store failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("checkpoint store: {0}")]
pub struct StoreError(pub String);

/// Where checkpoints live. Every call returns a pollable `Task` (never blocks a tick).
pub trait CheckpointStore: Send + Sync + 'static {
    /// Save (replace) a room's checkpoint.
    fn put(&self, room: RoomId, bytes: Vec<u8>) -> Task<Result<(), StoreError>>;
    /// Latest checkpoint of a room.
    fn get(&self, room: RoomId) -> Task<Result<Option<Vec<u8>>, StoreError>>;
    /// Rooms with checkpoints newer than `min_saved_ms`.
    fn recent(&self, min_saved_ms: i64) -> Task<Result<Vec<RoomId>, StoreError>>;
    /// Delete (room closed normally).
    fn delete(&self, room: RoomId) -> Task<Result<(), StoreError>>;
}

/// Local SQLite (`space3d_db::Db` on the game server's volume; single-host setups).
pub struct LocalCheckpoints {
    /// Database.
    pub db: space3d_db::Db,
}

/// Player-service storage (`PUT/GET /internal/v1/rooms/{id}/checkpoint`, fleet token): any game
/// server can restore a room after a server loss. Default in split mode.
pub struct RemoteCheckpoints {
    /// HTTP client.
    pub http: space3d_net::Http,
    /// Player service base URL.
    pub base_url: String,
    /// Fleet bearer token.
    pub token: String,
}

/// In-memory (tests).
#[derive(Default)]
pub struct MemoryCheckpoints {
    /// Stored bytes.
    pub map: parking_lot::Mutex<std::collections::BTreeMap<RoomId, (i64, Vec<u8>)>>,
}

/// No-op store (`checkpoint_store = "none"`).
pub struct NoCheckpoints;

macro_rules! todo_store {
    ($t:ty) => {
        impl CheckpointStore for $t {
            fn put(&self, room: RoomId, bytes: Vec<u8>) -> Task<Result<(), StoreError>> {
                todo!()
            }
            fn get(&self, room: RoomId) -> Task<Result<Option<Vec<u8>>, StoreError>> {
                todo!()
            }
            fn recent(&self, min_saved_ms: i64) -> Task<Result<Vec<RoomId>, StoreError>> {
                todo!()
            }
            fn delete(&self, room: RoomId) -> Task<Result<(), StoreError>> {
                todo!()
            }
        }
    };
}

todo_store!(LocalCheckpoints);
todo_store!(RemoteCheckpoints);
todo_store!(MemoryCheckpoints);
todo_store!(NoCheckpoints);
