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

use super::rooms::{RoomStatus, RoomTick};
use crate::game::RoomSpec;
use crate::gs::room::MatchResult;
use crate::security::bans::Ban;
use serde::{Deserialize, Serialize};
use space3d_replication::api::PlayerDataEntry;
use space3d_replication::{PlayerId, RoomId};
use std::sync::Arc;

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
    /// Modes this server hosts (`GameDefinition::modes`); empty = any mode. Quick play and
    /// lobbies only create rooms of a mode on servers that list it.
    #[serde(default)]
    pub modes: Vec<String>,
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

/// Where an outbox op was produced: its room and the point in that room copy's history (the
/// tick whose systems requested it). A player service drops the ops of a stale copy (one left
/// behind when the room was restored on another server) made after the point the live copy
/// was restored from — the live copy's state does not include them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpOrigin {
    /// Room.
    pub room_id: RoomId,
    /// Room epoch and tick.
    pub at: RoomTick,
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
        /// The room's state was checkpointed for a move (drain handoff of a persistent room,
        /// shutdown at the drain deadline): the player service keeps it as orphaned so its
        /// players' rejoin restores it on another server. `false` = gone for good.
        #[serde(default)]
        restorable: bool,
    },
    /// A draining server froze a persistent room for a handoff: its clients were disconnected
    /// and its final checkpoint is uploading (`RoomClosed { restorable: true }` follows once it
    /// is stored). A rejoin meanwhile waits for that instead of going back to the frozen room
    /// or restoring an older checkpoint.
    RoomMoving {
        /// Room.
        room_id: RoomId,
    },
    /// The server is leaving the fleet (drain finished): the player service marks it dead
    /// at once instead of waiting for its lease to expire.
    Deregister {
        /// Why.
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
        /// The room copy and tick that produced it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<OpOrigin>,
    },
    /// Outbox: player-data writes.
    PlayerDataWrite {
        /// Idempotency key.
        op_id: String,
        /// Player.
        player: PlayerId,
        /// Writes.
        writes: Vec<DataWrite>,
        /// The room copy and tick that produced it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<OpOrigin>,
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
        /// The room copy and tick that produced it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<OpOrigin>,
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
    /// This server's copy of a room is stale: the room was restored on another server while
    /// this server's lease had lapsed (a fleet-link partition). Close it without touching the
    /// stored checkpoint (it belongs to the live copy); its clients get
    /// `Disconnect(ServerShutdown)` and rejoin, which brings them to the live copy.
    RoomSuperseded {
        /// Room.
        room_id: RoomId,
    },
    /// The player service no longer knows this server (it was forgotten after its lease
    /// expired while the link stayed up): send `Register` again.
    Reregister,
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

impl FleetUp {
    /// Idempotency key of an outbox op (`None` for non-op messages).
    pub fn op_id(&self) -> Option<&str> {
        match self {
            FleetUp::MatchResult { op_id, .. }
            | FleetUp::PlayerDataWrite { op_id, .. }
            | FleetUp::SubmitScore { op_id, .. } => Some(op_id),
            _ => None,
        }
    }

    /// Where an outbox op was produced (`None` for non-op messages and ops from older servers).
    pub fn origin(&self) -> Option<OpOrigin> {
        match self {
            FleetUp::MatchResult { origin, .. }
            | FleetUp::PlayerDataWrite { origin, .. }
            | FleetUp::SubmitScore { origin, .. } => *origin,
            _ => None,
        }
    }

    /// Mutable idempotency key (outbox assigns missing ones).
    pub fn op_id_mut(&mut self) -> Option<&mut String> {
        match self {
            FleetUp::MatchResult { op_id, .. }
            | FleetUp::PlayerDataWrite { op_id, .. }
            | FleetUp::SubmitScore { op_id, .. } => Some(op_id),
            _ => None,
        }
    }

    /// Short name for logs / metrics.
    pub fn kind(&self) -> &'static str {
        match self {
            FleetUp::Register(_) => "register",
            FleetUp::Heartbeat(_) => "heartbeat",
            FleetUp::RoomOpened { .. } => "room_opened",
            FleetUp::RoomClosed { .. } => "room_closed",
            FleetUp::RoomMoving { .. } => "room_moving",
            FleetUp::Deregister { .. } => "deregister",
            FleetUp::PlayerJoined { .. } => "player_joined",
            FleetUp::PlayerLeft { .. } => "player_left",
            FleetUp::MatchResult { .. } => "match_result",
            FleetUp::PlayerDataWrite { .. } => "player_data_write",
            FleetUp::SubmitScore { .. } => "submit_score",
            FleetUp::FetchPlayerData { .. } => "fetch_player_data",
        }
    }
}

