//! Native-only servers: [`HttpServer`] (axum) and [`WsServer`].
//!
//! Servers run on the shared engine runtime ([`space3d_tasks::runtime`]) and
//! never block the frame: [`HttpServer::start`] binds synchronously (so the
//! actual port is known immediately, even when binding port `0`) and returns a
//! [`ServerHandle`] for graceful shutdown.
//!
//! ```no_run
//! use space3d_net::{HttpServer, WsServer};
//!
//! // Serve a web build with a live-reload socket, like `space3d run --web` does.
//! let reload = WsServer::new();
//! let server = HttpServer::new()
//!     .bind(([127, 0, 0, 1], 8080))
//!     .serve_dir("/", "target/web")
//!     .ws("/__space3d/reload", &reload)
//!     .no_cache()
//!     .cross_origin_isolated()
//!     .start()
//!     .expect("port in use");
//! println!("serving on {}", server.url());
//! reload.broadcast("reload");
//! server.wait().unwrap(); // CLI/tools only: blocks until shutdown
//! ```

mod conn;
mod ws;

pub use conn::{ConnectionLimits, ConnectionStats, PeerKey};
pub use ws::{AcceptHook, ClientId, ClientSender, ConnectMeta, ServerEvent, WsServer, WsServerConfig, joined_header};

use crate::NetError;
use axum::Router;
use axum::http::{HeaderName, HeaderValue};
use axum::routing::MethodRouter;
use serde::Serialize;
use serde::de::DeserializeOwned;
use space3d_tasks::{Task, TaskState};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use tokio_util::sync::CancellationToken;
use tower_http::services::{ServeDir, ServeFile};

/// Re-exported so custom routes ([`HttpServer::route`], [`HttpServer::merge`]) don't need their own axum dependency.
pub use axum;
/// CORS configuration for [`HttpServer::cors`].
pub use tower_http::cors::CorsLayer;

/// Builder for an axum HTTP server: static files, JSON routes, WebSockets, CORS. Connections
/// are protected by [`ConnectionLimits`] (header-read timeout, connection caps).
pub struct HttpServer {
    addr: SocketAddr,
    router: Router,
    cors: Option<CorsLayer>,
    headers: Vec<(HeaderName, HeaderValue)>,
    token: CancellationToken,
    limits: ConnectionLimits,
}

impl Default for HttpServer {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpServer {
    /// New server bound to `127.0.0.1:0` (an OS-assigned free port) by default.
    pub fn new() -> Self {
        Self {
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            router: Router::new(),
            cors: None,
            headers: Vec::new(),
            token: CancellationToken::new(),
            limits: ConnectionLimits::default(),
        }
    }

