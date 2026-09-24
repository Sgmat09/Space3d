//! Configuration: TOML file + environment overlay + validation.
//!
//! Load order: built-in defaults ← TOML file (`--config` / `SPACE3D_CONFIG`) ← env vars
//! `SPACE3D__SECTION__KEY=value` (split on `__`, lower-cased, value parsed as a TOML scalar,
//! falling back to a string) ← CLI flags (`--role`). `${VAR}` inside string values expands from
//! the environment (e.g. `server_id = "${HOSTNAME}"`) and `{server_id}` inside
//! `game.public_url` expands to the server id.
//!
//! Durations are strings (`"250ms"`, `"30s"`, `"15m"`, `"24h"`, `"30d"`); byte sizes accept
//! integers or strings (`"16KiB"`, `"1MiB"`). Secrets are **never** inline: the config names
//! the env var or file that holds them (`*_env` / `*_file`), and `--check-config` refuses
//! `env = "production"` with missing, short (< 32 bytes) or dev-default keys.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Configuration failure.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// File could not be read.
    #[error("reading {path}: {message}")]
    Io {
        /// Path.
        path: PathBuf,
        /// Error.
        message: String,
    },
    /// TOML syntax / type error.
    #[error("parse: {0}")]
    Parse(String),
    /// A value is invalid.
    #[error("invalid `{key}`: {message}")]
    Invalid {
        /// Dotted key.
        key: String,
        /// Why.
        message: String,
    },
    /// A required secret is missing.
    #[error("missing secret: {0}")]
    MissingSecret(String),
}

/// Which roles this process runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    /// Player service + game server in one process (in-process fleet link, one port).
    #[default]
    AllInOne,
    /// Player service only.
    PlayerService,
    /// Game server only (registers with the player service over the fleet link).
    GameServer,
}

/// Deployment environment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Env {
    /// Dev: pretty logs, ephemeral keys allowed, dev auth allowed on loopback.
    #[default]
    Dev,
    /// Production: strict secret checks, JSON logs.
    Production,
}

/// Log output format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LogFormat {
    /// Human-readable.
    #[default]
    Pretty,
    /// One JSON object per line.
    Json,
}

/// `[log]`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    /// Format.
    pub format: LogFormat,
    /// `EnvFilter` directives.
    pub filter: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            format: LogFormat::Pretty,
            filter: "info,space3d_server=info".into(),
        }
    }
}

/// `[game]`: game-server role.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GameConfig {
    /// Listen address for `/play` (+ `/ping`).
    pub bind: SocketAddr,
    /// WebSocket path.
    pub path: String,
    /// URL clients use (`wss://{server_id}.play.example.com/play`).
    pub public_url: String,
    /// Shard threads (0 = available_parallelism − 1, min 1).
    pub shards: usize,
    /// Room cap.
    pub max_rooms: usize,
    /// Player cap over all rooms.
    pub max_players: usize,
    /// Room tick rate.
    pub tick_hz: f64,
    /// Snapshots every N ticks.
    pub send_interval: u8,
    /// Resume window.
    #[serde(with = "serde_duration")]
    pub resume_window: Duration,
    /// Close empty non-persistent rooms after this.
    #[serde(with = "serde_duration")]
    pub empty_room_ttl: Duration,
    /// Graceful drain budget (match-based rooms run to their end up to this).
    #[serde(with = "serde_duration")]
    pub drain_timeout: Duration,
    /// Ticks a late room may catch up before skipping (default 3).
    pub max_catchup_ticks: u32,
    /// Accept `HelloAuth::Dev` (refused unless `bind` is loopback).
    pub allow_dev_auth: bool,
    /// Browser `Origin` allowlist (empty = any).
    pub allowed_origins: Vec<String>,
    /// Room inbox capacity.
    pub room_inbox: usize,
}

impl Default for GameConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([0, 0, 0, 0], 7777)),
            path: "/play".into(),
            public_url: "ws://127.0.0.1:7777/play".into(),
            shards: 0,
            max_rooms: 128,
            max_players: 1024,
            tick_hz: 60.0,
            send_interval: 2,
            resume_window: Duration::from_secs(20),
            empty_room_ttl: Duration::from_secs(30),
            drain_timeout: Duration::from_secs(900),
            max_catchup_ticks: 3,
            allow_dev_auth: false,
            allowed_origins: Vec::new(),
            room_inbox: 8192,
        }
    }
}

