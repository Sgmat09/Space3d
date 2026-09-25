//! Room directory (`RoomManager`, clone = shared): the only way the supervisor, ingress,
//! admin API and fleet link reach rooms. Never touches a room `World`: it sends
//! [`RoomCommand`]s (bounded channel, applied by `RoomPlugin` in PreUpdate) and reads the
//! [`RoomStatus`] each room publishes at 1 Hz.
//!
//! Creating a room is non-blocking: the inbox exists immediately (ingress can queue joins at
//! once); the room's latest checkpoint is looked up asynchronously (crash / server-loss
//! recovery: a room id that has a recent checkpoint is restored from it) and the build is then
//! handed to the least-loaded shard — or, in manual mode (`TestCluster`), queued for the
//! caller to build and step on its own thread.

use super::checkpoint::{Checkpoint, CheckpointStore};
use super::outbox::OutboxSender;
use super::room::RoomBuild;
use super::shard::{ShardCmd, ShardPool};
use crate::clock::UnixClock;
use crate::game::{GameDefinition, RecoveryPolicy, RoomSpec};
use crate::gs::fleet::FleetUp;
use crate::metrics::GameServerMetrics;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use space3d_replication::api::PlayerDataEntry;
use space3d_replication::protocol::control::NoticeLevel;
use space3d_replication::server::{RoomSender, ServerNetConfig, room_channel};
use space3d_replication::time::NetClockHandle;
use space3d_replication::{PlayerId, RoomId};
use space3d_tasks::Task;
use std::sync::Arc;
use std::time::Duration;

/// Room lifecycle phase (published in `RoomStatus`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoomPhase {
    /// Created, waiting for players.
    #[default]
    Waiting,
    /// Playing.
    Running,
    /// Match over / draining; no new joins.
    Ending,
    /// Closed (about to be removed).
    Closed,
    /// Panicked; being restored from its checkpoint.
    Crashed,
}

impl RoomPhase {
    /// Metric label.
    pub fn label(self) -> &'static str {
        match self {
            RoomPhase::Waiting => "waiting",
            RoomPhase::Running => "running",
            RoomPhase::Ending => "ending",
            RoomPhase::Closed => "closed",
            RoomPhase::Crashed => "crashed",
        }
    }
}

/// Commands to a room (applied on its shard thread in PreUpdate).
#[derive(Debug)]
pub enum RoomCommand {
    /// Kick a player.
    Kick {
        /// Player.
        player: PlayerId,
        /// Reason.
        reason: String,
    },
    /// Broadcast a notice.
    Broadcast {
        /// Text.
        text: String,
        /// Level.
        level: NoticeLevel,
    },
    /// Close the room (disconnect everyone, results flushed, checkpoint deleted, remove).
    Close {
        /// Reason.
        reason: String,
    },
    /// Server drain: warn clients; match rooms end by `deadline`, persistent rooms checkpoint
    /// now and ask clients to reconnect through the player service.
    Drain {
        /// Time left.
        deadline: Duration,
    },
    /// Server shutdown (drain deadline reached): final checkpoint (kept for restore
    /// elsewhere), `Disconnect(ServerShutdown)`, remove.
    Shutdown,
    /// Another server hosts the live copy of this room (`FleetDown::RoomSuperseded`): stop
    /// uploading checkpoints, `Disconnect(ServerShutdown)` ("rejoin"), keep the stored
    /// checkpoint, remove.
    Superseded,
    /// The player service recorded that this room is moving (its answer to
    /// `FleetUp::RoomMoving`, `FleetDown::RoomMovingAck`): it now holds the players' rejoins
    /// until the room is stored, so the room may send its clients away.
    MoveAcked,
    /// Checkpoint now.
    Checkpoint,
    /// Accept/refuse new joins.
    SetJoinable(bool),
    /// Request a detailed snapshot for the admin API.
    Inspect(futures::channel::oneshot::Sender<RoomDetail>),
    /// Answer to `RoomControl::load_player_data` (→ `Events<PlayerDataLoaded>`).
    PlayerData {
        /// Player.
        player: PlayerId,
        /// Entries.
        entries: Vec<PlayerDataEntry>,
    },
}