    /// Connection-level protections (defaults: 15 s header-read timeout, 16 384 connections,
    /// no per-peer cap).
    pub fn connection_limits(mut self, limits: ConnectionLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Address to bind, e.g. `([0, 0, 0, 0], 8080)`. Port `0` picks a free port.
    pub fn bind(mut self, addr: impl Into<SocketAddr>) -> Self {
        self.addr = addr.into();
        self
    }

    /// Change only the port (keeps the IP).
    pub fn port(mut self, port: u16) -> Self {
        self.addr.set_port(port);
        self
    }

    /// Serve files from `dir` under `route` (`"/"` = everything not matched by
    /// another route). Directories serve their `index.html`; `.wasm` is sent as
    /// `application/wasm`.
    pub fn serve_dir(mut self, route: &str, dir: impl AsRef<Path>) -> Self {
        let svc = ServeDir::new(dir.as_ref());
        let route = route.trim_end_matches('/');
        self.router = if route.is_empty() {
            self.router.fallback_service(svc)
        } else {
            self.router.nest_service(route, svc)
        };
        self
    }

    /// Single-page-app hosting: files from `dir`, with `dir/index.html` for any unknown path.
    pub fn serve_spa(mut self, dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref();
        let svc = ServeDir::new(dir).fallback(ServeFile::new(dir.join("index.html")));
        self.router = self.router.fallback_service(svc);
        self
    }

    /// `GET path` → `handler()` serialized as JSON.
    ///
    /// Handlers run on the server runtime; keep them short or use [`Self::route`] with an async handler.
    pub fn get_json<Res, F>(mut self, path: &str, handler: F) -> Self
    where
        Res: Serialize + Send + 'static,
        F: Fn() -> Res + Clone + Send + Sync + 'static,
    {
        self.router = self.router.route(
            path,
            axum::routing::get(move || {
                let h = handler.clone();
                async move { axum::Json(h()) }
            }),
        );
        self
    }

    /// `POST path` with a JSON body `Req` → `handler(req)` as JSON. Malformed bodies get a 4xx.
    pub fn post_json<Req, Res, F>(mut self, path: &str, handler: F) -> Self
    where
        Req: DeserializeOwned + Send + 'static,
        Res: Serialize + Send + 'static,
        F: Fn(Req) -> Res + Clone + Send + Sync + 'static,
    {
        self.router = self.router.route(
            path,
            axum::routing::post(move |axum::Json(req): axum::Json<Req>| {
                let h = handler.clone();
                async move { axum::Json(h(req)) }
            }),
        );
        self
    }

    /// `PUT path` with a JSON body `Req` → `handler(req)` as JSON.
    pub fn put_json<Req, Res, F>(mut self, path: &str, handler: F) -> Self
    where
        Req: DeserializeOwned + Send + 'static,
        Res: Serialize + Send + 'static,
        F: Fn(Req) -> Res + Clone + Send + Sync + 'static,
    {
        self.router = self.router.route(
            path,
            axum::routing::put(move |axum::Json(req): axum::Json<Req>| {
                let h = handler.clone();
                async move { axum::Json(h(req)) }
            }),
        );
        self
    }

    /// Mount any axum method router (async handlers, custom status codes, extractors).
    /// Paths use axum syntax (`/users/{id}`).
    pub fn route(mut self, path: &str, method_router: MethodRouter) -> Self {
        self.router = self.router.route(path, method_router);
        self
    }

    /// Merge a whole axum router.
    pub fn merge(mut self, router: Router) -> Self {
        self.router = self.router.merge(router);
        self
    }

    /// Accept WebSocket upgrades on `path` and hand the clients to `server`.
    /// One [`WsServer`] may be mounted on several paths/servers.
    pub fn ws(mut self, path: &str, server: &WsServer) -> Self {
        self.router = self.router.route(path, server.upgrade_route(self.token.clone()));
        self
    }

    /// Apply a CORS policy to every route.
    pub fn cors(mut self, layer: CorsLayer) -> Self {
        self.cors = Some(layer);
        self
    }

    /// Allow any origin, method and header (handy for local dev tools).
    pub fn cors_permissive(self) -> Self {
        self.cors(CorsLayer::permissive())
    }

    /// Add a header to every response.
    ///
    /// # Panics
    /// If `name` or `value` is not a valid header name/value (a programming error).
    pub fn header(mut self, name: &str, value: &str) -> Self {
        let name =
            HeaderName::from_bytes(name.as_bytes()).unwrap_or_else(|e| panic!("invalid header name {name:?}: {e}"));
        let value = HeaderValue::from_str(value).unwrap_or_else(|e| panic!("invalid header value {value:?}: {e}"));
        self.headers.push((name, value));
        self
    }

    /// `Cache-Control: no-store` on every response (dev servers: never serve a stale `.wasm`).
    pub fn no_cache(self) -> Self {
        self.header("cache-control", "no-store")
    }

    /// COOP/COEP headers so browsers enable `SharedArrayBuffer` (wasm threads).
    pub fn cross_origin_isolated(self) -> Self {
        self.header("cross-origin-opener-policy", "same-origin")
            .header("cross-origin-embedder-policy", "require-corp")
    }

    fn into_router(self) -> (Router, CancellationToken, ConnectionLimits) {
        let mut router = self.router;
        if !self.headers.is_empty() {
            let headers = std::sync::Arc::new(self.headers);
            router = router.layer(axum::middleware::map_response(
                move |mut res: axum::response::Response| {
                    let headers = headers.clone();
                    async move {
                        for (k, v) in headers.iter() {
                            res.headers_mut().insert(k.clone(), v.clone());
                        }
                        res
                    }
                },
            ));
        }
        if let Some(cors) = self.cors {
            router = router.layer(cors);
        }
        (router, self.token, self.limits)
    }

    /// Bind (synchronously, so errors and the real port are known right away) and
    /// start serving on the engine runtime. Never blocks on requests.
    pub fn start(self) -> Result<ServerHandle, NetError> {
        let addr = self.addr;
        let listener =
            std::net::TcpListener::bind(addr).map_err(|e| NetError::Io(format!("failed to bind {addr}: {e}")))?;
        listener.set_nonblocking(true)?;
        let local_addr = listener.local_addr()?;
        let (router, token, limits) = self.into_router();
        let listener = {
            let _guard = space3d_tasks::runtime().enter();
            tokio::net::TcpListener::from_std(listener)?
        };
        let stats = ConnectionStats::default();
        let serve = conn::serve(listener, router, token.clone(), limits, stats.clone());
        // Connections log inside the span that was current when the server started.
        let task = space3d_tasks::spawn(tracing::Instrument::instrument(
            async move { serve.await.map_err(NetError::from) },
            tracing::Span::current(),
        ));
        tracing::info!(addr = %local_addr, "http server listening");
        Ok(ServerHandle {
            addr: local_addr,
            token,
            task: Some(task),
            detached: false,
            stats,
        })
    }
}

/// Cloneable remote control for stopping a server (e.g. from a Ctrl-C handler).
#[derive(Clone, Debug)]
pub struct ShutdownTrigger(CancellationToken);

impl ShutdownTrigger {
    /// Request graceful shutdown. Idempotent.
    pub fn shutdown(&self) {
        self.0.cancel();
    }
    /// Whether shutdown was requested.
    pub fn is_shutdown(&self) -> bool {
        self.0.is_cancelled()
    }
}

/// A running server. **Dropping it shuts the server down** (use [`Self::detach`] to keep it running).
///
/// Shutdown is graceful: the listener closes immediately, in-flight HTTP requests
/// finish, and WebSocket clients accepted by this server get a `1001 going away` close.
pub struct ServerHandle {
    addr: SocketAddr,
    token: CancellationToken,
    task: Option<Task<Result<(), NetError>>>,
    detached: bool,
    stats: ConnectionStats,
}

impl std::fmt::Debug for ServerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerHandle")
            .field("addr", &self.addr)
            .field("shutdown", &self.token.is_cancelled())
            .finish()
    }
}

