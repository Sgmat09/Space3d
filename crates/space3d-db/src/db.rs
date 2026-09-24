use crate::error::{DbError, Result};
use crate::migrate::{Migration, MigrationReport};
use crate::store::Store;
use crate::stores::{KvStore, Leaderboard, SaveSlots};
use crate::value::{IntoParams, Rows, Value};
use serde::de::DeserializeOwned;
use space3d_tasks::Task;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Path that selects an in-memory database (native) or in-memory storage (web).
pub const MEMORY_PATH: &str = ":memory:";

/// Options for [`Db::open_with`].
#[derive(Clone, Debug)]
pub struct DbConfig {
    /// Database file (native) / `localStorage` namespace (web). [`MEMORY_PATH`] for in-memory.
    pub path: PathBuf,
    /// Maximum number of cached prepared statements (native).
    pub statement_cache_capacity: usize,
    /// How long SQLite waits on a locked database before failing (native).
    pub busy_timeout: Duration,
    /// Enable write-ahead logging (native, file databases only).
    pub wal: bool,
}

impl DbConfig {
    /// Defaults for a database at `path`: WAL on, 128 cached statements, 5 s busy timeout.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            statement_cache_capacity: 128,
            busy_timeout: Duration::from_secs(5),
            wal: true,
        }
    }

    /// Defaults for an in-memory database.
    pub fn in_memory() -> Self {
        Self::new(MEMORY_PATH)
    }

    /// `true` if [`Self::path`] is [`MEMORY_PATH`].
    pub fn is_in_memory(&self) -> bool {
        self.path.as_os_str() == MEMORY_PATH
    }
}

/// Which storage engine a [`Db`] runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    /// SQLite on a dedicated worker thread (native). Full API.
    Sqlite,
    /// Browser `localStorage` (web). Stores only; raw SQL returns [`DbError::Unsupported`].
    LocalStorage,
    /// Process-local memory with the web backend's semantics (web `:memory:`). Stores only.
    Memory,
}

pub(crate) enum Backend {
    #[cfg(not(target_arch = "wasm32"))]
    Sqlite(crate::native::Worker),
    #[cfg(any(test, target_arch = "wasm32"))]
    Web(crate::web::WebBackend),
}

struct DbInner {
    backend: Backend,
    path: PathBuf,
}

/// Handle to a database. Cheap to clone; store it as a resource (see [`crate::DbPlugin`]).
///
/// **Native:** SQLite (bundled) owned by a dedicated `space3d-db` worker
/// thread. Every call enqueues a job and immediately returns a
/// [`Task`] — poll it from a system, attach a callback with
/// `AsyncQueue::track`, or `.wait().await` it from async code. Jobs run in
/// submission order, so a write followed by a read observes the write.
/// Dropping the last handle shuts the worker down after pending jobs finish.
///
/// **Web:** [`KvStore`], [`SaveSlots`] and [`Leaderboard`] are backed by
/// `localStorage`; [`Db::execute`], [`Db::query`], [`Db::transaction`] and
/// friends return [`DbError::Unsupported`] and migrations are skipped.
#[derive(Clone)]
pub struct Db {
    inner: Arc<DbInner>,
}

impl fmt::Debug for Db {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Db")
            .field("path", &self.inner.path)
            .field("backend", &self.backend())
            .finish()
    }
}

#[cfg(any(test, target_arch = "wasm32"))]
const WEB_SQL: &str = "raw SQL is not available on the web backend; use KvStore / SaveSlots / Leaderboard";

impl Db {
    /// Open (or create) the database at `path` with default [`DbConfig`].
    /// Returns immediately; opening happens on the worker, and an open failure
    /// is reported as [`DbError::Open`] by every subsequent call.
    pub fn open(path: impl AsRef<Path>) -> Self {
        Self::open_with(DbConfig::new(path))
    }

    /// Open a private in-memory database (ideal for tests).
    pub fn open_in_memory() -> Self {
        Self::open_with(DbConfig::in_memory())
    }

