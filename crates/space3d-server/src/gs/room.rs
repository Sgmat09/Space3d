//! Inside a room `App`: construction ([`build_room_app`]) and the server-side room plugin.
//!
//! Construction order (on the shard thread): `App::new().with_tickrate(tick_hz)` →
//! resources (`NetInbox`, `SecretSource` (getrandom), `NetClockHandle`, `Metrics`,
//! [`RoomContext`], [`RoomControl`]) → `game.protocol(app)` → `ServerReplicationPlugin`
//! (config from `game.room_net_config`) → [`RoomPlugin`] → `game.build_room(app, spec)` →
//! `full_state::restore` + server-only restore if a checkpoint is given (epoch + 1).
//!
//! `RoomPlugin` systems: PreUpdate `room::commands` (apply [`RoomCommand`]s: kick, broadcast,
//! close, drain, shutdown, checkpoint, joinable, inspect, player data); Last `room::ops`
//! (forward [`RoomControl`] requests to the outbox, presence to the fleet link, 1 Hz
//! [`RoomStatus`] + metric deltas, checkpoint cadence, empty-room TTL, close/shutdown).
//!
//! Checkpoint capture needs the whole `App` (`full_state::capture(&App)`), so it runs right
//! after the tick in [`after_tick`] (called by the shard and by `TestCluster` through
//! [`run_tick`]); serialization and upload run off the room thread.

use super::checkpoint::{CHECKPOINT_VERSION, Checkpoint, StoreError};
use super::rooms::{RoomCommand, RoomDetail, RoomEnv, RoomError, RoomPhase, RoomStatus};
use crate::game::RoomSpec;
use crate::gs::fleet::{DataWrite, FleetUp};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use space3d_core::{App, EventReader, Events, Stage, SystemCtx};
use space3d_replication::protocol::control::{Notice, NoticeLevel};
use space3d_replication::protocol::handshake::DisconnectCode;
use space3d_replication::server::handshake::SecretSource;
use space3d_replication::server::{
    NetInbox, NetServer, PlayerJoined, PlayerLeft, ServerReplicationPlugin, SlotState, full_state,
};
use space3d_replication::stats::RoomNetStats;
use space3d_replication::{MatchId, PlayerId, Replicated, RoomId, Target};
use space3d_tasks::{Task, TaskState};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Duration;

/// Everything needed to build a room on its shard.
pub struct RoomBuild {
    /// Spec.
    pub spec: RoomSpec,
    /// Process environment (game, stores, outbox, clocks, metrics).
    pub env: RoomEnv,
    /// Inbox.
    pub inbox: NetInbox,
    /// Commands.
    pub commands: crossbeam_channel::Receiver<RoomCommand>,
    /// Status cell.
    pub status: Arc<Mutex<RoomStatus>>,
    /// Checkpoint to restore.
    pub restore: Option<Checkpoint>,
    /// Creation time (unix ms).
    pub created_ms: i64,
    /// Forced room epoch (a crash rebuild without a checkpoint bumps it so old resume tokens
    /// die); a restore uses `checkpoint epoch + 1`.
    pub epoch: Option<u32>,
}

