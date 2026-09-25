//! Liveness and readiness.
//!
//! * `GET /healthz` → 200 while the supervisor ticks and every shard heartbeat is younger than
//!   5 s (a stuck room tick fails liveness → the orchestrator restarts the pod).
//! * `GET /readyz` → 200 only if: not draining, DB ready, registered with the player service
//!   once (game-server role; a later player-service outage keeps the server ready: rooms keep
//!   running and resumes keep working), shards alive. 503 otherwise (load balancers stop
//!   routing).
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
    fleet_connected: AtomicBool,
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

    /// Mark fleet registration (game-server role). A latch for readiness: once registered, a
    /// later player-service outage does not make the server unready (rooms keep running and
    /// resumes keep working; see the failure matrix). The current link state is
    /// [`Self::set_fleet_connected`].
    pub fn set_registered(&self, yes: bool) {
        self.inner.registered.store(yes, Ordering::Relaxed);
        if yes {
            self.set_fleet_connected(true);
        }
    }

    /// Whether the fleet link is currently up (reported by admin status and a metric, not by
    /// `/readyz`).
    pub fn set_fleet_connected(&self, yes: bool) {
        self.inner.fleet_connected.store(yes, Ordering::Relaxed);
    }

    /// See [`Self::set_fleet_connected`].
    pub fn is_fleet_connected(&self) -> bool {
        self.inner.fleet_connected.load(Ordering::Relaxed)
    }

    /// Liveness at `now_ms`: the supervisor beat and every running shard's beat are younger
    /// than [`LIVENESS_WINDOW_MS`]. A shard that exited cleanly stores `i64::MAX` (ignored).
    pub fn is_live(&self, now_ms: i64) -> bool {
        let fresh = |beat: i64| beat == i64::MAX || (beat > 0 && now_ms.saturating_sub(beat) < LIVENESS_WINDOW_MS);
        fresh(self.inner.supervisor_beat_ms.load(Ordering::Relaxed))
            && self
                .inner
                .shard_beats_ms
                .lock()
                .iter()
                .all(|b| fresh(b.load(Ordering::Relaxed)))
    }

    /// Readiness at `now_ms` (`needs_registration` for the game-server role).
    pub fn is_ready(&self, now_ms: i64, needs_registration: bool) -> bool {
        self.is_live(now_ms)
            && !self.is_draining()
            && self.inner.db_ready.load(Ordering::Relaxed)
            && (!needs_registration || self.inner.registered.load(Ordering::Relaxed))
    }

    /// Whether a drain started.
    pub fn is_draining(&self) -> bool {
        self.inner.draining.load(Ordering::Relaxed)
    }

    /// Whether the DB is ready.
    pub fn is_db_ready(&self) -> bool {
        self.inner.db_ready.load(Ordering::Relaxed)
    }

    /// Whether the game server is registered with the player service.
    pub fn is_registered(&self) -> bool {
        self.inner.registered.load(Ordering::Relaxed)
    }

    /// Axum routes `/healthz` and `/readyz`.
    pub fn router(
        &self,
        clock: Arc<dyn crate::clock::UnixClock>,
        needs_registration: bool,
    ) -> space3d_net::server::axum::Router {
        use space3d_net::server::axum::{Router, http::StatusCode, routing::get};
        let live = self.clone();
        let live_clock = clock.clone();
        let ready = self.clone();
        Router::new()
            .route(
                "/healthz",
                get(move || {
                    let ok = live.is_live(live_clock.now_ms());
                    async move {
                        if ok {
                            (StatusCode::OK, "ok")
                        } else {
                            (StatusCode::SERVICE_UNAVAILABLE, "unhealthy")
                        }
                    }
                }),
            )
            .route(
                "/readyz",
                get(move || {
                    let ok = ready.is_ready(clock.now_ms(), needs_registration);
                    async move {
                        if ok {
                            (StatusCode::OK, "ready")
                        } else {
                            (StatusCode::SERVICE_UNAVAILABLE, "not ready")
                        }
                    }
                }),
            )
    }
}

/// `http://host:port` for a bind address (unspecified → loopback).
pub fn local_url(addr: std::net::SocketAddr) -> String {
    let mut a = addr;
    if a.ip().is_unspecified() {
        a.set_ip(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    }
    format!("http://{a}")
}

/// `healthcheck` subcommand (distroless images have no curl): GET `{base}/healthz` (or
/// `/readyz`); `true` on 200. Blocks: CLI only.
pub fn probe(base: &str, ready: bool) -> bool {
    let url = format!(
        "{}/{}",
        base.trim_end_matches('/'),
        if ready { "readyz" } else { "healthz" }
    );
    let http = space3d_net::Http::new();
    let req = space3d_net::HttpRequest::get(url.clone()).timeout(std::time::Duration::from_secs(3));
    match http.request(req).block() {
        Some(Ok(res)) if res.status == 200 => true,
        Some(Ok(res)) => {
            eprintln!("{url}: {} {}", res.status, res.text());
            false
        }
        Some(Err(e)) => {
            eprintln!("{url}: {e}");
            false
        }
        None => false,
    }
}

/// Heartbeats older than this fail liveness.
pub const LIVENESS_WINDOW_MS: i64 = 5_000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn liveness_and_readiness() {
        let h = HealthState::new();
        assert!(!h.is_live(1_000));
        h.beat_supervisor(1_000);
        assert!(h.is_live(1_000));
        assert!(!h.is_ready(1_000, false), "db not ready");
        h.set_db_ready(true);
        assert!(h.is_ready(1_000, false));
        assert!(!h.is_ready(1_000, true), "not registered");
        h.set_registered(true);
        assert!(h.is_ready(1_000, true));
        let shard = h.shard_heartbeat();
        assert!(!h.is_live(1_000), "shard never beat");
        shard.store(900, Ordering::Relaxed);
        assert!(h.is_live(1_000));
        assert!(!h.is_live(6_000), "stale heartbeats fail liveness");
        h.beat_supervisor(6_000);
        assert!(!h.is_live(6_000), "stuck shard");
        shard.store(i64::MAX, Ordering::Relaxed);
        assert!(h.is_live(6_000), "stopped shard is ignored");
        h.set_draining(true);
        assert!(!h.is_ready(6_000, true));
    }
}
