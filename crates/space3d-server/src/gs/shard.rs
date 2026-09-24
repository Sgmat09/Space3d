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
use super::room::RoomBuild;
use crate::health::HealthState;
use crate::metrics::GameServerMetrics;
use space3d_replication::RoomId;
use std::sync::Arc;

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
    pub load: Arc<std::sync::atomic::AtomicU64>,
    /// Thread.
    pub thread: Option<std::thread::JoinHandle<()>>,
}

/// All shards (clone = shared handles).
#[derive(Clone)]
pub struct ShardPool {
    shards: Arc<Vec<ShardHandle>>,
}

impl ShardPool {
    /// Start `n` shard threads (`space3d-shard-N`).
    pub fn start(n: usize, health: &HealthState, metrics: GameServerMetrics) -> Self {
        todo!()
    }

    /// A pool with no threads (tests / player-service role); `place` panics.
    pub fn empty() -> Self {
        Self {
            shards: Arc::new(Vec::new()),
        }
    }

    /// Index of the least-loaded shard.
    pub fn place(&self) -> usize {
        todo!()
    }

    /// Send a command to shard `i`.
    pub fn send(&self, i: usize, cmd: ShardCmd) {
        todo!()
    }

    /// Stop all shards and join them.
    pub fn stop(&self) {
        todo!()
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
