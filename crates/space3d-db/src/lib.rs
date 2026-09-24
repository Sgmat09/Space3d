//! Space3d database: SQLite that never blocks the frame.
//!
//! * **Native** — SQLite (bundled via `rusqlite`) lives on a dedicated
//!   `space3d-db` worker thread. [`Db`] methods enqueue a job and return a
//!   [`space3d_tasks::Task`] immediately; systems poll it (or attach an
//!   `AsyncQueue` callback) so a slow query never stalls the game loop.
//!   Connections use WAL mode, `synchronous=NORMAL`, foreign keys and a
//!   prepared-statement cache. `Db::blocking` (native only) exists for tools and tests.
//! * **Web** (`wasm32`) — the same API over `localStorage`. The high-level
//!   stores ([`KvStore`], [`SaveSlots`], [`Leaderboard`]) work; raw SQL
//!   ([`Db::execute`], [`Db::query`], [`Db::transaction`], ...) returns
//!   [`DbError::Unsupported`] and SQL migrations are skipped (reported as an
//!   empty [`MigrationReport`]). Open `:memory:` on web for non-persistent
//!   in-process storage.
//!
//! ```no_run
//! use space3d_core::prelude::*;
//! use space3d_db::{Db, DbPlugin, Migration};
//!
//! let mut app = App::new();
//! app.add_plugin(DbPlugin::new("saves/game.db").with_migration(Migration::new(
//!     1,
//!     "create_inventory",
//!     "CREATE TABLE inventory(item TEXT PRIMARY KEY, count INTEGER NOT NULL);",
//! )));
//! let db = app.resources.expect::<Db>().clone();
//! let mut task = db.query::<(String, i64)>("SELECT item, count FROM inventory", ());
//! app.add_system(Stage::Update, move |_ctx| {
//!     if let Some(Ok(items)) = task.poll() {
//!         println!("{items:?}");
//!     }
//! });
//! ```
//!
//! Tables named `space3d_*` are reserved for the built-in stores, and
//! `schema_migrations` records applied [`Migration`]s.

#![warn(missing_docs)]

mod db;
mod de;
mod error;
mod migrate;
#[cfg(not(target_arch = "wasm32"))]
mod native;
mod store;
mod stores;
mod value;
#[cfg(any(test, target_arch = "wasm32"))]
mod web;

#[cfg(not(target_arch = "wasm32"))]
pub use db::BlockingDb;
pub use db::{BackendKind, Db, DbConfig, MEMORY_PATH, Tx};
pub use error::{DbError, Result};
pub use migrate::{Migration, MigrationReport};
pub use stores::{
    KvStore, Leaderboard, LeaderboardEntry, NewSave, SaveGame, SaveMeta, SaveSlots, ScoreOrder, Submission,
};
pub use value::{IntoParams, Row, Rows, Value};

#[cfg(not(target_arch = "wasm32"))]
pub use rusqlite;

use space3d_core::{App, Plugin};
use space3d_tasks::{AsyncQueue, TasksPlugin};
use std::path::PathBuf;

/// Unix time in milliseconds (works on web too).
pub(crate) fn now_ms() -> i64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Opens a [`Db`] at startup, inserts it as a resource and applies migrations
/// on the worker. Progress is visible through the [`DbStatus`] resource.
///
/// Adds [`TasksPlugin`] if it is not present yet.
#[derive(Clone, Debug)]
pub struct DbPlugin {
    /// Database file (native) or `localStorage` namespace (web); `":memory:"` for in-memory.
    pub path: PathBuf,
    /// Migrations applied (idempotently) at startup.
    pub migrations: Vec<Migration>,
}

impl Default for DbPlugin {
    fn default() -> Self {
        Self::new("space3d.db")
    }
}

impl DbPlugin {
    /// Database at `path`, no migrations.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            migrations: Vec::new(),
        }
    }

    /// In-memory database (tests, tools).
    pub fn in_memory() -> Self {
        Self::new(MEMORY_PATH)
    }

    /// Add one migration.
    pub fn with_migration(mut self, migration: Migration) -> Self {
        self.migrations.push(migration);
        self
    }

    /// Add several migrations.
    pub fn with_migrations(mut self, migrations: impl IntoIterator<Item = Migration>) -> Self {
        self.migrations.extend(migrations);
        self
    }
}

/// Startup state of the database opened by [`DbPlugin`].
#[derive(Clone, Debug, PartialEq)]
pub enum DbStatus {
    /// Opening / migrating on the worker.
    Opening,
    /// Open and migrated.
    Ready(MigrationReport),
    /// Opening or a migration failed; the message explains why.
    Failed(String),
}

impl DbStatus {
    /// `true` once the database is open and migrated.
    pub fn is_ready(&self) -> bool {
        matches!(self, DbStatus::Ready(_))
    }
}

impl Plugin for DbPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugin(TasksPlugin);
        let db = Db::open(&self.path);
        let migrated = db.migrate(self.migrations.iter().cloned());
        app.insert_resource(DbStatus::Opening);
        app.resources.expect_mut::<AsyncQueue>().track(migrated, |ctx, result| {
            let status = match result {
                Ok(report) => DbStatus::Ready(report),
                Err(e) => {
                    tracing::error!(error = %e, "database startup failed");
                    DbStatus::Failed(e.to_string())
                }
            };
            ctx.resources.insert(status);
        });
        app.insert_resource(db);
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;