/// `[limits]`: gateway and per-connection limits.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LimitsConfig {
    /// Inbound WS message/frame cap on the game port (bytes).
    #[serde(with = "serde_bytes_size")]
    pub max_message_bytes: u64,
    /// Concurrent WS connections (all IPs).
    pub max_connections: usize,
    /// Concurrent connections per client IP.
    pub max_connections_per_ip: u32,
    /// New handshakes per IP per minute.
    pub handshakes_per_ip_per_min: u32,
    /// Hello must arrive within this.
    #[serde(with = "serde_duration")]
    pub handshake_timeout: Duration,
    /// No packet for this long → link dead.
    #[serde(with = "serde_duration")]
    pub idle_timeout: Duration,
    /// Default downstream budget per client (bytes/s).
    #[serde(with = "serde_bytes_size")]
    pub client_bandwidth: u64,
    /// Outbound backlog per link before `SlowConsumer` (bytes).
    #[serde(with = "serde_bytes_size")]
    pub outbound_backlog_bytes: u64,
    /// Input packets per second per link.
    pub input_packets_per_sec: u32,
    /// Messages per second per link.
    pub messages_per_sec: u32,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_message_bytes: 16 * 1024,
            max_connections: 10_000,
            max_connections_per_ip: 8,
            handshakes_per_ip_per_min: 20,
            handshake_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(10),
            client_bandwidth: 48 * 1024,
            outbound_backlog_bytes: 1 << 20,
            input_packets_per_sec: 90,
            messages_per_sec: 30,
        }
    }
}

/// `[player_service]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayerServiceConfig {
    /// Listen address (public API + internal fleet endpoint).
    pub bind: SocketAddr,
    /// SQLite path.
    pub db_path: PathBuf,
    /// Access-token lifetime.
    #[serde(with = "serde_duration")]
    pub session_ttl: Duration,
    /// Refresh-token lifetime.
    #[serde(with = "serde_duration")]
    pub refresh_ttl: Duration,
    /// Join-ticket lifetime.
    #[serde(with = "serde_duration")]
    pub join_ticket_ttl: Duration,
    /// Guest/device accounts.
    pub guest_auth: bool,
    /// Password accounts (feature `password-auth`).
    pub password_auth: bool,
    /// Player-data keys clients may write directly (`PUT /v1/me/data/{key}`).
    pub client_writable_keys: Vec<String>,
    /// HTTP body cap.
    #[serde(with = "serde_bytes_size")]
    pub max_body_bytes: u64,
    /// CORS allowlist for browser clients (empty = same-origin only).
    pub cors_origins: Vec<String>,
    /// Requests per second per IP (burst 2×).
    pub http_rps_per_ip: u32,
    /// Login attempts per minute per IP and per username.
    pub login_attempts_per_min: u32,
    /// Message of the day (`GET /v1/status`).
    pub motd: Option<String>,
}

impl Default for PlayerServiceConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([0, 0, 0, 0], 8080)),
            db_path: PathBuf::from("data/players.db"),
            session_ttl: Duration::from_secs(3600),
            refresh_ttl: Duration::from_secs(30 * 24 * 3600),
            join_ticket_ttl: Duration::from_secs(60),
            guest_auth: true,
            password_auth: false,
            client_writable_keys: vec!["settings".into()],
            max_body_bytes: 64 * 1024,
            cors_origins: Vec::new(),
            http_rps_per_ip: 20,
            login_attempts_per_min: 5,
            motd: None,
        }
    }
}

/// `[fleet]`: game server ↔ player service link.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FleetConfig {
    /// Player service fleet endpoint (`ws://player-service:8080/internal/v1/fleet`).
    pub player_service_url: String,
    /// Player service base URL for HTTP (checkpoint upload/download).
    pub player_service_http: String,
    /// Heartbeat period.
    #[serde(with = "serde_duration")]
    pub heartbeat: Duration,
    /// Lease TTL (a server missing heartbeats this long is dead).
    #[serde(with = "serde_duration")]
    pub lease_ttl: Duration,
}

impl Default for FleetConfig {
    fn default() -> Self {
        Self {
            player_service_url: "ws://127.0.0.1:8080/internal/v1/fleet".into(),
            player_service_http: "http://127.0.0.1:8080".into(),
            heartbeat: Duration::from_secs(2),
            lease_ttl: Duration::from_secs(10),
        }
    }
}

/// `[admin]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AdminConfig {
    /// Admin + metrics + health listener (keep private: loopback / cluster-internal).
    pub bind: SocketAddr,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 9090)),
        }
    }
}

/// `[security]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    /// Key id used for signing (all listed keys verify).
    pub signing_kid: String,
    /// TOML file `{ k1 = "<base64 ≥ 32 bytes>", k2 = "..." }` (k8s secret mount).
    pub keys_file: Option<PathBuf>,
    /// Or an env var `k2:<b64>,k1:<b64>`.
    pub keys_env: String,
    /// Fleet bearer token file / env.
    pub fleet_token_file: Option<PathBuf>,
    /// Env var holding the fleet token.
    pub fleet_token_env: String,
    /// Admin bearer token file / env (comma-separated list allowed).
    pub admin_token_file: Option<PathBuf>,
    /// Env var holding admin tokens.
    pub admin_token_env: String,
    /// CIDRs whose `X-Forwarded-For` is trusted.
    pub trusted_proxies: Vec<String>,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            signing_kid: "k1".into(),
            keys_file: None,
            keys_env: "SPACE3D_KEYS".into(),
            fleet_token_file: None,
            fleet_token_env: "SPACE3D_FLEET_TOKEN".into(),
            admin_token_file: None,
            admin_token_env: "SPACE3D_ADMIN_TOKENS".into(),
            trusted_proxies: Vec::new(),
        }
    }
}

