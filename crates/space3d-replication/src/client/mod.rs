//! Client-side replication (compiles for wasm32): connection + reconnect/resume, snapshot
//! application, prediction with rollback/replay, interpolation, input sending, time sync.
//!
//! Per frame (variable rate; predicted ticks at `tick_hz × pacer scale`):
//!
//! | stage | system | work |
//! |---|---|---|
//! | PreUpdate | `replication::client_receive` | connection state machine (connect, Hello, Welcome/Reject, reconnect + resume), pings; decode packets: Pong → `ServerClock`; snapshots → entity map spawn/despawn, `Apply` components now, `Interpolate`/`Delayed` into buffers, `Predict*` → confirmed history (sets `rollback_from` on mismatch); messages → `Events<FromServer<M>>` (or render-timed queue); control → events |
//! | PreUpdate | `replication::client_pace` | `TickPacer` → `TickRate::hz = nominal × scale`, or a hard resync |
//! | FixedUpdate | `replication::client_tick` | rollback + replay once if needed (`SimTime::resimulating`), sample input (`InputSampler<I>`), store `InputHistory`, write `PlayerInput<I>` on `Controlled` entities, run `SimSchedule`, record `PredictionHistory`, queue the INPUT window |
//! | Update | `replication::client_interpolate` | `InterpClock` → render tick; sample buffers into components; release `Delayed` values and `EventTiming::Interpolated` messages; predicted visuals += decaying correction |
//! | PostUpdate | `replication::client_send` | one Data packet per frame: acks (datagram), INPUT, messages, Control, pings |
//!
//! Games add `ClientReplicationPlugin` *before* their own plugins (so `client_tick` runs
//! first in `FixedUpdate` and interpolation runs before camera systems in `Update`).

pub mod connection;
pub mod interpolation;
pub mod prediction;
pub mod receive;

use crate::protocol::control::Notice;
use crate::protocol::handshake::{DisconnectCode, HelloAuth, Platform, Reject};
use crate::stats::NetStats;
use crate::time::{InterpConfig, PacerConfig};
use crate::transport::Connector;
use crate::{NetClientId, NetId, NetMessage, PlayerId, RoomId, Tick};
use space3d_core::{App, Entity, Plugin, Stage};
use space3d_net::ReconnectPolicy;
use std::sync::Arc;
use std::time::Duration;

/// Prediction settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PredictionConfig {
    /// Enable prediction of `Controlled` entities (default true).
    pub enabled: bool,
    /// Longest replay; older mismatches snap to the server state (default 32 ticks).
    pub max_rollback_ticks: u16,
    /// Visual error decay half-life (default 60 ms).
    pub smoothing_half_life: Duration,
    /// Corrections larger than this (m) snap instead of smoothing (default 2 m).
    pub snap_distance: f32,
}

impl Default for PredictionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_rollback_ticks: 32,
            smoothing_half_life: Duration::from_millis(60),
            snap_distance: 2.0,
        }
    }
}

/// Client settings.
#[derive(Clone)]
pub struct ClientConfig {
    /// `Hello::game_id`.
    pub game_id: String,
    /// `Hello::game_version`.
    pub game_version: String,
    /// Opens links (default `WsConnector`).
    pub connector: Arc<dyn Connector>,
    /// Reconnect schedule after an unexpected drop (default 250 ms → 10 s, 10 attempts).
    pub reconnect: ReconnectPolicy,
    /// Try `HelloAuth::Resume` on reconnect (default true).
    pub resume: bool,
    /// Inputs per INPUT block on datagram transports (default 8; streams send 1).
    pub input_redundancy: u8,
    /// Prediction.
    pub prediction: PredictionConfig,
    /// Interpolation.
    pub interp: InterpConfig,
    /// Tick pacing.
    pub pacer: PacerConfig,
    /// Ping period after the initial burst (default 250 ms).
    pub ping_interval: Duration,
    /// No packet for this long → link dead, start reconnecting (default 5 s).
    pub idle_timeout: Duration,
    /// Handshake must finish within this (default 5 s).
    pub handshake_timeout: Duration,
    /// Requested downstream budget, bytes/s (0 = server default).
    pub max_down_bytes_per_sec: u32,
    /// Reported platform.
    pub platform: Platform,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            game_id: String::new(),
            game_version: String::new(),
            connector: Arc::new(crate::transport::ws::WsConnector::default()),
            reconnect: ReconnectPolicy::default().max_attempts(10),
            resume: true,
            input_redundancy: 8,
            prediction: PredictionConfig::default(),
            interp: InterpConfig::default(),
            pacer: PacerConfig::default(),
            ping_interval: Duration::from_millis(250),
            idle_timeout: Duration::from_secs(5),
            handshake_timeout: Duration::from_secs(5),
            max_down_bytes_per_sec: 0,
            platform: if cfg!(target_arch = "wasm32") {
                Platform::Web
            } else {
                Platform::Desktop
            },
        }
    }
}

