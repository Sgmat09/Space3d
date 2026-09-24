//! Shard threads: each owns a set of room `App`s (built on the shard because `App` is `!Send`)
//! and runs a deadline scheduler:
//!
//! ```text
//! loop {
//!   apply ShardCmd (Create/Remove/Stop) from a crossbeam channel
//!   for each due room: up to max_catchup_ticks × { catch_unwind(app.update(step)); observe tick time }
//!                      still behind → skip (ticks_skipped), next_at = now + step
//!                      panic → phase Crashed, room_crashes += 1, rebuild from last checkpoint
//!                              (epoch + 1; ≤ 3 crashes / 5 min, then Close)
//!   heartbeat (HealthState shard slot)
//!   park until min(next_at) or a command (crossbeam select with deadline)
//! }
//! ```
//!
//! One `App::update(step)` = exactly one fixed tick. `catch_unwind` only works with
//! `panic = "unwind"`: build servers with `cargo build --profile server`. Placement: least EWMA
//! tick cost; rooms never migrate (deferred).

use super::checkpoint::Checkpoint;
use super::room::{RoomBuild, RoomRuntime, build_room_app, panic_message, run_tick_guarded};
use super::rooms::{RoomEnv, RoomPhase, RoomStatus};
use crate::clock::{SystemUnixClock, UnixClock};
use crate::game::RoomSpec;
use crate::gs::fleet::FleetUp;
use crate::health::HealthState;
use crate::metrics::{GameServerMetrics, Gauge};
use parking_lot::Mutex;
use space3d_core::{App, TickRate};
use space3d_replication::RoomId;
use space3d_replication::protocol::handshake::DisconnectCode;
use space3d_replication::server::{NetInbox, NetServer};
use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Command to a shard.
pub enum ShardCmd {
    /// Build and start a room on this shard.
    Create(Box<RoomBuild>),
    /// Drop a room.
    Remove(RoomId),
    /// Stop the thread after removing all rooms.
    Stop,
}

/// Handle to one shard thread.
pub struct ShardHandle {
    /// Command channel.
    pub commands: crossbeam_channel::Sender<ShardCmd>,
    /// EWMA of summed room tick cost (µs per second), for placement.
    pub load: Arc<AtomicU64>,
    /// Rooms hosted.
    pub rooms: Arc<AtomicUsize>,
    /// Thread.
    pub thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// All shards (clone = shared handles).
#[derive(Clone)]
pub struct ShardPool {
    shards: Arc<Vec<ShardHandle>>,
}

/// Crash-loop guard: this many crashes within [`CRASH_WINDOW`] close the room.
pub const MAX_CRASHES: usize = 3;
/// See [`MAX_CRASHES`].
pub const CRASH_WINDOW: Duration = Duration::from_secs(300);

/// Default shard count: `available_parallelism − 1`, at least 1.
pub fn default_shards() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(1))
        .unwrap_or(1)
        .max(1)
}

impl ShardPool {
    /// Start `n` shard threads (`space3d-shard-N`).
    pub fn start(n: usize, health: &HealthState, metrics: GameServerMetrics, max_catchup_ticks: u32) -> Self {
        let shards = (0..n.max(1))
            .map(|i| {
                let (tx, rx) = crossbeam_channel::unbounded();
                let load = Arc::new(AtomicU64::new(0));
                let rooms = Arc::new(AtomicUsize::new(0));
                let beat = health.shard_heartbeat();
                let label = i.to_string();
                let gauge = metrics.registry.gauge(
                    "space3d_shard_load",
                    "Room tick cost per shard (µs/s).",
                    &[("shard", &label)],
                );
                let shard = Shard {
                    rx,
                    load: load.clone(),
                    room_count: rooms.clone(),
                    beat,
                    metrics: metrics.clone(),
                    gauge,
                    max_catchup: max_catchup_ticks.max(1),
                };
                let thread = std::thread::Builder::new()
                    .name(format!("space3d-shard-{i}"))
                    .spawn(move || shard.run())
                    .expect("spawn shard thread");
                ShardHandle {
                    commands: tx,
                    load,
                    rooms,
                    thread: Mutex::new(Some(thread)),
                }
            })
            .collect();
        Self {
            shards: Arc::new(shards),
        }
    }

