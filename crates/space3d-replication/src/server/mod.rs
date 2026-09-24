//! Room-side replication (pure logic, no sockets, no threads): one [`ServerReplicationPlugin`]
//! per room `App`. The gateway (`space3d-server`) or the harness feeds it through a bounded
//! [`NetInbox`]; it answers through each link's [`LinkTx`].
//!
//! Per room tick (one `App::update(step)` = exactly one fixed tick):
//!
//! | stage | system | work |
//! |---|---|---|
//! | PreUpdate | `replication::server_receive` | drain [`NetInbox`] (≤ `max_inbound_per_tick`): joins/resumes → Welcome/Reject + events; packets → acks, RTT, inputs → `InputBuffer`, messages → `Events<FromClient<M>>`, control; closes → resume grace |
//! | FixedUpdate | `replication::server_tick` | `tick += 1`; write `PlayerInput<I>` (buffered / repeated / neutral) onto `ControlledBy` entities; run [`crate::sim::SimSchedule`]; quantize-authoritative write-back |
//! | PostUpdate | `core::propagate_transforms` (core) | |
//! | Render | `replication::server_collect` | on send ticks: assign `NetId`s, detect despawns, encode every replicated component once, compare bytes → `changed_tick` |
//! | Render | `replication::server_send` | on send ticks, per client: interest → priority → budgeted snapshot + queued messages + pongs → `LinkTx::send`; flow control on backlog |
//!
//! The headless server has no renderer, so `Stage::Render` runs after every game `PostUpdate`
//! system ("the network is the server's display"). Games add `ServerReplicationPlugin`
//! *before* their own plugins.

pub mod change;
pub mod full_state;
pub mod handshake;
pub mod inputs;
pub mod interest;
pub mod lagcomp;
pub mod priority;
pub mod receive;
pub mod send;
pub mod slots;

use crate::protocol::WireLimits;
use crate::protocol::control::Notice;
use crate::protocol::handshake::{DisconnectCode, Hello};
use crate::stats::{NetStats, RoomNetStats};
use crate::transport::{LinkTx, PeerInfo};
use crate::{LinkId, NetClientId, NetId, NetMessage, PlayerId, RoomId, Target, Tick};
use serde::{Deserialize, Serialize};
use space3d_core::{App, Entity, Plugin};
use std::sync::Arc;
use std::time::Duration;

/// Who a connection authenticated as (from the join ticket, or dev auth). Carried into the
/// room so it never needs a database read on the join path.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlayerIdentity {
    /// Account.
    pub player: PlayerId,
    /// Display name.
    pub display_name: String,
    /// Team assigned by matchmaking.
    pub team: Option<u16>,
    /// Server-side bot (no network link).
    pub is_bot: bool,
    /// Extra ticket claims (rating, cosmetics, party...), game-defined JSON.
    pub extra: serde_json::Value,
}

impl PlayerIdentity {
    /// Identity for `HelloAuth::Dev { name }`: `PlayerId` = FNV-1a 64 of `"dev:" + name` (never 0).
    pub fn dev(name: &str) -> Self {
        todo!()
    }
}

/// A connection that passed the gateway (structure + authentication) and is routed to a room.
pub struct JoinRequest {
    /// Link id.
    pub link: LinkId,
    /// Sender for the link.
    pub tx: Arc<dyn LinkTx>,
    /// Peer metadata.
    pub peer: PeerInfo,
    /// The client's Hello (already `Hello::validate`d).
    pub hello: Hello,
    /// Authenticated identity; `None` for `HelloAuth::Resume` (the room verifies the token).
    pub identity: Option<PlayerIdentity>,
}

impl std::fmt::Debug for JoinRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinRequest")
            .field("link", &self.link)
            .field("peer", &self.peer)
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

