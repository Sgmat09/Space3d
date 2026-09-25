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
use crate::protocol::handshake::{DisconnectCode, Hello, Reject, RejectCode};
use crate::stats::{NetStats, RoomNetStats};
use crate::time::NetClockHandle;
use crate::transport::{LinkTx, PeerInfo};
use crate::{LinkId, NetClientId, NetId, NetMessage, PlayerId, RoomId, Target, Tick};
use serde::{Deserialize, Serialize};
use space3d_core::{App, Entity, Events, Plugin, Stage};
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
        let h = crate::registry::fnv1a(0xcbf2_9ce4_8422_2325, format!("dev:{name}").as_bytes());
        Self {
            player: PlayerId(h.max(1)),
            display_name: name.to_owned(),
            team: None,
            is_bot: false,
            extra: serde_json::Value::Null,
        }
    }

    /// Identity of a server-side bot (`PlayerId` derived like [`Self::dev`] from `"bot:" + name`).
    pub fn bot(name: &str) -> Self {
        let h = crate::registry::fnv1a(0xcbf2_9ce4_8422_2325, format!("bot:{name}").as_bytes());
        Self {
            player: PlayerId(h.max(1)),
            display_name: name.to_owned(),
            team: None,
            is_bot: true,
            extra: serde_json::Value::Null,
        }
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
        app.init_resource::<crate::ReplicationRegistry>();
        app.init_resource::<crate::SimSchedule>();
        app.init_resource::<NetClockHandle>();
        if !app.resources.contains::<NetServer>() {
            app.insert_resource(NetServer::new(self.config.clone()));
        }
        app.add_event::<PlayerJoined>();
        app.add_event::<PlayerDisconnected>();
        app.add_event::<PlayerResumed>();
        app.add_event::<PlayerLeft>();
        crate::check_plugin_order(app, "ServerReplicationPlugin");
        app.add_system_named(Stage::Startup, "replication::server_startup", |ctx| {
            crate::freeze_registry(ctx);
            if let Some(rate) = ctx.resources.get::<space3d_core::TickRate>()
                && (rate.hz.fract() != 0.0 || rate.hz < 1.0)
            {
                // `Welcome::tick_hz` is an integer: clients would predict with a different dt.
                tracing::error!(
                    hz = rate.hz,
                    "the room tick rate must be a whole number of Hz; clients will mispredict"
                );
            }
            if !ctx.resources.contains::<handshake::SecretSource>() {
                tracing::warn!("no SecretSource resource: resume secrets use an insecure seeded generator (dev only)");
                let seed = web_time::SystemTime::now()
                    .duration_since(web_time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0x5eed);
                ctx.resources.insert(handshake::SecretSource::seeded(seed));
            }
        });
        app.add_system_named(Stage::PreUpdate, "replication::server_receive", receive::server_receive);
        app.add_system_named(Stage::FixedUpdate, "replication::server_tick", inputs::server_tick);
        app.add_system_named(Stage::PostUpdate, "replication::lagcomp_record", lagcomp::record_system);
        app.add_system_named(Stage::Render, "replication::server_collect", change::server_collect);
        app.add_system_named(Stage::Render, "replication::server_send", send::server_send);
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
    /// Reserved: the same player joined again on another link. The room currently *rebinds*
    /// the existing slot instead (old link closed with `Superseded`, `PlayerResumed` fired),
    /// so this reason is not emitted.
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

/// Events produced inside `NetServer` methods, flushed into `Events<T>` by the systems.
#[derive(Debug)]
pub(crate) enum PendingEvent {
    Joined(PlayerJoined),
    Disconnected(PlayerDisconnected),
    Resumed(PlayerResumed),
    Left(PlayerLeft),
}

/// A message queued with [`NetServer::send`], resolved (kind, recipients) at the next send.
pub(crate) struct QueuedMessage {
    pub(crate) type_id: std::any::TypeId,
    pub(crate) type_name: &'static str,
    pub(crate) target: Target,
    pub(crate) bytes: Vec<u8>,
}

/// Room-side replication state and API (resource).
pub struct NetServer {
    pub(crate) config: ServerNetConfig,
    pub(crate) tick: Tick,
    /// How new joins are refused (`None`: joinable; see [`NetServer::refuse_joins`]).
    pub(crate) join_refusal: Option<Reject>,
    pub(crate) ids: crate::mapping::NetIdMap,
    pub(crate) slots: slots::SlotTable,
    pub(crate) stats: RoomNetStats,
    pub(crate) cache: change::ChangeCache,
    pub(crate) visibility: interest::VisibilityOverrides,
    pub(crate) rules: Vec<Box<dyn interest::VisibilityRule>>,
    pub(crate) pending: Vec<PendingEvent>,
    pub(crate) messages: Vec<QueuedMessage>,
    pub(crate) tick_started_at: Duration,
}

impl std::fmt::Debug for NetServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetServer")
            .field("room", &self.config.room)
            .field("tick", &self.tick)
            .field("clients", &self.slots.iter().count())
            .field("entities", &self.cache.records.len())
            .finish_non_exhaustive()
    }
}