impl std::fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientConfig")
            .field("game_id", &self.game_id)
            .field("game_version", &self.game_version)
            .field("resume", &self.resume)
            .field("input_redundancy", &self.input_redundancy)
            .field("prediction", &self.prediction)
            .field("interp", &self.interp)
            .field("pacer", &self.pacer)
            .finish_non_exhaustive()
    }
}

/// Client replication plugin. Requires the game's shared protocol plugin. Inserts
/// [`NetClient`] and registers [`ConnectionEvent`], [`RemoteDespawned`], [`Mispredicted`].
#[derive(Clone, Debug, Default)]
pub struct ClientReplicationPlugin {
    /// Settings.
    pub config: ClientConfig,
}

impl Plugin for ClientReplicationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<crate::ReplicationRegistry>();
        app.init_resource::<crate::SimSchedule>();
        app.init_resource::<crate::time::NetClockHandle>();
        if !app.resources.contains::<NetClient>() {
            app.insert_resource(NetClient::new(self.config.clone()));
        }
        app.add_event::<ConnectionEvent>();
        app.add_event::<RemoteDespawned>();
        app.add_event::<Mispredicted>();
        app.add_system_named(Stage::Startup, "replication::client_startup", crate::freeze_registry);
        app.add_system_named(Stage::PreUpdate, "replication::client_receive", receive::client_receive);
        app.add_system_named(Stage::PreUpdate, "replication::client_pace", prediction::client_pace);
        app.add_system_named(Stage::FixedUpdate, "replication::client_tick", prediction::client_tick);
        app.add_system_named(
            Stage::Update,
            "replication::client_interpolate",
            interpolation::client_interpolate,
        );
        app.add_system_named(Stage::PostUpdate, "replication::client_send", connection::client_send);
        app.add_system_named(Stage::Last, "replication::client_despawn", receive::client_despawn);
    }
}

/// Where to connect.
#[derive(Clone)]
pub enum ConnectTarget {
    /// Dev server without a player service (`HelloAuth::Dev`; the server must allow it).
    Dev {
        /// `ws://127.0.0.1:7777/play`.
        url: String,
        /// Display name.
        name: String,
    },
    /// A player-service assignment (`api::Assignment { server_url, join_ticket, .. }`).
    Ticket {
        /// Game server URL.
        url: String,
        /// Join ticket.
        ticket: String,
    },
    /// Custom connector (memory transport in tests, future QUIC).
    Custom {
        /// Link factory.
        connector: Arc<dyn Connector>,
        /// URL passed to the connector.
        url: String,
        /// Credentials.
        auth: HelloAuth,
    },
}

impl std::fmt::Debug for ConnectTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectTarget::Dev { url, name } => f.debug_struct("Dev").field("url", url).field("name", name).finish(),
            ConnectTarget::Ticket { url, .. } => f.debug_struct("Ticket").field("url", url).finish_non_exhaustive(),
            ConnectTarget::Custom { url, .. } => f.debug_struct("Custom").field("url", url).finish_non_exhaustive(),
        }
    }
}

/// Why the client gave up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FailReason {
    /// The server refused the join.
    Rejected(Reject),
    /// The server ended the session without resume.
    Disconnected(DisconnectCode, String),
    /// Reconnect attempts exhausted.
    Unreachable(String),
}