/// Room summary, published at 1 Hz (admin API, fleet heartbeat, metrics).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RoomStatus {
    /// Room.
    pub room_id: RoomId,
    /// Mode.
    pub mode: String,
    /// Phase.
    pub phase: RoomPhase,
    /// Connected players (syncing + connected).
    pub players: u16,
    /// Capacity.
    pub max_players: u16,
    /// Accepting joins.
    pub joinable: bool,
    /// Current tick.
    pub tick: u32,
    /// Tick time p99 over the last second (µs).
    pub tick_p99_us: u32,
    /// Downstream bytes/s.
    pub bytes_out_per_sec: u64,
    /// Created (unix ms).
    pub created_ms: i64,
    /// Slots whose link dropped (resumable).
    #[serde(default)]
    pub disconnected: u16,
    /// Players holding a slot (presence for rejoin; never used as a metrics label).
    #[serde(default)]
    pub player_ids: Vec<PlayerId>,
    /// Persistent world room.
    #[serde(default)]
    pub persistent: bool,
    /// `RecoveryPolicy::Abandon`: if the hosting server is lost, the player service voids the
    /// room instead of restoring it (rejoin answers 404, players return to matchmaking).
    #[serde(default)]
    pub abandon_on_loss: bool,
    /// How this copy of the room started (fresh, or restored from which point). After a room
    /// moved to another server, the player service drops the outbox ops of the copy it left
    /// behind that were made after this point (`ps::fleet`).
    #[serde(default)]
    pub origin: RoomOrigin,
    /// The room moves to another server when its server drains (a persistent room with
    /// checkpoints enabled): once the server reports draining, the player service holds its
    /// players' rejoins until the room is stored, instead of sending them back to it.
    #[serde(default)]
    pub moves_on_drain: bool,
}

/// A point in a room's history: its epoch (bumped by every restore and crash rebuild), then
/// its tick. Ordered by epoch, then tick.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RoomTick {
    /// Room epoch.
    pub epoch: u32,
    /// Tick.
    pub tick: u32,
}

/// How a room copy started (see [`RoomStatus::origin`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoomOrigin {
    /// Not built yet (checkpoint lookup in flight).
    #[default]
    Unknown,
    /// Built without a checkpoint.
    Fresh,
    /// Restored from the checkpoint captured at this point.
    Restored(RoomTick),
}

/// Detailed room view (admin `GET /admin/v1/rooms/{id}`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RoomDetail {
    /// Summary.
    pub status: RoomStatus,
    /// `(player, name, rtt_ms, down_bytes_per_sec, state)` per slot.
    pub players: Vec<(PlayerId, String, f32, u32, String)>,
    /// Replicated entities.
    pub entities: u32,
}

/// Why a room could not be created.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RoomError {
    /// `max_rooms` / `max_players` reached.
    #[error("room capacity reached")]
    Capacity,
    /// Draining.
    #[error("server draining")]
    Draining,
    /// Spec rejected by the game.
    #[error("invalid spec: {0}")]
    InvalidSpec(String),
    /// The room is gone.
    #[error("room gone")]
    Gone,
    /// Building the room failed (game panic, restore error).
    #[error("room build failed: {0}")]
    Build(String),
}

/// Handle to one room.
#[derive(Clone)]
pub struct RoomHandle {
    /// Id.
    pub id: RoomId,
    /// Spec it was created from.
    pub spec: RoomSpec,
    /// Inbox (ingress → room).
    pub sender: RoomSender,
    /// Commands.
    pub commands: crossbeam_channel::Sender<RoomCommand>,
    /// Latest published status.
    pub status: Arc<Mutex<RoomStatus>>,
}

impl RoomHandle {
    /// Send a command without blocking.
    pub fn command(&self, cmd: RoomCommand) -> Result<(), RoomError> {
        self.commands.try_send(cmd).map_err(|_| RoomError::Gone)
    }

    /// Latest status.
    pub fn status(&self) -> RoomStatus {
        self.status.lock().clone()
    }
}

/// Messages from rooms to the fleet link (presence, room lifecycle, player-data fetches).
pub type Uplink = async_channel::Sender<(RoomId, FleetUp)>;

