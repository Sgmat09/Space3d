use std::fmt::Display;

/// Result alias used across `space3d-db`.
pub type Result<T, E = DbError> = std::result::Result<T, E>;

/// Everything that can go wrong talking to the database.
///
/// The type is `Clone` and platform independent (SQLite errors are flattened
/// into [`DbError::Sqlite`]) so it can be stored in resources/events and
/// matched the same way on native and web.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum DbError {
    /// SQLite reported an error. `code` is the SQLite *extended* result code when known.
    #[error("sqlite error: {message}")]
    Sqlite {
        /// Extended SQLite result code (e.g. `2067` = `SQLITE_CONSTRAINT_UNIQUE`).
        code: Option<i32>,
        /// Human readable message.
        message: String,
    },
    /// The database could not be opened; every queued job fails with this.
    #[error("failed to open database: {0}")]
    Open(String),
    /// A migration's `up_sql` failed (the migration was rolled back).
    #[error("migration {version} ({name}) failed: {message}")]
    Migration {
        /// Version of the failing migration.
        version: i64,
        /// Name of the failing migration.
        name: String,
        /// Underlying error.
        message: String,
    },
    /// A value could not be (de)serialized to/from a row, JSON or storage entry.
    #[error("(de)serialization failed: {0}")]
    Serde(String),
    /// The operation is not available on this platform/backend
    /// (e.g. raw SQL on the web `localStorage` backend).
    #[error("not supported on this platform: {0}")]
    Unsupported(&'static str),
    /// The web storage backend failed (quota exceeded, storage disabled, ...).
    #[error("storage error: {0}")]
    Storage(String),
    /// The database worker thread has shut down.
    #[error("database worker is gone")]
    WorkerGone,
    /// A job panicked on the worker thread (the worker itself keeps running).
    #[error("database job panicked: {0}")]
    Panicked(String),
    /// Application defined error, e.g. returned from a [`crate::Db::transaction`] closure to force a rollback.
    #[error("{0}")]
    Other(String),
}

impl DbError {
    /// Convenience constructor for [`DbError::Other`].
    pub fn other(msg: impl Display) -> Self {
        Self::Other(msg.to_string())
    }

    /// `true` if this is a SQLite constraint violation (unique, foreign key, check, ...).
    pub fn is_constraint_violation(&self) -> bool {
        // SQLITE_CONSTRAINT = 19; extended codes keep the primary code in the low byte.
        matches!(self, DbError::Sqlite { code: Some(c), .. } if c & 0xff == 19)
    }
}

impl From<serde_json::Error> for DbError {
    fn from(e: serde_json::Error) -> Self {
        Self::Serde(e.to_string())
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl From<rusqlite::Error> for DbError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite {
            code: e.sqlite_error().map(|e| e.extended_code),
            message: e.to_string(),
        }
    }
}

impl serde::de::Error for DbError {
    fn custom<T: Display>(msg: T) -> Self {
        Self::Serde(msg.to_string())
    }
}
