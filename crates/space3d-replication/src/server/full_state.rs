//! Full-state encoding of a room, used for **checkpoints** (crash recovery, graceful restart)
//! and for tests. Reuses the replication codecs, so it is compact and fast enough to run on the
//! room thread (bytes are then compressed/uploaded off-thread by `space3d-server`).
//!
//! Server-only state (AI, timers in resources) is out of scope here: the server crate adds it
//! with `space3d_scene::ComponentRegistry` and a game resource hook.

use crate::wire::{DecodeError, EncodeError};
use crate::{NetId, Tick};
use serde::{Deserialize, Serialize};
use space3d_core::App;

/// One entity in a [`FullState`]: its `NetId` and `(registered component name, codec bytes)`.
pub type EncodedEntity = (NetId, Vec<(String, Vec<u8>)>);

/// Replicated state of a room at a tick.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FullState {
    /// Schema hash of the registry that wrote it (restore refuses a mismatch).
    pub schema_hash: u64,
    /// Room tick.
    pub tick: Tick,
    /// Room epoch at capture (restore bumps it).
    pub epoch: u32,
    /// Next `NetId` to allocate (ids are kept across restore).
    pub next_net_id: u32,
    /// `(NetId, [(kind name, codec bytes)])` for every `Replicated` entity.
    pub entities: Vec<EncodedEntity>,
}

/// Capture every `Replicated` entity of a room (call on the room thread, e.g. from a system via
/// `AsyncQueue` bookkeeping in `space3d-server`).
pub fn capture(app: &App) -> Result<FullState, EncodeError> {
    todo!()
}

/// Recreate entities (with `Replicated` + all registered components) and continue at
/// `state.tick`, epoch + 1. Call after plugins are added and before the first `update`.
/// Returns the restored `(NetId, Entity)` pairs.
pub fn restore(app: &mut App, state: &FullState) -> Result<Vec<(NetId, space3d_core::Entity)>, DecodeError> {
    todo!()
}