/// Build a room `App` (call on the thread that will run it). Game code runs here, so callers
/// on shard threads wrap it in `catch_unwind`.
pub fn build_room_app(build: RoomBuild) -> Result<App, RoomError> {
    let RoomBuild {
        spec,
        env,
        inbox,
        commands,
        status,
        restore,
        created_ms,
        epoch,
    } = build;
    let mut app = App::new().with_tickrate(env.tick_hz);
    app.add_plugin(space3d_tasks::TasksPlugin);
    app.insert_resource(inbox);
    app.insert_resource(match env.secret_seed {
        Some(seed) => SecretSource::seeded(seed ^ spec.room_id.0),
        None => SecretSource::from_fn(crate::security::random_bytes::<16>),
    });
    app.insert_resource(env.clock.clone());
    app.insert_resource(env.metrics.registry.clone());
    app.insert_resource(env.metrics.clone());
    app.insert_resource(RoomContext {
        spec: spec.clone(),
        server_id: env.server_id.clone(),
        created_ms,
        drain_deadline_ms: None,
    });
    app.insert_resource(RoomControl::default());
    env.game.protocol(&mut app);
    let mut net = env.game.room_net_config(&spec, &env.net);
    net.room = spec.room_id;
    net.game_id = env.game.game_id().to_owned();
    if spec.max_players > 0 {
        net.max_clients = spec.max_players;
    }
    let room_epoch = restore
        .as_ref()
        .map(|cp| cp.state.epoch.wrapping_add(1))
        .or(epoch)
        .unwrap_or(net.room_epoch);
    net.room_epoch = room_epoch;
    app.add_plugin(ServerReplicationPlugin { config: net });
    app.insert_resource(RoomRuntime::new(
        env.clone(),
        spec.clone(),
        commands,
        status,
        created_ms,
        room_epoch,
    ));
    app.add_plugin(RoomPlugin);
    env.game.build_room(&mut app, &spec);
    if let Some(cp) = &restore {
        full_state::restore(&mut app, &cp.state).map_err(|e| RoomError::Build(format!("restore: {e}")))?;
        if let (Some(json), Some(registry)) = (&cp.server_only, env.game.checkpoint_registry()) {
            space3d_scene::Scene::from_json(json)
                .and_then(|scene| scene.spawn_into(&mut app.world, &registry))
                .map_err(|e| RoomError::Build(format!("server-only state: {e}")))?;
        }
        if let Some(rt) = app.resources.get_mut::<RoomRuntime>() {
            rt.last_checkpoint = Some(cp.clone());
        }
        tracing::info!(room = %spec.room_id, tick = cp.state.tick.0, epoch = room_epoch, "room restored from checkpoint");
    }
    Ok(app)
}

/// One room tick: `app.update(step)` (exactly one fixed tick) + [`after_tick`].
pub fn run_tick(app: &mut App, step: Duration) {
    app.update(step);
    after_tick(app);
}

/// [`run_tick`] with panics caught (`profile.server` builds unwind). `Err` = the room crashed.
pub fn run_tick_guarded(app: &mut App, step: Duration) -> Result<(), String> {
    catch_unwind(AssertUnwindSafe(|| run_tick(app, step))).map_err(panic_message)
}

/// Text of a panic payload.
pub fn panic_message(p: Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

/// Post-tick work that needs the whole `App`: checkpoint capture.
pub fn after_tick(app: &mut App) {
    let Some(rt) = app.resources.get_mut::<RoomRuntime>() else {
        return;
    };
    if !std::mem::take(&mut rt.want_checkpoint) {
        return;
    }
    rt.ticks_since_checkpoint = 0;
    let started = std::time::Instant::now();
    let state = match full_state::capture(app) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "checkpoint capture failed");
            return;
        }
    };
    let rt = app.resources.expect::<RoomRuntime>();
    let server_only = rt
        .env
        .game
        .checkpoint_registry()
        .and_then(|registry| space3d_scene::save_world(&app.world, &registry).ok())
        .and_then(|scene| scene.to_json().ok());
    let cp = Checkpoint {
        version: CHECKPOINT_VERSION,
        room_id: rt.spec.room_id,
        spec: rt.spec.clone(),
        state,
        server_only,
        game_blob: None,
        saved_ms: rt.env.unix.now_ms(),
    };
    rt.env.metrics.checkpoint_seconds.observe_duration(started.elapsed());
    let rt = app.resources.expect_mut::<RoomRuntime>();
    rt.last_checkpoint = Some(cp.clone());
    rt.start_upload(cp);
}

/// Forced stop (second signal): capture the room right now, write the checkpoint to the local
/// rescue store and end every link with `Disconnect(ServerShutdown)` (no resume: this process
/// is going away). Returns the pending write (the caller waits for it, bounded) or `None`
/// when checkpoints are disabled or the capture failed.
pub fn emergency_stop(app: &mut App) -> Option<Task<Result<(), StoreError>>> {
    let write = emergency_checkpoint(app);
    if let Some(net) = app.resources.get_mut::<NetServer>() {
        net.disconnect_all(DisconnectCode::ServerShutdown, "server stopped", false);
    }
    write
}

