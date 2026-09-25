//! Space3d netcode: server-authoritative replication shared by the dedicated game server
//! (`space3d-server`), native clients and web (wasm32) clients.
//!
//! Design: `docs/design/game-server.md`. In one paragraph: a game describes its networked
//! protocol once, in a shared plugin that registers replicated components, the input type,
//! messages and *simulation systems* ([`AppReplicationExt`]). The server runs one
//! [`space3d_core::App`] per room with [`server::ServerReplicationPlugin`]; every entity
//! marked [`Replicated`] is sent (encode-once, per-client acked deltas, interest + bandwidth
//! budget) to the clients that can see it. Clients run [`client::ClientReplicationPlugin`]:
//! locally controlled entities are predicted and reconciled by rollback + replay of the shared
//! [`sim::SimSchedule`]; remote entities are interpolated from a jitter buffer.
//!
//! Transports are pluggable ([`transport::ClientTransport`] / [`transport::LinkTx`]):
//! WebSocket for every client today (browser compatible), an in-memory network with a
//! deterministic conditioner for tests, and a datagram transport later.
//!
//! Crate features: `server`, `client`, `api` (player-service DTOs + HTTP client; default on),
//! `harness` (deterministic in-process server + N clients, bots).
//!
//! ```ignore
//! use space3d_core::prelude::*;
//! use space3d_replication::prelude::*;
//!
//! pub struct ArenaProtocol;
//! impl Plugin for ArenaProtocol {
//!     fn build(&self, app: &mut App) {
//!         app.add_plugin(ReplicationPlugin::default()) // Transform, Name, Parent
//!             .replicate::<Health>("arena.health")
//!             .add_input::<Move>("arena.move")
//!             .add_message::<Chat>("arena.chat", MessageConfig::reliable(Direction::ServerToClient))
//!             .add_sim_system("arena::walk", walk);
//!     }
//! }
//! ```
#![warn(missing_docs)]
// Without `server` or `client` only the protocol, transports and registry types remain; the
// type-erased registry operations are then unused.
#![cfg_attr(not(any(feature = "server", feature = "client")), allow(dead_code))]

pub mod components;
pub mod ids;
pub mod input;
pub mod mapping;
pub mod messages;
pub mod protocol;
pub mod quant;
pub mod registry;
pub mod sim;
pub mod stats;
pub mod time;
pub mod transport;
pub mod wire;

#[cfg(feature = "client")]
pub mod bots;
#[cfg(feature = "client")]
pub mod client;
#[cfg(feature = "server")]
pub mod server;

#[cfg(feature = "api")]
pub mod api;
// Owned by the space3d-server builder (docs/design/game-server.md §3.1).
#[cfg(all(feature = "api", feature = "client"))]
pub mod services;

#[cfg(feature = "harness")]
pub mod harness;

pub use components::{
    Controlled, ControlledBy, InterestSource, Interpolated, NetEntity, NetPriority, NetRelevance, NetTeam, Owner,
    Predicted, Remote, Replicated, Simulated,
};
pub use ids::{LinkId, MatchId, NetClientId, NetId, PlayerId, RoomId, Tick};
pub use input::{InputSampler, InvalidInput, NetInput, PlayerInput};
pub use messages::{FromClient, FromServer, InvalidMessage, NetMessage, Target};
pub use protocol::PROTOCOL_VERSION;
pub use registry::{
    Audience, ClientMode, ComponentKind, ComponentRule, Direction, EventTiming, Interpolate, MessageConfig, NetCodec,
    PostcardCodec, ReplicationRegistry, SendRule,
};
pub use sim::{SimRole, SimSchedule, SimTime};

use space3d_core::hecs::Component;
use space3d_core::{App, Plugin, Stage, SystemCtx};

/// Deterministic fast hash map (fixed-key aHash: iteration order is reproducible across runs,
/// which the harness' byte-level determinism relies on). Keys are server-assigned ids, so
/// HashDoS is not a concern.
pub type FastMap<K, V> = std::collections::HashMap<K, V, std::hash::BuildHasherDefault<ahash::AHasher>>;