impl ServerHandle {
    /// The actually bound address (resolves port `0`).
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// The actually bound port.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Live connection counters (open connections, refused by a cap).
    pub fn connections(&self) -> ConnectionStats {
        self.stats.clone()
    }

    /// `http://host:port` usable from this machine (unspecified IPs map to loopback).
    pub fn url(&self) -> String {
        format!("http://{}", self.reachable_addr())
    }

    /// `ws://host:port{path}` for a route mounted with [`HttpServer::ws`].
    pub fn ws_url(&self, path: &str) -> String {
        let path = if path.starts_with('/') {
            path.to_owned()
        } else {
            format!("/{path}")
        };
        format!("ws://{}{path}", self.reachable_addr())
    }

    fn reachable_addr(&self) -> SocketAddr {
        let mut a = self.addr;
        if a.ip().is_unspecified() {
            a.set_ip(match a.ip() {
                IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(_) => IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
            });
        }
        a
    }

    /// Request graceful shutdown (non-blocking, idempotent).
    pub fn shutdown(&self) {
        self.token.cancel();
    }

    /// A cloneable trigger that can stop this server from elsewhere.
    pub fn trigger(&self) -> ShutdownTrigger {
        ShutdownTrigger(self.token.clone())
    }

    /// Non-blocking: has the server task exited?
    pub fn is_finished(&mut self) -> bool {
        self.task
            .as_mut()
            .is_none_or(|t| !matches!(t.state(), TaskState::Pending))
    }

    /// Block the calling thread until the server stops (after [`Self::shutdown`] or a
    /// [`ShutdownTrigger`]). For CLIs/tools/tests only — never call from a system.
    pub fn wait(mut self) -> Result<(), NetError> {
        match self.task.take() {
            Some(t) => t.block().unwrap_or(Ok(())),
            None => Ok(()),
        }
    }

    /// Async version of [`Self::wait`].
    pub async fn stopped(mut self) -> Result<(), NetError> {
        match self.task.take() {
            Some(t) => t.wait().await.unwrap_or(Ok(())),
            None => Ok(()),
        }
    }

    /// Keep serving after this handle is dropped (until process exit or a [`ShutdownTrigger`]).
    pub fn detach(mut self) -> ShutdownTrigger {
        self.detached = true;
        self.trigger()
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        if !self.detached {
            self.token.cancel();
        }
    }
}