    /// Open with explicit configuration.
    pub fn open_with(config: DbConfig) -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        let backend = Backend::Sqlite(crate::native::Worker::spawn(&config));
        #[cfg(target_arch = "wasm32")]
        let backend = Backend::Web(crate::web::WebBackend::for_config(&config));
        Self {
            inner: Arc::new(DbInner {
                backend,
                path: config.path,
            }),
        }
    }

    /// A native-test-only handle using the web backend over in-memory storage.
    #[cfg(test)]
    pub(crate) fn open_web_memory() -> Self {
        Self {
            inner: Arc::new(DbInner {
                backend: Backend::Web(crate::web::WebBackend::memory(MEMORY_PATH)),
                path: MEMORY_PATH.into(),
            }),
        }
    }

    /// Path (native) or namespace (web) this database was opened with.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// The storage engine in use.
    pub fn backend(&self) -> BackendKind {
        match &self.inner.backend {
            #[cfg(not(target_arch = "wasm32"))]
            Backend::Sqlite(_) => BackendKind::Sqlite,
            #[cfg(any(test, target_arch = "wasm32"))]
            Backend::Web(w) => w.kind(),
        }
    }

    /// `true` when raw SQL ([`Db::execute`], [`Db::query`], ...) is available.
    pub fn supports_sql(&self) -> bool {
        self.backend() == BackendKind::Sqlite
    }

    // ---- dispatch -------------------------------------------------------

    /// Run `f` against a connection (outside any explicit transaction).
    fn sql<R: Send + 'static>(&self, f: impl FnOnce(&Tx<'_>) -> Result<R> + Send + 'static) -> Pending<R> {
        match &self.inner.backend {
            #[cfg(not(target_arch = "wasm32"))]
            Backend::Sqlite(w) => Pending::Waiting(w.submit(move |conn| f(&Tx::new(conn)))),
            #[cfg(any(test, target_arch = "wasm32"))]
            Backend::Web(_) => {
                drop(f);
                Pending::Ready(Err(DbError::Unsupported(WEB_SQL)))
            }
        }
    }

    /// Run `f` inside `BEGIN IMMEDIATE ... COMMIT`; any `Err` (or panic) rolls back.
    fn sql_tx<R: Send + 'static>(&self, f: impl FnOnce(&Tx<'_>) -> Result<R> + Send + 'static) -> Pending<R> {
        match &self.inner.backend {
            #[cfg(not(target_arch = "wasm32"))]
            Backend::Sqlite(w) => Pending::Waiting(w.submit(move |conn| {
                let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let out = f(&Tx::new(&tx))?;
                tx.commit()?;
                Ok(out)
            })),
            #[cfg(any(test, target_arch = "wasm32"))]
            Backend::Web(_) => {
                drop(f);
                Pending::Ready(Err(DbError::Unsupported(WEB_SQL)))
            }
        }
    }

    /// Run a high-level store operation on whichever backend is active.
    pub(crate) fn store<R: Send + 'static>(
        &self,
        f: impl FnOnce(&dyn Store) -> Result<R> + Send + 'static,
    ) -> Pending<R> {
        match &self.inner.backend {
            #[cfg(not(target_arch = "wasm32"))]
            Backend::Sqlite(w) => Pending::Waiting(w.submit(move |conn| f(&*conn))),
            #[cfg(any(test, target_arch = "wasm32"))]
            Backend::Web(w) => Pending::Ready(f(w)),
        }
    }

    fn execute_pending(&self, sql: String, params: Vec<Value>) -> Pending<usize> {
        self.sql(move |c| c.execute(&sql, params))
    }

    fn execute_batch_pending(&self, sql: String) -> Pending<()> {
        self.sql(move |c| c.execute_batch(&sql))
    }

    fn query_pending<T: DeserializeOwned + Send + 'static>(&self, sql: String, params: Vec<Value>) -> Pending<Vec<T>> {
        self.sql(move |c| c.query(&sql, params))
    }

    fn query_one_pending<T: DeserializeOwned + Send + 'static>(
        &self,
        sql: String,
        params: Vec<Value>,
    ) -> Pending<Option<T>> {
        self.sql(move |c| c.query_one(&sql, params))
    }

    fn query_raw_pending(&self, sql: String, params: Vec<Value>) -> Pending<Rows> {
        self.sql(move |c| c.query_raw(&sql, params))
    }

    fn migrate_pending(&self, migrations: Vec<Migration>) -> Pending<MigrationReport> {
        match &self.inner.backend {
            #[cfg(not(target_arch = "wasm32"))]
            Backend::Sqlite(w) => {
                Pending::Waiting(w.submit(move |conn| crate::native::apply_migrations(conn, &migrations)))
            }
            #[cfg(any(test, target_arch = "wasm32"))]
            Backend::Web(_) => {
                if !migrations.is_empty() {
                    tracing::debug!(count = migrations.len(), "web db backend: skipping SQL migrations");
                }
                Pending::Ready(Ok(MigrationReport::default()))
            }
        }
    }

    // ---- public async API -----------------------------------------------

    /// Execute one statement (INSERT/UPDATE/DELETE/DDL); resolves to the number of changed rows.
    /// Statements go through the prepared-statement cache.
    pub fn execute(&self, sql: impl Into<String>, params: impl IntoParams) -> Task<Result<usize>> {
        self.execute_pending(sql.into(), params.into_params()).into_task()
    }

    /// Execute several `;`-separated statements without parameters.
    pub fn execute_batch(&self, sql: impl Into<String>) -> Task<Result<()>> {
        self.execute_batch_pending(sql.into()).into_task()
    }

    /// Run a query and map every row onto `T` by column name (see [`crate::Row::deserialize`]).
    ///
    /// ```
    /// # use space3d_db::Db;
    /// #[derive(serde::Deserialize)]
    /// struct Answer { n: i64 }
    /// let db = Db::open_in_memory();
    /// let rows = db.query::<Answer>("SELECT ?1 * 2 AS n", (21,)).block().unwrap().unwrap();
    /// assert_eq!(rows[0].n, 42);
    /// ```
    pub fn query<T: DeserializeOwned + Send + 'static>(
        &self,
        sql: impl Into<String>,
        params: impl IntoParams,
    ) -> Task<Result<Vec<T>>> {
        self.query_pending(sql.into(), params.into_params()).into_task()
    }

    /// Like [`Db::query`] but returns only the first row (if any).
    pub fn query_one<T: DeserializeOwned + Send + 'static>(
        &self,
        sql: impl Into<String>,
        params: impl IntoParams,
    ) -> Task<Result<Option<T>>> {
        self.query_one_pending(sql.into(), params.into_params()).into_task()
    }

    /// Run a query and return untyped [`Rows`].
    pub fn query_raw(&self, sql: impl Into<String>, params: impl IntoParams) -> Task<Result<Rows>> {
        self.query_raw_pending(sql.into(), params.into_params()).into_task()
    }

    /// Run `f` inside a transaction on the worker thread. Returning `Err`
    /// (e.g. `Err(DbError::other("abort"))`) or panicking rolls everything back;
    /// `Ok` commits.
    ///
    /// ```
    /// # use space3d_db::{Db, DbError};
    /// let db = Db::open_in_memory();
    /// let n = db.transaction(|tx| {
    ///     tx.execute_batch("CREATE TABLE t(x INTEGER)")?;
    ///     for i in 0..10 { tx.execute("INSERT INTO t(x) VALUES (?1)", (i,))?; }
    ///     tx.query_one::<i64>("SELECT count(*) FROM t", ()).map(|n| n.unwrap_or(0))
    /// }).block().unwrap().unwrap();
    /// assert_eq!(n, 10);
    /// ```
    pub fn transaction<R, F>(&self, f: F) -> Task<Result<R>>
    where
        R: Send + 'static,
        F: FnOnce(&Tx<'_>) -> Result<R> + Send + 'static,
    {
        self.sql_tx(f).into_task()
    }

    /// Apply `migrations` (idempotent; see [`Migration`]). [`crate::DbPlugin`] calls this at startup.
    pub fn migrate(&self, migrations: impl IntoIterator<Item = Migration>) -> Task<Result<MigrationReport>> {
        self.migrate_pending(migrations.into_iter().collect()).into_task()
    }

    /// Run an arbitrary closure with the raw `rusqlite` connection on the worker (native only).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn with_connection<R, F>(&self, f: F) -> Task<Result<R>>
    where
        R: Send + 'static,
        F: FnOnce(&mut rusqlite::Connection) -> Result<R> + Send + 'static,
    {
        match &self.inner.backend {
            Backend::Sqlite(w) => Pending::Waiting(w.submit(f)),
            #[cfg(test)]
            Backend::Web(_) => {
                drop(f);
                Pending::Ready(Err(DbError::Unsupported(WEB_SQL)))
            }
        }
        .into_task()
    }

    /// Synchronous access for tools, CLIs and tests. **Never use from a system** —
    /// it blocks the calling thread until the worker answers.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn blocking(&self) -> BlockingDb<'_> {
        BlockingDb { db: self }
    }

    // ---- stores ----------------------------------------------------------

    /// Typed key/value store in `namespace` (JSON-encoded values).
    pub fn kv(&self, namespace: impl Into<String>) -> KvStore {
        KvStore::new(self.clone(), namespace.into())
    }

    /// Game save slots.
    pub fn save_slots(&self) -> SaveSlots {
        SaveSlots::new(self.clone())
    }

    /// Local leaderboard called `name` (higher scores rank first by default).
    pub fn leaderboard(&self, name: impl Into<String>) -> Leaderboard {
        Leaderboard::new(self.clone(), name.into())
    }
}