fn emergency_checkpoint(app: &mut App) -> Option<Task<Result<(), StoreError>>> {
    let rt = app.resources.get::<RoomRuntime>()?;
    if !rt.env.checkpoints.enabled() {
        return None;
    }
    let store = rt.env.rescue.clone()?;
    let state = match full_state::capture(app) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "forced stop: checkpoint capture failed");
            return None;
        }
    };
    let rt = app.resources.expect::<RoomRuntime>();
    let server_only = rt
        .env
        .game
        .checkpoint_registry()
        .and_then(|registry| space3d_scene::save_world(&app.world, &registry).ok())
        .and_then(|scene| scene.to_json().ok());
    let cp = Checkpoint {
        version: CHECKPOINT_VERSION,
        room_id: rt.spec.room_id,
        spec: rt.spec.clone(),
        state,
        server_only,
        game_blob: None,
        saved_ms: rt.env.unix.now_ms(),
    };
    tracing::warn!(room = %cp.room_id, tick = cp.state.tick.0, "forced stop: room checkpointed locally");
    Some(store.put(cp.room_id, cp.to_bytes()))
}

/// Resource: static facts about the room.
#[derive(Clone, Debug)]
pub struct RoomContext {
    /// Spec.
    pub spec: RoomSpec,
    /// Hosting server.
    pub server_id: String,
    /// Created (unix ms).
    pub created_ms: i64,
    /// Set when the server drains: the room should wrap up by then (unix ms).
    pub drain_deadline_ms: Option<i64>,
}

/// One player's outcome.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlayerResult {
    /// Player.
    pub player: PlayerId,
    /// Team.
    pub team: Option<u16>,
    /// Score.
    pub score: i64,
    /// Placement (1 = winner).
    pub placement: Option<u16>,
    /// Game stats.
    pub stats: serde_json::Value,
}

/// Result of a match (→ outbox → player service: match history, leaderboards, Elo).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MatchResult {
    /// Match.
    pub match_id: MatchId,
    /// Room.
    pub room_id: RoomId,
    /// Mode.
    pub mode: String,
    /// Start (unix ms).
    pub started_ms: i64,
    /// End (unix ms).
    pub ended_ms: i64,
    /// Winning team, if any.
    pub winner_team: Option<u16>,
    /// Players.
    pub players: Vec<PlayerResult>,
    /// Leaderboard submissions `(board, player, score)`.
    pub scores: Vec<(String, PlayerId, i64)>,
    /// Extra data.
    pub data: serde_json::Value,
}

/// Resource: room → outside world (all calls non-blocking; delivered at-least-once through the
/// durable outbox, deduplicated by op id on the player service).
#[derive(Default)]
pub struct RoomControl {
    pending: Vec<RoomRequest>,
}

/// Queued room request (internal).
#[derive(Debug)]
pub enum RoomRequest {
    /// Match finished.
    EndMatch(MatchResult),
    /// Player data write.
    WritePlayerData {
        /// Player.
        player: PlayerId,
        /// Key.
        key: String,
        /// Value.
        value: serde_json::Value,
    },
    /// Leaderboard score.
    SubmitScore {
        /// Board.
        board: String,
        /// Player.
        player: PlayerId,
        /// Score.
        score: i64,
    },
    /// Load player data (answer arrives as `Events<PlayerDataLoaded>`).
    LoadPlayerData {
        /// Player.
        player: PlayerId,
        /// Keys.
        keys: Vec<String>,
    },
    /// Checkpoint now.
    Checkpoint,
    /// Close the room.
    Close(String),
}

impl RoomControl {
    /// Report the match result.
    pub fn end_match(&mut self, result: MatchResult) {
        self.pending.push(RoomRequest::EndMatch(result));
    }

    /// Persist a player-data key (e.g. `"progress"`, `"inventory"`).
    pub fn write_player_data(&mut self, player: PlayerId, key: &str, value: serde_json::Value) {
        self.pending.push(RoomRequest::WritePlayerData {
            player,
            key: key.to_owned(),
            value,
        });
    }