impl NetServer {
    /// New state (inserted by the plugin).
    pub fn new(config: ServerNetConfig) -> Self {
        Self {
            config,
            tick: Tick::ZERO,
            join_refusal: None,
            ids: Default::default(),
            slots: Default::default(),
            stats: Default::default(),
            cache: Default::default(),
            visibility: Default::default(),
            rules: Vec::new(),
            pending: Vec::new(),
            messages: Vec::new(),
            tick_started_at: Duration::ZERO,
        }
    }

    /// Settings.
    pub fn config(&self) -> &ServerNetConfig {
        &self.config
    }

    /// Mutable settings (e.g. `room_epoch` after a restore). Changing `send_interval` or limits
    /// affects clients joining afterwards.
    pub fn config_mut(&mut self) -> &mut ServerNetConfig {
        &mut self.config
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
    /// recipient; reliable-lane messages are never dropped, unreliable ones may be). Messages
    /// for clients without a live link are dropped. The type must be registered with
    /// `add_message` in a server-to-client direction (checked when the message is sent).
    pub fn send<M: NetMessage>(&mut self, target: Target, msg: &M) {
        match postcard::to_stdvec(msg) {
            Ok(bytes) => self.messages.push(QueuedMessage {
                type_id: std::any::TypeId::of::<M>(),
                type_name: std::any::type_name::<M>(),
                target,
                bytes,
            }),
            Err(e) => tracing::error!(message = std::any::type_name::<M>(), %e, "message encode failed"),
        }
    }

    /// Queue a notice (admin broadcast, shutdown warning).
    pub fn notice(&mut self, target: Target, notice: Notice) {
        let msg = crate::protocol::control::ControlMsg::Notice(notice);
        let bytes: Arc<[u8]> = match postcard::to_stdvec(&msg) {
            Ok(b) => b.into(),
            Err(_) => return,
        };
        for c in self.recipients(&target) {
            if let Some(slot) = self.slots.get_mut(c) {
                slot.queue_reliable(crate::protocol::ChannelId::Control, bytes.clone());
            }
        }
    }

    /// Slots addressed by `target` (live links only).
    pub(crate) fn recipients(&self, target: &Target) -> Vec<NetClientId> {
        let net_id = match target {
            Target::ObserversOf(e) => self.ids.net_id(*e),
            _ => None,
        };
        self.slots
            .iter()
            .filter(|s| s.link.is_some())
            .filter(|s| match target {
                Target::All => true,
                Target::Client(c) => s.client == *c,
                Target::Player(p) => s.identity.player == *p,
                Target::AllExcept(p) => s.identity.player != *p,
                Target::Players(ps) => ps.contains(&s.identity.player),
                Target::ObserversOf(_) => net_id.is_some_and(|id| s.view.entities.get(&id).is_some_and(|e| e.spawned)),
                Target::Team(t) => s.identity.team == Some(*t),
            })
            .map(|s| s.client)
            .collect()
    }

    /// Disconnect a player (`Disconnect` packet, then close). Emits `PlayerLeft` (or
    /// `PlayerDisconnected` when `resume_allowed`).
    pub fn kick(&mut self, player: PlayerId, code: DisconnectCode, reason: &str, resume_allowed: bool) {
        let Some(client) = self.slots.by_player.get(&player).copied() else {
            return;
        };
        self.stats.kicks += 1;
        self.drop_link(client, Some((code, reason)), resume_allowed);
        if resume_allowed {
            let until = self.resume_deadline();
            if let Some(slot) = self.slots.get_mut(client) {
                slot.state = SlotState::Disconnected { until };
                self.pending.push(PendingEvent::Disconnected(PlayerDisconnected {
                    client,
                    player,
                    resume_until: until,
                }));
            }
        } else {
            self.free_slot(client, LeaveReason::Kicked(code, reason.to_owned()));
        }
    }

    /// Disconnect everyone (room close / server shutdown).
    pub fn disconnect_all(&mut self, code: DisconnectCode, reason: &str, resume_allowed: bool) {
        let players: Vec<PlayerId> = self.slots.iter().map(|s| s.identity.player).collect();
        for p in players {
            if resume_allowed {
                self.kick(p, code, reason, true);
            } else {
                let Some(client) = self.slots.by_player.get(&p).copied() else {
                    continue;
                };
                self.drop_link(client, Some((code, reason)), false);
                let reason = if code == DisconnectCode::RoomClosed || code == DisconnectCode::ServerShutdown {
                    LeaveReason::RoomClosed
                } else {
                    LeaveReason::Kicked(code, reason.to_owned())
                };
                self.free_slot(client, reason);
            }
        }
    }

    /// Whether new joins are accepted (resumes always are). A closed room refuses new players
    /// with `Reject(RoomClosed)`, which says the room is gone for good; see
    /// [`Self::refuse_joins`] for another refusal.
    pub fn set_joinable(&mut self, joinable: bool) {
        self.join_refusal = (!joinable).then(|| Reject::new(RejectCode::RoomClosed, "room is not accepting players"));
    }

    /// Stop accepting new joins and refuse them with `refusal` (resumes still pass). For a room
    /// that is about to move to another server, a transient refusal
    /// (`ServerDraining` with `retry_after_ms`, see [`Reject::is_transient`]) tells a player
    /// whose ticket was issued before the move to ask the player service again — which places
    /// them in the room once it moved — instead of reading the room as closed for good.
    /// [`Self::set_joinable`] replaces it.
    pub fn refuse_joins(&mut self, refusal: Reject) {
        self.join_refusal = Some(refusal);
    }

    /// How new joins are refused (`None` while joinable).
    pub fn join_refusal(&self) -> Option<&Reject> {
        self.join_refusal.as_ref()
    }

    /// See [`Self::set_joinable`].
    pub fn is_joinable(&self) -> bool {
        self.join_refusal.is_none()
    }

    /// All slots.
    pub fn clients(&self) -> Vec<ClientInfo> {
        self.slots
            .iter()
            .map(|s| ClientInfo {
                client: s.client,
                identity: s.identity.clone(),
                link: s.link.as_ref().map(|l| l.0),
                state: s.state,
                joined_tick: s.joined_tick,
            })
            .collect()
    }

    /// Slot of a player.
    pub fn client_of(&self, player: PlayerId) -> Option<NetClientId> {
        self.slots.by_player.get(&player).copied()
    }

    /// Connected (not disconnected) slot count.
    pub fn connected_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| !matches!(s.state, SlotState::Disconnected { .. }))
            .count()
    }

    /// Link statistics of a slot.
    pub fn stats(&self, client: NetClientId) -> Option<NetStats> {
        let s = self.slots.get(client)?;
        let mut stats = s.stats.clone();
        stats.rtt = s.rtt.srtt();
        stats.jitter = s.rtt.rttvar();
        stats.input_slack = s.inputs.slack();
        Some(stats)
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
    /// Overrides are forgotten when the entity despawns (or stops being replicated) or the
    /// player leaves the room.
    pub fn set_visibility(&mut self, player: PlayerId, entity: Entity, visibility: Visibility) {
        self.visibility.set(player, entity, visibility);
    }

    /// Visibility overrides currently tracked (diagnostics).
    pub fn visibility_overrides(&self) -> usize {
        self.visibility.len()
    }

    /// Add a game relevancy rule, evaluated for candidates on interest ticks (fog of war).
    pub fn add_visibility_rule(&mut self, rule: impl interest::VisibilityRule) {
        self.rules.push(Box::new(rule));
    }

    /// Plug a server-side bot into a slot (no link; its input comes from `brain` each tick).
    /// Returns `NetClientId(u16::MAX)` when the room is full.
    pub fn add_virtual_client(
        &mut self,
        identity: PlayerIdentity,
        brain: Box<dyn inputs::VirtualBrain>,
    ) -> NetClientId {
        let Some(client) = self.slots.allocate(self.config.max_clients) else {
            tracing::warn!(room = %self.config.room, "add_virtual_client: room full");
            return NetClientId(u16::MAX);
        };
        let mut slot = slots::Slot::new(client, identity.clone(), self.tick, &self.config, Duration::ZERO);
        slot.state = SlotState::Connected;
        slot.brain = Some(brain);
        self.slots.insert(slot);
        self.stats.joins += 1;
        self.pending
            .push(PendingEvent::Joined(PlayerJoined { client, identity }));
        client
    }

    /// Players whose slot is currently in the resume window.
    pub fn disconnected_players(&self) -> Vec<PlayerId> {
        self.slots
            .iter()
            .filter(|s| matches!(s.state, SlotState::Disconnected { .. }))
            .map(|s| s.identity.player)
            .collect()
    }

    /// Number of replicated entities known to the change cache (after the last send tick).
    pub fn replicated_count(&self) -> usize {
        self.cache.records.len()
    }

    /// Whether `client` currently has `entity` spawned (in scope and delivered).
    pub fn in_scope(&self, client: NetClientId, entity: Entity) -> bool {
        let Some(id) = self.ids.net_id(entity) else {
            return false;
        };
        self.slots
            .get(client)
            .is_some_and(|s| s.view.entities.get(&id).is_some_and(|e| e.spawned))
    }

    /// Visible `NetId`s of a slot (spawned on the client), ascending.
    pub fn scope_of(&self, client: NetClientId) -> Vec<NetId> {
        let Some(s) = self.slots.get(client) else {
            return Vec::new();
        };
        let mut v: Vec<NetId> = s
            .view
            .entities
            .iter()
            .filter(|(_, e)| e.spawned)
            .map(|(id, _)| *id)
            .collect();
        v.sort_unstable();
        v
    }

    /// Current interpolation delay of a client's view in ticks (lag compensation).
    pub fn view_delay_ticks(&self, client: NetClientId) -> Option<f32> {
        self.slots.get(client).map(|s| s.view_delay_q8 as f32 / 256.0)
    }

    /// Room tick at which a disconnected slot expires.
    pub(crate) fn resume_deadline(&self) -> Tick {
        let hz = self.tick_hz();
        let ticks = (self.config.resume_window.as_secs_f64() * hz).ceil() as u32;
        self.tick.after(ticks.max(1))
    }

    /// Nominal tick rate (from `ServerNetConfig`-independent `TickRate`, cached at receive time).
    pub(crate) fn tick_hz(&self) -> f64 {
        self.slots.tick_hz
    }

    /// Close a slot's link (optionally sending `Disconnect` first). The slot stays.
    pub(crate) fn drop_link(
        &mut self,
        client: NetClientId,
        disconnect: Option<(DisconnectCode, &str)>,
        resume_allowed: bool,
    ) {
        let Some(slot) = self.slots.get_mut(client) else { return };
        let Some((link, tx)) = slot.link.take() else { return };
        if let Some((code, _)) = disconnect {
            self.stats.count_disconnect(code);
        }
        if let Some((code, reason)) = disconnect {
            let pkt = crate::protocol::encode_handshake(
                crate::protocol::PacketKind::Disconnect,
                &crate::protocol::handshake::Disconnect::new(code, reason, resume_allowed),
            );
            let _ = tx.send(crate::transport::Lane::Reliable, pkt);
            let close = match code {
                DisconnectCode::ProtocolViolation | DisconnectCode::RateLimited | DisconnectCode::SlowConsumer => {
                    crate::transport::CloseCode::POLICY
                }
                DisconnectCode::ServerShutdown => crate::transport::CloseCode::GOING_AWAY,
                _ => crate::transport::CloseCode::NORMAL,
            };
            tx.close(close, reason);
        } else {
            tx.close(crate::transport::CloseCode::NORMAL, "closed");
        }
        slot.clear_link_state();
        self.slots.by_link.remove(&link);
    }

    /// Free a slot for good and emit `PlayerLeft`.
    pub(crate) fn free_slot(&mut self, client: NetClientId, reason: LeaveReason) {
        self.drop_link(client, None, false);
        if let Some(slot) = self.slots.remove(client) {
            self.visibility.forget_player(slot.identity.player);
            self.pending.push(PendingEvent::Left(PlayerLeft {
                client,
                player: slot.identity.player,
                reason,
            }));
        }
    }

    /// Move pending events into `Events<T>`.
    pub(crate) fn flush_events(&mut self, resources: &mut space3d_core::Resources) {
        for ev in self.pending.drain(..) {
            match ev {
                PendingEvent::Joined(e) => resources.init::<Events<PlayerJoined>>().send(e),
                PendingEvent::Disconnected(e) => resources.init::<Events<PlayerDisconnected>>().send(e),
                PendingEvent::Resumed(e) => resources.init::<Events<PlayerResumed>>().send(e),
                PendingEvent::Left(e) => resources.init::<Events<PlayerLeft>>().send(e),
            }
        }
    }
}