/// Traffic into a room (from the gateway's ingress task or the harness).
#[derive(Debug)]
pub enum RoomInbound {
    /// A new or resuming connection.
    Join(Box<JoinRequest>),
    /// A packet from a joined link.
    Packet {
        /// Link.
        link: LinkId,
        /// Bytes (size-capped by the transport).
        bytes: Vec<u8>,
    },
    /// A joined link closed.
    Closed {
        /// Link.
        link: LinkId,
        /// Reason.
        reason: String,
    },
}

/// The room's inbox is full; the gateway drops unreliable-only packets and disconnects the
/// sender for anything else.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("room inbox full")]
pub struct InboxFull;

/// Sending half of a room inbox (clone freely; lives on the I/O runtime).
#[derive(Clone, Debug)]
pub struct RoomSender {
    tx: async_channel::Sender<RoomInbound>,
}

impl RoomSender {
    /// Queue without blocking.
    pub fn try_send(&self, msg: RoomInbound) -> Result<(), InboxFull> {
        self.tx.try_send(msg).map_err(|_| InboxFull)
    }

    /// Messages waiting.
    pub fn len(&self) -> usize {
        self.tx.len()
    }

    /// Whether empty.
    pub fn is_empty(&self) -> bool {
        self.tx.is_empty()
    }

    /// Whether the room dropped its inbox (closed).
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

/// Receiving half (resource in the room `App`; drained with `try_recv` only).
#[derive(Debug)]
pub struct NetInbox {
    rx: async_channel::Receiver<RoomInbound>,
}

impl NetInbox {
    /// Next message. Never blocks.
    pub fn try_recv(&self) -> Option<RoomInbound> {
        self.rx.try_recv().ok()
    }
}

/// A bounded room inbox (`capacity` messages; default 8192).
pub fn room_channel(capacity: usize) -> (RoomSender, NetInbox) {
    let (tx, rx) = async_channel::bounded(capacity);
    (RoomSender { tx }, NetInbox { rx })
}

/// Interest management policy of a room.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InterestPolicy {
    /// Every replicated entity is relevant to every client (small rooms).
    RoomWide,
    /// Spatial grid around each client's `InterestSource`s; entities enter within `radius`
    /// and leave beyond `radius × hysteresis`.
    Spatial {
        /// Grid cell size (m).
        cell: f32,
        /// Default view radius (m) when `InterestSource` has none.
        radius: f32,
        /// Leave-radius factor (default 1.2).
        hysteresis: f32,
    },
}

/// Per-entity visibility override for one player (`NetServer::set_visibility`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    /// Use the room policy / `NetRelevance`.
    Default,
    /// Always relevant.
    Visible,
    /// Never relevant.
    Hidden,
}

/// Strike policy: violations within a window before a kick.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StrikePolicy {
    /// Strikes tolerated within `window`.
    pub max: u32,
    /// Sliding window.
    pub window: Duration,
}

impl Default for StrikePolicy {
    fn default() -> Self {
        Self {
            max: 5,
            window: Duration::from_secs(10),
        }
    }
}

/// Room replication settings.
#[derive(Clone, Debug, PartialEq)]
pub struct ServerNetConfig {
    /// Room id (resume tokens, Welcome).
    pub room: RoomId,
    /// Room epoch (bumped by a checkpoint restore; clients fully resync).
    pub room_epoch: u32,
    /// `Hello::game_id` accepted by this room.
    pub game_id: String,
    /// Snapshot every `send_interval` ticks (default 2 → 30 Hz at 60 Hz).
    pub send_interval: u8,
    /// Slots (default 16).
    pub max_clients: u16,
    /// Resume grace after a link drops (default 20 s).
    pub resume_window: Duration,
    /// No packet for this long → link considered dead (default 10 s).
    pub idle_timeout: Duration,
    /// Default downstream budget per client, bytes/s (default 48 KiB/s).
    pub client_bandwidth: u32,
    /// Upper clamp for `ClientCaps::max_down_bytes_per_sec` / `SetRate` (default 128 KiB/s).
    pub client_bandwidth_max: u32,
    /// Interest policy (default `RoomWide`).
    pub interest: InterestPolicy,
    /// Re-evaluate interest every N send ticks (default 3).
    pub interest_every: u16,
    /// Accept inputs for ticks up to `tick + max_input_ahead` (default 32).
    pub max_input_ahead: u32,
    /// Wire limits enforced on inbound packets and sent in `Welcome`.
    pub limits: WireLimits,
    /// Skip building snapshots for a client while its link backlog exceeds this (default 256 KiB).
    pub backlog_skip_bytes: usize,
    /// Disconnect (`SlowConsumer`, resumable) when the backlog stays above this for 5 s (default 1 MiB).
    pub backlog_kick_bytes: usize,
    /// Inbox messages processed per tick (default 4096); the rest wait for the next tick.
    pub max_inbound_per_tick: usize,
    /// Violations before a kick.
    pub strikes: StrikePolicy,
}