    /// Submit a leaderboard score.
    pub fn submit_score(&mut self, board: &str, player: PlayerId, score: i64) {
        self.pending.push(RoomRequest::SubmitScore {
            board: board.to_owned(),
            player,
            score,
        });
    }

    /// Ask for player data (e.g. on `PlayerJoined`); answered via `Events<PlayerDataLoaded>`.
    pub fn load_player_data(&mut self, player: PlayerId, keys: &[&str]) {
        self.pending.push(RoomRequest::LoadPlayerData {
            player,
            keys: keys.iter().map(|k| (*k).to_owned()).collect(),
        });
    }

    /// Checkpoint at the end of this tick.
    pub fn request_checkpoint(&mut self) {
        self.pending.push(RoomRequest::Checkpoint);
    }

    /// Close the room after this tick.
    pub fn close(&mut self, reason: &str) {
        self.pending.push(RoomRequest::Close(reason.to_owned()));
    }

    /// Drain queued requests (room plugin).
    pub fn take(&mut self) -> Vec<RoomRequest> {
        std::mem::take(&mut self.pending)
    }
}

/// Event: player data requested with `RoomControl::load_player_data` arrived.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerDataLoaded {
    /// Player.
    pub player: PlayerId,
    /// Entries (missing keys are absent).
    pub entries: Vec<space3d_replication::api::PlayerDataEntry>,
}

#[derive(Clone, Debug, PartialEq)]
enum Closing {
    Close(String),
    Shutdown { waited: u32 },
}

/// Resource: the room's operational state (internal; inserted by [`build_room_app`]).
pub struct RoomRuntime {
    env: RoomEnv,
    spec: RoomSpec,
    commands: crossbeam_channel::Receiver<RoomCommand>,
    status: Arc<Mutex<RoomStatus>>,
    created_ms: i64,
    epoch: u32,
    nonce: u64,
    op_counter: u64,
    phase: RoomPhase,
    closing: Option<Closing>,
    want_checkpoint: bool,
    last_checkpoint: Option<Checkpoint>,
    upload: Option<Task<Result<(), StoreError>>>,
    queued_upload: Option<Checkpoint>,
    ticks_since_checkpoint: u32,
    checkpoint_every: u32,
    ticks_since_status: u32,
    ticks_per_second: u32,
    empty_ticks: u32,
    empty_ttl_ticks: u32,
    prev_stats: RoomNetStats,
    joined: EventReader,
    left: EventReader,
}

impl RoomRuntime {
    fn new(
        env: RoomEnv,
        spec: RoomSpec,
        commands: crossbeam_channel::Receiver<RoomCommand>,
        status: Arc<Mutex<RoomStatus>>,
        created_ms: i64,
        epoch: u32,
    ) -> Self {
        let hz = env.tick_hz.max(1.0);
        let ticks = |d: Duration| ((d.as_secs_f64() * hz).round() as u32).max(1);
        Self {
            checkpoint_every: ticks(env.checkpoint_interval),
            ticks_per_second: ticks(Duration::from_secs(1)),
            empty_ttl_ticks: ticks(env.empty_room_ttl),
            env,
            spec,
            commands,
            status,
            created_ms,
            epoch,
            nonce: u64::from_le_bytes(crate::security::random_bytes::<8>()),
            op_counter: 0,
            phase: RoomPhase::Waiting,
            closing: None,
            want_checkpoint: false,
            last_checkpoint: None,
            upload: None,
            queued_upload: None,
            ticks_since_checkpoint: 0,
            ticks_since_status: u32::MAX / 2,
            empty_ticks: 0,
            prev_stats: RoomNetStats::default(),
            joined: EventReader::default(),
            left: EventReader::default(),
        }
    }

    /// Split for a crash rebuild: the command receiver and the latest in-memory checkpoint.
    pub fn into_parts(self) -> (crossbeam_channel::Receiver<RoomCommand>, Option<Checkpoint>) {
        (self.commands, self.last_checkpoint)
    }

    /// Current phase.
    pub fn phase(&self) -> RoomPhase {
        self.phase
    }

    /// Latest in-memory checkpoint.
    pub fn last_checkpoint(&self) -> Option<&Checkpoint> {
        self.last_checkpoint.as_ref()
    }