/// Shared protocol plugin: inserts [`ReplicationRegistry`] and [`SimSchedule`] and registers
/// the built-in components. Add it (directly or via the game's protocol plugin) on the server,
/// on native clients and on web clients alike — the registration list must be identical, which
/// the schema hash in the handshake enforces.
#[derive(Clone, Copy, Debug)]
pub struct ReplicationPlugin {
    /// Register `space3d_core::Transform` as `"core.transform"` with
    /// [`quant::TransformCodec::default`] and [`ClientMode::PredictAndInterpolate`].
    pub transform: bool,
    /// Register `space3d_core::Name` as `"core.name"` (postcard, [`ClientMode::Apply`]).
    pub name: bool,
    /// Register `space3d_core::Parent` as `"core.parent"` (entity reference mapped through `NetId`).
    pub parent: bool,
}

impl Default for ReplicationPlugin {
    fn default() -> Self {
        Self {
            transform: true,
            name: true,
            parent: true,
        }
    }
}

impl Plugin for ReplicationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ReplicationRegistry>();
        app.init_resource::<SimSchedule>();
        if self.transform {
            app.replicate_with::<space3d_core::Transform>(
                "core.transform",
                ComponentRule::new(quant::TransformCodec::default()).predict_and_interpolate(),
            );
        }
        if self.name {
            app.replicate::<space3d_core::Name>("core.name");
        }
        if self.parent {
            app.replicate_with::<space3d_core::Parent>(
                "core.parent",
                ComponentRule::mapped(
                    |p: &space3d_core::Parent, ctx: &mapping::EncodeCtx<'_>| ctx.net_id(p.0),
                    |id: Option<NetId>, ctx: &mut mapping::DecodeCtx<'_>| ctx.entity(id?).map(space3d_core::Parent),
                ),
            );
        }
        app.add_system_named(space3d_core::Stage::Startup, "replication::freeze", freeze_registry);
    }
}

/// Name prefixes of engine systems that may be registered before a replication plugin.
const ENGINE_SYSTEM_PREFIXES: &[&str] = &[
    "replication::",
    "tasks::",
    "net::",
    "assets::",
    "scene::",
    "input::",
    "physics::",
    "core::",
    "room::",
    "server::",
    "ui::",
    "render::",
    "audio::",
    "platform::",
];

/// Game (non-engine) systems already registered in `PreUpdate` or `FixedUpdate`, i.e. before a
/// replication plugin that is being added now. They would run before `replication::*_receive`
/// / `*_tick` (reading last frame's inputs and messages, or simulating outside the sim
/// schedule): replication plugins must be added before game plugins. The plugins warn about
/// them in debug builds.
pub fn systems_before_replication(app: &App) -> Vec<&'static str> {
    [Stage::PreUpdate, Stage::FixedUpdate]
        .into_iter()
        .flat_map(|stage| app.system_names(stage))
        .filter(|name| !ENGINE_SYSTEM_PREFIXES.iter().any(|p| name.starts_with(p)))
        .collect()
}

/// Debug builds: warn about [`systems_before_replication`].
pub(crate) fn check_plugin_order(app: &App, plugin: &str) {
    if cfg!(debug_assertions) {
        let early = systems_before_replication(app);
        if !early.is_empty() {
            tracing::warn!(
                plugin,
                systems = ?early,
                "game systems registered before the replication plugin run before it; add the \
                 replication plugin before game plugins"
            );
        }
    }
}

/// Freeze the registry (idempotent). Runs at the first `Startup`; the replication plugins also
/// call it before first use.
pub fn freeze_registry(ctx: &mut SystemCtx) {
    ctx.resources.init::<ReplicationRegistry>().freeze();
}

