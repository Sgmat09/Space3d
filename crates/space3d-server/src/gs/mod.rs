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

/// A `RoomControl::load_player_data` request waiting for the player service.
struct Fetch {
    room: RoomId,
    player: space3d_replication::PlayerId,
    sent_ms: i64,
}

/// A player-data fetch unanswered for this long is answered empty (the link may have dropped
/// after the request went out), so the game never waits forever for `PlayerDataLoaded`.
pub const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

struct GsState {
    rt: GameServerRuntime,
    checkpoint_gc: checkpoint::CheckpointGc,
    heartbeat_seq: u64,
    next_heartbeat_ms: i64,
    fetches: BTreeMap<u64, Fetch>,
    next_request: u64,
    drain_started: bool,
    stop_started: bool,
    stopped: bool,
    deregistered: bool,
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
            checkpoint_gc: checkpoint::CheckpointGc::default(),
            heartbeat_seq: 0,
            next_heartbeat_ms: i64::MIN,
            fetches: BTreeMap::new(),
            next_request: 1,
            drain_started: false,
            stop_started: false,
            stopped: false,
            deregistered: false,
        };
        app.add_system_named(Stage::Update, "gs::supervisor", move |ctx| {
            state.tick();
            // One report for both: the lifecycle may stop the process one tick after the gate
            // reads flushed, so `Deregister` must already be on its way by then (a room closing
            // on its shard between two reports used to let the process stop first and send it
            // right before the link closed).
            let report = state.drain_report();
            state.deregister_when_drained(report);
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
        self.expire_fetches(now);
        self.prune_checkpoints(now);
        self.metrics();
        self.lifecycle();
    }

    /// Drain done (rooms closed or handed off, outbox flushed): leave the fleet now rather than
    /// when the lease runs out (best effort; the lease still covers it).
    fn deregister_when_drained(&mut self, report: DrainReport) {
        if !self.deregistered && report.flushed {
            self.deregistered = true;
            let _ = self.rt.fleet.send(FleetUp::Deregister {
                reason: "drained".into(),
            });
        }
    }

    /// Garbage-collect checkpoints too old to restore in this server's own stores (the local
    /// store in single-host setups, and the rescue store).
    fn prune_checkpoints(&mut self, now: i64) {
        let env = self.rt.rooms.env();
        if !env.checkpoints.enabled() {
            return;
        }
        let (store, rescue) = (env.checkpoints.clone(), env.rescue.clone());
        self.checkpoint_gc.tick(now, env.recovery_max_age, |cutoff| {
            std::iter::once(store).chain(rescue).map(|s| s.prune(cutoff)).collect()
        });
    }

    fn expire_fetches(&mut self, now: i64) {
        let limit = FETCH_TIMEOUT.as_millis() as i64;
        let expired: Vec<u64> = self
            .fetches
            .iter()
            .filter(|(_, f)| now.saturating_sub(f.sent_ms) >= limit)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some(f) = self.fetches.remove(&id) {
                tracing::warn!(room = %f.room, player = %f.player, "player data fetch timed out; answering empty");
                self.answer_fetch(f.room, f.player, Vec::new());
            }
        }
    }

    fn answer_fetch(
        &self,
        room: RoomId,
        player: space3d_replication::PlayerId,
        entries: Vec<space3d_replication::api::PlayerDataEntry>,
    ) {
        if let Some(h) = self.rt.rooms.get(room) {
            let _ = h.command(RoomCommand::PlayerData { player, entries });
        }
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
                        // Release the seats the player service reserved for it.
                        let _ = self.rt.fleet.send(FleetUp::RoomClosed {
                            room_id: spec.room_id,
                            reason: format!("not created: {e}"),
                            restorable: false,
                        });
                    }
                }
                FleetDown::CloseRoom { room_id, reason } => {
                    if let Some(h) = self.rt.rooms.get(room_id) {
                        let _ = h.command(RoomCommand::Close { reason });
                    }
                }
                FleetDown::RoomSuperseded { room_id } => {
                    if let Some(h) = self.rt.rooms.get(room_id) {
                        tracing::warn!(room = %room_id, "room restored on another server during a fleet partition; closing this copy");
                        let _ = h.command(RoomCommand::Superseded);
                    }
                }
                FleetDown::Reregister if !self.deregistered => {
                    tracing::warn!("the player service asked for a new registration");
                    let _ = self.rt.fleet.send(FleetUp::Register(self.rt.registration.clone()));
                    // Right behind it: the rooms (stale copies get closed) and the drain state
                    // (a registration alone reads as ready for new rooms).
                    self.heartbeat();
                }
                FleetDown::Reregister => {}
                FleetDown::Kick { player, reason } => self.rt.rooms.kick_everywhere(player, &reason),
                FleetDown::Ban(ban) => {
                    let ip = matches!(ban.target, crate::security::bans::BanTarget::Ip(_));
                    // Account bans: every room the player holds a slot in removes them
                    // (`Disconnect(Banned)`) when it sees the ban list change.
                    self.rt.bans.upsert(ban);
                    if ip {
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
                FleetDown::RoomMovingAck { room_id } => {
                    if let Some(h) = self.rt.rooms.get(room_id) {
                        let _ = h.command(RoomCommand::MoveAcked);
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
                    if let Some(f) = self.fetches.remove(&request_id) {
                        self.answer_fetch(f.room, player, entries);
                    }
                }
            }
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
                        self.fetches.insert(
                            id,
                            Fetch {
                                room,
                                player,
                                sent_ms: self.rt.unix.now_ms(),
                            },
                        );
                    } else {
                        // Player service unreachable: answer empty so the game can proceed.
                        self.answer_fetch(room, player, Vec::new());
                    }
                }
                FleetUp::RoomMoving { room_id } => {
                    // The room keeps its clients until the player service acknowledged the
                    // move; with the link down there is nobody to wait for.
                    if self.rt.fleet.send(FleetUp::RoomMoving { room_id }).is_err()
                        && let Some(h) = self.rt.rooms.get(room)
                    {
                        let _ = h.command(RoomCommand::MoveAcked);
                    }
                }
                other => {
                    let _ = self.rt.fleet.send(other);
                }
            }
        }
        if self.fetches.len() > 10_000 {
            // Bounded; the oldest are answered empty.
            let keep = self.fetches.split_off(&(self.next_request - 5_000));
            let dropped = std::mem::replace(&mut self.fetches, keep);
            for f in dropped.into_values() {
                self.answer_fetch(f.room, f.player, Vec::new());
            }
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
