//! The accept loop of [`super::HttpServer`]: hyper connections with a header-read timeout
//! (also closes idle keep-alive connections) and caps on concurrent connections overall and per
//! peer, applied at accept time — before a single request byte is parsed, so a client that
//! opens sockets and never finishes a request cannot pile them up.
//!
//! A connection counts until it closes or is upgraded (a WebSocket's socket is then governed by
//! `WsServerConfig::max_connections` and the application's own per-client limits).

use axum::Router;
use axum::extract::ConnectInfo;
use hyper::body::Incoming;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Maps a peer address to the key its per-peer connection cap is counted under (e.g. the IPv6
/// /64), or `None` to exempt it (a trusted reverse proxy carrying many clients).
pub type PeerKey = Arc<dyn Fn(IpAddr) -> Option<IpAddr> + Send + Sync>;

/// Connection-level protections of an [`super::HttpServer`].
#[derive(Clone)]
pub struct ConnectionLimits {
    /// Longest wait for a complete request head, from the moment the server starts reading one
    /// (a new connection, or an idle keep-alive connection between requests). Slow or silent
    /// clients are disconnected. Default 15 s.
    pub header_read_timeout: Duration,
    /// Concurrent connections, all peers (`0` = unlimited). Default 16 384.
    pub max_connections: usize,
    /// Concurrent connections per peer key (`0` = unlimited, the default).
    pub max_per_peer: usize,
    /// Peer → cap key (default: the peer IP itself).
    pub peer_key: PeerKey,
}

impl Default for ConnectionLimits {
    fn default() -> Self {
        Self {
            header_read_timeout: Duration::from_secs(15),
            max_connections: 16_384,
            max_per_peer: 0,
            peer_key: Arc::new(Option::<IpAddr>::Some),
        }
    }
}

impl std::fmt::Debug for ConnectionLimits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionLimits")
            .field("header_read_timeout", &self.header_read_timeout)
            .field("max_connections", &self.max_connections)
            .field("max_per_peer", &self.max_per_peer)
            .finish_non_exhaustive()
    }
}

/// Live connection counters of a running server (clone = shared).
#[derive(Clone, Debug, Default)]
pub struct ConnectionStats {
    inner: Arc<StatsInner>,
}

#[derive(Debug, Default)]
struct StatsInner {
    open: AtomicUsize,
    refused: AtomicU64,
}

impl ConnectionStats {
    /// Connections currently counted (accepted, not yet closed or upgraded).
    pub fn open(&self) -> usize {
        self.inner.open.load(Ordering::Relaxed)
    }

    /// Connections refused at accept time by a cap so far.
    pub fn refused(&self) -> u64 {
        self.inner.refused.load(Ordering::Relaxed)
    }
}

struct Tracker {
    limits: ConnectionLimits,
    per_peer: Mutex<HashMap<IpAddr, usize>>,
    stats: ConnectionStats,
}

/// A counted connection; releases its slot on drop.
struct Slot {
    tracker: Arc<Tracker>,
    key: Option<IpAddr>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.tracker.stats.inner.open.fetch_sub(1, Ordering::AcqRel);
        if let Some(key) = self.key {
            let mut map = self.tracker.per_peer.lock();
            if let Some(n) = map.get_mut(&key) {
                *n -= 1;
                if *n == 0 {
                    map.remove(&key);
                }
            }
        }
    }
}

impl Tracker {
    fn admit(self: &Arc<Self>, peer: IpAddr) -> Option<Slot> {
        let l = &self.limits;
        let open = &self.stats.inner.open;
        if l.max_connections > 0 && open.load(Ordering::Acquire) >= l.max_connections {
            return None;
        }
        let key = if l.max_per_peer > 0 { (l.peer_key)(peer) } else { None };
        if let Some(key) = key {
            let mut map = self.per_peer.lock();
            let n = map.entry(key).or_insert(0);
            if *n >= l.max_per_peer {
                return None;
            }
            *n += 1;
        }
        open.fetch_add(1, Ordering::AcqRel);
        Some(Slot {
            tracker: self.clone(),
            key,
        })
    }
}

/// Serve `router` on `listener` until `token` is cancelled, then finish open connections
/// gracefully.
pub(super) async fn serve(
    listener: tokio::net::TcpListener,
    router: Router,
    token: CancellationToken,
    limits: ConnectionLimits,
    stats: ConnectionStats,
) -> std::io::Result<()> {
    let tracker = Arc::new(Tracker {
        limits,
        per_peer: Mutex::new(HashMap::new()),
        stats,
    });
    // Every connection task holds a sender; `recv` yields `None` once all of them ended.
    let (alive_tx, mut alive_rx) = tokio::sync::mpsc::channel::<()>(1);
    loop {
        let (tcp, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(a) => a,
                Err(e) => {
                    // EMFILE / ECONNABORTED and friends: back off briefly, keep serving.
                    tracing::debug!(error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue;
                }
            },
            _ = token.cancelled() => break,
        };
        let Some(slot) = tracker.admit(peer.ip()) else {
            tracker.stats.inner.refused.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(%peer, "connection refused: connection cap");
            drop(tcp);
            continue;
        };
        let _ = tcp.set_nodelay(true);
        let router = router.clone();
        let token = token.clone();
        let alive = alive_tx.clone();
        let header_timeout = tracker.limits.header_read_timeout;
        let span = tracing::Span::current();
        tokio::spawn(tracing::Instrument::instrument(
            async move {
                let svc = tower::ServiceExt::map_request(router, move |mut req: hyper::Request<Incoming>| {
                    req.extensions_mut().insert(ConnectInfo(peer));
                    req
                });
                // HTTP/1.1 only (WebSocket upgrades, JSON APIs, files): no protocol sniffing that
                // would wait for a first byte outside the header-read timeout.
                let mut builder = hyper::server::conn::http1::Builder::new();
                builder.timer(TokioTimer::new()).header_read_timeout(header_timeout);
                let conn = builder
                    .serve_connection(TokioIo::new(tcp), TowerToHyperService::new(svc))
                    .with_upgrades();
                tokio::pin!(conn);
                tokio::select! {
                    r = conn.as_mut() => {
                        if let Err(e) = r {
                            tracing::trace!(%peer, error = %e, "connection ended");
                        }
                    }
                    _ = token.cancelled() => {
                        conn.as_mut().graceful_shutdown();
                        let _ = conn.await;
                    }
                }
                drop(slot);
                drop(alive);
            },
            span,
        ));
    }
    drop(listener);
    drop(alive_tx);
    let _ = alive_rx.recv().await;
    Ok(())
}