/// Connection state (`NetClient::state`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    /// Not connected, not trying.
    Idle,
    /// Opening the transport.
    Connecting {
        /// Attempt number (0 = first).
        attempt: u32,
    },
    /// Hello sent, waiting for Welcome.
    Handshaking,
    /// Welcome received, initial snapshots streaming in (loading screen).
    Syncing,
    /// Fully synced and playing.
    Connected,
    /// Lost the link; retrying (with resume if possible).
    Reconnecting {
        /// Attempt number.
        attempt: u32,
    },
    /// Gave up.
    Failed(FailReason),
}

/// Connection events (`Events<ConnectionEvent>`).
#[derive(Clone, Debug, PartialEq)]
pub enum ConnectionEvent {
    /// Opening a transport.
    Connecting,
    /// Welcome received.
    Connected {
        /// Slot.
        client: NetClientId,
        /// Account.
        player: PlayerId,
        /// Room.
        room: RoomId,
        /// Existing slot resumed.
        resumed: bool,
    },
    /// Initial sync finished (`SYNC_COMPLETE`).
    Synced,
    /// Retrying after a drop.
    Reconnecting {
        /// Attempt number.
        attempt: u32,
    },
    /// Join refused.
    Rejected(Reject),
    /// Connection ended.
    Disconnected {
        /// Server-provided code, if any.
        code: Option<DisconnectCode>,
        /// Detail.
        reason: String,
        /// Whether the client will retry.
        will_reconnect: bool,
    },
    /// Server announcement.
    Notice(Notice),
}

/// A replicated entity is about to be despawned (sent before the despawn, same frame).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteDespawned {
    /// Wire id.
    pub net_id: NetId,
    /// Local entity (still alive while the event is read this frame).
    pub entity: Entity,
    /// Left interest (vs destroyed).
    pub out_of_scope: bool,
}

/// A rollback happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mispredicted {
    /// Authoritative tick that disagreed.
    pub tick: Tick,
    /// Ticks replayed.
    pub replayed: u32,
}

/// Client replication state and API (resource).
pub struct NetClient {
    pub(crate) config: ClientConfig,
    pub(crate) state: ConnectionState,
    pub(crate) conn: Option<connection::Connection>,
    pub(crate) map: crate::mapping::EntityMap,
    pub(crate) stats: NetStats,
    /// Connection events waiting to be emitted.
    pub(crate) events: Vec<ConnectionEvent>,
    /// `TypeId` → (kind, config) of registered messages (filled at the first frame).
    pub(crate) msg_table: crate::FastMap<std::any::TypeId, (u16, crate::MessageConfig)>,
    /// Outgoing blocks for the next packet.
    pub(crate) out_reliable: Vec<(crate::protocol::ChannelId, Vec<u8>)>,
    pub(crate) out_unreliable: Vec<(crate::protocol::ChannelId, Vec<u8>)>,
    /// Per-entity replication metadata.
    pub(crate) metas: crate::FastMap<NetId, receive::EntityMeta>,
    /// Entities to despawn in `Last`.
    pub(crate) despawn_queue: Vec<Entity>,
    /// After a resume: `NetId`s re-confirmed since (the rest is dropped at `SYNC_COMPLETE`).
    pub(crate) reset_seen: Option<crate::FastMap<NetId, ()>>,
    /// Values waiting for referenced entities.
    pub(crate) pending_refs: receive::PendingRefs,
    /// Server messages held for `EventTiming::Interpolated`: `(tick, kind, bytes)`.
    pub(crate) timed_messages: Vec<(Tick, u16, Vec<u8>)>,
    /// Newest snapshot tick applied.
    pub(crate) last_snapshot_tick: Tick,
    /// Datagram receive window for acks: newest seq + 32-bit history.
    pub(crate) recv_ack: Option<crate::protocol::AckField>,
    /// Prediction.
    pub(crate) prediction: prediction::PredictionState,
    /// Interpolation clock.
    pub(crate) interp: crate::time::InterpClock,
    /// Latest server-reported input slack (quarter ticks) waiting for the pacer.
    pub(crate) pending_slack: Option<i8>,
    /// `RemoteDespawned` events waiting to be emitted.
    pub(crate) pending_despawn_events: Vec<RemoteDespawned>,
    /// Report bookkeeping.
    pub(crate) report_at: Duration,
    pub(crate) report_rollbacks: u64,
    pub(crate) report_resims: u64,
}

