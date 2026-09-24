use std::borrow::Cow;

/// One schema migration. Applied in ascending `version` order, each inside its
/// own transaction, and recorded in the `schema_migrations` table so re-running
/// the same list is a no-op.
///
/// ```
/// use space3d_db::Migration;
/// const MIGRATIONS: &[Migration] = &[
///     Migration::new(1, "create_players", "CREATE TABLE players(id INTEGER PRIMARY KEY, name TEXT NOT NULL);"),
///     Migration::new(2, "add_level", "ALTER TABLE players ADD COLUMN level INTEGER NOT NULL DEFAULT 1;"),
/// ];
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Migration {
    /// Unique, ordered version number (e.g. `1, 2, 3` or `20260924`).
    pub version: i64,
    /// Human-readable name, stored alongside the version.
    pub name: Cow<'static, str>,
    /// SQL executed as a batch (may contain several statements).
    pub up_sql: Cow<'static, str>,
}

impl Migration {
    /// Const constructor for static migration lists.
    pub const fn new(version: i64, name: &'static str, up_sql: &'static str) -> Self {
        Self {
            version,
            name: Cow::Borrowed(name),
            up_sql: Cow::Borrowed(up_sql),
        }
    }

    /// Constructor for migrations built at runtime (e.g. loaded from files).
    pub fn owned(version: i64, name: impl Into<String>, up_sql: impl Into<String>) -> Self {
        Self {
            version,
            name: Cow::Owned(name.into()),
            up_sql: Cow::Owned(up_sql.into()),
        }
    }
}

/// Outcome of [`crate::Db::migrate`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MigrationReport {
    /// Versions applied by this run (empty when already up to date).
    pub applied: Vec<i64>,
    /// Highest applied version after the run (`None` if no migrations were ever
    /// applied, and always `None` on the web backend, which has no SQL).
    pub current_version: Option<i64>,
}