/// Everything a room needs from the process (clone = shared).
#[derive(Clone)]
pub struct RoomEnv {
    /// The game.
    pub game: Arc<dyn GameDefinition>,
    /// Base replication config (per room: `GameDefinition::room_net_config`).
    pub net: ServerNetConfig,
    /// Tick rate.
    pub tick_hz: f64,
    /// Durable outbox (results, player data, scores).
    pub outbox: OutboxSender,
    /// Presence / lifecycle / fetches → supervisor → fleet link.
    pub uplink: Uplink,
    /// Checkpoint store.
    pub checkpoints: Arc<dyn CheckpointStore>,
    /// Local store on the game server's own database for a forced stop (second signal): every
    /// room is captured synchronously and written there, then uploaded to `checkpoints` on the
    /// next start (`gs::checkpoint::upload_rescued`). `None` disables it.
    pub rescue: Option<Arc<dyn CheckpointStore>>,
    /// Metrics.
    pub metrics: GameServerMetrics,
    /// Server id.
    pub server_id: String,
    /// Checkpoint cadence.
    pub checkpoint_interval: Duration,
    /// Close empty non-persistent rooms after this.
    pub empty_room_ttl: Duration,
    /// Checkpoints older than this are not restored.
    pub recovery_max_age: Duration,
    /// Monotonic clock (replication).
    pub clock: NetClockHandle,
    /// Wall clock.
    pub unix: Arc<dyn UnixClock>,
    /// Room inbox capacity.
    pub inbox_capacity: usize,
    /// Deterministic resume secrets (tests); `None` = OS CSPRNG.
    pub secret_seed: Option<u64>,
    /// Account bans (rooms remove banned players, including ones that resume).
    pub bans: Option<crate::security::bans::BanList>,
}

/// A room build waiting for its checkpoint lookup (manual mode).
pub struct PendingBuild {
    /// The build (its `restore` is filled in by [`PendingBuild::poll`]).
    pub build: RoomBuild,
    /// Checkpoint lookup in flight.
    pub lookup: Option<Task<Option<Checkpoint>>>,
}

impl PendingBuild {
    /// Non-blocking: the build once the lookup finished.
    pub fn poll(&mut self) -> bool {
        match &mut self.lookup {
            None => true,
            Some(t) => match t.state() {
                space3d_tasks::TaskState::Pending => false,
                space3d_tasks::TaskState::Ready => {
                    let restore = t.poll().flatten();
                    if self.build.restore.is_none() {
                        self.build.restore = restore;
                    }
                    self.lookup = None;
                    true
                }
                space3d_tasks::TaskState::Cancelled => {
                    self.lookup = None;
                    true
                }
            },
        }
    }
}

/// How rooms get onto threads.
#[derive(Clone)]
pub enum Placement {
    /// Shard threads (production).
    Shards(ShardPool),
    /// Builds are queued for the caller (`TestCluster` steps rooms itself).
    Manual(Arc<Mutex<Vec<PendingBuild>>>),
}

/// Room directory (clone = shared).
#[derive(Clone)]
pub struct RoomManager {
    inner: Arc<Mutex<Directory>>,
    env: RoomEnv,
    placement: Placement,
}

struct Directory {
    rooms: std::collections::BTreeMap<RoomId, RoomHandle>,
    draining: bool,
    max_rooms: usize,
    max_players: usize,
}

/// Decide whether a stored checkpoint restores `spec`'s room.
pub fn restore_decision(env: &RoomEnv, spec: &RoomSpec, bytes: Option<Vec<u8>>) -> Option<Checkpoint> {
    let bytes = bytes?;
    let cp = match Checkpoint::from_bytes(&bytes) {
        Ok(cp) => cp,
        Err(e) => {
            tracing::warn!(room = %spec.room_id, error = %e, "unreadable checkpoint; starting fresh");
            return None;
        }
    };
    let age = env.unix.now_ms().saturating_sub(cp.saved_ms);
    if cp.room_id != spec.room_id || age > env.recovery_max_age.as_millis() as i64 {
        tracing::info!(room = %spec.room_id, age_ms = age, "checkpoint too old or foreign; starting fresh");
        return None;
    }
    if env.game.recovery_policy(spec) == RecoveryPolicy::Abandon {
        // The player service voids such rooms on rejoin; a room created anyway (e.g. a stale
        // ticket) never resurrects the abandoned state.
        tracing::info!(room = %spec.room_id, "recovery policy Abandon; not restoring");
        return None;
    }
    Some(cp)
}