/// Game-server side of the fleet link. Non-blocking.
pub trait FleetLink: Send + Sync + 'static {
    /// Queue a message (`Err(Down)` while not registered; the outbox keeps its ops).
    fn send(&self, msg: FleetUp) -> Result<(), FleetError>;
    /// Next message from the player service.
    fn try_recv(&self) -> Option<FleetDown>;
    /// Whether registered and connected.
    fn is_connected(&self) -> bool;
    /// Registration counter: changes every time the link (re)registers, so the outbox knows
    /// to replay everything unacked.
    fn session(&self) -> u64 {
        u64::from(self.is_connected())
    }
}

/// Largest fleet message (JSON).
pub const MAX_FLEET_MESSAGE: usize = 16 << 20;

/// Concurrent fleet WebSocket connections the player service accepts (game servers).
pub const MAX_FLEET_CONNECTIONS: usize = 1024;

/// A fleet connection that has not sent a valid `Register` within this is closed.
pub const FLEET_REGISTER_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// Accept hook of the fleet WebSocket: the upgrade must carry `Authorization: Bearer
/// <fleet token>` (checked in constant time), so unauthenticated peers never get a socket.
pub fn fleet_upgrade_authorized(token: &str, meta: &space3d_net::ConnectMeta) -> Result<(), u16> {
    let presented = meta
        .authorization
        .as_deref()
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or_default();
    if !token.is_empty() && crate::security::ct_eq(presented.as_bytes(), token.as_bytes()) {
        Ok(())
    } else {
        Err(401)
    }
}

#[derive(Default)]
struct WsLinkState {
    connected: std::sync::atomic::AtomicBool,
    session: std::sync::atomic::AtomicU64,
    stop: std::sync::atomic::AtomicBool,
    last_error: parking_lot::Mutex<String>,
}

/// WebSocket fleet link (split mode): a background loop on the I/O runtime connects to the
/// player service with its own backoff (never `WsConfig::reconnect`), registers, forwards
/// queued `FleetUp`s and collects `FleetDown`s. JSON text frames.
pub struct WsFleetLink {
    up: async_channel::Sender<FleetUp>,
    down: async_channel::Receiver<FleetDown>,
    state: std::sync::Arc<WsLinkState>,
    _task: space3d_tasks::Task<()>,
}

impl WsFleetLink {
    /// Start connecting (background reconnect loop on the I/O runtime).
    pub fn connect(url: &str, registration: ServerRegistration) -> Self {
        let (up_tx, up_rx) = async_channel::bounded::<FleetUp>(16_384);
        let (down_tx, down_rx) = async_channel::bounded::<FleetDown>(16_384);
        let state = std::sync::Arc::new(WsLinkState::default());
        let task = space3d_tasks::spawn(tracing::Instrument::instrument(
            run_ws_link(url.to_owned(), registration, up_rx, down_tx, state.clone()),
            tracing::Span::current(),
        ));
        Self {
            up: up_tx,
            down: down_rx,
            state,
            _task: task,
        }
    }

    /// Last connection error (diagnostics).
    pub fn last_error(&self) -> String {
        self.state.last_error.lock().clone()
    }
}

impl Drop for WsFleetLink {
    fn drop(&mut self) {
        self.state.stop.store(true, std::sync::atomic::Ordering::Release);
    }
}