impl Default for ServerNetConfig {
    fn default() -> Self {
        Self {
            room: RoomId(1),
            room_epoch: 1,
            game_id: String::new(),
            send_interval: 2,
            max_clients: 16,
            resume_window: Duration::from_secs(20),
            idle_timeout: Duration::from_secs(10),
            client_bandwidth: 48 * 1024,
            client_bandwidth_max: 128 * 1024,
            interest: InterestPolicy::RoomWide,
            interest_every: 3,
            max_input_ahead: 32,
            limits: WireLimits::default(),
            backlog_skip_bytes: 256 * 1024,
            backlog_kick_bytes: 1 << 20,
            max_inbound_per_tick: 4096,
            strikes: StrikePolicy::default(),
        }
    }
}

/// Server replication plugin (one per room `App`). Requires the game's shared protocol plugin
/// (or at least `crate::ReplicationPlugin`) and a [`NetInbox`] resource (inserted by the room
/// factory in `space3d-server` or by the harness). Inserts [`NetServer`] and registers the
/// player lifecycle events.
#[derive(Clone, Debug, Default)]
pub struct ServerReplicationPlugin {
    /// Settings.
    pub config: ServerNetConfig,
}

impl Plugin for ServerReplicationPlugin {
    fn build(&self, app: &mut App) {
        todo!("resources (NetServer, NetClockHandle if missing), events, systems per the module table")
    }
}

/// Lifecycle of a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotState {
    /// Welcome sent, initial sync in progress.
    Syncing,
    /// Fully synced.
    Connected,
    /// Link lost; entity kept, neutral input, resumable until `until`.
    Disconnected {
        /// Resume deadline (server tick).
        until: Tick,
    },
}

/// Read-only view of one client slot.
#[derive(Clone, Debug, PartialEq)]
pub struct ClientInfo {
    /// Slot.
    pub client: NetClientId,
    /// Identity.
    pub identity: PlayerIdentity,
    /// Current link (none while disconnected).
    pub link: Option<LinkId>,
    /// State.
    pub state: SlotState,
    /// Tick the player first joined.
    pub joined_tick: Tick,
}

/// A player joined the room (new slot). Spawn the pawn here with `Replicated` +
/// `ControlledBy(identity.player)`.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerJoined {
    /// Slot.
    pub client: NetClientId,
    /// Identity.
    pub identity: PlayerIdentity,
}

/// A player's link dropped; the slot is kept for the resume window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerDisconnected {
    /// Slot.
    pub client: NetClientId,
    /// Account.
    pub player: PlayerId,
    /// Resume deadline.
    pub resume_until: Tick,
}

/// A disconnected player came back within the window (same slot, same entities).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerResumed {
    /// Slot.
    pub client: NetClientId,
    /// Account.
    pub player: PlayerId,
}

/// Why a player left for good.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeaveReason {
    /// Client sent `Leave`.
    Left,
    /// Resume window expired.
    Timeout,
    /// Kicked (game, admin, strikes, slow consumer).
    Kicked(DisconnectCode, String),
    /// The same player joined again on another link (old slot freed).
    Superseded,
    /// The room is closing.
    RoomClosed,
}