/// Where room checkpoints go.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckpointStoreKind {
    /// Disabled.
    None,
    /// Local SQLite on the game server's volume (single host).
    Local,
    /// Uploaded to the player service (any game server can restore; default in split mode).
    #[default]
    PlayerService,
}

/// `[persistence]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistenceConfig {
    /// Data directory (game-server local DB: outbox + local checkpoints).
    pub data_dir: PathBuf,
    /// Checkpoint every N (and on phase change / shutdown).
    #[serde(with = "serde_duration")]
    pub checkpoint_interval: Duration,
    /// Store.
    pub checkpoint_store: CheckpointStoreKind,
    /// Checkpoints older than this are not restored.
    #[serde(with = "serde_duration")]
    pub recovery_max_age: Duration,
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("data"),
            checkpoint_interval: Duration::from_secs(10),
            checkpoint_store: CheckpointStoreKind::PlayerService,
            recovery_max_age: Duration::from_secs(300),
        }
    }
}

/// One matchmaking queue (`[[matchmaking.queues]]`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QueueConfig {
    /// Queue name (clients use it).
    pub name: String,
    /// Game mode → `RoomSpec::mode`.
    pub mode: String,
    /// Teams per match.
    pub teams: u16,
    /// Players per team.
    pub team_size: u16,
    /// Start with fewer players after `max_wait` (≥ 1 per team).
    pub min_players: u16,
    /// Longest wait before starting short / expiring.
    #[serde(with = "serde_duration")]
    pub max_wait: Duration,
    /// Initial rating window (±).
    pub rating_window: f64,
    /// Rating window growth per second waited.
    pub rating_window_growth: f64,
    /// Best-region RTT threshold (ms), widening to `region_ping_max_ms` after 30 s.
    pub region_ping_ms: u32,
    /// Max acceptable region RTT.
    pub region_ping_max_ms: u32,
    /// Fill open rooms of this mode before creating new ones (drop-in / persistent worlds).
    pub backfill: bool,
    /// Extra `RoomSpec::params`.
    pub params: BTreeMap<String, String>,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            name: "default".into(),
            mode: "default".into(),
            teams: 2,
            team_size: 1,
            min_players: 2,
            max_wait: Duration::from_secs(60),
            rating_window: 100.0,
            rating_window_growth: 20.0,
            region_ping_ms: 80,
            region_ping_max_ms: 150,
            backfill: false,
            params: BTreeMap::new(),
        }
    }
}

/// `[matchmaking]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MatchmakingConfig {
    /// Queues.
    pub queues: Vec<QueueConfig>,
    /// Matcher period (default 250 ms).
    #[serde(with = "serde_duration")]
    pub interval: Duration,
}

impl Default for MatchmakingConfig {
    fn default() -> Self {
        Self {
            queues: Vec::new(),
            interval: Duration::from_millis(250),
        }
    }
}

/// The whole configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Roles.
    pub role: Role,
    /// Environment.
    pub env: Env,
    /// Stable server id (`${HOSTNAME}` / pod name). Used in tickets (`server_id` binding).
    pub server_id: String,
    /// Region name.
    pub region: String,
    /// `[log]`.
    pub log: LogConfig,
    /// `[game]`.
    pub game: GameConfig,
    /// `[limits]`.
    pub limits: LimitsConfig,
    /// `[player_service]`.
    pub player_service: PlayerServiceConfig,
    /// `[fleet]`.
    pub fleet: FleetConfig,
    /// `[admin]`.
    pub admin: AdminConfig,
    /// `[security]`.
    pub security: SecurityConfig,
    /// `[persistence]`.
    pub persistence: PersistenceConfig,
    /// `[matchmaking]`.
    pub matchmaking: MatchmakingConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            role: Role::AllInOne,
            env: Env::Dev,
            server_id: "local-1".into(),
            region: "local".into(),
            log: LogConfig::default(),
            game: GameConfig::default(),
            limits: LimitsConfig::default(),
            player_service: PlayerServiceConfig::default(),
            fleet: FleetConfig::default(),
            admin: AdminConfig::default(),
            security: SecurityConfig::default(),
            persistence: PersistenceConfig::default(),
            matchmaking: MatchmakingConfig::default(),
        }
    }
}