    /// Whether a checkpoint upload is in flight or queued.
    pub fn uploading(&self) -> bool {
        self.upload.is_some() || self.queued_upload.is_some()
    }

    fn request_checkpoint(&mut self) {
        if self.env.checkpoints.enabled() {
            self.want_checkpoint = true;
        }
    }

    fn next_op_id(&mut self) -> String {
        self.op_counter += 1;
        format!(
            "{}:{}:{:016x}:{}",
            self.env.server_id, self.spec.room_id, self.nonce, self.op_counter
        )
    }

    fn start_upload(&mut self, cp: Checkpoint) {
        if self.upload.is_some() {
            self.queued_upload = Some(cp);
            return;
        }
        let store = self.env.checkpoints.clone();
        let metrics = self.env.metrics.clone();
        let room = cp.room_id;
        self.upload = Some(space3d_tasks::spawn(async move {
            let bytes = space3d_tasks::spawn_compute(move || cp.to_bytes())
                .wait()
                .await
                .ok_or_else(|| StoreError("serialization task lost".into()))?;
            metrics.checkpoint_bytes.observe(bytes.len() as f64);
            store
                .put(room, bytes)
                .wait()
                .await
                .unwrap_or_else(|| Err(StoreError("store task lost".into())))
        }));
    }

    fn poll_upload(&mut self) {
        let Some(task) = &mut self.upload else {
            return;
        };
        match task.state() {
            TaskState::Pending => return,
            TaskState::Ready => {
                if let Some(Err(e)) = task.poll() {
                    tracing::warn!(room = %self.spec.room_id, error = %e, "checkpoint upload failed");
                }
            }
            TaskState::Cancelled => tracing::warn!(room = %self.spec.room_id, "checkpoint upload cancelled"),
        }
        self.upload = None;
        if let Some(next) = self.queued_upload.take() {
            self.start_upload(next);
        }
    }

    fn set_phase(&mut self, phase: RoomPhase) {
        if self.phase != phase {
            self.phase = phase;
            self.ticks_since_status = u32::MAX / 2;
            if matches!(phase, RoomPhase::Running | RoomPhase::Ending) {
                self.request_checkpoint();
            }
        }
    }

    fn uplink(&self, msg: FleetUp) {
        if self.env.uplink.try_send((self.spec.room_id, msg)).is_err() {
            tracing::debug!(room = %self.spec.room_id, "uplink full; presence update dropped");
        }
    }
}

/// The server-side room plugin (see the module docs).
pub struct RoomPlugin;

impl space3d_core::Plugin for RoomPlugin {
    fn build(&self, app: &mut App) {
        app.add_event::<PlayerDataLoaded>();
        app.add_system_named(Stage::PreUpdate, "room::commands", room_commands);
        app.add_system_named(Stage::Last, "room::ops", room_ops);
    }
}

fn room_commands(ctx: &mut SystemCtx) {
    if !ctx.resources.contains::<RoomRuntime>() || !ctx.resources.contains::<NetServer>() {
        return;
    }
    let world = &*ctx.world;
    ctx.resources.scope::<RoomRuntime, _>(|res, rt| {
        while let Ok(cmd) = rt.commands.try_recv() {
            apply_command(world, res, rt, cmd);
        }
    });
}