/// Blocking facade returned by [`Db::blocking`] (native only; tools/tests only).
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, Debug)]
pub struct BlockingDb<'a> {
    db: &'a Db,
}

#[cfg(not(target_arch = "wasm32"))]
impl BlockingDb<'_> {
    /// Blocking [`Db::execute`].
    pub fn execute(&self, sql: impl Into<String>, params: impl IntoParams) -> Result<usize> {
        self.db.execute_pending(sql.into(), params.into_params()).wait()
    }

    /// Blocking [`Db::execute_batch`].
    pub fn execute_batch(&self, sql: impl Into<String>) -> Result<()> {
        self.db.execute_batch_pending(sql.into()).wait()
    }

    /// Blocking [`Db::query`].
    pub fn query<T: DeserializeOwned + Send + 'static>(
        &self,
        sql: impl Into<String>,
        params: impl IntoParams,
    ) -> Result<Vec<T>> {
        self.db.query_pending(sql.into(), params.into_params()).wait()
    }

    /// Blocking [`Db::query_one`].
    pub fn query_one<T: DeserializeOwned + Send + 'static>(
        &self,
        sql: impl Into<String>,
        params: impl IntoParams,
    ) -> Result<Option<T>> {
        self.db.query_one_pending(sql.into(), params.into_params()).wait()
    }

    /// Blocking [`Db::query_raw`].
    pub fn query_raw(&self, sql: impl Into<String>, params: impl IntoParams) -> Result<Rows> {
        self.db.query_raw_pending(sql.into(), params.into_params()).wait()
    }

    /// Blocking [`Db::transaction`].
    pub fn transaction<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&Tx<'_>) -> Result<R> + Send + 'static,
    {
        self.db.sql_tx(f).wait()
    }

    /// Blocking [`Db::migrate`].
    pub fn migrate(&self, migrations: impl IntoIterator<Item = Migration>) -> Result<MigrationReport> {
        self.db.migrate_pending(migrations.into_iter().collect()).wait()
    }
}