impl ServerConfig {
    /// Load `path` (if any) and overlay `env` (`(name, value)` pairs, usually `std::env::vars()`),
    /// then expand `${VAR}` / `{server_id}` and validate.
    pub fn load(path: Option<&Path>, env: impl IntoIterator<Item = (String, String)>) -> Result<Self, ConfigError> {
        Self::load_layered(None, path, env)
    }

    /// Like [`Self::load`] with a game-supplied TOML layer **under** the file: defaults ←
    /// `defaults` ← file ← env. Tables merge key by key (a file only needs the keys it
    /// changes); arrays and scalars replace. Lets a game binary ship working dev settings
    /// (`ServerMain::from_env_with_defaults`).
    pub fn load_layered(
        defaults: Option<&str>,
        path: Option<&Path>,
        env: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, ConfigError> {
        let env: Vec<(String, String)> = env.into_iter().collect();
        let mut table = match defaults {
            Some(text) => text
                .parse::<toml::Table>()
                .map_err(|e| ConfigError::Parse(format!("built-in defaults: {e}")))?,
            None => toml::Table::new(),
        };
        if let Some(path) = path {
            let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
                path: path.to_path_buf(),
                message: e.to_string(),
            })?;
            let file = text
                .parse::<toml::Table>()
                .map_err(|e| ConfigError::Parse(format!("{}: {e}", path.display())))?;
            merge_tables(&mut table, file);
        }
        overlay_env(&mut table, env.iter().cloned());
        let lookup: BTreeMap<&str, &str> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        expand_env_table(&mut table, &|name| lookup.get(name).map(|v| (*v).to_owned()));
        let mut config: ServerConfig = toml::Value::Table(table)
            .try_into()
            .map_err(|e: toml::de::Error| ConfigError::Parse(e.to_string()))?;
        config.finish_expansion();
        config.validate()?;
        Ok(config)
    }

    /// Parse TOML text (no env overlay, no validation).
    pub fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    /// Post-deserialization expansion: empty `server_id` → `"local-1"`, `{server_id}` in
    /// `game.public_url`.
    fn finish_expansion(&mut self) {
        if self.server_id.trim().is_empty() {
            self.server_id = "local-1".into();
        }
        self.game.public_url = self.game.public_url.replace("{server_id}", &self.server_id);
    }

    /// Whether this process runs the game-server role.
    pub fn runs_game_server(&self) -> bool {
        matches!(self.role, Role::AllInOne | Role::GameServer)
    }

    /// Whether this process runs the player-service role.
    pub fn runs_player_service(&self) -> bool {
        matches!(self.role, Role::AllInOne | Role::PlayerService)
    }

    /// Semantic checks (ports, rates, dev auth only on loopback, production secrets present).
    ///
    /// Secrets themselves live in env vars / files and are checked when they are loaded
    /// (`security::Secrets::load`), which also refuses dev defaults when `env = "production"`.
    pub fn validate(&self) -> Result<(), ConfigError> {
        fn invalid(key: &str, message: impl Into<String>) -> ConfigError {
            ConfigError::Invalid {
                key: key.to_owned(),
                message: message.into(),
            }
        }
        let id = &self.server_id;
        if id.is_empty() || id.len() > 64 || !id.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
            return Err(invalid("server_id", "1-64 characters of [A-Za-z0-9-_.]"));
        }
        let g = &self.game;
        if !(g.tick_hz > 0.0 && g.tick_hz <= 1000.0) {
            return Err(invalid("game.tick_hz", "must be in (0, 1000]"));
        }
        if g.send_interval == 0 {
            return Err(invalid("game.send_interval", "must be at least 1"));
        }
        if !g.path.starts_with('/') {
            return Err(invalid("game.path", "must start with '/'"));
        }
        if g.max_rooms == 0 || g.max_players == 0 {
            return Err(invalid("game.max_rooms", "room and player caps must be at least 1"));
        }
        if g.room_inbox < 16 {
            return Err(invalid("game.room_inbox", "must be at least 16"));
        }
        if g.max_catchup_ticks == 0 {
            return Err(invalid("game.max_catchup_ticks", "must be at least 1"));
        }
        if g.allow_dev_auth && !g.bind.ip().is_loopback() {
            return Err(invalid(
                "game.allow_dev_auth",
                format!("dev auth is only allowed on a loopback bind (game.bind = {})", g.bind),
            ));
        }
        if g.allow_dev_auth && self.env == Env::Production {
            return Err(invalid("game.allow_dev_auth", "not allowed with env = \"production\""));
        }
        let l = &self.limits;
        if !(1024..=(16 << 20)).contains(&l.max_message_bytes) {
            return Err(invalid("limits.max_message_bytes", "must be between 1 KiB and 16 MiB"));
        }
        if l.handshake_timeout.is_zero() || l.idle_timeout.is_zero() {
            return Err(invalid("limits.handshake_timeout", "timeouts must be positive"));
        }
        if l.max_connections_per_ip == 0 || l.handshakes_per_ip_per_min == 0 {
            return Err(invalid(
                "limits.max_connections_per_ip",
                "per-IP limits must be at least 1",
            ));
        }
        if l.client_bandwidth < 1024 {
            return Err(invalid("limits.client_bandwidth", "must be at least 1 KiB/s"));
        }
        let p = &self.player_service;
        if p.join_ticket_ttl.is_zero() || p.join_ticket_ttl > Duration::from_secs(600) {
            return Err(invalid("player_service.join_ticket_ttl", "must be in (0, 10m]"));
        }
        if p.session_ttl < Duration::from_secs(60) || p.refresh_ttl < p.session_ttl {
            return Err(invalid(
                "player_service.session_ttl",
                "session_ttl must be ≥ 1m and ≤ refresh_ttl",
            ));
        }
        if p.password_auth && !cfg!(feature = "password-auth") {
            return Err(invalid(
                "player_service.password_auth",
                "this build lacks the `password-auth` feature",
            ));
        }
        if p.max_body_bytes < 1024 {
            return Err(invalid("player_service.max_body_bytes", "must be at least 1 KiB"));
        }
        if p.http_rps_per_ip == 0 || p.login_attempts_per_min == 0 {
            return Err(invalid(
                "player_service.http_rps_per_ip",
                "rate limits must be at least 1",
            ));
        }
        let f = &self.fleet;
        if f.heartbeat.is_zero() || f.heartbeat >= f.lease_ttl {
            return Err(invalid(
                "fleet.heartbeat",
                "must be positive and shorter than fleet.lease_ttl",
            ));
        }
        if self.role == Role::GameServer && !f.player_service_url.starts_with("ws") {
            return Err(invalid("fleet.player_service_url", "must be a ws:// or wss:// URL"));
        }
        for cidr in &self.security.trusted_proxies {
            crate::security::netaddr::Cidr::parse(cidr)
                .map_err(|e| invalid("security.trusted_proxies", format!("{cidr}: {e}")))?;
        }
        if self.security.signing_kid.is_empty() {
            return Err(invalid("security.signing_kid", "must not be empty"));
        }
        if self.persistence.checkpoint_interval.is_zero() {
            return Err(invalid("persistence.checkpoint_interval", "must be positive"));
        }
        let mut names = std::collections::BTreeSet::new();
        for q in &self.matchmaking.queues {
            let key = format!("matchmaking.queues.{}", q.name);
            if q.name.is_empty() || !names.insert(q.name.clone()) {
                return Err(invalid(&key, "queue names must be unique and non-empty"));
            }
            if q.teams == 0 || q.team_size == 0 {
                return Err(invalid(&key, "teams and team_size must be at least 1"));
            }
            let full = u32::from(q.teams) * u32::from(q.team_size);
            if q.min_players == 0 || u32::from(q.min_players) > full {
                return Err(invalid(&key, "min_players must be in 1..=teams*team_size"));
            }
            if q.region_ping_max_ms < q.region_ping_ms {
                return Err(invalid(&key, "region_ping_max_ms must be ≥ region_ping_ms"));
            }
        }
        if self.matchmaking.interval.is_zero() {
            return Err(invalid("matchmaking.interval", "must be positive"));
        }
        Ok(())
    }

    /// The effective config as TOML with secrets redacted (`--check-config`, admin `GET /config`).
    ///
    /// The config never holds secret values (only the env var / file names that do); URL
    /// credentials (`scheme://user:pass@host`) are redacted anyway.
    pub fn redacted_toml(&self) -> String {
        let mut copy = self.clone();
        copy.fleet.player_service_url = redact_url(&copy.fleet.player_service_url);
        copy.fleet.player_service_http = redact_url(&copy.fleet.player_service_http);
        copy.game.public_url = redact_url(&copy.game.public_url);
        toml::to_string_pretty(&copy).unwrap_or_else(|e| format!("# failed to render config: {e}\n"))
    }

    /// Commented example config (`--print-default-config`; mirrors `deploy/server.example.toml`).
    pub fn example_toml() -> &'static str {
        include_str!("../deploy/server.example.toml")
    }
}

