//! Durable outbox (game server): results, player-data writes and scores are written to the
//! local `space3d-db` (`outbox(op_id PK, payload, created_ms, attempts)`) before being sent over
//! the fleet link, and deleted only on `FleetDown::Ack { op_id }`. Survives player-service
//! outages and graceful restarts; replayed on the next start with the same `server_id` and
//! volume (StatefulSet PVC). On an ephemeral volume a hard crash loses unsent entries —
//! documented trade-off.
//!
//! `op_id = "<server_id>:<room_id>:<counter>"` (or `match_id:player_id` for results), so
//! retries are idempotent on the player service (`applied_ops`).

use super::fleet::FleetUp;
use space3d_tasks::Task;

/// The outbox channel is full.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("outbox full")]
pub struct OutboxFull;

/// Non-blocking handle rooms use to enqueue (bounded channel into the outbox service).
#[derive(Clone, Debug)]
pub struct OutboxSender {
    tx: async_channel::Sender<FleetUp>,
}

impl OutboxSender {
    /// Queue a message (assigns an op id if the variant has one and it is empty).
    /// `Err(OutboxFull)` if the outbox service is not keeping up (the room retries next tick).
    pub fn push(&self, msg: FleetUp) -> Result<(), OutboxFull> {
        self.tx.try_send(msg).map_err(|_| OutboxFull)
    }
}

/// The outbox service (owned by the supervisor).
pub struct Outbox {
    db: space3d_db::Db,
    rx: async_channel::Receiver<FleetUp>,
    tx: async_channel::Sender<FleetUp>,
}

impl Outbox {
    /// Open (runs the outbox migration) — resolves when ready.
    pub fn open(db: space3d_db::Db) -> Task<Result<Self, space3d_db::DbError>> {
        todo!()
    }

    /// Sender for rooms.
    pub fn sender(&self) -> OutboxSender {
        OutboxSender { tx: self.tx.clone() }
    }

    /// Persist newly queued messages and send everything unacked over `link` (supervisor tick).
    pub fn pump(&mut self, link: &dyn super::fleet::FleetLink) {
        todo!()
    }

    /// Mark an op applied (deletes it).
    pub fn ack(&mut self, op_id: &str) {
        todo!()
    }

    /// Entries not yet acked.
    pub fn pending(&self) -> usize {
        todo!()
    }
}