/// A job result that is either already known or will arrive from the worker.
pub(crate) enum Pending<R> {
    #[cfg(not(target_arch = "wasm32"))]
    Waiting(futures::channel::oneshot::Receiver<Result<R>>),
    Ready(Result<R>),
}

impl<R: Send + 'static> Pending<R> {
    #[cfg_attr(target_arch = "wasm32", allow(clippy::infallible_destructuring_match))]
    async fn resolve(self) -> Result<R> {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Pending::Waiting(rx) => rx.await.unwrap_or(Err(DbError::WorkerGone)),
            Pending::Ready(r) => r,
        }
    }

    pub(crate) fn into_task(self) -> Task<Result<R>> {
        space3d_tasks::spawn(self.resolve())
    }

    /// Chain a cheap synchronous conversion onto the result (runs on the task runtime).
    pub(crate) fn map_task<U: Send + 'static>(
        self,
        f: impl FnOnce(R) -> Result<U> + Send + 'static,
    ) -> Task<Result<U>> {
        space3d_tasks::spawn(async move { self.resolve().await.and_then(f) })
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn wait(self) -> Result<R> {
        match self {
            Pending::Waiting(rx) => futures::executor::block_on(rx).unwrap_or(Err(DbError::WorkerGone)),
            Pending::Ready(r) => r,
        }
    }
}