fn apply_command(
    world: &space3d_core::World,
    res: &mut space3d_core::Resources,
    rt: &mut RoomRuntime,
    cmd: RoomCommand,
) {
    match cmd {
        RoomCommand::Kick { player, reason } => {
            // Counted in `space3d_disconnects_total{reason="kicked"}` from the room stats.
            res.expect_mut::<NetServer>()
                .kick(player, DisconnectCode::Kicked, &reason, false);
            tracing::info!(event = "kick", room = %rt.spec.room_id, %player, %reason, "player kicked");
        }
        RoomCommand::Broadcast { text, level } => {
            res.expect_mut::<NetServer>()
                .notice(Target::All, Notice::Text { level, text });
        }
        RoomCommand::Close { reason } => {
            rt.closing.get_or_insert(Closing::Close(reason));
        }
        RoomCommand::Shutdown => {
            if !matches!(rt.closing, Some(Closing::Shutdown { .. })) {
                rt.closing = Some(Closing::Shutdown { waited: 0 });
                rt.request_checkpoint();
            }
        }
        RoomCommand::Drain { deadline } => {
            let in_ms = deadline.as_millis().min(u128::from(u32::MAX)) as u32;
            if let Some(ctx) = res.get_mut::<RoomContext>() {
                ctx.drain_deadline_ms = Some(rt.env.unix.now_ms() + i64::from(in_ms));
            }
            let net = res.expect_mut::<NetServer>();
            net.set_joinable(false);
            net.notice(Target::All, Notice::ShuttingDown { in_ms });
            if rt.spec.persistent {
                // Persistent worlds move: checkpoint now, clients rejoin via the player service
                // (which restores the room on another server).
                net.notice(Target::All, Notice::Reconnect);
                rt.request_checkpoint();
            }
            rt.set_phase(RoomPhase::Ending);
        }
        RoomCommand::Checkpoint => rt.request_checkpoint(),
        RoomCommand::SetJoinable(yes) => {
            res.expect_mut::<NetServer>().set_joinable(yes);
            rt.ticks_since_status = u32::MAX / 2;
        }
        RoomCommand::Inspect(reply) => {
            let net = res.expect::<NetServer>();
            let players = net
                .clients()
                .into_iter()
                .map(|c| {
                    let stats = net.stats(c.client).unwrap_or_default();
                    (
                        c.identity.player,
                        c.identity.display_name.clone(),
                        stats.rtt.as_secs_f32() * 1000.0,
                        stats.bytes_out_per_sec,
                        slot_label(c.state).to_owned(),
                    )
                })
                .collect();
            let entities = world.query::<&Replicated>().iter().count() as u32;
            let _ = reply.send(RoomDetail {
                status: rt.status.lock().clone(),
                players,
                entities,
            });
        }
        RoomCommand::PlayerData { player, entries } => {
            res.init::<Events<PlayerDataLoaded>>()
                .send(PlayerDataLoaded { player, entries });
        }
    }
}

fn slot_label(state: SlotState) -> &'static str {
    match state {
        SlotState::Syncing => "syncing",
        SlotState::Connected => "connected",
        SlotState::Disconnected { .. } => "disconnected",
    }
}

fn room_ops(ctx: &mut SystemCtx) {
    if !ctx.resources.contains::<RoomRuntime>() || !ctx.resources.contains::<NetServer>() {
        return;
    }
    ctx.resources.scope::<RoomRuntime, _>(|res, rt| {
        if rt.phase == RoomPhase::Closed {
            return;
        }
        forward_requests(res, rt);
        presence(res, rt);
        rt.poll_upload();
        if rt.env.checkpoints.enabled() {
            rt.ticks_since_checkpoint = rt.ticks_since_checkpoint.saturating_add(1);
            if rt.ticks_since_checkpoint >= rt.checkpoint_every {
                rt.request_checkpoint();
            }
        }
        // Empty-room TTL (the TTL also covers the time for ticket holders to arrive).
        let slots = res.expect::<NetServer>().clients().len();
        if slots == 0 && !rt.spec.persistent {
            rt.empty_ticks = rt.empty_ticks.saturating_add(1);
            if rt.empty_ticks >= rt.empty_ttl_ticks && rt.closing.is_none() {
                rt.closing = Some(Closing::Close("empty".into()));
            }
        } else {
            rt.empty_ticks = 0;
        }
        close_if_requested(res, rt);
        rt.ticks_since_status = rt.ticks_since_status.saturating_add(1);
        if rt.ticks_since_status >= rt.ticks_per_second || rt.phase == RoomPhase::Closed {
            rt.ticks_since_status = 0;
            publish_status(res, rt);
        }
    });
}

