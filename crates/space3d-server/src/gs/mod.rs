//! Game-server role.
//!
//! ```text
//! tokio (space3d_tasks::runtime())                     shard threads × S (std::thread)
//!  axum /play (WsServerTransport) ─LinkEvent─▶ ingress ─RoomInbound (bounded, try_send)─▶ room App
//!  /ping, /healthz, /readyz            (Gatekeeper: Hello, ticket HMAC,     (built on the shard;
//!  admin :9090, /metrics                jti, bans, per-IP limits, routing)   App is !Send)
//!  fleet link task, checkpoint uploads, outbox flusher        ◀─ RoomStatus (1 Hz, Mutex) ─┘
//! supervisor App (main thread, 10 Hz): lifecycle, room directory, fleet heartbeats, drain
//! space3d-db worker: local outbox + local checkpoints      rayon: checkpoint serialization
//! ```
//!
//! Rule: a room tick never awaits, blocks or takes a contended lock; everything crossing
//! threads goes through bounded queues (`try_send` / `try_recv`).

pub mod checkpoint;
pub mod fleet;
pub mod gatekeeper;
pub mod ingress;
pub mod outbox;
pub mod room;
pub mod rooms;
pub mod shard;
pub mod transport_ws;

use crate::clock::UnixClock;
use crate::config::ServerConfig;
use crate::health::HealthState;
use crate::lifecycle::{DrainGates, DrainReport, LifecycleHandle, Phase};
use crate::metrics::GameServerMetrics;
use crate::security::bans::BanList;
use fleet::{FleetDown, FleetLink, FleetUp, Heartbeat, ServerRegistration};
use gatekeeper::Gatekeeper;
use outbox::Outbox;
use parking_lot::Mutex;
use rooms::{RoomCommand, RoomManager, RoomPhase};
use space3d_core::{App, Stage};
use space3d_replication::RoomId;
use space3d_replication::protocol::control::NoticeLevel;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Everything the game-server supervisor systems own (built by `ServerMain`).
pub struct GameServerRuntime {
    /// Config.
    pub config: ServerConfig,
    /// Room directory.
    pub rooms: RoomManager,
    /// Gatekeeper (shared with the ingress task).
    pub gate: Arc<Mutex<Gatekeeper>>,
    /// Fleet link.
    pub fleet: Arc<dyn FleetLink>,
    /// Durable outbox.
    pub outbox: Outbox,
    /// Room → supervisor messages.
    pub uplink: async_channel::Receiver<(RoomId, FleetUp)>,
    /// Bans (shared with the gatekeeper and the admission hook).
    pub bans: BanList,
    /// Health.
    pub health: HealthState,
    /// Lifecycle.
    pub lifecycle: LifecycleHandle,
    /// Drain flag read by the ingress task.
    pub draining: Arc<AtomicBool>,
    /// Metrics.
    pub metrics: GameServerMetrics,
    /// Wall clock.
    pub unix: Arc<dyn UnixClock>,
    /// Shard threads (`None` in manual mode).
    pub shards: Option<shard::ShardPool>,
    /// Registration sent to the player service.
    pub registration: ServerRegistration,
}

struct GsState {
    rt: GameServerRuntime,
    heartbeat_seq: u64,
    next_heartbeat_ms: i64,
    fetches: BTreeMap<u64, RoomId>,
    next_request: u64,
    drain_started: bool,
    stop_started: bool,
    stopped: bool,
}

/// Supervisor plugin for the game-server role: fleet link (register, heartbeats, commands),
/// outbox pump, room uplink (presence, player-data fetches), room sweep, metrics and drain.
/// Listeners, shards and the ingress task are started by `ServerMain` before this is added.
pub struct GameServerPlugin {
    runtime: Mutex<Option<GameServerRuntime>>,
}

impl GameServerPlugin {
    /// Plugin over a started runtime.
    pub fn new(runtime: GameServerRuntime) -> Self {
        Self {
            runtime: Mutex::new(Some(runtime)),
        }
    }
}

impl space3d_core::Plugin for GameServerPlugin {
    fn build(&self, app: &mut App) {
        let Some(rt) = self.runtime.lock().take() else {
            return;
        };
        let _ = rt.fleet.send(FleetUp::Register(rt.registration.clone()));
        let mut state = GsState {
            rt,
            heartbeat_seq: 0,
            next_heartbeat_ms: i64::MIN,
            fetches: BTreeMap::new(),
            next_request: 1,
            drain_started: false,
            stop_started: false,
            stopped: false,
        };
        app.add_system_named(Stage::Update, "gs::supervisor", move |ctx| {
            state.tick();
            let report = state.drain_report();
            if let Some(gates) = ctx.resources.get_mut::<DrainGates>() {
                gates.set("game-server", report);
            }
        });
    }
}