impl std::fmt::Debug for NetClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetClient")
            .field("state", &self.state)
            .field("entities", &self.map.len())
            .field("predicted_tick", &self.prediction.next_tick)
            .finish_non_exhaustive()
    }
}

impl NetClient {
    /// New idle client (inserted by the plugin).
    pub fn new(config: ClientConfig) -> Self {
        let interp = crate::time::InterpClock::new(config.interp);
        let prediction = prediction::PredictionState::new();
        Self {
            config,
            state: ConnectionState::Idle,
            conn: None,
            map: Default::default(),
            stats: NetStats::default(),
            events: Vec::new(),
            msg_table: Default::default(),
            out_reliable: Vec::new(),
            out_unreliable: Vec::new(),
            metas: Default::default(),
            despawn_queue: Vec::new(),
            reset_seen: None,
            pending_refs: Default::default(),
            timed_messages: Vec::new(),
            last_snapshot_tick: Tick::ZERO,
            recv_ack: None,
            prediction,
            interp,
            pending_slack: None,
            pending_despawn_events: Vec::new(),
            report_at: Duration::ZERO,
            report_rollbacks: 0,
            report_resims: 0,
        }
    }

    /// Settings.
    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// Start connecting (replaces any current connection).
    pub fn connect(&mut self, target: ConnectTarget) {
        if let Some(mut old) = self.conn.take()
            && let Some(mut t) = old.transport.take()
        {
            t.close(crate::transport::CloseCode::NORMAL, "reconnecting");
        }
        let (connector, url, auth) = match target {
            ConnectTarget::Dev { url, name } => (self.config.connector.clone(), url, HelloAuth::Dev { name }),
            ConnectTarget::Ticket { url, ticket } => (self.config.connector.clone(), url, HelloAuth::Ticket(ticket)),
            ConnectTarget::Custom { connector, url, auth } => (connector, url, auth),
        };
        self.conn = Some(connection::Connection::new(connector, url, auth, &self.config));
        self.state = ConnectionState::Connecting { attempt: 0 };
        self.events.push(ConnectionEvent::Connecting);
    }

    /// Leave (`ControlMsg::Leave` + `Disconnect(Leave)`, no reconnect). Replicated entities are
    /// despawned (with `RemoteDespawned`) at the next frame.
    pub fn disconnect(&mut self) {
        if let Some(conn) = self.conn.as_mut()
            && let Some(mut t) = conn.transport.take()
        {
            if conn.welcome.is_some() {
                let mut dw = crate::protocol::DataWriter::new(None, None, Tick::ZERO);
                if let Ok(b) = postcard::to_stdvec(&crate::protocol::control::ControlMsg::Leave) {
                    dw.block_bytes(crate::protocol::ChannelId::Control, &b);
                }
                let _ = t.send(crate::transport::Lane::Reliable, dw.finish());
            }
            let _ = t.send(
                crate::transport::Lane::Reliable,
                crate::protocol::encode_handshake(
                    crate::protocol::PacketKind::Disconnect,
                    &crate::protocol::handshake::Disconnect::new(DisconnectCode::Leave, "leaving", false),
                ),
            );
            t.close(crate::transport::CloseCode::NORMAL, "leaving");
        }
        if self.conn.take().is_some() {
            self.events.push(ConnectionEvent::Disconnected {
                code: Some(DisconnectCode::Leave),
                reason: "left".into(),
                will_reconnect: false,
            });
        }
        self.state = ConnectionState::Idle;
        self.clear_session_state(true);
    }

    /// Current state.
    pub fn state(&self) -> &ConnectionState {
        &self.state
    }

    /// Whether synced and playing.
    pub fn is_connected(&self) -> bool {
        self.state == ConnectionState::Connected
    }

    /// Slot, once welcomed.
    pub fn client_id(&self) -> Option<NetClientId> {
        self.conn.as_ref()?.welcome.as_ref().map(|w| w.client)
    }