    /// A pool with no threads (player-service role); `place`/`send` are no-ops.
    pub fn empty() -> Self {
        Self {
            shards: Arc::new(Vec::new()),
        }
    }

    /// Index of the least-loaded shard (ties: fewest rooms, lowest index).
    pub fn place(&self) -> usize {
        self.shards
            .iter()
            .enumerate()
            .min_by_key(|(i, s)| (s.load.load(Ordering::Relaxed), s.rooms.load(Ordering::Relaxed), *i))
            .map_or(0, |(i, _)| i)
    }

    /// Send a command to shard `i`.
    pub fn send(&self, i: usize, cmd: ShardCmd) {
        match self.shards.get(i) {
            Some(s) => {
                if let ShardCmd::Create(_) = &cmd {
                    // Count immediately so back-to-back placements spread out.
                    s.rooms.fetch_add(1, Ordering::Relaxed);
                }
                let _ = s.commands.send(cmd);
            }
            None => tracing::error!(shard = i, "no such shard; room dropped"),
        }
    }

    /// Stop all shards and join them.
    pub fn stop(&self) {
        for s in self.shards.iter() {
            let _ = s.commands.send(ShardCmd::Stop);
        }
        for s in self.shards.iter() {
            if let Some(t) = s.thread.lock().take() {
                let _ = t.join();
            }
        }
    }

    /// Number of shards.
    pub fn len(&self) -> usize {
        self.shards.len()
    }

    /// Whether there are no shards.
    pub fn is_empty(&self) -> bool {
        self.shards.is_empty()
    }
}

struct Hosted {
    id: RoomId,
    app: App,
    step: Duration,
    next_at: Instant,
    status: Arc<Mutex<RoomStatus>>,
    spec: RoomSpec,
    env: RoomEnv,
    created_ms: i64,
    epoch: u32,
    crashes: VecDeque<Instant>,
    tick_us: Vec<u32>,
    window_start: Instant,
    cost_us: f64,
}

struct Shard {
    rx: crossbeam_channel::Receiver<ShardCmd>,
    load: Arc<AtomicU64>,
    room_count: Arc<AtomicUsize>,
    beat: Arc<std::sync::atomic::AtomicI64>,
    metrics: GameServerMetrics,
    gauge: Gauge,
    max_catchup: u32,
}

fn close_status(status: &Arc<Mutex<RoomStatus>>, env: &RoomEnv, id: RoomId, reason: &str) {
    status.lock().phase = RoomPhase::Closed;
    let _ = env.uplink.try_send((
        id,
        FleetUp::RoomClosed {
            room_id: id,
            reason: reason.to_owned(),
        },
    ));
}

fn epoch_of(app: &App) -> u32 {
    app.resources.get::<NetServer>().map_or(1, |n| n.config().room_epoch)
}

