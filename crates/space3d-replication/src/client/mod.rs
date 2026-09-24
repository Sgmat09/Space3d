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
use space3d_core::{App, Entity, Plugin};
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
        todo!("resources (NetClient, NetClockHandle if missing), events, systems per the module table")
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
    config: ClientConfig,
    state: ConnectionState,
    conn: Option<connection::Connection>,
    map: crate::mapping::EntityMap,
    stats: NetStats,
}

impl NetClient {
    /// New idle client (inserted by the plugin).
    pub fn new(config: ClientConfig) -> Self {
        Self {
            config,
            state: ConnectionState::Idle,
            conn: None,
            map: Default::default(),
            stats: NetStats::default(),
        }
    }

    /// Start connecting (replaces any current connection).
    pub fn connect(&mut self, target: ConnectTarget) {
        todo!()
    }

    /// Leave (`ControlMsg::Leave` + `Disconnect(Leave)`, no reconnect).
    pub fn disconnect(&mut self) {
        todo!()
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
        todo!()
    }

    /// Account, once welcomed.
    pub fn player_id(&self) -> Option<PlayerId> {
        todo!()
    }

    /// Queue a client command (`Events<FromClient<M>>` on the server). Fails when not
    /// connected, when `M` is not registered `ClientToServer`/`Both`, or over its size cap.
    pub fn send<M: NetMessage>(&mut self, msg: &M) -> Result<(), crate::transport::TransportError> {
        todo!()
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
        todo!()
    }

    /// Tick of the next predicted simulation step.
    pub fn predicted_tick(&self) -> Option<Tick> {
        todo!()
    }

    /// Render tick used for interpolation (fractional).
    pub fn render_tick(&self) -> Option<f64> {
        todo!()
    }

    /// Statistics.
    pub fn stats(&self) -> &NetStats {
        &self.stats
    }
}
