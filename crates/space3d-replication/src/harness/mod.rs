//! Deterministic in-process harness (feature `harness`): one room `App` with
//! `ServerReplicationPlugin` + N client `App`s with `ClientReplicationPlugin`, connected over a
//! [`MemoryNetwork`] with per-link conditions, all driven by one [`ManualClock`]. No threads,
//! no sockets, no wall clock: the same seed produces the same bytes ([`NetHarness::wire_digest`]).
//!
//! Clients authenticate with `HelloAuth::Dev` (the harness plays the gateway: it validates the
//! Hello with `server::handshake::parse_hello` and routes `RoomInbound::Join` directly).
//! `space3d-server` builds its full-stack `TestCluster` (gatekeeper, tickets, player service)
//! from the same pieces.
//!
//! ```ignore
//! let mut h = NetHarness::new(HarnessConfig { seed: 7, ..Default::default() }, |app| {
//!     app.add_plugin(ArenaProtocol).add_plugin(ArenaServerRules);
//! });
//! let a = h.add_client(ClientSpec::named("alice"), |app| { app.add_plugin(ArenaProtocol); });
//! assert!(h.run_until(600, |h| h.client_net(a).is_connected()));
//! h.step_ticks(120);
//! h.assert_converged(a).unwrap();
//! ```

use crate::client::ClientConfig;
use crate::server::{RoomSender, ServerNetConfig};
use crate::time::ManualClock;
use crate::transport::LinkEvent;
use crate::transport::conditioner::{LinkProfile, Semantics};
use crate::transport::memory::{MemoryNetwork, MemoryServerEndpoint};
use crate::{LinkId, NetId};
use space3d_core::{App, Entity};
use std::time::Duration;

/// Harness settings.
#[derive(Clone, Debug)]
pub struct HarnessConfig {
    /// Seed for the network conditioner and resume secrets.
    pub seed: u64,
    /// Room tick rate (default 60).
    pub tick_hz: f64,
    /// Stream (WebSocket-like, default) or datagram semantics.
    pub semantics: Semantics,
    /// Room replication config.
    pub server: ServerNetConfig,
    /// Client template (`connector` is replaced by the memory connector).
    pub client: ClientConfig,
    /// Default link profile for new clients.
    pub link: LinkProfile,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            seed: 1,
            tick_hz: 60.0,
            semantics: Semantics::Stream,
            server: ServerNetConfig::default(),
            client: ClientConfig::default(),
            link: LinkProfile::default(),
        }
    }
}

/// One simulated client.
#[derive(Clone, Debug)]
pub struct ClientSpec {
    /// Dev name (→ `PlayerId` via `PlayerIdentity::dev`).
    pub name: String,
    /// Link conditions (`None` = harness default).
    pub link: Option<LinkProfile>,
    /// Client frame time (default 1/60 s; use e.g. 1/144 or 1/30 to expose aliasing bugs).
    pub frame_dt: Duration,
    /// Offset of the client's first frame relative to the server tick.
    pub phase: Duration,
}

impl ClientSpec {
    /// Default spec with a name.
    pub fn named(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            link: None,
            frame_dt: Duration::from_secs_f64(1.0 / 60.0),
            phase: Duration::ZERO,
        }
    }
}

/// Index of a harness client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ClientHandle(pub usize);

/// Why [`NetHarness::assert_converged`] failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConvergenceError {
    /// An in-scope server entity has no client counterpart.
    #[error("entity {0:?} missing on client")]
    Missing(NetId),
    /// A client entity has no server counterpart.
    #[error("entity {0:?} should not exist on client")]
    Extra(NetId),
    /// Encoded component bytes differ.
    #[error("component {component} of {net_id:?} differs")]
    Differs {
        /// Entity.
        net_id: NetId,
        /// Component name.
        component: String,
    },
}

struct ClientSlot {
    app: App,
    spec: ClientSpec,
    next_frame_at: Duration,
    link: Option<LinkId>,
}

