//! Space3d dedicated servers (native only).
//!
//! One library, two roles, runnable together or apart (design: `docs/design/game-server.md`):
//!
//! * **Game server** — headless and authoritative. Rooms (one `space3d_core::App` + isolated
//!   `World` each) run on shard threads at a fixed tick; clients connect over WebSocket
//!   (`/play`) with a signed join ticket; the gateway verifies tickets offline (HMAC) and routes
//!   links to rooms; rooms replicate with `space3d_replication::server`. Periodic checkpoints,
//!   a durable outbox for results, admin API, metrics, health, graceful drain.
//! * **Player service** — accounts (guest/device, password), sessions, profiles and player
//!   data, leaderboards, matchmaking, lobbies, and the fleet registry of game servers
//!   (register/heartbeat/lease over the fleet link). SQLite via `space3d-db`.
//!
//! `role = "all-in-one"` runs both in one process (in-process fleet link, one SQLite file);
//! `"player-service"` and `"game-server"` split them for scale-out.
//!
//! A game's server binary is a few lines:
//!
//! ```ignore
//! fn main() -> std::process::ExitCode {
//!     match space3d_server::ServerMain::from_env(arena::ArenaGame) {
//!         Ok(main) => main.run(),
//!         Err(e) => { eprintln!("{e}"); std::process::ExitCode::from(2) }
//!     }
//! }
//! ```
#![cfg(not(target_arch = "wasm32"))]
#![warn(missing_docs)]

pub mod admin;
pub mod clock;
pub mod config;
pub mod game;
pub mod gs;
pub mod health;
pub mod lifecycle;
pub mod loadtest;
pub mod logging;
pub mod metrics;
pub mod ps;
pub mod security;
#[cfg(test)]
mod test_util;
#[cfg(feature = "testing")]
pub mod testing;

pub use config::{ConfigError, Role, ServerConfig};
pub use game::{GameDefinition, NoGame, RecoveryPolicy, RoomSpec};

use clock::UnixClock;
use gs::checkpoint::{CheckpointStore, LocalCheckpoints, NoCheckpoints, PlayerServiceCheckpoints, RemoteCheckpoints};
use gs::fleet::{Capacity, FleetLink, ServerRegistration, WsFleetLink, in_process_pair};
use gs::gatekeeper::{Gatekeeper, GatekeeperConfig};
use gs::outbox::Outbox;
use gs::rooms::{Placement, RoomEnv, RoomManager};
use gs::transport_ws::{AdmissionPolicy, WsServerTransport};
use gs::{GameServerHandle, GameServerPlugin, GameServerRuntime};
use health::HealthState;
use lifecycle::{ExitStatus, LifecycleHandle, LifecyclePlugin};
use metrics::{GameServerMetrics, Metrics, PlayerServiceMetrics};
use parking_lot::Mutex;
use ps::{PlayerService, PlayerServiceHandle, PlayerServicePlugin};
use security::Secrets;
use security::bans::BanList;
use space3d_core::{App, Stage};
use space3d_net::{HttpServer, ServerHandle, WsServer, WsServerConfig};
use space3d_replication::protocol::WireLimits;
use space3d_replication::server::ServerNetConfig;
use space3d_replication::time::NetClockHandle;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Top-level server failure.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// Invalid configuration.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// Binding a listener failed.
    #[error("bind: {0}")]
    Bind(#[from] space3d_net::NetError),
    /// Database failure.
    #[error("db: {0}")]
    Db(#[from] space3d_db::DbError),
    /// Signing keys missing / invalid.
    #[error("keys: {0}")]
    Keys(String),
    /// Anything else.
    #[error("{0}")]
    Other(String),
}

/// Supervisor tick rate (lifecycle, fleet heartbeats, matchmaking).
pub const SUPERVISOR_HZ: f64 = 10.0;

/// Knobs for embedding and tests (none are needed for a production binary).
#[derive(Clone, Default)]
pub struct BuildOptions {
    /// Queue room builds for the caller instead of running shard threads (`TestCluster`).
    pub manual_rooms: bool,
    /// Monotonic clock for rooms and the gatekeeper (default: system clock).
    pub net_clock: Option<NetClockHandle>,
    /// Deterministic resume secrets (tests only).
    pub secret_seed: Option<u64>,
    /// Secrets instead of env / files.
    pub secrets: Option<Secrets>,
    /// Use this database for every role (e.g. `Db::open_in_memory()`).
    pub db: Option<space3d_db::Db>,
    /// Game-server role: use this fleet link instead of dialing `fleet.player_service_url`
    /// (in-process split topologies: `TestCluster`, embedders).
    pub fleet_link: Option<Arc<dyn FleetLink>>,
    /// Game-server role: use this checkpoint store instead of `persistence.checkpoint_store`.
    pub checkpoints: Option<Arc<dyn CheckpointStore>>,
}

/// Resource: bound admin address.
#[derive(Clone, Copy, Debug)]
pub struct AdminHandle {
    /// Bound address.
    pub addr: SocketAddr,
}

/// Resource: the process' HTTP listeners (dropping them shuts them down).
pub struct Listeners(pub Vec<ServerHandle>);

/// Resource: stops the ingress task and shard threads when the supervisor `App` is dropped.
pub struct ShutdownGuard {
    ingress_stop: Option<async_channel::Sender<()>>,
    shards: Option<gs::shard::ShardPool>,
}

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        self.ingress_stop.take();
        if let Some(s) = self.shards.take() {
            s.stop();
        }
    }
}

