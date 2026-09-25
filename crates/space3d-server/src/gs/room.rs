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

use super::checkpoint::{Checkpoint, StoreError};
use super::rooms::{RoomCommand, RoomDetail, RoomEnv, RoomError, RoomOrigin, RoomPhase, RoomStatus, RoomTick};
use crate::game::RoomSpec;
use crate::gs::fleet::{DataWrite, FleetUp, OpOrigin};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use space3d_core::{App, EventReader, Events, Stage, SystemCtx};
use space3d_replication::protocol::control::{Notice, NoticeLevel};
use space3d_replication::protocol::handshake::{DisconnectCode, Reject, RejectCode};
use space3d_replication::server::handshake::SecretSource;
use space3d_replication::server::{
    NetInbox, NetServer, PlayerJoined, PlayerLeft, PlayerResumed, ServerReplicationPlugin, SlotState, full_state,
};
use space3d_replication::stats::RoomNetStats;
use space3d_replication::{MatchId, PlayerId, Replicated, RoomId, Target};
use space3d_tasks::{Task, TaskState};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
            rt.origin = RoomOrigin::Restored(RoomTick {
                epoch: cp.state.epoch,
                tick: cp.state.tick.0,
            });
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
    let saved_ms = rt.next_capture_ms();
    let cp = Checkpoint::new(rt.spec.clone(), state, server_only, None, saved_ms);
    rt.env.metrics.checkpoint_seconds.observe_duration(started.elapsed());
    let rt = app.resources.expect_mut::<RoomRuntime>();
    if let Handoff::Frozen { capture: freeze @ None } = &mut rt.handoff {
        // The first capture after the freeze is the one the handoff carries over.
        *freeze = Some(saved_ms);
    }
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
    if let Some(cp) = rt.freeze_capture() {
        // Frozen for a handoff: the freeze capture holds everything the players saw (a new
        // capture could miss their pawns, removed when they left).
        tracing::warn!(room = %cp.room_id, tick = cp.state.tick.0, "forced stop: handoff checkpoint kept locally");
        return Some(store.put(cp.room_id, cp.to_bytes()));
    }
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
    let cp = Checkpoint::new(rt.spec.clone(), state, server_only, None, rt.next_capture_ms());
    tracing::warn!(room = %cp.room_id, tick = cp.state.tick.0, "forced stop: room checkpointed locally");
    Some(store.put(cp.room_id, cp.to_bytes()))
}

/// Graceful stop (the shard removes a room that is still hosted when the process stops): a
/// room frozen for a handoff whose freeze capture never reached the checkpoint store (store
/// slow or unreachable for the whole stop window) writes that capture to the local rescue
/// store, which the next start uploads (`gs::checkpoint::upload_rescued`), instead of dropping
/// it. Returns the pending write (the caller waits for it, bounded), `None` when there is
/// nothing to keep.
pub fn stop_hosted(app: &mut App) -> Option<Task<Result<(), StoreError>>> {
    let rt = app.resources.get::<RoomRuntime>()?;
    if !rt.handoff_pending() {
        return None;
    }
    if rt.handoff == Handoff::Freeze {
        // Still waiting to freeze (the move was never acknowledged): capture it now, locally.
        return emergency_stop(app);
    }
    let store = rt.env.rescue.clone()?;
    let cp = rt.freeze_capture()?;
    tracing::error!(
        room = %cp.room_id,
        tick = cp.state.tick.0,
        "handoff incomplete at stop: the checkpoint store never confirmed the freeze capture; kept in the local rescue store"
    );
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

/// Drain handoff of a persistent room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Handoff {
    /// Not handing off.
    None,
    /// Drain received: freeze ([`freeze_for_handoff`]) at the end of the first tick after the
    /// player service acknowledged the move ([`MoveNotice`]).
    Freeze,
    /// Frozen (clients gone): the capture taken right after the freeze tick is the room's
    /// final state; the room closes once exactly that capture is stored.
    Frozen {
        /// Capture time (`saved_ms`) of the freeze capture; `None` until it is taken. Capture
        /// times of a room strictly increase ([`RoomRuntime::next_capture_ms`]), so a stored
        /// capture time at or past it can only be the freeze capture itself — an earlier
        /// capture taken in the same unix millisecond (catch-up ticks) never counts.
        capture: Option<i64>,
    },
}