impl GsState {
    fn tick(&mut self) {
        let now = self.rt.unix.now_ms();
        self.fleet_down();
        self.uplink();
        self.rt.outbox.pump(self.rt.fleet.as_ref());
        for closed in self.rt.rooms.sweep() {
            self.rt.gate.lock().forget_room(closed);
        }
        if now >= self.next_heartbeat_ms {
            self.next_heartbeat_ms = now + self.rt.config.fleet.heartbeat.as_millis() as i64;
            self.heartbeat();
        }
        self.metrics();
        self.lifecycle();
    }

    fn fleet_down(&mut self) {
        while let Some(msg) = self.rt.fleet.try_recv() {
            match msg {
                FleetDown::Registered { bans, .. } => {
                    self.rt.health.set_registered(true);
                    for b in bans {
                        self.rt.bans.upsert(b);
                    }
                }
                FleetDown::PrepareRoom { spec } => {
                    if let Err(e) = self.rt.rooms.get_or_create(&spec) {
                        tracing::warn!(room = %spec.room_id, error = %e, "prepare room failed");
                    }
                }
                FleetDown::CloseRoom { room_id, reason } => {
                    if let Some(h) = self.rt.rooms.get(room_id) {
                        let _ = h.command(RoomCommand::Close { reason });
                    }
                }
                FleetDown::Kick { player, reason } => self.kick(player, &reason),
                FleetDown::Ban(ban) => {
                    let player = match ban.target {
                        crate::security::bans::BanTarget::Player(p) => Some(p),
                        crate::security::bans::BanTarget::Ip(_) => None,
                    };
                    if let Some(p) = player {
                        self.kick(p, &ban.reason);
                    }
                    self.rt.bans.upsert(ban);
                    if player.is_none() {
                        // Connected clients in the banned range go now (not at their next join).
                        self.rt.gate.lock().enforce_ip_bans(self.rt.unix.now_ms());
                    }
                }
                FleetDown::Unban { ban_id } => {
                    self.rt.bans.remove(ban_id);
                }
                FleetDown::Broadcast { room_id, text } => {
                    let cmd = || RoomCommand::Broadcast {
                        text: text.clone(),
                        level: NoticeLevel::Info,
                    };
                    match room_id {
                        Some(id) => {
                            if let Some(h) = self.rt.rooms.get(id) {
                                let _ = h.command(cmd());
                            }
                        }
                        None => self.rt.rooms.broadcast(cmd),
                    }
                }
                FleetDown::Drain => {
                    if self.rt.lifecycle.requests() == 0 {
                        self.rt.lifecycle.drain();
                    }
                }
                FleetDown::Ack { op_id } => self.rt.outbox.ack(&op_id),
                FleetDown::PlayerData {
                    request_id,
                    player,
                    entries,
                } => {
                    if let Some(room) = self.fetches.remove(&request_id)
                        && let Some(h) = self.rt.rooms.get(room)
                    {
                        let _ = h.command(RoomCommand::PlayerData { player, entries });
                    }
                }
            }
        }
    }

    fn kick(&self, player: space3d_replication::PlayerId, reason: &str) {
        if let Some(room) = self.rt.rooms.find_player(player)
            && let Some(h) = self.rt.rooms.get(room)
        {
            let _ = h.command(RoomCommand::Kick {
                player,
                reason: reason.to_owned(),
            });
        }
    }

    fn uplink(&mut self) {
        while let Ok((room, msg)) = self.rt.uplink.try_recv() {
            match msg {
                FleetUp::FetchPlayerData { player, keys, .. } => {
                    let id = self.next_request;
                    self.next_request += 1;
                    let sent = self.rt.fleet.send(FleetUp::FetchPlayerData {
                        request_id: id,
                        player,
                        keys,
                    });
                    if sent.is_ok() {
                        self.fetches.insert(id, room);
                    } else if let Some(h) = self.rt.rooms.get(room) {
                        // Player service unreachable: answer empty so the game can proceed.
                        let _ = h.command(RoomCommand::PlayerData {
                            player,
                            entries: Vec::new(),
                        });
                    }
                }
                other => {
                    let _ = self.rt.fleet.send(other);
                }
            }
        }
        if self.fetches.len() > 10_000 {
            let keep = self.fetches.split_off(&(self.next_request - 5_000));
            self.fetches = keep;
        }
    }