/// CLI of game server binaries (`ServerMain::from_env`).
#[derive(clap::Parser, Debug)]
#[command(about = "Space3d dedicated server")]
struct Cli {
    /// Config file (TOML).
    #[arg(long, env = "SPACE3D_CONFIG")]
    config: Option<PathBuf>,
    /// Override `role` (all-in-one | player-service | game-server).
    #[arg(long)]
    role: Option<String>,
    /// Validate the config and secrets, print the redacted effective config, exit.
    #[arg(long)]
    check_config: bool,
    /// Print the commented example config and exit.
    #[arg(long)]
    print_default_config: bool,
}

/// Process entry point for a game (or `NoGame` for a player-service-only binary).
pub struct ServerMain<G: GameDefinition> {
    game: Arc<G>,
    config: ServerConfig,
    unix_clock: Arc<dyn UnixClock>,
    check_only: bool,
    options: BuildOptions,
}

/// Parse a role name.
pub fn parse_role(s: &str) -> Result<Role, ConfigError> {
    match s {
        "all-in-one" => Ok(Role::AllInOne),
        "player-service" => Ok(Role::PlayerService),
        "game-server" => Ok(Role::GameServer),
        other => Err(ConfigError::Invalid {
            key: "role".into(),
            message: format!("unknown role `{other}`"),
        }),
    }
}

impl<G: GameDefinition> ServerMain<G> {
    /// Parse CLI args (`--config <path>` / `SPACE3D_CONFIG`, `--role`, `--check-config`,
    /// `--print-default-config`) and load the TOML file with the `SPACE3D__A__B` env overlay.
    pub fn from_env(game: G) -> Result<Self, ServerError> {
        Self::from_env_layered(game, None)
    }

    /// [`Self::from_env`] with game-supplied config defaults (TOML) layered **under** the
    /// config file and the env overlay: built-in defaults ← `defaults` ← `--config` file ←
    /// `SPACE3D__*` env ← `--role`. A game binary can ship settings that work out of the box
    /// (e.g. an all-in-one loopback dev server) while deployments still override everything.
    pub fn from_env_with_defaults(game: G, defaults: &str) -> Result<Self, ServerError> {
        Self::from_env_layered(game, Some(defaults))
    }