fn redact_url(url: &str) -> String {
    if let Some(scheme_end) = url.find("://") {
        let rest = &url[scheme_end + 3..];
        let host_end = rest.find('/').unwrap_or(rest.len());
        if let Some(at) = rest[..host_end].rfind('@') {
            return format!("{}://<redacted>@{}", &url[..scheme_end], &rest[at + 1..]);
        }
    }
    url.to_owned()
}

/// Expand `${VAR}` and `${VAR:-default}` in every string of `table` (recursively).
/// Unknown variables without a default expand to the empty string.
pub fn expand_env_table(table: &mut toml::Table, lookup: &dyn Fn(&str) -> Option<String>) {
    fn value(v: &mut toml::Value, lookup: &dyn Fn(&str) -> Option<String>) {
        match v {
            toml::Value::String(s) => {
                if s.contains("${") {
                    *s = expand_env(s, lookup);
                }
            }
            toml::Value::Array(items) => items.iter_mut().for_each(|i| value(i, lookup)),
            toml::Value::Table(t) => t.iter_mut().for_each(|(_, i)| value(i, lookup)),
            _ => {}
        }
    }
    table.iter_mut().for_each(|(_, v)| value(v, lookup));
}

/// Expand `${VAR}` / `${VAR:-default}` in one string.
pub fn expand_env(s: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let expr = &after[..end];
        let (name, default) = match expr.split_once(":-") {
            Some((n, d)) => (n, Some(d)),
            None => (expr, None),
        };
        match lookup(name).filter(|v| !v.is_empty()) {
            Some(v) => out.push_str(&v),
            None => out.push_str(default.unwrap_or("")),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Merge `top` into `base`: nested tables merge key by key, everything else replaces.
pub fn merge_tables(base: &mut toml::Table, top: toml::Table) {
    for (key, value) in top {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(b)), toml::Value::Table(t)) => merge_tables(b, t),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

/// Apply `SPACE3D__A__B=value` overrides to a parsed TOML table.
///
/// Segments are lower-cased and nested tables are created as needed. The value is parsed as a
/// TOML value (`8080`, `true`, `"x"`, `["a", "b"]`, `{ k = 1 }`), falling back to a plain
/// string (`0.0.0.0:7777`, `game-server`).
pub fn overlay_env(table: &mut toml::Table, vars: impl IntoIterator<Item = (String, String)>) {
    const PREFIX: &str = "SPACE3D__";
    for (name, raw) in vars {
        let Some(path) = name.strip_prefix(PREFIX) else {
            continue;
        };
        let segments: Vec<String> = path
            .split("__")
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
            .collect();
        let Some((last, parents)) = segments.split_last() else {
            continue;
        };
        set_path(table, parents, last, parse_env_value(&raw));
    }
}

fn set_path(table: &mut toml::Table, parents: &[String], last: &str, value: toml::Value) {
    match parents.split_first() {
        None => {
            table.insert(last.to_owned(), value);
        }
        Some((seg, rest)) => {
            let entry = table
                .entry(seg.clone())
                .or_insert_with(|| toml::Value::Table(toml::Table::new()));
            if !entry.is_table() {
                *entry = toml::Value::Table(toml::Table::new());
            }
            if let Some(t) = entry.as_table_mut() {
                set_path(t, rest, last, value);
            }
        }
    }
}

fn parse_env_value(raw: &str) -> toml::Value {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return toml::Value::String(String::new());
    }
    match format!("v = {trimmed}").parse::<toml::Table>() {
        Ok(mut t) => t.remove("v").unwrap_or_else(|| toml::Value::String(raw.to_owned())),
        Err(_) => toml::Value::String(raw.to_owned()),
    }
}

/// Parse `"250ms"`, `"30s"`, `"15m"`, `"24h"`, `"30d"` (also `"us"`, `"ns"`, fractions like
/// `"1.5s"`, and a bare `"0"`).
pub fn parse_duration(s: &str) -> Result<Duration, ConfigError> {
    let bad = || ConfigError::Invalid {
        key: "duration".into(),
        message: format!("`{s}` is not a duration like 250ms, 30s, 15m, 24h or 30d"),
    };
    let t = s.trim();
    if t == "0" {
        return Ok(Duration::ZERO);
    }
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).ok_or_else(bad)?;
    let (num, unit) = t.split_at(split);
    let n: f64 = num.parse().map_err(|_| bad())?;
    let unit_secs = match unit.trim() {
        "ns" => 1e-9,
        "us" | "µs" => 1e-6,
        "ms" => 1e-3,
        "s" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        "d" => 86_400.0,
        _ => return Err(bad()),
    };
    let secs = n * unit_secs;
    if !secs.is_finite() || secs < 0.0 {
        return Err(bad());
    }
    Duration::try_from_secs_f64(secs).map_err(|_| bad())
}

