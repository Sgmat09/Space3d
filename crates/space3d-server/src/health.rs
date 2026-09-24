//! Liveness and readiness.
//!
//! * `GET /healthz` → 200 while the supervisor ticks and every shard heartbeat is younger than
//!   5 s (a stuck room tick fails liveness → the orchestrator restarts the pod).
//! * `GET /readyz` → 200 only if: not draining, DB ready, registered with the player service
//!   (game-server role), shards alive. 503 otherwise (load balancers stop routing).
//!
//! Served on the admin port and (without details) on the public game/player ports.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

/// Shared health state (clone = shared).
#[derive(Clone, Debug, Default)]
pub struct HealthState {
    inner: Arc<HealthInner>,
}

#[derive(Debug, Default)]
struct HealthInner {
    supervisor_beat_ms: AtomicI64,
    shard_beats_ms: parking_lot::Mutex<Vec<Arc<AtomicI64>>>,
    draining: AtomicBool,
    db_ready: AtomicBool,
    registered: AtomicBool,
}

impl HealthState {
    /// New state (not ready).
    pub fn new() -> Self {
        Self::default()
    }

    /// Heartbeat slot for a new shard thread (store unix ms each loop).
    pub fn shard_heartbeat(&self) -> Arc<AtomicI64> {
        let beat = Arc::new(AtomicI64::new(0));
        self.inner.shard_beats_ms.lock().push(beat.clone());
        beat
    }

    /// Supervisor heartbeat.
    pub fn beat_supervisor(&self, now_ms: i64) {
        self.inner.supervisor_beat_ms.store(now_ms, Ordering::Relaxed);
    }

    /// Mark draining.
    pub fn set_draining(&self, yes: bool) {
        self.inner.draining.store(yes, Ordering::Relaxed);
    }

    /// Mark DB readiness.
    pub fn set_db_ready(&self, yes: bool) {
        self.inner.db_ready.store(yes, Ordering::Relaxed);
    }

    /// Mark fleet registration (game-server role).
    pub fn set_registered(&self, yes: bool) {
        self.inner.registered.store(yes, Ordering::Relaxed);
    }

    /// Liveness at `now_ms`.
    pub fn is_live(&self, now_ms: i64) -> bool {
        todo!()
    }

    /// Readiness at `now_ms` (`needs_registration` for the game-server role).
    pub fn is_ready(&self, now_ms: i64, needs_registration: bool) -> bool {
        todo!()
    }

    /// Axum routes `/healthz` and `/readyz`.
    pub fn router(
        &self,
        clock: Arc<dyn crate::clock::UnixClock>,
        needs_registration: bool,
    ) -> space3d_net::server::axum::Router {
        todo!()
    }
}