async fn run_ws_link(
    url: String,
    registration: ServerRegistration,
    up: async_channel::Receiver<FleetUp>,
    down: async_channel::Sender<FleetDown>,
    state: std::sync::Arc<WsLinkState>,
) {
    use space3d_net::{Backoff, WsClient, WsConfig, WsEventKind};
    use std::sync::atomic::Ordering;
    let backoff = Backoff::new(
        std::time::Duration::from_millis(200),
        std::time::Duration::from_secs(10),
    );
    let mut attempt = 0u32;
    // The player service authenticates the upgrade itself (no socket without the token).
    let bearer = format!("Bearer {}", registration.token);
    let register = match serde_json::to_string(&FleetUp::Register(registration)) {
        Ok(s) => s,
        Err(e) => {
            *state.last_error.lock() = e.to_string();
            return;
        }
    };
    while !state.stop.load(Ordering::Acquire) {
        let conn = WsClient::connect_with(
            url.clone(),
            WsConfig::default()
                .max_message_size(MAX_FLEET_MESSAGE)
                .header("authorization", bearer.clone()),
        );
        let _ = conn.send(register.clone());
        let reason: String;
        'link: loop {
            if state.stop.load(Ordering::Acquire) {
                conn.close();
                return;
            }
            while let Some(ev) = conn.poll_event() {
                match ev {
                    WsEventKind::Opened => {}
                    WsEventKind::Message(m) => {
                        let Some(text) = m.as_text() else { continue };
                        match serde_json::from_str::<FleetDown>(text) {
                            Ok(msg) => {
                                if matches!(msg, FleetDown::Registered { .. }) {
                                    attempt = 0;
                                    state.session.fetch_add(1, Ordering::AcqRel);
                                    state.connected.store(true, Ordering::Release);
                                    tracing::info!(%url, "registered with the player service");
                                }
                                if down.try_send(msg).is_err() {
                                    tracing::warn!("fleet inbox full; dropping a player-service message");
                                }
                            }
                            Err(e) => tracing::warn!(error = %e, "bad fleet message"),
                        }
                    }
                    WsEventKind::Closed { reason: r, .. } => {
                        reason = r;
                        break 'link;
                    }
                }
            }
            if state.connected.load(Ordering::Acquire) {
                while let Ok(msg) = up.try_recv() {
                    match serde_json::to_string(&msg) {
                        Ok(text) => {
                            if conn.send(text).is_err() {
                                break;
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "unserializable fleet message"),
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        state.connected.store(false, Ordering::Release);
        // Anything queued for the dead link is stale (heartbeats) or kept by the outbox.
        while up.try_recv().is_ok() {}
        *state.last_error.lock() = reason.clone();
        tracing::warn!(%url, %reason, attempt, "fleet link down; reconnecting");
        tokio::time::sleep(backoff.delay(attempt)).await;
        attempt = attempt.saturating_add(1);
    }
}

impl FleetLink for WsFleetLink {
    fn send(&self, msg: FleetUp) -> Result<(), FleetError> {
        if !self.is_connected() {
            return Err(FleetError::Down);
        }
        self.up.try_send(msg).map_err(|_| FleetError::Down)
    }
    fn try_recv(&self) -> Option<FleetDown> {
        self.down.try_recv().ok()
    }
    fn is_connected(&self) -> bool {
        self.state.connected.load(std::sync::atomic::Ordering::Acquire)
    }
    fn session(&self) -> u64 {
        self.state.session.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// In-process fleet link (all-in-one mode and tests).
pub struct InProcessFleetLink {
    up: async_channel::Sender<FleetUp>,
    down: async_channel::Receiver<FleetDown>,
    outage: FleetOutage,
}

/// Test switch that takes an in-process fleet link down (player service unreachable) and back
/// up (clone = shared). While down, sends fail with `FleetError::Down`, nothing is received
/// and `is_connected` is false; coming back bumps the session so the outbox replays.
#[derive(Clone, Debug, Default)]
pub struct FleetOutage {
    down: Arc<std::sync::atomic::AtomicBool>,
    session: Arc<std::sync::atomic::AtomicU64>,
    swallow: Arc<std::sync::atomic::AtomicBool>,
}

impl FleetOutage {
    /// Lose every message from the player service while set (sends still succeed): a link that
    /// drops right after a request went out, so its answer never arrives.
    pub fn swallow_downlink(&self, yes: bool) {
        self.swallow.store(yes, std::sync::atomic::Ordering::Release);
    }

    /// Take the link down (`true`) or bring it back (`false`).
    pub fn set(&self, down: bool) {
        let was = self.down.swap(down, std::sync::atomic::Ordering::AcqRel);
        if was && !down {
            self.session.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
    }

    /// Whether the link is down.
    pub fn is_down(&self) -> bool {
        self.down.load(std::sync::atomic::Ordering::Acquire)
    }
}

impl InProcessFleetLink {
    /// The outage switch of this link.
    pub fn outage(&self) -> FleetOutage {
        self.outage.clone()
    }
}

/// Player-service end of an in-process link.
#[derive(Clone)]
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
            outage: FleetOutage::default(),
        },
        InProcessFleetPeer {
            up: up_rx,
            down: down_tx,
        },
    )
}

impl FleetLink for InProcessFleetLink {
    fn send(&self, msg: FleetUp) -> Result<(), FleetError> {
        if self.outage.is_down() {
            return Err(FleetError::Down);
        }
        self.up.try_send(msg).map_err(|_| FleetError::Down)
    }
    fn try_recv(&self) -> Option<FleetDown> {
        if self.outage.is_down() {
            return None;
        }
        if self.outage.swallow.load(std::sync::atomic::Ordering::Acquire) {
            while self.down.try_recv().is_ok() {}
            return None;
        }
        self.down.try_recv().ok()
    }
    fn is_connected(&self) -> bool {
        !self.outage.is_down() && !self.up.is_closed()
    }
    fn session(&self) -> u64 {
        u64::from(self.is_connected()) + 2 * self.outage.session.load(std::sync::atomic::Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_roundtrip_as_json() {
        let msgs = vec![
            FleetUp::Heartbeat(Heartbeat {
                seq: 3,
                players: 2,
                tick_p99_us: 900,
                draining: false,
                rooms: vec![RoomStatus {
                    room_id: RoomId(9),
                    mode: "duel".into(),
                    players: 2,
                    ..Default::default()
                }],
            }),
            FleetUp::PlayerDataWrite {
                op_id: "gs-1:9:1".into(),
                player: PlayerId(4),
                writes: vec![DataWrite {
                    key: "progress".into(),
                    value: serde_json::json!({"level": 3}),
                    expected_version: None,
                }],
                origin: Some(OpOrigin {
                    room_id: RoomId(9),
                    at: RoomTick { epoch: 2, tick: 77 },
                }),
            },
        ];
        for m in msgs {
            let text = serde_json::to_string(&m).unwrap();
            assert_eq!(serde_json::from_str::<FleetUp>(&text).unwrap(), m);
        }
        let mut m = FleetUp::SubmitScore {
            op_id: String::new(),
            board: "b".into(),
            player: PlayerId(1),
            score: 2,
            origin: None,
        };
        // Ops from servers that predate `origin` (no such field) still parse.
        let old = serde_json::to_string(&m).unwrap();
        assert!(!old.contains("origin"), "{old}");
        assert_eq!(serde_json::from_str::<FleetUp>(&old).unwrap().origin(), None);
        *m.op_id_mut().unwrap() = "x".into();
        assert_eq!(m.op_id(), Some("x"));
        assert_eq!(FleetUp::Heartbeat(Heartbeat::default()).op_id(), None);
    }

    #[test]
    fn in_process_pair_delivers_both_ways() {
        let (link, peer) = in_process_pair();
        link.send(FleetUp::RoomClosed {
            restorable: false,
            room_id: RoomId(1),
            reason: "done".into(),
        })
        .unwrap();
        assert!(matches!(peer.up.try_recv(), Ok(FleetUp::RoomClosed { .. })));
        peer.down.try_send(FleetDown::Drain).unwrap();
        assert_eq!(link.try_recv(), Some(FleetDown::Drain));
        assert!(link.is_connected());
        drop(peer);
        assert!(!link.is_connected());
    }
}
