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

use crate::client::{ClientConfig, ClientReplicationPlugin, ConnectTarget, NetClient};
use crate::mapping::EncodeCtx;
use crate::protocol::handshake::{HelloAuth, ResumeToken};
use crate::registry::ReplicationRegistry;
use crate::server::handshake::{SecretSource, parse_hello, reject_packet};
use crate::server::{
    JoinRequest, PlayerIdentity, RoomInbound, RoomSender, ServerNetConfig, ServerReplicationPlugin, room_channel,
};
use crate::time::{ManualClock, NetClockHandle};
use crate::transport::conditioner::{LinkProfile, Semantics};
use crate::transport::memory::{MemoryNetwork, MemoryServerEndpoint};
use crate::transport::{CloseCode, Lane, LinkEvent, LinkTx, PeerInfo};
use crate::wire::Writer;
use crate::{FastMap, LinkId, NetId, PlayerId};
use space3d_core::{App, Entity};
use std::sync::Arc;
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
    /// Team of the player's identity (what matchmaking puts in a ticket).
    pub team: Option<u16>,
}

impl ClientSpec {
    /// Default spec with a name.
    pub fn named(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            link: None,
            frame_dt: Duration::from_secs_f64(1.0 / 60.0),
            phase: Duration::ZERO,
            team: None,
        }
    }

    /// Builder: team of the player.
    pub fn with_team(mut self, team: u16) -> Self {
        self.team = Some(team);
        self
    }

    /// Builder: link conditions.
    pub fn with_link(mut self, link: LinkProfile) -> Self {
        self.link = Some(link);
        self
    }

    /// Builder: frame time.
    pub fn with_frame_dt(mut self, dt: Duration) -> Self {
        self.frame_dt = dt;
        self
    }

    /// Builder: phase offset.
    pub fn with_phase(mut self, phase: Duration) -> Self {
        self.phase = phase;
        self
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
    /// Extra frame time of the next frame (a simulated hitch, see [`NetHarness::stall`]).
    stall: Duration,
    link: Option<LinkId>,
    player: PlayerId,
}

enum Route {
    Pending { tx: Arc<dyn LinkTx>, peer: PeerInfo },
    Joined,
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
    now: Duration,
    routes: FastMap<LinkId, Route>,
}

impl NetHarness {
    /// Build the room `App` (`ServerReplicationPlugin` + inbox + clock are added for you; the
    /// closure adds the game's protocol and server rules).
    pub fn new(mut config: HarnessConfig, build_server: impl FnOnce(&mut App)) -> Self {
        if config.server.game_id.is_empty() {
            config.server.game_id = "harness".into();
        }
        let clock = ManualClock::new();
        let handle = NetClockHandle(Arc::new(clock.clone()));
        let network = MemoryNetwork::new(config.seed, handle.clone(), config.semantics);
        let endpoint = network.server_endpoint();
        let mut server = App::new().with_tickrate(config.tick_hz);
        server.insert_resource(handle);
        let (room, inbox) = room_channel(8192);
        server.insert_resource(inbox);
        server.insert_resource(SecretSource::seeded(config.seed ^ 0x5ec2_e75e));
        server.add_plugin(ServerReplicationPlugin {
            config: config.server.clone(),
        });
        build_server(&mut server);
        let step = Duration::from_secs_f64(1.0 / config.tick_hz);
        Self {
            clock,
            network,
            endpoint,
            server,
            room,
            clients: Vec::new(),
            config,
            step,
            now: Duration::ZERO,
            routes: FastMap::default(),
        }
    }

    /// Add a client (`ClientReplicationPlugin` + clock added; the closure adds the game's
    /// protocol and input sampler). It starts connecting immediately with `HelloAuth::Dev`.
    pub fn add_client(&mut self, spec: ClientSpec, build: impl FnOnce(&mut App)) -> ClientHandle {
        let mut app = App::new().with_tickrate(self.config.tick_hz);
        app.insert_resource(NetClockHandle(Arc::new(self.clock.clone())));
        let profile = spec.link.clone().unwrap_or_else(|| self.config.link.clone());
        let connector: Arc<dyn crate::transport::Connector> = Arc::new(self.network.connector(profile));
        let mut cc = self.config.client.clone();
        cc.connector = connector.clone();
        if cc.game_id.is_empty() {
            cc.game_id = self.config.server.game_id.clone();
        }
        app.add_plugin(ClientReplicationPlugin { config: cc });
        build(&mut app);
        app.resources.expect_mut::<NetClient>().connect(ConnectTarget::Custom {
            connector,
            url: "mem://room".into(),
            auth: HelloAuth::Dev {
                name: spec.name.clone(),
            },
        });
        let handle = ClientHandle(self.clients.len());
        self.clients.push(ClientSlot {
            app,
            next_frame_at: self.now + spec.phase,
            player: PlayerIdentity::dev(&spec.name).player,
            spec,
            stall: Duration::ZERO,
            link: None,
        });
        handle
    }