fn forward_requests(res: &mut space3d_core::Resources, rt: &mut RoomRuntime) {
    let requests = res.expect_mut::<RoomControl>().take();
    let mut retry = Vec::new();
    for req in requests {
        let pushed = match &req {
            RoomRequest::EndMatch(result) => {
                let msg = FleetUp::MatchResult {
                    op_id: format!("{}:{}:match:{}", rt.env.server_id, result.room_id, result.match_id),
                    result: result.clone(),
                };
                let ok = rt.env.outbox.push(msg).is_ok();
                if ok {
                    res.expect_mut::<NetServer>().notice(Target::All, Notice::MatchEnded);
                    rt.set_phase(RoomPhase::Ending);
                }
                ok
            }
            RoomRequest::WritePlayerData { player, key, value } => {
                let op_id = rt.next_op_id();
                rt.env
                    .outbox
                    .push(FleetUp::PlayerDataWrite {
                        op_id,
                        player: *player,
                        writes: vec![DataWrite {
                            key: key.clone(),
                            value: value.clone(),
                            expected_version: None,
                        }],
                    })
                    .is_ok()
            }
            RoomRequest::SubmitScore { board, player, score } => {
                let op_id = rt.next_op_id();
                rt.env
                    .outbox
                    .push(FleetUp::SubmitScore {
                        op_id,
                        board: board.clone(),
                        player: *player,
                        score: *score,
                    })
                    .is_ok()
            }
            RoomRequest::LoadPlayerData { player, keys } => {
                rt.uplink(FleetUp::FetchPlayerData {
                    request_id: 0,
                    player: *player,
                    keys: keys.clone(),
                });
                true
            }
            RoomRequest::Checkpoint => {
                rt.request_checkpoint();
                true
            }
            RoomRequest::Close(reason) => {
                rt.closing.get_or_insert(Closing::Close(reason.clone()));
                true
            }
        };
        if !pushed {
            retry.push(req);
        }
    }
    if !retry.is_empty() {
        tracing::warn!(room = %rt.spec.room_id, n = retry.len(), "outbox full; retrying next tick");
        res.expect_mut::<RoomControl>().pending.extend(retry);
    }
}

fn presence(res: &mut space3d_core::Resources, rt: &mut RoomRuntime) {
    let mut joined = Vec::new();
    if let Some(events) = res.get::<Events<PlayerJoined>>() {
        joined.extend(rt.joined.read(events).map(|e| e.identity.player));
    }
    let mut left = Vec::new();
    if let Some(events) = res.get::<Events<PlayerLeft>>() {
        left.extend(rt.left.read(events).map(|e| e.player));
    }
    let room_id = rt.spec.room_id;
    for player in joined {
        rt.uplink(FleetUp::PlayerJoined { room_id, player });
        if rt.phase == RoomPhase::Waiting {
            rt.set_phase(RoomPhase::Running);
        }
    }
    for player in left {
        rt.uplink(FleetUp::PlayerLeft { room_id, player });
    }
}

/// Ticks a shutting-down room waits for its final checkpoint upload.
const SHUTDOWN_UPLOAD_WAIT_S: u32 = 15;

fn close_if_requested(res: &mut space3d_core::Resources, rt: &mut RoomRuntime) {
    let Some(closing) = rt.closing.clone() else {
        return;
    };
    let (code, reason, keep_checkpoint) = match closing {
        Closing::Close(reason) => (DisconnectCode::RoomClosed, reason, false),
        Closing::Shutdown { waited } => {
            let busy = rt.want_checkpoint || rt.uploading();
            if busy && waited < SHUTDOWN_UPLOAD_WAIT_S * rt.ticks_per_second {
                rt.closing = Some(Closing::Shutdown { waited: waited + 1 });
                return;
            }
            (DisconnectCode::ServerShutdown, "server shutting down".to_owned(), true)
        }
    };
    res.expect_mut::<NetServer>().disconnect_all(code, &reason, false);
    if !keep_checkpoint && rt.env.checkpoints.enabled() {
        drop(rt.env.checkpoints.delete(rt.spec.room_id));
    }
    rt.uplink(FleetUp::RoomClosed {
        room_id: rt.spec.room_id,
        reason: reason.clone(),
    });
    tracing::info!(room = %rt.spec.room_id, %reason, "room closed");
    rt.phase = RoomPhase::Closed;
}