/// Look up a room's checkpoint off the calling thread.
pub fn lookup_checkpoint(env: &RoomEnv, spec: &RoomSpec) -> Option<Task<Option<Checkpoint>>> {
    if !env.checkpoints.enabled() {
        return None;
    }
    let get = env.checkpoints.get(spec.room_id);
    let env2 = env.clone();
    let spec = spec.clone();
    Some(space3d_tasks::spawn(async move {
        match get.wait().await {
            Some(Ok(bytes)) => restore_decision(&env2, &spec, bytes),
            Some(Err(e)) => {
                tracing::warn!(room = %spec.room_id, error = %e, "checkpoint lookup failed; starting fresh");
                None
            }
            None => None,
        }
    }))
}

impl RoomManager {
    /// Directory over a placement.
    pub fn new(placement: Placement, env: RoomEnv, max_rooms: usize, max_players: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Directory {
                rooms: Default::default(),
                draining: false,
                max_rooms,
                max_players,
            })),
            env,
            placement,
        }
    }

    /// The shared room environment.
    pub fn env(&self) -> &RoomEnv {
        &self.env
    }

    /// Existing, not-closed room.
    pub fn get(&self, id: RoomId) -> Option<RoomHandle> {
        self.inner
            .lock()
            .rooms
            .get(&id)
            .filter(|h| h.status.lock().phase != RoomPhase::Closed)
            .cloned()
    }

    /// Existing room, or create it from `spec` (non-blocking: the inbox exists immediately; the
    /// `App` is built on the least-loaded shard after the checkpoint lookup).
    pub fn get_or_create(&self, spec: &RoomSpec) -> Result<RoomHandle, RoomError> {
        if let Some(h) = self.get(spec.room_id) {
            return Ok(h);
        }
        self.create_inner(spec.clone(), None, true)
    }

    /// Create a room, optionally restoring a checkpoint (no lookup when one is given).
    pub fn create(&self, spec: RoomSpec, restore: Option<Checkpoint>) -> Result<RoomHandle, RoomError> {
        let lookup = restore.is_none();
        self.create_inner(spec, restore, lookup)
    }

    fn create_inner(&self, spec: RoomSpec, restore: Option<Checkpoint>, lookup: bool) -> Result<RoomHandle, RoomError> {
        self.env.game.validate_spec(&spec).map_err(RoomError::InvalidSpec)?;
        if spec.room_id == RoomId(0) {
            return Err(RoomError::InvalidSpec("room id 0".into()));
        }
        let now_ms = self.env.unix.now_ms();
        let (handle, build) = {
            let mut dir = self.inner.lock();
            if let Some(h) = dir.rooms.get(&spec.room_id)
                && h.status.lock().phase != RoomPhase::Closed
            {
                return Ok(h.clone());
            }
            if dir.draining {
                return Err(RoomError::Draining);
            }
            let open = dir
                .rooms
                .values()
                .filter(|h| h.status.lock().phase != RoomPhase::Closed)
                .count();
            let players: usize = dir.rooms.values().map(|h| usize::from(h.status.lock().players)).sum();
            if open >= dir.max_rooms || players >= dir.max_players {
                return Err(RoomError::Capacity);
            }
            let (sender, inbox) = room_channel(self.env.inbox_capacity);
            let (cmd_tx, cmd_rx) = crossbeam_channel::bounded(256);
            let status = Arc::new(Mutex::new(RoomStatus {
                room_id: spec.room_id,
                mode: spec.mode.clone(),
                phase: RoomPhase::Waiting,
                max_players: spec.max_players,
                joinable: true,
                created_ms: now_ms,
                persistent: spec.persistent,
                abandon_on_loss: self.env.game.recovery_policy(&spec) == crate::game::RecoveryPolicy::Abandon,
                moves_on_drain: spec.persistent && self.env.checkpoints.enabled(),
                ..Default::default()
            }));
            let handle = RoomHandle {
                id: spec.room_id,
                spec: spec.clone(),
                sender,
                commands: cmd_tx,
                status: status.clone(),
            };
            dir.rooms.insert(spec.room_id, handle.clone());
            let build = RoomBuild {
                spec: spec.clone(),
                env: self.env.clone(),
                inbox,
                commands: cmd_rx,
                status,
                restore,
                created_ms: now_ms,
                epoch: None,
            };
            (handle, build)
        };
        let _ = self.env.uplink.try_send((
            spec.room_id,
            FleetUp::RoomOpened {
                room_id: spec.room_id,
                spec: spec.clone(),
            },
        ));
        tracing::info!(room = %spec.room_id, mode = %spec.mode, "room created");
        let lookup = if lookup {
            lookup_checkpoint(&self.env, &spec)
        } else {
            None
        };
        match &self.placement {
            Placement::Manual(queue) => queue.lock().push(PendingBuild { build, lookup }),
            Placement::Shards(pool) => {
                let pool = pool.clone();
                match lookup {
                    None => pool.send(pool.place(), ShardCmd::Create(Box::new(build))),
                    Some(task) => {
                        let mut build = build;
                        space3d_tasks::runtime().spawn(async move {
                            if let Some(restore) = task.wait().await.flatten() {
                                build.restore.get_or_insert(restore);
                            }
                            pool.send(pool.place(), ShardCmd::Create(Box::new(build)));
                        });
                    }
                }
            }
        }
        Ok(handle)
    }

    /// Builds queued in manual mode (drained).
    pub fn take_pending_builds(&self) -> Vec<PendingBuild> {
        match &self.placement {
            Placement::Manual(q) => std::mem::take(&mut *q.lock()),
            Placement::Shards(_) => Vec::new(),
        }
    }

    /// All room statuses (closed rooms excluded).
    pub fn list(&self) -> Vec<RoomStatus> {
        self.inner
            .lock()
            .rooms
            .values()
            .map(|h| h.status.lock().clone())
            .filter(|s| s.phase != RoomPhase::Closed)
            .collect()
    }

    /// All handles.
    pub fn handles(&self) -> Vec<RoomHandle> {
        self.inner.lock().rooms.values().cloned().collect()
    }

    /// A room hosting `player` (per the 1 Hz status), if any. A player may hold slots in
    /// several rooms: see [`Self::find_player_rooms`].
    pub fn find_player(&self, player: PlayerId) -> Option<RoomId> {
        self.find_player_rooms(player).into_iter().next()
    }

    /// Every room hosting `player` (per the 1 Hz status).
    pub fn find_player_rooms(&self, player: PlayerId) -> Vec<RoomId> {
        self.inner
            .lock()
            .rooms
            .values()
            .filter(|h| {
                let s = h.status.lock();
                s.phase != RoomPhase::Closed && s.player_ids.contains(&player)
            })
            .map(|h| h.id)
            .collect()
    }

    /// Kick `player` from every room of this server. Sent to all rooms (not just the ones the
    /// 1 Hz status lists) so a slot taken in the last second is not missed; rooms without the
    /// player ignore it.
    pub fn kick_everywhere(&self, player: PlayerId, reason: &str) {
        self.broadcast(|| RoomCommand::Kick {
            player,
            reason: reason.to_owned(),
        });
    }

    /// Send a command to every room.
    pub fn broadcast(&self, make: impl Fn() -> RoomCommand) {
        for h in self.handles() {
            let _ = h.command(make());
        }
    }

    /// Stop creating rooms.
    pub fn set_draining(&self, yes: bool) {
        self.inner.lock().draining = yes;
    }

    /// Whether draining.
    pub fn is_draining(&self) -> bool {
        self.inner.lock().draining
    }

    /// Remove a room from the directory.
    pub fn remove(&self, id: RoomId) {
        self.inner.lock().rooms.remove(&id);
    }

    /// Drop rooms that reported `Closed`; returns their ids.
    pub fn sweep(&self) -> Vec<RoomId> {
        let mut dir = self.inner.lock();
        let closed: Vec<RoomId> = dir
            .rooms
            .values()
            .filter(|h| h.status.lock().phase == RoomPhase::Closed)
            .map(|h| h.id)
            .collect();
        for id in &closed {
            dir.rooms.remove(id);
        }
        closed
    }

    /// Number of rooms (not closed).
    pub fn len(&self) -> usize {
        self.list().len()
    }

    /// Whether no room is open.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