    /// Advance virtual time by one server tick: deliver due packets, run client frames that
    /// are due (ordered by `(time, client index)`), run one server tick, route link events.
    pub fn step(&mut self) {
        let target = self.now + self.step;
        loop {
            let next = self
                .clients
                .iter()
                .enumerate()
                .filter(|(_, c)| c.next_frame_at <= target)
                .min_by_key(|(i, c)| (c.next_frame_at, *i))
                .map(|(i, c)| (i, c.next_frame_at));
            let Some((i, at)) = next else { break };
            if at > self.clock_now() {
                self.clock.set(at);
            }
            self.network.pump();
            self.route_all();
            let dt = self.clients[i].spec.frame_dt + std::mem::take(&mut self.clients[i].stall);
            self.clients[i].app.update(dt);
            self.clients[i].next_frame_at = at + self.clients[i].spec.frame_dt;
        }
        self.clock.set(target);
        self.network.pump();
        self.route_all();
        self.server.update(self.step);
        self.now = target;
    }

    fn clock_now(&self) -> Duration {
        use crate::time::NetClock;
        self.clock.now()
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

    /// Number of clients added.
    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    /// Account of a harness client.
    pub fn player(&self, c: ClientHandle) -> PlayerId {
        self.clients[c.0].player
    }

    /// Current link of a client.
    pub fn link(&self, c: ClientHandle) -> Option<LinkId> {
        self.clients[c.0].link
    }

    /// Change a client's link conditions.
    pub fn set_link(&mut self, c: ClientHandle, profile: LinkProfile) {
        if let Some(link) = self.clients[c.0].link {
            self.network.set_profile(link, profile.clone());
        }
        self.clients[c.0].spec.link = Some(profile);
    }

    /// Freeze a client for `duration` (a frame hitch: GC pause, tab in background, debugger).
    /// Its next frame starts `duration` late and sees a correspondingly long frame time.
    pub fn stall(&mut self, c: ClientHandle, duration: Duration) {
        let slot = &mut self.clients[c.0];
        slot.next_frame_at += duration;
        slot.stall += duration;
    }

    /// Drop a client's link for `duration` (it should resume afterwards).
    pub fn partition(&mut self, c: ClientHandle, duration: Duration) {
        if let Some(link) = self.clients[c.0].link {
            self.network.partition(link, duration);
        }
    }

    /// The client entity mirroring a server entity.
    pub fn client_entity(&self, c: ClientHandle, server_entity: Entity) -> Option<Entity> {
        let id = self.server_net().net_id(server_entity)?;
        self.client_net(c).entity(id)
    }

    /// Compare every entity in the client's scope: same set, identical encoded bytes for every
    /// replicated component the client may see (predicted components compared at the last
    /// confirmed tick). Changes newer than the client's last received snapshot are skipped
    /// (still in flight).
    pub fn assert_converged(&self, c: ClientHandle) -> Result<(), ConvergenceError> {
        let net = self.client_net(c);
        let server = self.server_net();
        let Some(client_id) = net.client_id() else {
            return Err(ConvergenceError::Missing(NetId(0)));
        };
        let registry = self.server.resources.expect::<ReplicationRegistry>();
        let client_registry = self.clients[c.0].app.resources.expect::<ReplicationRegistry>();
        let cworld = &self.clients[c.0].app.world;
        let player = self.clients[c.0].player;
        let team = server
            .clients()
            .into_iter()
            .find(|i| i.client == client_id)
            .and_then(|i| i.identity.team);
        let scope = server.scope_of(client_id);
        let last = net.last_snapshot_tick();
        let enc = EncodeCtx::client(&net.map);
        let mut w = Writer::new();
        for &id in &scope {
            let Some(e) = net.entity(id) else {
                return Err(ConvergenceError::Missing(id));
            };
            let Some(rec) = server.cache.get(id) else { continue };
            let allowed = crate::server::send::audience_mask(rec, registry, player, team, &self.server.world);
            let predicted = cworld.satisfies::<&crate::Predicted>(e);
            for entry in client_registry.components() {
                let kind = entry.kind.expect("frozen").0;
                let bit = 1u64 << kind;
                let server_slot = rec.comp(kind).filter(|_| allowed & bit != 0);
                if let Some(s) = server_slot
                    && (s.changed_tick > last || rec.structure_tick > last)
                {
                    continue; // in flight
                }
                if server_slot.is_none() && rec.structure_tick > last {
                    continue;
                }
                w.clear();
                let client_bytes: Option<Vec<u8>> = if predicted && entry.mode.is_predicted() {
                    net.prediction
                        .confirmed
                        .get(&id)
                        .and_then(|c| c.comps.iter().find(|(k, _)| *k == kind).map(|(_, b)| b.clone()))
                } else if entry.mode.is_interpolated() || entry.mode == crate::ClientMode::Delayed {
                    match entry.ops.newest_sample(cworld, e, &mut w, &enc) {
                        Some(_) => Some(w.as_slice().to_vec()),
                        None => entry.ops.encode(cworld, e, &mut w, &enc).then(|| w.as_slice().to_vec()),
                    }
                } else {
                    entry.ops.encode(cworld, e, &mut w, &enc).then(|| w.as_slice().to_vec())
                };
                let same = match (server_slot, &client_bytes) {
                    (Some(s), Some(b)) => s.bytes.as_slice() == b.as_slice(),
                    (None, None) => true,
                    _ => false,
                };
                if !same {
                    return Err(ConvergenceError::Differs {
                        net_id: id,
                        component: entry.name.to_owned(),
                    });
                }
            }
        }
        for id in net.net_ids() {
            if scope.binary_search(&id).is_err() {
                return Err(ConvergenceError::Extra(id));
            }
        }
        Ok(())
    }

    /// Hash of every packet delivered so far (determinism: equal for equal seeds).
    pub fn wire_digest(&self) -> u64 {
        self.network.wire_digest()
    }

    fn route_all(&mut self) {
        for ev in self.endpoint.drain() {
            self.route(ev);
        }
    }

    fn route(&mut self, event: LinkEvent) {
        match event {
            LinkEvent::Opened { link, peer, tx } => {
                self.routes.insert(link, Route::Pending { tx, peer });
            }
            LinkEvent::Packet { link, data, .. } => match self.routes.remove(&link) {
                Some(Route::Pending { tx, peer }) => match parse_hello(&data) {
                    Ok(hello) => {
                        let identity = match &hello.auth {
                            HelloAuth::Dev { name } => {
                                let mut id = PlayerIdentity::dev(name);
                                id.team = self
                                    .clients
                                    .iter()
                                    .find(|c| c.player == id.player)
                                    .and_then(|c| c.spec.team);
                                Some(id)
                            }
                            _ => None,
                        };
                        let player = match &hello.auth {
                            HelloAuth::Dev { name } => Some(PlayerIdentity::dev(name).player),
                            HelloAuth::Resume(bytes) => ResumeToken::decode(bytes).ok().and_then(|t| {
                                self.server_net()
                                    .clients()
                                    .into_iter()
                                    .find(|i| i.client == t.client)
                                    .map(|i| i.identity.player)
                            }),
                            HelloAuth::Ticket(_) => None,
                        };
                        if let Some(p) = player
                            && let Some(slot) = self.clients.iter_mut().find(|c| c.player == p)
                        {
                            slot.link = Some(link);
                        }
                        let req = JoinRequest {
                            link,
                            tx: tx.clone(),
                            peer,
                            hello,
                            identity,
                        };
                        if self.room.try_send(RoomInbound::Join(Box::new(req))).is_ok() {
                            self.routes.insert(link, Route::Joined);
                        } else {
                            tx.close(CloseCode::POLICY, "room inbox full");
                        }
                    }
                    Err(code) => {
                        let _ = tx.send(Lane::Reliable, reject_packet(code, "bad hello", None));
                        tx.close(CloseCode::POLICY, "bad hello");
                    }
                },
                Some(Route::Joined) => {
                    self.routes.insert(link, Route::Joined);
                    let _ = self.room.try_send(RoomInbound::Packet { link, bytes: data });
                }
                None => {}
            },
            LinkEvent::Closed { link, reason } => {
                if let Some(Route::Joined) = self.routes.remove(&link) {
                    let _ = self.room.try_send(RoomInbound::Closed { link, reason });
                }
            }
        }
    }
}

impl std::fmt::Debug for NetHarness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetHarness")
            .field("now", &self.now)
            .field("clients", &self.clients.len())
            .finish_non_exhaustive()
    }
}
