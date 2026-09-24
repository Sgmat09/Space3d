//! Fleet link: game server ↔ player service. JSON messages (low volume, debuggable) over a
//! WebSocket the game server opens to `fleet.player_service_url` (so game servers need no
//! inbound internal port), or an in-process channel pair in `all-in-one` mode.
//!
//! * First message: `FleetUp::Register` (carries the fleet token; the player service closes the
//!   link on a bad token). Reply: `FleetDown::Registered { lease_ttl_ms, bans }`.
//! * Heartbeat every `fleet.heartbeat` (load, per-room status, draining). A server whose lease
//!   (`fleet.lease_ttl`) lapses is Dead; its rooms become Orphaned (rejoin → restore elsewhere).
//! * Results / player data / scores travel as outbox entries with an `op_id` and are removed
//!   locally only on `FleetDown::Ack { op_id }` (at-least-once; the player service applies each
//!   op id once).
//! * Reconnect: own loop with `space3d_net::Backoff` (never `WsConfig::reconnect`); on reconnect
//!   Register again and replay the whole outbox.

use super::rooms::RoomStatus;
use crate::game::RoomSpec;
use crate::gs::room::MatchResult;
use crate::security::bans::Ban;
use serde::{Deserialize, Serialize};
use space3d_replication::api::PlayerDataEntry;
use space3d_replication::{PlayerId, RoomId};

/// Capacity advertised by a game server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capacity {
    /// Room cap.
    pub max_rooms: u32,
    /// Player cap.
    pub max_players: u32,
}

/// `FleetUp::Register` payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerRegistration {
    /// Fleet bearer token (checked in constant time).
    pub token: String,
    /// Server id (tickets are bound to it).
    pub server_id: String,
    /// Region.
    pub region: String,
    /// Client-facing `/play` URL.
    pub public_url: String,
    /// Game id.
    pub game_id: String,
    /// Build string.
    pub build: String,
    /// Wire protocol version.
    pub protocol: u16,
    /// Schema hash of the game protocol (allocation only matches compatible servers).
    pub schema_hash: u64,
    /// Capacity.
    pub capacity: Capacity,
}

/// Load report.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Heartbeat {
    /// Sequence.
    pub seq: u64,
    /// Connected players.
    pub players: u32,
    /// Server tick p99 (µs).
    pub tick_p99_us: u32,
    /// Draining.
    pub draining: bool,
    /// Rooms.
    pub rooms: Vec<RoomStatus>,
}

/// One player-data write (optimistic concurrency when `expected_version` is set).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DataWrite {
    /// Key.
    pub key: String,
    /// Value.
    pub value: serde_json::Value,
    /// Required version.
    pub expected_version: Option<i64>,
}

/// Game server → player service.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FleetUp {
    /// First message.
    Register(ServerRegistration),
    /// Periodic load report.
    Heartbeat(Heartbeat),
    /// A room was created (lazily from a ticket, or restored).
    RoomOpened {
        /// Room.
        room_id: RoomId,
        /// Spec.
        spec: RoomSpec,
    },
    /// A room closed.
    RoomClosed {
        /// Room.
        room_id: RoomId,
        /// Reason.
        reason: String,
    },
    /// Presence: player joined a room (rejoin / assignment tracking).
    PlayerJoined {
        /// Room.
        room_id: RoomId,
        /// Player.
        player: PlayerId,
    },
    /// Presence: player left.
    PlayerLeft {
        /// Room.
        room_id: RoomId,
        /// Player.
        player: PlayerId,
    },
    /// Outbox: match result.
    MatchResult {
        /// Idempotency key.
        op_id: String,
        /// Result.
        result: MatchResult,
    },
    /// Outbox: player-data writes.
    PlayerDataWrite {
        /// Idempotency key.
        op_id: String,
        /// Player.
        player: PlayerId,
        /// Writes.
        writes: Vec<DataWrite>,
    },
    /// Outbox: leaderboard score.
    SubmitScore {
        /// Idempotency key.
        op_id: String,
        /// Board.
        board: String,
        /// Player.
        player: PlayerId,
        /// Score.
        score: i64,
    },
    /// Request player data (answer: `FleetDown::PlayerData`).
    FetchPlayerData {
        /// Correlation id.
        request_id: u64,
        /// Player.
        player: PlayerId,
        /// Keys.
        keys: Vec<String>,
    },
}