/// Rebuild a room whose tick panicked, in place: disconnect its clients (`RoomCrashed`, no
/// resume: they rejoin through the player service), then build a fresh `App` from the room's
/// latest **in-memory** checkpoint with `epoch + 1`, reusing its inbox and command channel.
/// Shared by the shard threads and `TestCluster`. `Err` = the room must be closed (the reason).
pub fn rebuild_crashed(
    app: &mut App,
    spec: &RoomSpec,
    env: &RoomEnv,
    status: &Arc<Mutex<RoomStatus>>,
    created_ms: i64,
) -> Result<(), String> {
    status.lock().phase = RoomPhase::Crashed;
    // Best effort: the crashed World may be inconsistent.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if let Some(net) = app.resources.get_mut::<NetServer>() {
            net.disconnect_all(DisconnectCode::RoomCrashed, "room restarting", false);
        }
    }));
    let epoch = epoch_of(app);
    let inbox = app.resources.remove::<NetInbox>();
    let runtime = app.resources.remove::<RoomRuntime>();
    let (Some(inbox), Some(runtime)) = (inbox, runtime) else {
        return Err("crashed".into());
    };
    let (commands, last): (_, Option<Checkpoint>) = runtime.into_parts();
    let build = RoomBuild {
        spec: spec.clone(),
        env: env.clone(),
        inbox,
        commands,
        status: status.clone(),
        restore: last,
        created_ms,
        epoch: Some(epoch.wrapping_add(1)),
    };
    match catch_unwind(AssertUnwindSafe(|| build_room_app(build))) {
        Ok(Ok(fresh)) => {
            let old = std::mem::replace(app, fresh);
            let _ = catch_unwind(AssertUnwindSafe(move || drop(old)));
            status.lock().phase = RoomPhase::Running;
            tracing::warn!(room = %spec.room_id, epoch = epoch_of(app), "room rebuilt after crash");
            Ok(())
        }
        Ok(Err(e)) => {
            tracing::error!(room = %spec.room_id, error = %e, "room rebuild failed");
            Err("rebuild failed".into())
        }
        Err(p) => {
            tracing::error!(room = %spec.room_id, panic = %panic_message(p), "room rebuild panicked");
            Err("rebuild panicked".into())
        }
    }
}

/// Build a hosted room (game code may panic: contained).
fn host(build: RoomBuild) -> Option<Hosted> {
    let (id, status, spec, env, created_ms) = (
        build.spec.room_id,
        build.status.clone(),
        build.spec.clone(),
        build.env.clone(),
        build.created_ms,
    );
    match catch_unwind(AssertUnwindSafe(|| build_room_app(build))) {
        Ok(Ok(app)) => {
            let step = app.resources.expect::<TickRate>().step();
            Some(Hosted {
                id,
                epoch: epoch_of(&app),
                app,
                step,
                next_at: Instant::now(),
                status,
                spec,
                env,
                created_ms,
                crashes: VecDeque::new(),
                tick_us: Vec::with_capacity(128),
                window_start: Instant::now(),
                cost_us: 0.0,
            })
        }
        Ok(Err(e)) => {
            tracing::error!(room = %id, error = %e, "room build failed");
            close_status(&status, &env, id, "build failed");
            None
        }
        Err(p) => {
            tracing::error!(room = %id, panic = %panic_message(p), "room build panicked");
            close_status(&status, &env, id, "build panicked");
            None
        }
    }
}

impl Hosted {
    /// A tick panicked: rebuild from the last in-memory checkpoint (epoch + 1) unless the room
    /// is crash-looping. Returns `false` when the room is gone.
    fn recover(&mut self, metrics: &GameServerMetrics, why: &str) -> bool {
        metrics.room_crashes.inc();
        tracing::error!(room = %self.id, panic = %why, "room tick panicked");
        self.status.lock().phase = RoomPhase::Crashed;
        let now = Instant::now();
        self.crashes.push_back(now);
        while self
            .crashes
            .front()
            .is_some_and(|t| now.duration_since(*t) > CRASH_WINDOW)
        {
            self.crashes.pop_front();
        }
        if self.crashes.len() >= MAX_CRASHES {
            tracing::error!(room = %self.id, "room crash loop; closing");
            let _ = catch_unwind(AssertUnwindSafe(|| {
                if let Some(net) = self.app.resources.get_mut::<NetServer>() {
                    net.disconnect_all(DisconnectCode::RoomCrashed, "room closed", false);
                }
            }));
            close_status(&self.status, &self.env, self.id, "crash loop");
            return false;
        }
        match rebuild_crashed(&mut self.app, &self.spec, &self.env, &self.status, self.created_ms) {
            Ok(()) => {
                self.epoch = epoch_of(&self.app);
                true
            }
            Err(why) => {
                close_status(&self.status, &self.env, self.id, &why);
                false
            }
        }
    }

    fn closed(&self) -> bool {
        self.status.lock().phase == RoomPhase::Closed
    }