    fn from_env_layered(game: G, defaults: Option<&str>) -> Result<Self, ServerError> {
        use clap::Parser;
        // `server healthcheck [--ready] [--url ...]` (container probes) works for game binaries too.
        let args: Vec<String> = std::env::args().collect();
        if args.get(1).map(String::as_str) == Some("healthcheck") {
            let ready = args.iter().any(|a| a == "--ready");
            let url = args
                .iter()
                .position(|a| a == "--url")
                .and_then(|i| args.get(i + 1).cloned())
                .unwrap_or_else(|| {
                    let admin = ServerConfig::load_layered(
                        defaults,
                        std::env::var_os("SPACE3D_CONFIG").map(PathBuf::from).as_deref(),
                        std::env::vars(),
                    )
                    .map_or_else(|_| config::AdminConfig::default().bind, |c| c.admin.bind);
                    health::local_url(admin)
                });
            std::process::exit(if health::probe(&url, ready) { 0 } else { 1 });
        }
        let cli = Cli::parse();
        if cli.print_default_config {
            print!("{}", ServerConfig::example_toml());
            if let Some(defaults) = defaults {
                println!("\n# ---- built into this binary (applied under --config / env) ----");
                for line in defaults.lines() {
                    println!("# {line}");
                }
            }
            std::process::exit(0);
        }
        let mut config = ServerConfig::load_layered(defaults, cli.config.as_deref(), std::env::vars())?;
        if let Some(role) = cli.role.as_deref() {
            config.role = parse_role(role)?;
            config.validate()?;
        }
        let mut main = Self::with_config(game, config);
        if cli.check_config {
            Secrets::load(&main.config, &|k| std::env::var(k).ok())?;
            print!("{}", main.config.redacted_toml());
            eprintln!("config OK");
            main.check_only = true;
        }
        Ok(main)
    }

    /// Use an explicit, already-loaded config (tests, embedding).
    pub fn with_config(game: G, config: ServerConfig) -> Self {
        Self::with_shared_game(Arc::new(game), config)
    }

    /// Like [`Self::with_config`] for a game definition shared by several servers in one
    /// process (`TestCluster`, embedders).
    pub fn with_shared_game(game: Arc<G>, config: ServerConfig) -> Self {
        Self {
            game,
            config,
            unix_clock: Arc::new(clock::SystemUnixClock),
            check_only: false,
            options: BuildOptions::default(),
        }
    }

    /// Replace the wall clock used for tokens, leases and limits (tests).
    pub fn unix_clock(mut self, clock: Arc<dyn UnixClock>) -> Self {
        self.unix_clock = clock;
        self
    }

    /// Embedding / test options.
    pub fn options(mut self, options: BuildOptions) -> Self {
        self.options = options;
        self
    }