/// Format a duration in the shortest exact unit (`"30s"`).
pub fn format_duration(d: Duration) -> String {
    let ns = d.as_nanos();
    if ns == 0 {
        return "0s".into();
    }
    const UNITS: [(u128, &str); 7] = [
        (86_400_000_000_000, "d"),
        (3_600_000_000_000, "h"),
        (60_000_000_000, "m"),
        (1_000_000_000, "s"),
        (1_000_000, "ms"),
        (1_000, "us"),
        (1, "ns"),
    ];
    for (size, unit) in UNITS {
        if ns.is_multiple_of(size) {
            return format!("{}{unit}", ns / size);
        }
    }
    format!("{ns}ns")
}

/// Parse `"16KiB"`, `"1MiB"`, `"64KB"`, `"1024"` (case-insensitive; `B` optional).
pub fn parse_bytes(s: &str) -> Result<u64, ConfigError> {
    let bad = || ConfigError::Invalid {
        key: "bytes".into(),
        message: format!("`{s}` is not a byte size like 16KiB, 1MiB or 1024"),
    };
    let t = s.trim();
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: u64 = num.parse().map_err(|_| bad())?;
    let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" => 1000,
        "ki" | "kib" => 1024,
        "m" | "mb" => 1_000_000,
        "mi" | "mib" => 1 << 20,
        "g" | "gb" => 1_000_000_000,
        "gi" | "gib" => 1 << 30,
        _ => return Err(bad()),
    };
    n.checked_mul(mult).ok_or_else(bad)
}

