//! `replication::server_send` (Render, send ticks): one Data packet per client.
//!
//! Per client: skip if `backlog_bytes > backlog_skip_bytes` (halve its snapshot rate until it
//! drains; kick `SlowConsumer` after 5 s above `backlog_kick_bytes`) → interest diff →
//! priority/budget selection → `SnapshotWriter` (copying cached component bytes, applying
//! `Audience` per component) → queued reliable/unreliable messages, Control, Pongs →
//! `LinkTx::send`. On datagram transports the snapshot is split at entity granularity into
//! packets ≤ `caps.max_unreliable`, each recorded in the `SentRecord` ring for acks; on
//! streams the entity baselines advance optimistically when the packet is handed to the link.

use crate::{NetId, Tick};

/// Per-client, per-entity replication state.
#[derive(Clone, Debug, Default)]
pub struct ClientEntity {
    /// Spawn delivered (acked / sent on a stream).
    pub spawned: bool,
    /// Client holds every component change up to this tick.
    pub ack_tick: Tick,
    /// Last tick included in a packet (avoids resending while an ack is outstanding).
    pub last_sent_tick: Tick,
    /// Priority accumulator.
    pub acc: f32,
    /// Out of the leave radius since (interest hysteresis).
    pub leaving_since: Option<Tick>,
}

/// Entities in one sent datagram packet (acks advance their `ack_tick`).
#[derive(Clone, Debug, Default)]
pub struct SentRecord {
    /// Packet sequence.
    pub seq: u16,
    /// Snapshot tick.
    pub tick: Tick,
    /// Entities written.
    pub entities: Vec<NetId>,
    /// Despawns written.
    pub despawns: Vec<NetId>,
}

/// Per-client view (internal to the replication builder).
#[derive(Default)]
pub struct ClientView {
    /// Visible entities.
    pub entities: crate::FastMap<NetId, ClientEntity>,
    /// Datagram send history (256 entries).
    pub sent: Vec<SentRecord>,
    /// Next datagram sequence.
    pub next_seq: u16,
    /// Budget.
    pub budget: Option<super::priority::ByteBudget>,
    /// Every in-scope entity delivered once.
    pub sync_complete: bool,
    /// Next snapshot must carry `FULL_RESET`.
    pub full_reset: bool,
}