/// Telling the player service that the room is about to move (`FleetUp::RoomMoving`) before
/// its clients are sent away: a drain handoff freezes the room, a shutdown closes it
/// restorable, and either way the clients rejoin through the player service within
/// milliseconds. The player service must already hold those rejoins
/// (`ps::routes::wait_for_handoff`), or it sends them straight back to the room they just
/// left, whose server refuses them (`Reject(ServerDraining)`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum MoveNotice {
    /// Not moving.
    #[default]
    None,
    /// `RoomMoving` sent; the clients stay until `RoomCommand::MoveAcked` arrives, for at most
    /// [`MOVE_ACK_WAIT`] (ticks waited so far).
    Sent {
        /// Ticks waited.
        waited: u32,
    },
    /// Acknowledged (or given up waiting): the clients may go.
    Done,
}

/// Longest a moving room keeps its clients while waiting for the player service to
/// acknowledge the move (a lost or slow fleet link must not hold the drain).
pub const MOVE_ACK_WAIT: Duration = Duration::from_secs(2);

/// Retry delay a room that moves on drain gives the newcomers it refuses (see
/// [`moving_refusal`]).
pub const MOVING_RETRY_AFTER_MS: u32 = 1_000;

/// How a draining room that moves to another server refuses new players: transient
/// (`ServerDraining` with a retry delay), so they ask the player service again.
pub fn moving_refusal() -> Reject {
    Reject {
        code: RejectCode::ServerDraining,
        message: "room is moving to another server; ask the player service again".into(),
        retry_after_ms: Some(MOVING_RETRY_AFTER_MS),
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Closing {
    Close(String),
    Shutdown {
        waited: u32,
    },
    /// A draining persistent room whose drain checkpoint reached the store: close it and let
    /// the player service restore it elsewhere when its players rejoin.
    Handoff,
    /// The room was restored on another server while this server's lease had lapsed (fleet
    /// partition): this copy is stale. Its checkpoints are refused (older epoch) and the stored
    /// one belongs to the live copy, so nothing is uploaded or deleted.
    Superseded,
}

/// A checkpoint upload in flight.
pub type Upload = Task<Result<(), StoreError>>;

/// Delete `room`'s stored checkpoint once `uploads` (uploads already in flight, of this copy
/// and of the incarnations a crash rebuild replaced) settled — a later landing would resurrect
/// the row. Only a checkpoint of epoch `through_epoch` or older goes: a newer one belongs to a
/// copy restored elsewhere (a stale copy left running behind a fleet partition that closes
/// normally must not delete the live copy's checkpoint).
pub fn delete_checkpoint_after(env: &RoomEnv, room: RoomId, through_epoch: u32, uploads: Vec<Upload>) {
    if !env.checkpoints.enabled() {
        return;
    }
    let store = env.checkpoints.clone();
    drop(space3d_tasks::spawn(async move {
        for upload in uploads {
            let _ = upload.wait().await;
        }
        if let Some(Err(e)) = store.delete(room, through_epoch).wait().await {
            tracing::warn!(%room, error = %e, "checkpoint delete failed");
        }
    }));
}

/// The room is closed for good outside a normal close (crash loop): stop its uploads and
/// delete its stored checkpoint (after every upload in flight settled, including those its
/// earlier incarnations started before they crashed). Otherwise the row stays until garbage
/// collection, and a join ticket that is still valid recreates the crash-looping room from it.
pub fn discard_checkpoint(app: &mut App) {
    let Some(rt) = app.resources.get_mut::<RoomRuntime>() else {
        return;
    };
    rt.retired.store(true, Ordering::Release);
    rt.want_checkpoint = false;
    rt.queued_upload = None;
    let uploads = rt.take_uploads();
    delete_checkpoint_after(&rt.env, rt.spec.room_id, rt.epoch, uploads);
}

/// What a crash rebuild carries from the crashed incarnation of a room into the rebuilt one
/// ([`take_for_rebuild`] → [`adopt_carried`]): its uploads still in flight, and where it was in
/// its drain (phase, joinability, drain deadline, handoff, move notice, pending close,
/// confirmed capture). A panic while a persistent room was frozen for its drain handoff used to
/// rebuild it as a fresh, joinable room that never finished the handoff — the player service
/// kept it "moving" and every rejoin failed.
pub struct Carried {
    uploads: Vec<Upload>,
    phase: RoomPhase,
    join_refusal: Option<Reject>,
    drain_deadline_ms: Option<i64>,
    handoff: Handoff,
    move_notice: MoveNotice,
    closing: Option<Closing>,
    uploaded_ms: Option<i64>,
}

impl Carried {
    /// The rebuild failed: the room is gone for good; delete its stored checkpoint once the
    /// carried uploads settled.
    pub fn discard(self, env: &RoomEnv, room: RoomId, through_epoch: u32) {
        delete_checkpoint_after(env, room, through_epoch, self.uploads);
    }
}

/// A crashed room's parts for its rebuild: the command receiver, its latest in-memory
/// checkpoint and what the rebuilt room carries over ([`Carried`]). Removes the `RoomRuntime`.
/// A queued upload is abandoned (the rebuilt room, epoch + 1, uploads its own; stores refuse
/// older epochs anyway).
pub fn take_for_rebuild(
    app: &mut App,
) -> Option<(crossbeam_channel::Receiver<RoomCommand>, Option<Checkpoint>, Carried)> {
    let join_refusal = app.resources.get::<NetServer>().and_then(|n| n.join_refusal().cloned());
    let drain_deadline_ms = app.resources.get::<RoomContext>().and_then(|c| c.drain_deadline_ms);
    let mut rt = app.resources.remove::<RoomRuntime>()?;
    let uploads = rt.take_uploads();
    let carried = Carried {
        uploads,
        phase: rt.phase,
        join_refusal,
        drain_deadline_ms,
        handoff: rt.handoff,
        move_notice: rt.move_notice,
        closing: rt.closing.take(),
        uploaded_ms: rt.uploaded_ms,
    };
    Some((rt.commands.clone(), rt.last_checkpoint.take(), carried))
}

/// Continue the crashed incarnation's state in its rebuilt room (see [`Carried`]). A room
/// frozen for its handoff stays frozen on exactly its freeze capture (the room was rebuilt from
/// it) and uploads it again; a room frozen before its freeze capture was taken freezes again
/// at once (its clients are gone already).
pub fn adopt_carried(app: &mut App, carried: Carried) {
    let Carried {
        uploads,
        phase,
        join_refusal,
        drain_deadline_ms,
        handoff,
        move_notice,
        closing,
        uploaded_ms,
    } = carried;
    if let Some(ctx) = app.resources.get_mut::<RoomContext>() {
        ctx.drain_deadline_ms = drain_deadline_ms;
    }
    if let Some(refusal) = join_refusal
        && let Some(net) = app.resources.get_mut::<NetServer>()
    {
        net.refuse_joins(refusal);
    }
    let Some(rt) = app.resources.get_mut::<RoomRuntime>() else {
        return;
    };
    let restored_from = rt.last_checkpoint.as_ref().map(|cp| cp.saved_ms);
    rt.earlier_uploads = uploads;
    if matches!(phase, RoomPhase::Running | RoomPhase::Ending) {
        rt.phase = phase;
    }
    rt.closing = closing;
    rt.uploaded_ms = uploaded_ms;
    rt.move_notice = move_notice;
    rt.handoff = match handoff {
        Handoff::Frozen { capture: Some(at) } if restored_from == Some(at) => Handoff::Frozen { capture: Some(at) },
        Handoff::Frozen { .. } => {
            rt.move_notice = MoveNotice::Done;
            Handoff::Freeze
        }
        other => other,
    };
    if rt.handoff != Handoff::None {
        tracing::warn!(room = %rt.spec.room_id, handoff = ?rt.handoff, "room rebuilt during its drain handoff; continuing it");
    }
}

/// Resource: the room's operational state (internal; inserted by [`build_room_app`]).
pub struct RoomRuntime {
    env: RoomEnv,
    spec: RoomSpec,
    commands: crossbeam_channel::Receiver<RoomCommand>,
    status: Arc<Mutex<RoomStatus>>,
    created_ms: i64,
    epoch: u32,
    /// How this copy started (reported in the status: the player service fences stale copies).
    origin: RoomOrigin,
    nonce: u64,
    op_counter: u64,
    phase: RoomPhase,
    closing: Option<Closing>,
    want_checkpoint: bool,
    last_checkpoint: Option<Checkpoint>,
    upload: Option<Upload>,
    queued_upload: Option<Checkpoint>,
    /// Set when this runtime is dropped (room removed, crash rebuild, server killed): an
    /// upload that has not reached the store yet is abandoned.
    retired: Arc<AtomicBool>,
    /// Capture time of the checkpoint being uploaded.
    uploading_ms: Option<i64>,
    /// Capture time of the newest checkpoint the store confirmed.
    uploaded_ms: Option<i64>,
    /// Drain handoff of a persistent room (see [`Handoff`]).
    handoff: Handoff,
    /// Announcing a move to the player service (see [`MoveNotice`]).
    move_notice: MoveNotice,
    /// [`MOVE_ACK_WAIT`] in ticks.
    move_ack_wait_ticks: u32,
    /// Uploads that earlier incarnations of this room (replaced by crash rebuilds) had in
    /// flight: deleting the stored checkpoint waits for them too.
    earlier_uploads: Vec<Upload>,
    /// Ticks until a failed handoff upload is retried.
    handoff_retry_in: u32,
    resumed: EventReader,
    bans_seen: u64,
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
            origin: RoomOrigin::Fresh,
            nonce: u64::from_le_bytes(crate::security::random_bytes::<8>()),
            op_counter: 0,
            phase: RoomPhase::Waiting,
            closing: None,
            want_checkpoint: false,
            last_checkpoint: None,
            upload: None,
            queued_upload: None,
            retired: Arc::new(AtomicBool::new(false)),
            uploading_ms: None,
            uploaded_ms: None,
            handoff: Handoff::None,
            move_notice: MoveNotice::None,
            move_ack_wait_ticks: ticks(MOVE_ACK_WAIT),
            earlier_uploads: Vec::new(),
            handoff_retry_in: 0,
            resumed: EventReader::default(),
            bans_seen: u64::MAX,
            ticks_since_checkpoint: 0,
            ticks_since_status: u32::MAX / 2,
            empty_ticks: 0,
            prev_stats: RoomNetStats::default(),
            joined: EventReader::default(),
            left: EventReader::default(),
        }
    }

    /// Every upload still in flight: this copy's and its earlier incarnations' (finished ones
    /// are dropped). A queued upload is abandoned.
    fn take_uploads(&mut self) -> Vec<Upload> {
        self.queued_upload = None;
        let mut uploads = std::mem::take(&mut self.earlier_uploads);
        uploads.extend(self.upload.take());
        uploads.retain_mut(|t| t.state() == TaskState::Pending);
        uploads
    }

    /// Tell the player service the room is about to move (once); the clients stay until it
    /// acknowledged ([`MoveNotice`]).
    fn announce_move(&mut self) {
        if self.move_notice == MoveNotice::None {
            self.uplink(FleetUp::RoomMoving {
                room_id: self.spec.room_id,
            });
            self.move_notice = MoveNotice::Sent { waited: 0 };
        }
    }

    /// Still waiting for the player service to acknowledge the move.
    fn awaiting_move_ack(&self) -> bool {
        matches!(self.move_notice, MoveNotice::Sent { .. })
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
        // A frozen room keeps exactly its freeze capture (later state was never observed, and
        // the game may already have removed the departed players' pawns); a superseded copy's
        // checkpoints are refused anyway.
        let frozen = matches!(self.handoff, Handoff::Frozen { .. });
        if self.env.checkpoints.enabled() && !frozen && !matches!(self.closing, Some(Closing::Superseded)) {
            self.want_checkpoint = true;
        }
    }

    /// Handing off and the freeze capture is not stored yet (not taken yet while the room waits
    /// for the player service to acknowledge the move).
    fn handoff_pending(&self) -> bool {
        match self.handoff {
            Handoff::None => false,
            Handoff::Freeze => true,
            Handoff::Frozen { capture } => capture.is_none_or(|at| self.uploaded_ms.is_none_or(|saved| saved < at)),
        }
    }

    /// The freeze capture of a room frozen for a handoff (once taken).
    fn freeze_capture(&self) -> Option<Checkpoint> {
        let Handoff::Frozen { capture: Some(at) } = self.handoff else {
            return None;
        };
        self.last_checkpoint.clone().filter(|cp| cp.saved_ms == at)
    }

    /// Capture time for a new checkpoint: the wall clock, but strictly after the previous
    /// capture of this room. Catch-up ticks run back to back, so two captures can fall in the
    /// same unix millisecond; they must still differ (the handoff tells its freeze capture
    /// from an earlier one by capture time, and stores keep the greater key).
    fn next_capture_ms(&self) -> i64 {
        let now = self.env.unix.now_ms();
        match &self.last_checkpoint {
            Some(cp) => now.max(cp.saved_ms.saturating_add(1)),
            None => now,
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
        let retired = self.retired.clone();
        let room = cp.room_id;
        self.uploading_ms = Some(cp.saved_ms);
        self.upload = Some(space3d_tasks::spawn(async move {
            let bytes = space3d_tasks::spawn_compute(move || cp.to_bytes())
                .wait()
                .await
                .ok_or_else(|| StoreError("serialization task lost".into()))?;
            metrics.checkpoint_bytes.observe(bytes.len() as f64);
            if retired.load(Ordering::Acquire) {
                return Err(StoreError("room retired before the upload".into()));
            }
            store
                .put(room, bytes)
                .wait()
                .await
                .unwrap_or_else(|| Err(StoreError("store task lost".into())))
        }));
    }

    fn poll_upload(&mut self) {
        self.earlier_uploads.retain_mut(|t| t.state() == TaskState::Pending);
        let Some(task) = &mut self.upload else {
            return;
        };
        let ok = match task.state() {
            TaskState::Pending => return,
            TaskState::Ready => match task.poll() {
                Some(Err(e)) => {
                    tracing::warn!(room = %self.spec.room_id, error = %e, "checkpoint upload failed");
                    false
                }
                _ => true,
            },
            TaskState::Cancelled => {
                tracing::warn!(room = %self.spec.room_id, "checkpoint upload cancelled");
                false
            }
        };
        let saved = self.uploading_ms.take();
        if ok {
            self.uploaded_ms = self.uploaded_ms.max(saved);
        } else if self.handoff_pending() && self.queued_upload.is_none() {
            // The handoff needs its freeze capture stored: upload it again in a second.
            self.handoff_retry_in = self.ticks_per_second;
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

impl Drop for RoomRuntime {
    fn drop(&mut self) {
        self.retired.store(true, Ordering::Release);
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
            // Sent to every room of the server (a player may hold slots in several rooms).
            // Counted in `space3d_disconnects_total{reason="kicked"}` from the room stats.
            let net = res.expect_mut::<NetServer>();
            if net.clients().iter().any(|c| c.identity.player == player) {
                net.kick(player, DisconnectCode::Kicked, &reason, false);
                tracing::info!(event = "kick", room = %rt.spec.room_id, %player, %reason, "player kicked");
            }
        }
        RoomCommand::Broadcast { text, level } => {
            res.expect_mut::<NetServer>()
                .notice(Target::All, Notice::Text { level, text });
        }
        RoomCommand::Close { reason } => {
            rt.closing.get_or_insert(Closing::Close(reason));
        }
        RoomCommand::Shutdown => {
            // A superseded copy closes as such (nothing uploaded; the live copy owns the room).
            if !matches!(rt.closing, Some(Closing::Shutdown { .. } | Closing::Superseded)) {
                rt.closing = Some(Closing::Shutdown { waited: 0 });
                rt.request_checkpoint();
                if rt.env.checkpoints.enabled() {
                    // It closes restorable: its players' rejoin restores it elsewhere.
                    rt.announce_move();
                }
            }
        }
        RoomCommand::MoveAcked => {
            if rt.awaiting_move_ack() {
                rt.move_notice = MoveNotice::Done;
            }
        }
        RoomCommand::Drain { deadline } => {
            let in_ms = deadline.as_millis().min(u128::from(u32::MAX)) as u32;
            if let Some(ctx) = res.get_mut::<RoomContext>() {
                ctx.drain_deadline_ms = Some(rt.env.unix.now_ms() + i64::from(in_ms));
            }
            let net = res.expect_mut::<NetServer>();
            let moves = rt.spec.persistent && rt.env.checkpoints.enabled();
            if moves {
                // A ticket the player service issued before it heard of the drain: the player
                // is to ask again (the player service holds the request until the room moved,
                // then places them in it) — `RoomClosed` would read as "gone for good", and
                // they would open a second copy of the world elsewhere.
                net.refuse_joins(moving_refusal());
            } else {
                net.set_joinable(false);
            }
            net.notice(Target::All, Notice::ShuttingDown { in_ms });
            if moves && rt.handoff == Handoff::None {
                // Persistent worlds move: once the player service acknowledged the move, the
                // room freezes (see `freeze_for_handoff`) and closes when its freeze capture is
                // stored; its players' rejoin then restores it on another server from exactly
                // that state.
                net.notice(Target::All, Notice::Reconnect);
                rt.handoff = Handoff::Freeze;
                rt.announce_move();
            }
            rt.set_phase(RoomPhase::Ending);
        }
        RoomCommand::Superseded => {
            rt.want_checkpoint = false;
            rt.queued_upload = None;
            rt.closing = Some(Closing::Superseded);
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
        let arrived = presence(res, rt);
        enforce_bans(res, rt, arrived);
        if let MoveNotice::Sent { waited } = rt.move_notice {
            if waited >= rt.move_ack_wait_ticks {
                tracing::warn!(room = %rt.spec.room_id, "the player service did not acknowledge the move in time; sending the clients away anyway");
                rt.move_notice = MoveNotice::Done;
            } else {
                rt.move_notice = MoveNotice::Sent { waited: waited + 1 };
            }
        }
        if rt.handoff == Handoff::Freeze && !rt.awaiting_move_ack() {
            freeze_for_handoff(res, rt);
        }
        rt.poll_upload();
        if matches!(rt.handoff, Handoff::Frozen { .. })
            && rt.handoff_pending()
            && !rt.uploading()
            && !rt.want_checkpoint
        {
            // Retry the freeze capture itself (a new capture would miss the players' pawns
            // once the game handled their departure) — for as long as the process runs: the
            // room is not reported restorable before that capture is stored.
            rt.handoff_retry_in = rt.handoff_retry_in.saturating_sub(1);
            if rt.handoff_retry_in == 0 {
                match rt.freeze_capture() {
                    Some(cp) => rt.start_upload(cp),
                    // The freeze capture itself failed: take it again.
                    None => rt.want_checkpoint = true,
                }
            }
        }
        if rt.closing.is_none() && matches!(rt.handoff, Handoff::Frozen { .. }) && !rt.handoff_pending() {
            rt.closing = Some(Closing::Handoff);
        }
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
    if requests.is_empty() {
        return;
    }
    // This tick's point in the room's history: a checkpoint captured after this tick includes
    // whatever led to these requests.
    let origin = Some(OpOrigin {
        room_id: rt.spec.room_id,
        at: RoomTick {
            epoch: rt.epoch,
            tick: res.expect::<NetServer>().tick().0,
        },
    });
    let mut retry = Vec::new();
    for req in requests {
        let pushed = match &req {
            RoomRequest::EndMatch(result) => {
                // One result per match, whichever server reports it (a room restored elsewhere
                // may end the same match again; the player service applies the first).
                let msg = FleetUp::MatchResult {
                    op_id: format!("{}:match:{}", result.room_id, result.match_id),
                    result: result.clone(),
                    origin,
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
                        origin,
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
                        origin,
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
            RoomRequest::Close(reason) if rt.handoff != Handoff::None => {
                // Game code closing a room whose players the handoff just moved out (e.g. "last
                // player left") would delete the checkpoint that carries the world over.
                tracing::debug!(room = %rt.spec.room_id, %reason, "close request ignored during a handoff");
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

/// Freeze a draining persistent room for its handoff (end of the first tick after the player
/// service acknowledged the move, after this tick's snapshot went out): its clients are
/// disconnected (`Disconnect(ServerShutdown)`, "rejoin"), so from here on nobody can change or
/// observe it, and the capture taken right after this tick holds everything its players saw.
/// The player service already holds their rejoins (it acknowledged `FleetUp::RoomMoving`) until
/// that capture is stored and the room closes (`RoomClosed { restorable }`). Keeping the room
/// playable during the upload lost every change made in that window: the restore used the
/// earlier capture.
fn freeze_for_handoff(res: &mut space3d_core::Resources, rt: &mut RoomRuntime) {
    res.expect_mut::<NetServer>().disconnect_all(
        DisconnectCode::ServerShutdown,
        "room moved to another server; rejoin",
        false,
    );
    rt.want_checkpoint = true;
    rt.queued_upload = None;
    rt.handoff = Handoff::Frozen { capture: None };
    tracing::info!(room = %rt.spec.room_id, "room frozen for a handoff; storing its final checkpoint");
}

/// Players who joined or resumed this tick (the ban check looks at them).
fn presence(res: &mut space3d_core::Resources, rt: &mut RoomRuntime) -> Vec<PlayerId> {
    let mut joined = Vec::new();
    if let Some(events) = res.get::<Events<PlayerJoined>>() {
        joined.extend(rt.joined.read(events).map(|e| e.identity.player));
    }
    let mut arrived = joined.clone();
    if let Some(events) = res.get::<Events<PlayerResumed>>() {
        arrived.extend(rt.resumed.read(events).map(|e| e.player));
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
    // A frozen room's players are in transit to its next server, not leaving it.
    if rt.handoff == Handoff::None {
        for player in left {
            rt.uplink(FleetUp::PlayerLeft { room_id, player });
        }
    }
    arrived
}

/// Account bans inside the room: players who (re)joined this tick — tickets are checked by the
/// gatekeeper, but resumes and supersedes are verified by the room alone — and, whenever the
/// ban list changed, everyone holding a slot (a ban reaches every room the player is in).
fn enforce_bans(res: &mut space3d_core::Resources, rt: &mut RoomRuntime, arrived: Vec<PlayerId>) {
    let Some(bans) = rt.env.bans.clone() else {
        return;
    };
    let generation = bans.generation();
    let net = res.expect_mut::<NetServer>();
    let check: Vec<PlayerId> = if generation != rt.bans_seen {
        rt.bans_seen = generation;
        net.clients().iter().map(|c| c.identity.player).collect()
    } else {
        arrived
    };
    if check.is_empty() {
        return;
    }
    let now = rt.env.unix.now_ms();
    for player in check {
        if let Some(ban) = bans.player(player, now) {
            net.kick(player, DisconnectCode::Banned, &ban.reason, false);
            tracing::info!(event = "ban_enforced", room = %rt.spec.room_id, %player, "banned player removed from room");
        }
    }
}

/// Seconds a shutting-down room waits for its final checkpoint upload.
const SHUTDOWN_UPLOAD_WAIT_S: u32 = 15;

fn close_if_requested(res: &mut space3d_core::Resources, rt: &mut RoomRuntime) {
    let Some(closing) = rt.closing.clone() else {
        return;
    };
    // `keep_checkpoint`: the stored checkpoint stays and the player service keeps the room
    // restorable (players' rejoin restores it on another server).
    let (code, reason, keep_checkpoint) = match closing {
        Closing::Close(reason) => (DisconnectCode::RoomClosed, reason, false),
        Closing::Shutdown { .. } if rt.handoff_pending() => {
            // A frozen room closes only once its freeze capture is stored: reporting it
            // restorable earlier would let a rejoin restore an older checkpoint (everything
            // since lost). It keeps retrying until then; if the process stops first, the
            // capture goes to the local rescue store ([`stop_hosted`]) and the room counts as
            // lost with its server.
            return;
        }
        Closing::Shutdown { waited } => {
            let busy = rt.want_checkpoint || rt.uploading();
            // The clients go once the player service holds their rejoins (bounded by
            // `MOVE_ACK_WAIT`), and the final capture is stored (bounded).
            if (busy && waited < SHUTDOWN_UPLOAD_WAIT_S * rt.ticks_per_second) || rt.awaiting_move_ack() {
                rt.closing = Some(Closing::Shutdown { waited: waited + 1 });
                return;
            }
            if busy {
                rescue_unstored(rt);
            }
            (DisconnectCode::ServerShutdown, "server shutting down".to_owned(), true)
        }
        Closing::Handoff | Closing::Superseded => (
            DisconnectCode::ServerShutdown,
            "room moved to another server; rejoin".to_owned(),
            true,
        ),
    };
    res.expect_mut::<NetServer>().disconnect_all(code, &reason, false);
    if !keep_checkpoint && rt.env.checkpoints.enabled() {
        // Delete after the uploads in flight settle (a later upload would resurrect the row),
        // and only this copy's checkpoint (or an older one).
        let uploads = rt.take_uploads();
        delete_checkpoint_after(&rt.env, rt.spec.room_id, rt.epoch, uploads);
    }
    rt.uplink(FleetUp::RoomClosed {
        room_id: rt.spec.room_id,
        reason: reason.clone(),
        restorable: keep_checkpoint && rt.env.checkpoints.enabled(),
    });
    tracing::info!(room = %rt.spec.room_id, %reason, "room closed");
    rt.phase = RoomPhase::Closed;
}

/// A shutting-down room gave up waiting for its final upload: keep its newest capture in the
/// local rescue store (uploaded on the next start) rather than only in memory.
fn rescue_unstored(rt: &RoomRuntime) {
    let (Some(store), Some(cp)) = (rt.env.rescue.clone(), rt.last_checkpoint.as_ref()) else {
        return;
    };
    if rt.uploaded_ms.is_some_and(|saved| saved >= cp.saved_ms) {
        return;
    }
    tracing::warn!(room = %cp.room_id, tick = cp.state.tick.0, "final checkpoint not confirmed by the store; kept in the local rescue store");
    let (room, cp) = (cp.room_id, cp.clone());
    drop(space3d_tasks::spawn(async move {
        let Some(bytes) = space3d_tasks::spawn_compute(move || cp.to_bytes()).wait().await else {
            return;
        };
        if let Some(Err(e)) = store.put(room, bytes).wait().await {
            tracing::error!(%room, error = %e, "rescue checkpoint write failed");
        }
    }));
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
        origin: rt.origin,
        moves_on_drain: rt.spec.persistent && rt.env.checkpoints.enabled(),
    };
}

/// Level for admin broadcasts parsed from text (`"warn"` / anything else).
pub fn notice_level(s: &str) -> NoticeLevel {
    if s.eq_ignore_ascii_case("warn") {
        NoticeLevel::Warn
    } else {
        NoticeLevel::Info
    }
}