    fn heartbeat(&mut self) {
        self.heartbeat_seq += 1;
        let rooms = self.rt.rooms.list();
        let hb = Heartbeat {
            seq: self.heartbeat_seq,
            players: rooms.iter().map(|r| u32::from(r.players)).sum(),
            tick_p99_us: rooms.iter().map(|r| r.tick_p99_us).max().unwrap_or(0),
            draining: self.rt.lifecycle.phase() >= Phase::Draining,
            rooms,
        };
        let _ = self.rt.fleet.send(FleetUp::Heartbeat(hb));
        // Registration is a readiness latch; the live link state is reported separately.
        let connected = self.rt.fleet.is_connected();
        self.rt.health.set_fleet_connected(connected);
        self.rt
            .metrics
            .registry
            .gauge(
                "space3d_fleet_connected",
                "1 while the fleet link to the player service is up.",
                &[],
            )
            .set(if connected { 1.0 } else { 0.0 });
    }

    fn metrics(&self) {
        let reg = &self.rt.metrics.registry;
        let rooms = self.rt.rooms.list();
        for phase in [
            RoomPhase::Waiting,
            RoomPhase::Running,
            RoomPhase::Ending,
            RoomPhase::Closed,
            RoomPhase::Crashed,
        ] {
            let n = rooms.iter().filter(|r| r.phase == phase).count();
            reg.gauge("space3d_rooms", "Rooms by phase.", &[("phase", phase.label())])
                .set(n as f64);
        }
        let connected: u32 = rooms.iter().map(|r| u32::from(r.players)).sum();
        let disconnected: u32 = rooms.iter().map(|r| u32::from(r.disconnected)).sum();
        reg.gauge(
            "space3d_players",
            "Players by connection state.",
            &[("state", "connected")],
        )
        .set(f64::from(connected));
        reg.gauge(
            "space3d_players",
            "Players by connection state.",
            &[("state", "disconnected")],
        )
        .set(f64::from(disconnected));
        self.rt.metrics.outbox_pending.set(self.rt.outbox.pending() as f64);
    }

    fn lifecycle(&mut self) {
        let phase = self.rt.lifecycle.phase();
        if phase >= Phase::Draining && !self.drain_started {
            self.drain_started = true;
            self.rt.draining.store(true, Ordering::Release);
            self.rt.gate.lock().set_draining(true);
            self.rt.health.set_draining(true);
            self.rt.rooms.set_draining(true);
            let deadline = self.rt.config.game.drain_timeout;
            self.rt.rooms.broadcast(|| RoomCommand::Drain { deadline });
            // Tell the player service right away (no new allocations here).
            self.next_heartbeat_ms = i64::MIN;
            tracing::info!(rooms = self.rt.rooms.len(), "game server draining");
        }
        if phase >= Phase::Stopping && !self.stop_started {
            self.stop_started = true;
            self.rt.rooms.broadcast(|| RoomCommand::Shutdown);
            tracing::info!("game server stopping: final checkpoints");
        }
        if phase == Phase::Stopped && !self.stopped {
            self.stopped = true;
            if let Some(shards) = &self.rt.shards {
                if self.rt.lifecycle.requests() >= 2 {
                    // Second signal: no graceful close happened; checkpoint every room to the
                    // local store and disconnect its clients before the process exits.
                    tracing::warn!("forced stop: local checkpoints");
                    shards.force_stop();
                } else {
                    shards.stop();
                }
            }
        }
    }

    fn drain_report(&self) -> DrainReport {
        let rooms = self.rt.rooms.list();
        DrainReport {
            // Match rooms finish naturally; persistent rooms already checkpointed and asked
            // clients to move, so they do not hold the drain.
            idle: rooms
                .iter()
                .all(|r| r.persistent || (r.players == 0 && r.disconnected == 0)),
            flushed: self.stop_started && rooms.is_empty() && self.rt.outbox.is_flushed(),
        }
    }
}

/// Resource: addresses and handles of a running game server.
#[derive(Clone)]
pub struct GameServerHandle {
    /// Bound `/play` address (real port when configured with 0).
    pub game_addr: SocketAddr,
    /// Bound admin address.
    pub admin_addr: SocketAddr,
    /// Room directory.
    pub rooms: RoomManager,
    /// Schema hash of the game's protocol.
    pub schema_hash: u64,
    /// Gatekeeper (TestCluster feeds memory-network events through it).
    pub gate: Arc<Mutex<Gatekeeper>>,
    /// Bans.
    pub bans: BanList,
}

impl GameServerHandle {
    /// `ws://host:port/play` reachable from this machine.
    pub fn play_url(&self, path: &str) -> String {
        let mut a = self.game_addr;
        if a.ip().is_unspecified() {
            a.set_ip(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        }
        format!("ws://{a}{path}")
    }
}