    /// The config in use.
    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    /// Build the **supervisor** `App` (headless, 10 Hz): lifecycle, game-server and/or
    /// player-service plugins per `config.role`; listeners are bound here (port 0 works; see
    /// `gs::GameServerHandle` / `ps::PlayerServiceHandle` / [`AdminHandle`] resources for the
    /// real addresses). Tests step it with `App::update`; [`Self::run`] runs it until shutdown.
    ///
    /// Startup (database open + migrations, schema hash) completes before this returns; it is
    /// not frame code.
    pub fn build(self) -> Result<App, ServerError> {
        let ServerMain {
            game,
            config,
            unix_clock,
            options,
            ..
        } = self;
        config.validate()?;
        let secrets = match options.secrets.clone() {
            Some(s) => s,
            None => Secrets::load(&config, &|k| std::env::var(k).ok())?,
        };
        let registry = Metrics::new();
        let health = HealthState::new();
        let lifecycle = LifecycleHandle::default();
        let bans = BanList::default();
        let net_clock = options.net_clock.clone().unwrap_or_default();
        let game_dyn: Arc<dyn GameDefinition> = game.clone();

        let mut app = App::new().with_tickrate(SUPERVISOR_HZ);
        app.add_plugin(space3d_tasks::TasksPlugin);
        app.insert_resource(registry.clone());
        app.insert_resource(health.clone());
        app.insert_resource(lifecycle.clone());
        app.insert_resource(bans.clone());
        app.insert_resource(net_clock.clone());
        app.add_plugin(LifecyclePlugin {
            handle: lifecycle.clone(),
            drain_timeout: config.game.drain_timeout,
            clock: unix_clock.clone(),
        });

        let open_db = |path: &std::path::Path| -> space3d_db::Db {
            match &options.db {
                Some(db) => db.clone(),
                None => space3d_db::Db::open(path),
            }
        };
        let ps_db = config
            .runs_player_service()
            .then(|| open_db(&config.player_service.db_path));
        let gs_db = config.runs_game_server().then(|| match (&ps_db, config.role) {
            (Some(db), Role::AllInOne) => db.clone(),
            _ => open_db(
                &config
                    .persistence
                    .data_dir
                    .join(format!("gameserver-{}.db", config.server_id)),
            ),
        });

        let (schema_hash, manifest) = if config.runs_game_server() {
            schema_of(game_dyn.as_ref())
        } else {
            (0, Vec::new())
        };

        // HTTP listeners, grouped by bind address (equal binds share one server).
        let mut servers: Vec<ServerSlot> = Vec::new();

        // ---- player service ---------------------------------------------------------------
        let mut svc: Option<Arc<PlayerService>> = None;
        let mut local_fleet: Option<Arc<dyn FleetLink>> = None;
        if let Some(db) = &ps_db {
            let service = PlayerService::open(
                config.clone(),
                db.clone(),
                secrets.keys.clone(),
                unix_clock.clone(),
                PlayerServiceMetrics::register(&registry),
                secrets.fleet_token.clone(),
                bans.clone(),
            )
            .block()
            .ok_or_else(|| ServerError::Other("player service startup task lost".into()))??;
            let fleet_ws = WsServer::with_config(
                WsServerConfig::default()
                    .max_message_size(gs::fleet::MAX_FLEET_MESSAGE)
                    .inbound_queue(65_536),
            );
            let mut peers = Vec::new();
            if config.runs_game_server() {
                let (link, peer) = in_process_pair();
                peers.push(peer);
                local_fleet = Some(Arc::new(link));
            }
            let s2 = service.clone();
            let ws2 = fleet_ws.clone();
            with_server(&mut servers, config.player_service.bind, move |srv| {
                ps::routes::mount(srv, s2, &ws2)
            });
            app.add_plugin(PlayerServicePlugin {
                service: service.clone(),
                local_fleet: peers,
                fleet_ws: Some(fleet_ws),
            });
            svc = Some(service);
        }

        // ---- game server --------------------------------------------------------------------
        let mut gs_parts = None;
        if let Some(db) = &gs_db {
            let metrics = GameServerMetrics::register(&registry);
            let outbox = Outbox::open(db.clone(), &config.server_id, unix_clock.clone())
                .block()
                .ok_or_else(|| ServerError::Other("outbox startup task lost".into()))??;
            let checkpoints: Arc<dyn CheckpointStore> = match (config.persistence.checkpoint_store, &svc) {
                _ if options.checkpoints.is_some() => options.checkpoints.clone().expect("checked"),
                (config::CheckpointStoreKind::None, _) => Arc::new(NoCheckpoints),
                (config::CheckpointStoreKind::Local, _) => Arc::new(
                    LocalCheckpoints::open(db.clone(), unix_clock.clone())
                        .block()
                        .ok_or_else(|| ServerError::Other("checkpoint store task lost".into()))?
                        .map_err(|e| ServerError::Other(e.to_string()))?,
                ),
                (config::CheckpointStoreKind::PlayerService, Some(ps)) => Arc::new(PlayerServiceCheckpoints {
                    store: ps.store.clone(),
                    clock: unix_clock.clone(),
                }),
                (config::CheckpointStoreKind::PlayerService, None) => Arc::new(RemoteCheckpoints {
                    http: space3d_net::Http::new(),
                    base_url: config.fleet.player_service_http.clone(),
                    token: secrets.fleet_token.clone(),
                }),
            };
            let (uplink_tx, uplink_rx) = async_channel::bounded(16_384);
            let env = RoomEnv {
                game: game_dyn.clone(),
                net: base_net_config(&config),
                tick_hz: config.game.tick_hz,
                outbox: outbox.sender(),
                uplink: uplink_tx,
                checkpoints,
                metrics: metrics.clone(),
                server_id: config.server_id.clone(),
                checkpoint_interval: config.persistence.checkpoint_interval,
                empty_room_ttl: config.game.empty_room_ttl,
                recovery_max_age: config.persistence.recovery_max_age,
                clock: net_clock.clone(),
                unix: unix_clock.clone(),
                inbox_capacity: config.game.room_inbox,
                secret_seed: options.secret_seed,
            };
            let shards = (!options.manual_rooms).then(|| {
                let n = if config.game.shards == 0 {
                    gs::shard::default_shards()
                } else {
                    config.game.shards
                };
                gs::shard::ShardPool::start(n, &health, metrics.clone(), config.game.max_catchup_ticks)
            });
            let placement = match &shards {
                Some(pool) => Placement::Shards(pool.clone()),
                None => Placement::Manual(Arc::default()),
            };
            let rooms = RoomManager::new(placement, env, config.game.max_rooms, config.game.max_players);
            let gate = Arc::new(Mutex::new(Gatekeeper::new(
                GatekeeperConfig::from_config(&config, game_dyn.clone()),
                secrets.keys.clone(),
                bans.clone(),
            )));
            let outbound_limit = (config.limits.outbound_backlog_bytes as usize)
                .saturating_mul(4)
                .max(4 << 20);
            let transport = WsServerTransport::hardened(
                config.limits.max_message_bytes as usize,
                outbound_limit,
                config.limits.max_connections,
                AdmissionPolicy {
                    bans: bans.clone(),
                    allowed_origins: config.game.allowed_origins.clone(),
                    trusted_proxies: gate.lock().config().trusted_proxies.clone(),
                    clock: unix_clock.clone(),
                },
            );
            let ws = transport.ws().clone();
            let path = config.game.path.clone();
            with_server(&mut servers, config.game.bind, move |srv| {
                srv.ws(&path, &ws)
                    .route("/ping", space3d_net::server::axum::routing::get(|| async { "pong" }))
            });
            let registration = ServerRegistration {
                token: secrets.fleet_token.clone(),
                server_id: config.server_id.clone(),
                region: config.region.clone(),
                public_url: config.game.public_url.clone(),
                game_id: game_dyn.game_id().to_owned(),
                build: env!("CARGO_PKG_VERSION").to_owned(),
                protocol: space3d_replication::PROTOCOL_VERSION,
                schema_hash,
                capacity: Capacity {
                    max_rooms: config.game.max_rooms as u32,
                    max_players: config.game.max_players as u32,
                },
            };
            let fleet: Arc<dyn FleetLink> = match (local_fleet.take(), options.fleet_link.clone()) {
                (Some(link), _) | (None, Some(link)) => link,
                (None, None) => Arc::new(WsFleetLink::connect(
                    &config.fleet.player_service_url,
                    registration.clone(),
                )),
            };
            gs_parts = Some((
                GameServerRuntime {
                    config: config.clone(),
                    rooms,
                    gate,
                    fleet,
                    outbox,
                    uplink: uplink_rx,
                    bans: bans.clone(),
                    health: health.clone(),
                    lifecycle: lifecycle.clone(),
                    draining: Arc::new(AtomicBool::new(false)),
                    metrics,
                    unix: unix_clock.clone(),
                    shards,
                    registration,
                },
                transport,
            ));
        }

        // ---- admin ----------------------------------------------------------------------------
        let admin_open = secrets.admin_tokens.is_empty() && config.admin.bind.ip().is_loopback();
        if secrets.admin_tokens.is_empty() && !admin_open {
            tracing::warn!("no admin tokens configured and admin bind is not loopback: admin API locked");
        }
        let admin_state = admin::AdminState {
            token_hashes: Arc::new(secrets.admin_tokens.iter().map(|t| admin::hash_token(t)).collect()),
            open: admin_open,
            rooms: gs_parts.as_ref().map(|(rt, _)| rt.rooms.clone()),
            players: svc.clone(),
            bans: bans.clone(),
            lifecycle: lifecycle.clone(),
            metrics: registry.clone(),
            health: health.clone(),
            config_text: Arc::new(config.redacted_toml()),
            schema: Arc::new(manifest),
            info: Arc::new(serde_json::json!({
                "server_id": config.server_id,
                "role": config.role,
                "region": config.region,
                "version": env!("CARGO_PKG_VERSION"),
                "game_id": game_dyn.game_id(),
                "schema_hash": format!("{schema_hash:016x}"),
            })),
            clock: unix_clock.clone(),
            needs_registration: config.role == Role::GameServer,
            data_dir: config.persistence.data_dir.clone(),
        };
        let admin_bind = config.admin.bind;
        if servers.iter().any(|s| s.addr == admin_bind) {
            tracing::warn!("admin API shares a public listener ({admin_bind}); keep it on a private address");
        }
        with_server(&mut servers, admin_bind, move |srv| {
            srv.merge(admin::router(admin_state))
        });
        // Public listeners answer /healthz and /readyz too (the admin router already has them).
        for slot in servers.iter_mut().filter(|s| s.addr != admin_bind) {
            let router = health.router(unix_clock.clone(), config.role == Role::GameServer);
            slot.map(|srv| srv.merge(router));
        }

        // ---- bind -------------------------------------------------------------------------------
        let mut handles = Vec::new();
        let mut bound: Vec<(SocketAddr, SocketAddr)> = Vec::new();
        for slot in servers {
            let handle = slot.server.expect("server present").start()?;
            bound.push((slot.addr, handle.local_addr()));
            handles.push(handle);
        }
        let real = |addr: SocketAddr| bound.iter().find(|(a, _)| *a == addr).map_or(addr, |(_, r)| *r);
        let admin_addr = real(admin_bind);
        app.insert_resource(AdminHandle { addr: admin_addr });
        if let Some(service) = &svc {
            app.insert_resource(PlayerServiceHandle {
                addr: real(config.player_service.bind),
                service: service.clone(),
            });
        }
        health.set_db_ready(true);
        if config.role != Role::GameServer {
            health.set_registered(true);
        }

        let mut guard = ShutdownGuard {
            ingress_stop: None,
            shards: None,
        };
        if let Some((runtime, transport)) = gs_parts {
            let (stop_tx, stop_rx) = async_channel::bounded::<()>(1);
            let ingress = gs::ingress::Ingress {
                transport,
                gate: runtime.gate.clone(),
                rooms: runtime.rooms.clone(),
                draining: runtime.draining.clone(),
                metrics: runtime.metrics.clone(),
                clock: net_clock.clone(),
                unix: unix_clock.clone(),
            };
            drop(gs::ingress::spawn_until(ingress, stop_rx));
            guard.ingress_stop = Some(stop_tx);
            guard.shards = runtime.shards.clone();
            app.insert_resource(GameServerHandle {
                game_addr: real(config.game.bind),
                admin_addr,
                rooms: runtime.rooms.clone(),
                schema_hash,
                gate: runtime.gate.clone(),
                bans: bans.clone(),
            });
            app.add_plugin(GameServerPlugin::new(runtime));
        }
        app.insert_resource(Listeners(handles));
        app.insert_resource(guard);
        let beat_health = health.clone();
        let beat_clock = unix_clock.clone();
        app.add_system_named(Stage::PreUpdate, "server::heartbeat", move |_| {
            beat_health.beat_supervisor(beat_clock.now_ms());
        });
        tracing::info!(
            role = ?config.role,
            server_id = %config.server_id,
            admin = %admin_addr,
            "space3d server started"
        );
        Ok(app)
    }