/// A connection handle passed to [`Db::transaction`] closures (runs on the worker thread).
///
/// On the web backend this type is never constructed.
pub struct Tx<'a> {
    #[cfg(not(target_arch = "wasm32"))]
    conn: &'a rusqlite::Connection,
    #[cfg(target_arch = "wasm32")]
    never: std::convert::Infallible,
    #[cfg(target_arch = "wasm32")]
    _lt: std::marker::PhantomData<&'a ()>,
}

impl fmt::Debug for Tx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Tx")
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl<'a> Tx<'a> {
    pub(crate) fn new(conn: &'a rusqlite::Connection) -> Self {
        Self { conn }
    }

    /// The underlying `rusqlite` connection (native only).
    pub fn connection(&self) -> &'a rusqlite::Connection {
        self.conn
    }
}

macro_rules! tx_body {
    ($self:ident, $native:expr, $($unused:expr),*) => {{
        #[cfg(not(target_arch = "wasm32"))]
        {
            $native
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = ($($unused,)*);
            match $self.never {}
        }
    }};
}

impl Tx<'_> {
    /// Execute one statement; returns changed rows.
    pub fn execute(&self, sql: &str, params: impl IntoParams) -> Result<usize> {
        tx_body!(
            self,
            crate::native::execute(self.conn, sql, &params.into_params()),
            sql,
            params
        )
    }

    /// Execute several `;`-separated statements without parameters.
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        tx_body!(self, self.conn.execute_batch(sql).map_err(DbError::from), sql)
    }

    /// Query and deserialize every row.
    pub fn query<T: DeserializeOwned>(&self, sql: &str, params: impl IntoParams) -> Result<Vec<T>> {
        tx_body!(
            self,
            crate::native::query_with(self.conn, sql, &params.into_params(), None, |r| r.deserialize()),
            sql,
            params
        )
    }

    /// Query and deserialize the first row, if any.
    pub fn query_one<T: DeserializeOwned>(&self, sql: &str, params: impl IntoParams) -> Result<Option<T>> {
        tx_body!(
            self,
            crate::native::query_with(self.conn, sql, &params.into_params(), Some(1), |r| r.deserialize())
                .map(|v| v.into_iter().next()),
            sql,
            params
        )
    }

    /// Query into untyped [`Rows`].
    pub fn query_raw(&self, sql: &str, params: impl IntoParams) -> Result<Rows> {
        tx_body!(
            self,
            crate::native::query_raw(self.conn, sql, &params.into_params()),
            sql,
            params
        )
    }

    /// Rowid of the most recent successful INSERT on this connection.
    pub fn last_insert_rowid(&self) -> i64 {
        tx_body!(self, self.conn.last_insert_rowid(),)
    }
}
