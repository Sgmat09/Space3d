//! Fleet registry (player-service side): registrations, heartbeats, leases, room directory,
//! allocation, orphan detection.
//!
//! Allocation: servers in the region with matching `game_id` + `schema_hash`, `Ready`, not
//! draining, enough free seats (capacity − players − reserved) → lowest
//! `players/capacity + tick_p99/tick_budget` → reserve seats for the ticket TTL.

use crate::gs::fleet::{FleetDown, FleetUp, Heartbeat, ServerRegistration};
use space3d_replication::{PlayerId, RoomId};

/// Server liveness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerState {
    /// Heartbeating.
    Ready,
    /// Draining (no new allocations).
    Draining,
    /// Lease expired.
    Dead,
}

/// One registered game server.
#[derive(Clone, Debug, PartialEq)]
pub struct ServerRecord {
    /// Registration.
    pub registration: ServerRegistration,
    /// Last heartbeat.
    pub heartbeat: Heartbeat,
    /// Last heartbeat time (unix ms).
    pub last_seen_ms: i64,
    /// State.
    pub state: ServerState,
    /// Seats reserved by unexpired tickets.
    pub reserved: u32,
}

/// Side effects the supervisor must perform after feeding a message.
#[derive(Clone, Debug, PartialEq)]
pub enum FleetEffect {
    /// Send to a server.
    Send {
        /// Server id.
        server: String,
        /// Message.
        msg: FleetDown,
    },
    /// Apply an outbox op (store), then ack it.
    Apply {
        /// Server id.
        server: String,
        /// The op.
        op: FleetUp,
    },
    /// A server died: its rooms are orphaned (eligible for rejoin + restore).
    Orphaned {
        /// Rooms.
        rooms: Vec<RoomId>,
    },
}

/// The registry.
#[derive(Debug, Default)]
pub struct FleetRegistry {
    servers: std::collections::BTreeMap<String, ServerRecord>,
    rooms: std::collections::BTreeMap<RoomId, (String, crate::game::RoomSpec)>,
    presence: std::collections::BTreeMap<PlayerId, RoomId>,
}

impl FleetRegistry {
    /// Feed a message from server `server_id` (the connection's registered id) at `now_ms`.
    pub fn on_message(&mut self, server_id: &str, msg: FleetUp, now_ms: i64) -> Vec<FleetEffect> {
        todo!()
    }

    /// Expire leases; returns effects for dead servers.
    pub fn sweep(&mut self, now_ms: i64, lease_ttl_ms: i64) -> Vec<FleetEffect> {
        todo!()
    }

    /// Pick a server for `seats` new players.
    pub fn allocate(&mut self, region: &str, game_id: &str, schema_hash: u64, seats: u32) -> Option<ServerRecord> {
        todo!()
    }

    /// An open room of `mode` with `seats` free (quick play / backfill).
    pub fn open_room(&self, mode: &str, seats: u32) -> Option<(ServerRecord, crate::game::RoomSpec)> {
        todo!()
    }

    /// Where `player` currently plays (rejoin).
    pub fn room_of(&self, player: PlayerId) -> Option<(RoomId, String)> {
        todo!()
    }

    /// All servers.
    pub fn servers(&self) -> Vec<ServerRecord> {
        self.servers.values().cloned().collect()
    }
}