    /// Account, once welcomed.
    pub fn player_id(&self) -> Option<PlayerId> {
        self.conn.as_ref()?.welcome.as_ref().map(|w| w.player)
    }

    /// Room, once welcomed.
    pub fn room_id(&self) -> Option<RoomId> {
        self.conn.as_ref()?.welcome.as_ref().map(|w| w.room)
    }

    /// Queue a client command (`Events<FromClient<M>>` on the server). Fails when not
    /// connected, when `M` is not registered `ClientToServer`/`Both`, or over its size cap.
    pub fn send<M: NetMessage>(&mut self, msg: &M) -> Result<(), crate::transport::TransportError> {
        use crate::transport::TransportError;
        if !matches!(self.state, ConnectionState::Syncing | ConnectionState::Connected) {
            return Err(TransportError::Closed);
        }
        let Some((kind, config)) = self.msg_table.get(&std::any::TypeId::of::<M>()).copied() else {
            return Err(TransportError::Unsupported("message type not registered"));
        };
        if !matches!(
            config.direction,
            crate::Direction::ClientToServer | crate::Direction::Both
        ) {
            return Err(TransportError::Unsupported("message is server-to-client only"));
        }
        let body = postcard::to_stdvec(msg).map_err(|_| TransportError::Unsupported("message encode failed"))?;
        if body.len() > config.max_bytes as usize {
            return Err(TransportError::TooLarge {
                len: body.len(),
                max: config.max_bytes as usize,
            });
        }
        let mut w = crate::wire::Writer::with_capacity(body.len() + 3);
        w.varu16(kind);
        w.raw(&body);
        match config.lane {
            crate::transport::Lane::Reliable => self
                .out_reliable
                .push((crate::protocol::ChannelId::MsgReliable, w.into_vec())),
            crate::transport::Lane::Unreliable => self
                .out_unreliable
                .push((crate::protocol::ChannelId::MsgUnreliable, w.into_vec())),
        }
        Ok(())
    }

    /// Local entity of a `NetId`.
    pub fn entity(&self, id: NetId) -> Option<Entity> {
        self.map.entity(id)
    }

    /// `NetId` of a local replicated entity.
    pub fn net_id(&self, entity: Entity) -> Option<NetId> {
        self.map.net_id(entity)
    }

    /// Estimated current server tick (fractional).
    pub fn server_tick_estimate(&self) -> Option<f64> {
        let conn = self.conn.as_ref()?;
        conn.clock.estimate(conn.now)
    }

    /// Tick of the next predicted simulation step.
    pub fn predicted_tick(&self) -> Option<Tick> {
        self.prediction.next_tick
    }

    /// Render tick used for interpolation (fractional).
    pub fn render_tick(&self) -> Option<f64> {
        self.interp.render_tick()
    }

    /// Statistics.
    pub fn stats(&self) -> &NetStats {
        &self.stats
    }

    /// Newest snapshot tick applied.
    pub fn last_snapshot_tick(&self) -> Tick {
        self.last_snapshot_tick
    }

    /// All mapped `NetId`s, ascending.
    pub fn net_ids(&self) -> Vec<NetId> {
        self.map.net_ids()
    }

    /// Room tick rate from the last `Welcome`.
    pub fn tick_hz(&self) -> Option<f64> {
        self.conn.as_ref()?.welcome.as_ref().map(|w| f64::from(w.tick_hz))
    }

    /// Drop replicated entities and prediction state (leave / new epoch / fresh join).
    pub(crate) fn clear_session_state(&mut self, entities: bool) {
        if entities {
            for id in self.map.net_ids() {
                if let Some(e) = self.map.remove(id) {
                    self.despawn_queue.push(e);
                    self.pending_despawn_events.push(RemoteDespawned {
                        net_id: id,
                        entity: e,
                        out_of_scope: false,
                    });
                }
            }
            self.metas.clear();
            self.pending_refs.entries.clear();
        }
        self.reset_seen = None;
        self.timed_messages.clear();
        self.last_snapshot_tick = Tick::ZERO;
        self.recv_ack = None;
        self.prediction.clear();
        self.interp.reset();
        self.pending_slack = None;
        self.out_reliable.clear();
        self.out_unreliable.clear();
    }
}