fn publish_status(res: &mut space3d_core::Resources, rt: &mut RoomRuntime) {
    let net = res.expect::<NetServer>();
    let clients = net.clients();
    let disconnected = clients
        .iter()
        .filter(|c| matches!(c.state, SlotState::Disconnected { .. }))
        .count();
    let stats = net.room_stats().clone();
    let prev = std::mem::replace(&mut rt.prev_stats, stats.clone());
    let m = &rt.env.metrics;
    let reg = &m.registry;
    let delta = |a: u64, b: u64| a.saturating_sub(b);
    reg.counter(
        "space3d_bytes_total",
        "Bytes by direction.",
        &[("dir", "out"), ("channel", "all")],
    )
    .inc_by(delta(stats.bytes_out, prev.bytes_out));
    reg.counter(
        "space3d_bytes_total",
        "Bytes by direction.",
        &[("dir", "in"), ("channel", "all")],
    )
    .inc_by(delta(stats.bytes_in, prev.bytes_in));
    reg.counter("space3d_packets_total", "Packets by direction.", &[("dir", "out")])
        .inc_by(delta(stats.packets_out, prev.packets_out));
    reg.counter("space3d_packets_total", "Packets by direction.", &[("dir", "in")])
        .inc_by(delta(stats.packets_in, prev.packets_in));
    for (result, now, before) in [
        ("ok", stats.inputs_ok, prev.inputs_ok),
        ("late", stats.inputs_late, prev.inputs_late),
        ("missing", stats.inputs_missing, prev.inputs_missing),
        ("invalid", stats.inputs_invalid, prev.inputs_invalid),
    ] {
        reg.counter("space3d_input_total", "Inputs by result.", &[("result", result)])
            .inc_by(delta(now, before));
    }
    reg.counter("space3d_strikes_total", "Protocol strikes.", &[])
        .inc_by(delta(stats.strikes, prev.strikes));
    for code in DisconnectCode::ALL {
        let n = delta(stats.disconnects_with(code), prev.disconnects_with(code));
        if n > 0 {
            m.disconnects(code.label(), n);
        }
    }
    let lost = delta(stats.links_lost, prev.links_lost);
    if lost > 0 {
        m.disconnects("link_lost", lost);
    }
    for (kind, now, before) in [
        ("packets", stats.rate_limited_packets, prev.rate_limited_packets),
        ("messages", stats.rate_limited_messages, prev.rate_limited_messages),
    ] {
        let n = delta(now, before);
        if n > 0 {
            m.rate_limited_by(kind, n);
        }
    }
    m.snapshots_skipped("backlog", delta(stats.snapshots_skipped, prev.snapshots_skipped));
    let sent = delta(stats.snapshots_sent, prev.snapshots_sent);
    if sent > 0 {
        m.snapshot_bytes
            .observe(delta(stats.snapshot_bytes, prev.snapshot_bytes) as f64 / sent as f64);
    }
    for c in &clients {
        if let Some(s) = net.stats(c.client)
            && !s.rtt.is_zero()
        {
            m.rtt_seconds.observe_duration(s.rtt);
        }
    }
    let mut status = rt.status.lock();
    *status = RoomStatus {
        room_id: rt.spec.room_id,
        mode: rt.spec.mode.clone(),
        phase: rt.phase,
        players: (clients.len() - disconnected) as u16,
        max_players: rt.spec.max_players,
        joinable: net.is_joinable() && rt.phase != RoomPhase::Closed,
        tick: net.tick().0,
        tick_p99_us: status.tick_p99_us,
        bytes_out_per_sec: delta(stats.bytes_out, prev.bytes_out),
        created_ms: rt.created_ms,
        disconnected: disconnected as u16,
        player_ids: clients.iter().map(|c| c.identity.player).collect(),
        persistent: rt.spec.persistent,
        abandon_on_loss: rt.env.game.recovery_policy(&rt.spec) == crate::game::RecoveryPolicy::Abandon,
    };
    let _ = rt.epoch;
}

/// Level for admin broadcasts parsed from text (`"warn"` / anything else).
pub fn notice_level(s: &str) -> NoticeLevel {
    if s.eq_ignore_ascii_case("warn") {
        NoticeLevel::Warn
    } else {
        NoticeLevel::Info
    }
}