/// Player service → game server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FleetDown {
    /// Registration accepted.
    Registered {
        /// Lease TTL.
        lease_ttl_ms: u64,
        /// Current bans.
        bans: Vec<Ban>,
    },
    /// Pre-create a room (optional; tickets also carry the spec).
    PrepareRoom {
        /// Spec.
        spec: RoomSpec,
    },
    /// Close a room.
    CloseRoom {
        /// Room.
        room_id: RoomId,
        /// Reason.
        reason: String,
    },
    /// Kick a player wherever they are on this server.
    Kick {
        /// Player.
        player: PlayerId,
        /// Reason.
        reason: String,
    },
    /// New ban.
    Ban(Ban),
    /// Lift a ban.
    Unban {
        /// Ban id.
        ban_id: u64,
    },
    /// Admin broadcast.
    Broadcast {
        /// Room (all rooms if `None`).
        room_id: Option<RoomId>,
        /// Text.
        text: String,
    },
    /// Start draining.
    Drain,
    /// Outbox entry applied.
    Ack {
        /// Op id.
        op_id: String,
    },
    /// Answer to `FetchPlayerData`.
    PlayerData {
        /// Correlation id.
        request_id: u64,
        /// Player.
        player: PlayerId,
        /// Entries.
        entries: Vec<PlayerDataEntry>,
    },
}

/// Fleet link failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FleetError {
    /// Not connected (the message is kept by the caller / outbox).
    #[error("fleet link down")]
    Down,
    /// Registration refused.
    #[error("registration refused: {0}")]
    Refused(String),
}

/// Game-server side of the fleet link. Non-blocking.
pub trait FleetLink: Send + Sync + 'static {
    /// Queue a message.
    fn send(&self, msg: FleetUp) -> Result<(), FleetError>;
    /// Next message from the player service.
    fn try_recv(&self) -> Option<FleetDown>;
    /// Whether registered and connected.
    fn is_connected(&self) -> bool;
}

/// WebSocket fleet link (split mode).
pub struct WsFleetLink {
    url: String,
    registration: ServerRegistration,
}

impl WsFleetLink {
    /// Start connecting (background reconnect loop on the I/O runtime).
    pub fn connect(url: &str, registration: ServerRegistration) -> Self {
        todo!()
    }
}

impl FleetLink for WsFleetLink {
    fn send(&self, msg: FleetUp) -> Result<(), FleetError> {
        todo!()
    }
    fn try_recv(&self) -> Option<FleetDown> {
        todo!()
    }
    fn is_connected(&self) -> bool {
        todo!()
    }
}

/// In-process fleet link (all-in-one mode and tests).
pub struct InProcessFleetLink {
    up: async_channel::Sender<FleetUp>,
    down: async_channel::Receiver<FleetDown>,
}

/// Player-service end of an in-process link.
pub struct InProcessFleetPeer {
    /// Messages from the game server.
    pub up: async_channel::Receiver<FleetUp>,
    /// Messages to the game server.
    pub down: async_channel::Sender<FleetDown>,
}

/// A connected in-process pair.
pub fn in_process_pair() -> (InProcessFleetLink, InProcessFleetPeer) {
    let (up_tx, up_rx) = async_channel::unbounded();
    let (down_tx, down_rx) = async_channel::unbounded();
    (
        InProcessFleetLink {
            up: up_tx,
            down: down_rx,
        },
        InProcessFleetPeer {
            up: up_rx,
            down: down_tx,
        },
    )
}

impl FleetLink for InProcessFleetLink {
    fn send(&self, msg: FleetUp) -> Result<(), FleetError> {
        self.up.try_send(msg).map_err(|_| FleetError::Down)
    }
    fn try_recv(&self) -> Option<FleetDown> {
        self.down.try_recv().ok()
    }
    fn is_connected(&self) -> bool {
        !self.up.is_closed()
    }
}