    /// Build and run until SIGTERM/SIGINT finishes the drain. Exit codes: 0 clean, 1 forced
    /// (second signal), 2 config/startup error.
    pub fn run(self) -> ExitCode {
        logging::init(&self.config.log);
        if self.check_only {
            return ExitCode::SUCCESS;
        }
        let mut app = match self.build() {
            Ok(app) => app,
            Err(e) => {
                tracing::error!(error = %e, "startup failed");
                eprintln!("startup failed: {e}");
                return ExitCode::from(2);
            }
        };
        lifecycle::install_signal_handlers(app.resources.expect::<LifecycleHandle>().clone());
        let step = Duration::from_secs_f64(1.0 / SUPERVISOR_HZ);
        let mut last = std::time::Instant::now();
        loop {
            let now = std::time::Instant::now();
            app.update(now - last);
            last = now;
            if app.should_exit() {
                break;
            }
            let spent = now.elapsed();
            if spent < step {
                std::thread::sleep(step - spent);
            }
        }
        let code = app.resources.get::<ExitStatus>().map_or(0, |e| e.0);
        drop(app);
        tracing::info!(code, "space3d server stopped");
        ExitCode::from(code)
    }
}

struct ServerSlot {
    addr: SocketAddr,
    server: Option<HttpServer>,
}