/// Registration API used by a game's shared protocol plugin. Every method is order
/// independent (component kinds are assigned by sorted name when the registry freezes at the
/// first `Startup`) and lazily inserts [`ReplicationRegistry`] / [`SimSchedule`].
///
/// Names are explicit, stable strings (`"game.health"`), never `type_name`, because they feed
/// the schema hash that client and server builds must agree on.
pub trait AppReplicationExt {
    /// Replicate `T` with the postcard codec and default rule ([`ComponentRule::serde`]).
    fn replicate<T>(&mut self, name: &'static str) -> &mut Self
    where
        T: Component + Clone + serde::Serialize + serde::de::DeserializeOwned;

    /// Replicate `T` with an explicit [`ComponentRule`] (codec, client mode, audience, ...).
    fn replicate_with<T: Component + Clone>(&mut self, name: &'static str, rule: ComponentRule<T>) -> &mut Self;

    /// Register the game's single input type (sent every tick, redundantly, by clients).
    fn add_input<I: NetInput>(&mut self, name: &'static str) -> &mut Self;

    /// Register a message type (server events and/or client commands). Registers
    /// `Events<FromServer<M>>` / `Events<FromClient<M>>` as appropriate.
    fn add_message<M: NetMessage>(&mut self, name: &'static str, config: MessageConfig) -> &mut Self;

    /// Add a system to the shared deterministic [`SimSchedule`]. It runs once per tick on the
    /// server and once per predicted tick (plus replays) on clients. Sim systems must query
    /// `With<&Simulated>`, read [`SimTime`] (not `Time`), and skip cosmetic side effects while
    /// `SimTime::resimulating`.
    fn add_sim_system(&mut self, name: &'static str, system: impl FnMut(&mut SystemCtx) + Send + 'static) -> &mut Self;

    /// Client only: how to sample this frame's input for the next predicted tick.
    #[cfg(feature = "client")]
    fn set_input_sampler<I: NetInput>(
        &mut self,
        sampler: impl FnMut(&mut SystemCtx) -> I + Send + 'static,
    ) -> &mut Self;
}

impl AppReplicationExt for App {
    fn replicate<T>(&mut self, name: &'static str) -> &mut Self
    where
        T: Component + Clone + serde::Serialize + serde::de::DeserializeOwned,
    {
        self.replicate_with::<T>(name, ComponentRule::serde())
    }

    fn replicate_with<T: Component + Clone>(&mut self, name: &'static str, rule: ComponentRule<T>) -> &mut Self {
        self.resources
            .init::<ReplicationRegistry>()
            .register_component(name, rule);
        self
    }

    fn add_input<I: NetInput>(&mut self, name: &'static str) -> &mut Self {
        self.resources.init::<ReplicationRegistry>().register_input::<I>(name);
        self
    }

    fn add_message<M: NetMessage>(&mut self, name: &'static str, config: MessageConfig) -> &mut Self {
        self.resources
            .init::<ReplicationRegistry>()
            .register_message::<M>(name, config);
        if matches!(config.direction, Direction::ServerToClient | Direction::Both) {
            self.add_event::<FromServer<M>>();
        }
        if matches!(config.direction, Direction::ClientToServer | Direction::Both) {
            self.add_event::<FromClient<M>>();
        }
        self
    }

    fn add_sim_system(&mut self, name: &'static str, system: impl FnMut(&mut SystemCtx) + Send + 'static) -> &mut Self {
        self.resources.init::<SimSchedule>().add(name, system);
        self
    }

    #[cfg(feature = "client")]
    fn set_input_sampler<I: NetInput>(
        &mut self,
        sampler: impl FnMut(&mut SystemCtx) -> I + Send + 'static,
    ) -> &mut Self {
        self.insert_resource(InputSampler::<I>::new(sampler));
        self
    }
}

/// `use space3d_replication::prelude::*;`
pub mod prelude {
    #[cfg(feature = "client")]
    pub use crate::client::{
        ClientConfig, ClientReplicationPlugin, ConnectTarget, ConnectionEvent, ConnectionState, NetClient,
    };
    #[cfg(feature = "server")]
    pub use crate::server::{
        LeaveReason, NetServer, PlayerDisconnected, PlayerIdentity, PlayerJoined, PlayerLeft, PlayerResumed,
        ServerNetConfig, ServerReplicationPlugin,
    };
    pub use crate::{
        AppReplicationExt, Audience, ClientMode, ComponentRule, ControlledBy, Direction, EventTiming, FromClient,
        FromServer, InputSampler, Interpolate, MessageConfig, NetClientId, NetId, NetInput, NetMessage, NetPriority,
        NetRelevance, Owner, PlayerId, PlayerInput, Predicted, Replicated, ReplicationPlugin, RoomId, SendRule,
        SimRole, SimTime, Simulated, Target, Tick,
    };
}