/// A player left for good (slot freed). Despawn or hand over their entities.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerLeft {
    /// Slot.
    pub client: NetClientId,
    /// Account.
    pub player: PlayerId,
    /// Reason.
    pub reason: LeaveReason,
}

/// Room-side replication state and API (resource).
pub struct NetServer {
    config: ServerNetConfig,
    tick: Tick,
    joinable: bool,
    ids: crate::mapping::NetIdMap,
    slots: slots::SlotTable,
    stats: RoomNetStats,
}

impl NetServer {
    /// New state (inserted by the plugin).
    pub fn new(config: ServerNetConfig) -> Self {
        Self {
            config,
            tick: Tick::ZERO,
            joinable: true,
            ids: Default::default(),
            slots: Default::default(),
            stats: Default::default(),
        }
    }

    /// Settings.
    pub fn config(&self) -> &ServerNetConfig {
        &self.config
    }

    /// Current room tick (the tick being / last simulated).
    pub fn tick(&self) -> Tick {
        self.tick
    }

    /// Continue from a restored tick (checkpoint restore, before the first update).
    pub fn set_tick(&mut self, tick: Tick) {
        self.tick = tick;
    }

    /// Queue a typed message for `target` (encoded once; sent with the next packet of each
    /// recipient; reliable-lane messages are never dropped, unreliable ones may be).
    pub fn send<M: NetMessage>(&mut self, target: Target, msg: &M) {
        todo!()
    }

    /// Queue a notice (admin broadcast, shutdown warning).
    pub fn notice(&mut self, target: Target, notice: Notice) {
        todo!()
    }

    /// Disconnect a player (`Disconnect` packet, then close). Emits `PlayerLeft` (or
    /// `PlayerDisconnected` when `resume_allowed`).
    pub fn kick(&mut self, player: PlayerId, code: DisconnectCode, reason: &str, resume_allowed: bool) {
        todo!()
    }

    /// Disconnect everyone (room close / server shutdown).
    pub fn disconnect_all(&mut self, code: DisconnectCode, reason: &str, resume_allowed: bool) {
        todo!()
    }

    /// Whether new joins are accepted (resumes always are).
    pub fn set_joinable(&mut self, joinable: bool) {
        self.joinable = joinable;
    }

    /// See [`Self::set_joinable`].
    pub fn is_joinable(&self) -> bool {
        self.joinable
    }

    /// All slots.
    pub fn clients(&self) -> Vec<ClientInfo> {
        todo!()
    }

    /// Slot of a player.
    pub fn client_of(&self, player: PlayerId) -> Option<NetClientId> {
        todo!()
    }

    /// Connected (not disconnected) slot count.
    pub fn connected_count(&self) -> usize {
        todo!()
    }

    /// Link statistics of a slot.
    pub fn stats(&self, client: NetClientId) -> Option<NetStats> {
        todo!()
    }

    /// Cumulative room counters (the server crate exports deltas as metrics).
    pub fn room_stats(&self) -> &RoomNetStats {
        &self.stats
    }

    /// `NetId` of a replicated entity.
    pub fn net_id(&self, entity: Entity) -> Option<NetId> {
        self.ids.net_id(entity)
    }

    /// Entity of a `NetId`.
    pub fn entity(&self, id: NetId) -> Option<Entity> {
        self.ids.entity(id)
    }

    /// Per-player visibility override of one entity.
    pub fn set_visibility(&mut self, player: PlayerId, entity: Entity, visibility: Visibility) {
        todo!()
    }

    /// Add a game relevancy rule, evaluated for candidates on interest ticks (fog of war).
    pub fn add_visibility_rule(&mut self, rule: impl interest::VisibilityRule) {
        todo!()
    }

    /// Plug a server-side bot into a slot (no link; its input comes from `brain` each tick).
    pub fn add_virtual_client(
        &mut self,
        identity: PlayerIdentity,
        brain: Box<dyn inputs::VirtualBrain>,
    ) -> NetClientId {
        todo!()
    }
}
