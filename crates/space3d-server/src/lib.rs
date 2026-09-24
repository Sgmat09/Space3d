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
// SKELETON PHASE: most bodies are `todo!()`; the server builder removes this allow.
#![allow(dead_code, unused_variables, unused_imports, clippy::todo, clippy::ptr_arg)]

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
#[cfg(feature = "testing")]
pub mod testing;

pub use config::{ConfigError, Role, ServerConfig};
pub use game::{GameDefinition, NoGame, RecoveryPolicy, RoomSpec};

use clock::UnixClock;
use space3d_core::App;
use std::process::ExitCode;
use std::sync::Arc;

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

/// Process entry point for a game (or `NoGame` for a player-service-only binary).
pub struct ServerMain<G: GameDefinition> {
    game: Arc<G>,
    config: ServerConfig,
    unix_clock: Arc<dyn UnixClock>,
    check_only: bool,
}

impl<G: GameDefinition> ServerMain<G> {
    /// Parse CLI args (`--config <path>` / `SPACE3D_CONFIG`, `--role`, `--check-config`,
    /// `--print-default-config`) and load the TOML file with the `SPACE3D__A__B` env overlay.
    pub fn from_env(game: G) -> Result<Self, ServerError> {
        todo!()
    }

    /// Use an explicit, already-loaded config (tests, embedding).
    pub fn with_config(game: G, config: ServerConfig) -> Self {
        Self {
            game: Arc::new(game),
            config,
            unix_clock: Arc::new(clock::SystemUnixClock),
            check_only: false,
        }
    }

    /// Replace the wall clock used for tokens, leases and limits (tests).
    pub fn unix_clock(mut self, clock: Arc<dyn UnixClock>) -> Self {
        self.unix_clock = clock;
        self
    }

    /// The config in use.
    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    /// Build the **supervisor** `App` (headless, 10 Hz): lifecycle, game-server and/or
    /// player-service plugins per `config.role`; listeners are bound here (port 0 works; see
    /// `gs::GameServerHandle` / `ps::PlayerServiceHandle` resources for the real addresses).
    /// Tests step it with `App::update`; [`Self::run`] runs it until shutdown.
    pub fn build(self) -> Result<App, ServerError> {
        todo!()
    }

    /// Build and run until SIGTERM/SIGINT finishes the drain. Exit codes: 0 clean, 1 forced
    /// (second signal), 2 config/startup error.
    pub fn run(self) -> ExitCode {
        todo!()
    }
}

/// `use space3d_server::prelude::*;`
pub mod prelude {
    pub use crate::game::{GameDefinition, NoGame, RecoveryPolicy, RoomSpec};
    pub use crate::gs::room::{MatchResult, PlayerResult, RoomContext, RoomControl};
    pub use crate::{Role, ServerConfig, ServerError, ServerMain};
}