/// The harness. See the module docs.
pub struct NetHarness {
    clock: ManualClock,
    network: MemoryNetwork,
    endpoint: MemoryServerEndpoint,
    server: App,
    room: RoomSender,
    clients: Vec<ClientSlot>,
    config: HarnessConfig,
    step: Duration,
}

impl NetHarness {
    /// Build the room `App` (`ServerReplicationPlugin` + inbox + clock are added for you; the
    /// closure adds the game's protocol and server rules).
    pub fn new(config: HarnessConfig, build_server: impl FnOnce(&mut App)) -> Self {
        todo!()
    }

    /// Add a client (`ClientReplicationPlugin` + clock added; the closure adds the game's
    /// protocol and input sampler). It starts connecting immediately with `HelloAuth::Dev`.
    pub fn add_client(&mut self, spec: ClientSpec, build: impl FnOnce(&mut App)) -> ClientHandle {
        todo!()
    }

    /// Advance virtual time by one server tick: deliver due packets, run client frames that
    /// are due (ordered by `(time, client index)`), run one server tick, route link events.
    pub fn step(&mut self) {
        todo!()
    }

    /// `step()` × n.
    pub fn step_ticks(&mut self, n: u64) {
        for _ in 0..n {
            self.step();
        }
    }

    /// Step until `pred` holds (checked after each step) or `max_ticks` elapse; returns whether it held.
    pub fn run_until(&mut self, max_ticks: u64, mut pred: impl FnMut(&Self) -> bool) -> bool {
        for _ in 0..max_ticks {
            if pred(self) {
                return true;
            }
            self.step();
        }
        pred(self)
    }

    /// Room app.
    pub fn server(&self) -> &App {
        &self.server
    }

    /// Room app (mutable).
    pub fn server_mut(&mut self) -> &mut App {
        &mut self.server
    }

    /// Client app.
    pub fn client(&self, c: ClientHandle) -> &App {
        &self.clients[c.0].app
    }

    /// Client app (mutable).
    pub fn client_mut(&mut self, c: ClientHandle) -> &mut App {
        &mut self.clients[c.0].app
    }

    /// Shortcut to a client's `NetClient`.
    pub fn client_net(&self, c: ClientHandle) -> &crate::client::NetClient {
        self.clients[c.0].app.resources.expect()
    }

    /// Shortcut to the room's `NetServer`.
    pub fn server_net(&self) -> &crate::server::NetServer {
        self.server.resources.expect()
    }

    /// The virtual clock.
    pub fn clock(&self) -> &ManualClock {
        &self.clock
    }

    /// The network (conditions, partitions).
    pub fn network(&self) -> &MemoryNetwork {
        &self.network
    }

    /// Current link of a client.
    pub fn link(&self, c: ClientHandle) -> Option<LinkId> {
        self.clients[c.0].link
    }

    /// Change a client's link conditions.
    pub fn set_link(&mut self, c: ClientHandle, profile: LinkProfile) {
        todo!()
    }

    /// Drop a client's link for `duration` (it should resume afterwards).
    pub fn partition(&mut self, c: ClientHandle, duration: Duration) {
        todo!()
    }

    /// The client entity mirroring a server entity.
    pub fn client_entity(&self, c: ClientHandle, server_entity: Entity) -> Option<Entity> {
        todo!()
    }

    /// Compare every entity in the client's scope: same set, identical encoded bytes for every
    /// replicated component the client may see (predicted components compared at the last
    /// confirmed tick).
    pub fn assert_converged(&self, c: ClientHandle) -> Result<(), ConvergenceError> {
        todo!()
    }

    /// Hash of every packet delivered so far (determinism: equal for equal seeds).
    pub fn wire_digest(&self) -> u64 {
        self.network.wire_digest()
    }

    fn route(&mut self, event: LinkEvent) {
        todo!()
    }
}