    fn observe(&mut self, us: u32, metrics: &GameServerMetrics) {
        metrics.tick_seconds.observe(f64::from(us) / 1e6);
        self.tick_us.push(us);
        self.cost_us = 0.9 * self.cost_us + 0.1 * f64::from(us);
        let now = Instant::now();
        if now.duration_since(self.window_start) >= Duration::from_secs(1) {
            self.window_start = now;
            let mut v = std::mem::take(&mut self.tick_us);
            v.sort_unstable();
            let p99 = v
                .get(((v.len() as f64 * 0.99).ceil() as usize).saturating_sub(1))
                .copied()
                .unwrap_or(0);
            self.status.lock().tick_p99_us = p99;
        }
    }
}

impl Shard {
    fn run(self) {
        // Rooms are `!Send`: they only ever exist on this thread.
        let mut rooms: Vec<Hosted> = Vec::new();
        loop {
            let mut stop = false;
            while let Ok(cmd) = self.rx.try_recv() {
                stop |= self.apply(&mut rooms, cmd);
            }
            if stop {
                break;
            }
            self.tick_due(&mut rooms);
            self.beat.store(SystemUnixClock.now_ms(), Ordering::Relaxed);
            let now = Instant::now();
            let wait = rooms
                .iter()
                .map(|r| r.next_at.saturating_duration_since(now))
                .min()
                .unwrap_or(Duration::from_millis(250))
                .min(Duration::from_millis(250));
            match self.rx.recv_timeout(wait) {
                Ok(cmd) => {
                    if self.apply(&mut rooms, cmd) {
                        break;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
        for room in rooms.drain(..) {
            let _ = catch_unwind(AssertUnwindSafe(move || drop(room)));
        }
        self.room_count.store(0, Ordering::Relaxed);
        self.beat.store(i64::MAX, Ordering::Relaxed);
    }

    /// Returns `true` on `Stop`.
    fn apply(&self, rooms: &mut Vec<Hosted>, cmd: ShardCmd) -> bool {
        match cmd {
            ShardCmd::Create(build) => {
                if let Some(h) = host(*build) {
                    rooms.push(h);
                }
                self.room_count.store(rooms.len(), Ordering::Relaxed);
                false
            }
            ShardCmd::Remove(id) => {
                rooms.retain(|r| r.id != id);
                self.room_count.store(rooms.len(), Ordering::Relaxed);
                false
            }
            ShardCmd::Stop => true,
        }
    }

    fn tick_due(&self, rooms: &mut Vec<Hosted>) {
        let metrics = &self.metrics;
        let mut i = 0;
        while i < rooms.len() {
            let room = &mut rooms[i];
            let mut rounds = 0;
            let mut alive = true;
            while Instant::now() >= room.next_at && rounds < self.max_catchup {
                let started = Instant::now();
                let result = run_tick_guarded(&mut room.app, room.step);
                let us = started.elapsed().as_micros().min(u128::from(u32::MAX)) as u32;
                room.next_at += room.step;
                rounds += 1;
                match result {
                    Ok(()) => room.observe(us, metrics),
                    Err(why) => {
                        alive = room.recover(metrics, &why);
                        break;
                    }
                }
                if room.closed() {
                    alive = false;
                    break;
                }
            }
            let now = Instant::now();
            if alive && now > room.next_at {
                // Still behind after max_catchup rounds: skip the backlog.
                let behind = now.duration_since(room.next_at);
                let skipped = (behind.as_nanos() / room.step.as_nanos().max(1)) as u64;
                if skipped > 0 {
                    metrics.ticks_skipped.inc_by(skipped);
                    room.next_at = now + room.step;
                }
            }
            if alive {
                i += 1;
            } else {
                let room = rooms.swap_remove(i);
                tracing::debug!(room = %room.id, "room removed from shard");
                let _ = catch_unwind(AssertUnwindSafe(move || drop(room)));
            }
        }
        self.room_count.store(rooms.len(), Ordering::Relaxed);
        let load: f64 = rooms.iter().map(|r| r.cost_us / r.step.as_secs_f64().max(1e-6)).sum();
        self.load.store(load as u64, Ordering::Relaxed);
        self.gauge.set(load);
    }
}