impl ServerSlot {
    fn map(&mut self, f: impl FnOnce(HttpServer) -> HttpServer) {
        let s = self.server.take().expect("server present");
        self.server = Some(f(s));
    }
}

fn with_server(servers: &mut Vec<ServerSlot>, addr: SocketAddr, f: impl FnOnce(HttpServer) -> HttpServer) {
    match servers.iter_mut().find(|s| s.addr == addr) {
        Some(slot) => slot.map(f),
        None => servers.push(ServerSlot {
            addr,
            server: Some(f(HttpServer::new().bind(addr))),
        }),
    }
}

/// Server-wide base replication config from `[game]` / `[limits]`.
pub fn base_net_config(config: &ServerConfig) -> ServerNetConfig {
    let l = &config.limits;
    ServerNetConfig {
        send_interval: config.game.send_interval,
        resume_window: config.game.resume_window,
        idle_timeout: l.idle_timeout,
        client_bandwidth: l.client_bandwidth.min(u64::from(u32::MAX)) as u32,
        limits: WireLimits {
            max_packet: l.max_message_bytes.min(u64::from(u32::MAX)) as u32,
            input_packets_per_sec: l.input_packets_per_sec,
            messages_per_sec: l.messages_per_sec,
            ..WireLimits::default()
        },
        backlog_kick_bytes: l.outbound_backlog_bytes as usize,
        ..ServerNetConfig::default()
    }
}

/// Schema hash + manifest of a game's protocol (built once in a scratch `App`).
pub fn schema_of(game: &dyn GameDefinition) -> (u64, Vec<String>) {
    let mut app = App::new();
    game.protocol(&mut app);
    match app.resources.get_mut::<space3d_replication::ReplicationRegistry>() {
        Some(reg) => {
            reg.freeze();
            (reg.schema_hash(), reg.manifest())
        }
        None => (0, Vec::new()),
    }
}

/// `use space3d_server::prelude::*;`
pub mod prelude {
    pub use crate::game::{GameDefinition, NoGame, RecoveryPolicy, RoomSpec};
    pub use crate::gs::room::{MatchResult, PlayerDataLoaded, PlayerResult, RoomContext, RoomControl};
    pub use crate::{BuildOptions, Role, ServerConfig, ServerError, ServerMain};
}
