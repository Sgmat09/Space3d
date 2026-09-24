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
        todo!()
    }

    /// Parse TOML text (no env overlay, no validation).
    pub fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    /// Semantic checks (ports, rates, dev auth only on loopback, production secrets present).
    pub fn validate(&self) -> Result<(), ConfigError> {
        todo!()
    }

    /// The effective config as TOML with secrets redacted (`--check-config`, admin `GET /config`).
    pub fn redacted_toml(&self) -> String {
        todo!()
    }

    /// Commented example config (`--print-default-config`; mirrors `deploy/server.example.toml`).
    pub fn example_toml() -> &'static str {
        include_str!("../deploy/server.example.toml")
    }
}

/// Apply `SPACE3D__A__B=value` overrides to a parsed TOML table.
pub fn overlay_env(table: &mut toml::Table, vars: impl IntoIterator<Item = (String, String)>) {
    todo!()
}

/// Parse `"250ms"`, `"30s"`, `"15m"`, `"24h"`, `"30d"`.
pub fn parse_duration(s: &str) -> Result<Duration, ConfigError> {
    todo!()
}

/// Format a duration in the shortest exact unit (`"30s"`).
pub fn format_duration(d: Duration) -> String {
    todo!()
}

/// Parse `"16KiB"`, `"1MiB"`, `"64KB"`, `"1024"`.
pub fn parse_bytes(s: &str) -> Result<u64, ConfigError> {
    todo!()
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