/// Serde adapter: durations as strings.
pub mod serde_duration {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    /// Serialize as `"30s"`.
    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::format_duration(*d))
    }

    /// Deserialize from `"30s"`.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let s = String::deserialize(d)?;
        super::parse_duration(&s).map_err(serde::de::Error::custom)
    }
}

/// Serde adapter: byte sizes as integers or strings.
pub mod serde_bytes_size {
    use serde::{Deserialize, Deserializer, Serializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        N(u64),
        S(String),
    }

    /// Serialize as an integer.
    pub fn serialize<S: Serializer>(n: &u64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(*n)
    }

    /// Deserialize from `16384` or `"16KiB"`.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        match Raw::deserialize(d)? {
            Raw::N(n) => Ok(n),
            Raw::S(s) => super::parse_bytes(&s).map_err(serde::de::Error::custom),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    #[test]
    fn durations_and_bytes() {
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("15m").unwrap(), Duration::from_secs(900));
        assert_eq!(parse_duration("24h").unwrap(), Duration::from_secs(86_400));
        assert_eq!(parse_duration("30d").unwrap(), Duration::from_secs(30 * 86_400));
        assert_eq!(parse_duration("1.5s").unwrap(), Duration::from_millis(1500));
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
        assert!(parse_duration("5").is_err());
        assert!(parse_duration("5 parsecs").is_err());
        assert!(parse_duration("-1s").is_err());
        for d in [
            Duration::from_millis(250),
            Duration::from_secs(90),
            Duration::from_secs(3600),
            Duration::from_secs(30 * 86_400),
            Duration::from_micros(5),
        ] {
            assert_eq!(parse_duration(&format_duration(d)).unwrap(), d);
        }
        assert_eq!(format_duration(Duration::from_secs(900)), "15m");
        assert_eq!(parse_bytes("16KiB").unwrap(), 16 * 1024);
        assert_eq!(parse_bytes("1MiB").unwrap(), 1 << 20);
        assert_eq!(parse_bytes("64KB").unwrap(), 64_000);
        assert_eq!(parse_bytes("1024").unwrap(), 1024);
        assert!(parse_bytes("lots").is_err());
        assert!(parse_bytes("99999999999999999999GiB").is_err());
    }

    #[test]
    fn layered_defaults_merge_under_file_and_env() {
        let defaults = "role = \"all-in-one\"\n[game]\nbind = \"127.0.0.1:7000\"\nallow_dev_auth = true\n\
                        [limits]\nmax_connections_per_ip = 64\n";
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("server.toml");
        std::fs::write(&file, "[game]\ntick_hz = 30.0\n").unwrap();
        let cfg = ServerConfig::load_layered(
            Some(defaults),
            Some(&file),
            env(&[("SPACE3D__LIMITS__MAX_CONNECTIONS_PER_IP", "128")]),
        )
        .unwrap();
        // Defaults survive where the file is silent (tables merge key by key).
        assert_eq!(cfg.game.bind, "127.0.0.1:7000".parse().unwrap());
        assert!(cfg.game.allow_dev_auth);
        assert_eq!(cfg.game.tick_hz, 30.0, "file over defaults");
        assert_eq!(cfg.limits.max_connections_per_ip, 128, "env over defaults");
        let plain = ServerConfig::load_layered(Some(defaults), None, Vec::new()).unwrap();
        assert_eq!(plain.game.tick_hz, 60.0);
        assert!(ServerConfig::load_layered(Some("[game"), None, Vec::new()).is_err());
    }

    #[test]
    fn example_config_parses_and_validates() {
        let cfg = ServerConfig::load(None, Vec::new()).unwrap();
        assert_eq!(cfg, {
            let mut d = ServerConfig::default();
            d.finish_expansion();
            d
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.toml");
        std::fs::write(&path, ServerConfig::example_toml()).unwrap();
        let cfg = ServerConfig::load(Some(&path), env(&[("HOSTNAME", "gs-7")])).unwrap();
        assert_eq!(cfg.server_id, "gs-7");
        assert_eq!(cfg.matchmaking.queues.len(), 1);
        assert_eq!(cfg.matchmaking.queues[0].max_wait, Duration::from_secs(90));
        assert_eq!(cfg.limits.max_message_bytes, 16 * 1024);
        // Round trip through the redacted rendering.
        let again = ServerConfig::from_toml_str(&cfg.redacted_toml()).unwrap();
        assert_eq!(again, cfg);
    }

    #[test]
    fn overlay_precedence_file_then_env() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.toml");
        std::fs::write(
            &path,
            "role = \"game-server\"\nserver_id = \"from-file\"\n[game]\ntick_hz = 30.0\nbind = \"127.0.0.1:7000\"\n\
             public_url = \"wss://{server_id}.play.example.com/play\"\n",
        )
        .unwrap();
        let cfg = ServerConfig::load(
            Some(&path),
            env(&[
                ("SPACE3D__SERVER_ID", "gs-${POD_INDEX}"),
                ("POD_INDEX", "3"),
                ("SPACE3D__GAME__TICK_HZ", "128"),
                (
                    "SPACE3D__GAME__ALLOWED_ORIGINS",
                    "[\"https://a.example\", \"https://b.example\"]",
                ),
                ("SPACE3D__GAME__ALLOW_DEV_AUTH", "true"),
                ("SPACE3D__PERSISTENCE__CHECKPOINT_INTERVAL", "5s"),
                ("SPACE3D__LIMITS__MAX_MESSAGE_BYTES", "32KiB"),
                ("SPACE3D_KEYS", "not-an-overlay"),
                ("OTHER__X", "1"),
            ]),
        )
        .unwrap();
        assert_eq!(cfg.role, Role::GameServer);
        assert_eq!(cfg.server_id, "gs-3");
        assert_eq!(cfg.game.tick_hz, 128.0);
        assert_eq!(cfg.game.bind, "127.0.0.1:7000".parse().unwrap());
        assert_eq!(cfg.game.allowed_origins.len(), 2);
        assert!(cfg.game.allow_dev_auth);
        assert_eq!(cfg.game.public_url, "wss://gs-3.play.example.com/play");
        assert_eq!(cfg.persistence.checkpoint_interval, Duration::from_secs(5));
        assert_eq!(cfg.limits.max_message_bytes, 32 * 1024);
    }

    #[test]
    fn env_expansion_defaults() {
        let lookup = |n: &str| (n == "A").then(|| "x".to_owned());
        assert_eq!(expand_env("${A}-${B:-dflt}-${C}", &lookup), "x-dflt-");
        assert_eq!(expand_env("no vars", &lookup), "no vars");
        assert_eq!(expand_env("broken ${A", &lookup), "broken ${A");
    }

    #[test]
    fn validation_rejects_bad_values() {
        let ok = ServerConfig::default();
        ok.validate().unwrap();
        let mut c = ok.clone();
        c.game.allow_dev_auth = true; // default bind is 0.0.0.0
        assert!(matches!(c.validate(), Err(ConfigError::Invalid { key, .. }) if key == "game.allow_dev_auth"));
        c.game.bind = "127.0.0.1:7777".parse().unwrap();
        c.validate().unwrap();
        c.env = Env::Production;
        assert!(c.validate().is_err());
        let mut c = ok.clone();
        c.game.tick_hz = 0.0;
        assert!(c.validate().is_err());
        let mut c = ok.clone();
        c.fleet.heartbeat = Duration::from_secs(20);
        assert!(c.validate().is_err());
        let mut c = ok.clone();
        c.security.trusted_proxies = vec!["10.0.0.0/33".into()];
        assert!(c.validate().is_err());
        let mut c = ok.clone();
        c.matchmaking.queues = vec![QueueConfig::default(), QueueConfig::default()];
        assert!(c.validate().is_err());
        let mut c = ok.clone();
        c.server_id = "bad id".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn bad_file_errors() {
        let err = ServerConfig::load(Some(Path::new("/definitely/not/here.toml")), Vec::new()).unwrap_err();
        assert!(matches!(err, ConfigError::Io { .. }));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "[game\n").unwrap();
        assert!(matches!(
            ServerConfig::load(Some(&path), Vec::new()),
            Err(ConfigError::Parse(_))
        ));
        std::fs::write(&path, "[game]\ntick_hz = \"fast\"\n").unwrap();
        assert!(matches!(
            ServerConfig::load(Some(&path), Vec::new()),
            Err(ConfigError::Parse(_))
        ));
    }
}
