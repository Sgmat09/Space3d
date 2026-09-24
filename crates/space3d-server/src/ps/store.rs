//! Player storage behind a trait (SQLite via `space3d-db` today; Postgres later for a
//! multi-instance player service). Internal to the server builder: shape may evolve.
//!
//! Writes from game servers are idempotent: each op runs in **one transaction** that checks
//! `applied_ops(op_id)`, applies, and records the op id.

use crate::gs::fleet::DataWrite;
use crate::gs::room::MatchResult;
use crate::security::bans::Ban;
use serde::{Deserialize, Serialize};
use space3d_replication::PlayerId;
use space3d_replication::api::{AccountKind, PlayerDataEntry};
use space3d_tasks::Task;

/// Storage failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// Unique constraint (username taken, ...).
    #[error("conflict: {0}")]
    Conflict(String),
    /// Optimistic version mismatch.
    #[error("version mismatch")]
    VersionMismatch,
    /// Not found.
    #[error("not found")]
    NotFound,
    /// Database failure.
    #[error("db: {0}")]
    Db(String),
}

/// An account row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Account {
    /// Id.
    pub id: PlayerId,
    /// Kind.
    pub kind: AccountKind,
    /// Display name.
    pub display_name: String,
    /// Username (password accounts).
    pub username: Option<String>,
    /// Elo rating.
    pub rating: f64,
    /// Created (unix ms).
    pub created_ms: i64,
    /// Banned until (unix ms).
    pub banned_until_ms: Option<i64>,
}

/// A stored checkpoint: `(saved unix ms, bytes)`.
pub type StoredCheckpoint = (i64, Vec<u8>);

/// Result of a refresh-token rotation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rotation {
    /// Rotated; the new token hash was stored in the same family.
    Rotated {
        /// Account.
        player: PlayerId,
        /// Family / session id.
        family: [u8; 16],
    },
    /// The presented token was already rotated: the whole family is revoked (theft signal).
    ReuseDetected,
    /// Unknown / expired / revoked.
    Invalid,
}

/// Storage operations used by the player service.
pub trait PlayerStore: Send + Sync + 'static {
    /// Find the guest bound to `device_hash` (SHA-256 of `device_id:device_secret`) or create it.
    fn guest(
        &self,
        device_hash: [u8; 32],
        display_name: Option<String>,
        now_ms: i64,
    ) -> Task<Result<Account, StoreError>>;
    /// Create a password account (or upgrade `upgrade` guest).
    fn create_password(
        &self,
        username: String,
        phc: String,
        display_name: String,
        upgrade: Option<PlayerId>,
        now_ms: i64,
    ) -> Task<Result<Account, StoreError>>;
    /// Account + PHC hash by username.
    fn by_username(&self, username: String) -> Task<Result<Option<(Account, String)>, StoreError>>;
    /// Account by id.
    fn account(&self, id: PlayerId) -> Task<Result<Option<Account>, StoreError>>;
    /// Rename.
    fn set_display_name(&self, id: PlayerId, name: String) -> Task<Result<Account, StoreError>>;
    /// Store a new refresh token hash in `family`.
    fn insert_refresh(
        &self,
        player: PlayerId,
        family: [u8; 16],
        token_hash: [u8; 32],
        expires_ms: i64,
    ) -> Task<Result<(), StoreError>>;
    /// Rotate `old_hash` → `new_hash`.
    fn rotate_refresh(
        &self,
        old_hash: [u8; 32],
        new_hash: [u8; 32],
        expires_ms: i64,
        now_ms: i64,
    ) -> Task<Result<Rotation, StoreError>>;
    /// Revoke a family (logout / theft).
    fn revoke_family(&self, family: [u8; 16]) -> Task<Result<(), StoreError>>;
    /// Player data by keys.
    fn get_data(&self, player: PlayerId, keys: Vec<String>) -> Task<Result<Vec<PlayerDataEntry>, StoreError>>;
    /// Idempotent data writes (op id).
    fn apply_writes(
        &self,
        op_id: String,
        player: PlayerId,
        writes: Vec<DataWrite>,
        now_ms: i64,
    ) -> Task<Result<Vec<PlayerDataEntry>, StoreError>>;
    /// Idempotent match result: history, ratings (Elo), leaderboard scores.
    fn apply_match(&self, op_id: String, result: MatchResult, now_ms: i64) -> Task<Result<(), StoreError>>;
    /// Idempotent leaderboard submission.
    fn submit_score(&self, op_id: String, board: String, player: PlayerId, score: i64) -> Task<Result<(), StoreError>>;
    /// Save a room checkpoint (split mode).
    fn put_checkpoint(
        &self,
        room: space3d_replication::RoomId,
        bytes: Vec<u8>,
        now_ms: i64,
    ) -> Task<Result<(), StoreError>>;
    /// Load a room checkpoint.
    fn get_checkpoint(&self, room: space3d_replication::RoomId) -> Task<Result<Option<StoredCheckpoint>, StoreError>>;
    /// All bans.
    fn bans(&self) -> Task<Result<Vec<Ban>, StoreError>>;
    /// Add a ban (returns it with its id).
    fn add_ban(&self, ban: Ban) -> Task<Result<Ban, StoreError>>;
    /// Remove a ban.
    fn remove_ban(&self, id: u64) -> Task<Result<bool, StoreError>>;
    /// Append to the audit log.
    fn audit(
        &self,
        actor: String,
        action: String,
        detail: serde_json::Value,
        now_ms: i64,
    ) -> Task<Result<(), StoreError>>;
}

/// SQLite implementation.
pub struct SqliteStore {
    /// Database (worker thread).
    pub db: space3d_db::Db,
}

macro_rules! todo_fn {
    ($($name:ident ( $($arg:ident : $ty:ty),* ) -> $ret:ty;)*) => {
        impl PlayerStore for SqliteStore {
            $(fn $name(&self, $($arg: $ty),*) -> $ret { todo!() })*
        }
    };
}

todo_fn! {
    guest(device_hash: [u8; 32], display_name: Option<String>, now_ms: i64) -> Task<Result<Account, StoreError>>;
    create_password(username: String, phc: String, display_name: String, upgrade: Option<PlayerId>, now_ms: i64) -> Task<Result<Account, StoreError>>;
    by_username(username: String) -> Task<Result<Option<(Account, String)>, StoreError>>;
    account(id: PlayerId) -> Task<Result<Option<Account>, StoreError>>;
    set_display_name(id: PlayerId, name: String) -> Task<Result<Account, StoreError>>;
    insert_refresh(player: PlayerId, family: [u8; 16], token_hash: [u8; 32], expires_ms: i64) -> Task<Result<(), StoreError>>;
    rotate_refresh(old_hash: [u8; 32], new_hash: [u8; 32], expires_ms: i64, now_ms: i64) -> Task<Result<Rotation, StoreError>>;
    revoke_family(family: [u8; 16]) -> Task<Result<(), StoreError>>;
    get_data(player: PlayerId, keys: Vec<String>) -> Task<Result<Vec<PlayerDataEntry>, StoreError>>;
    apply_writes(op_id: String, player: PlayerId, writes: Vec<DataWrite>, now_ms: i64) -> Task<Result<Vec<PlayerDataEntry>, StoreError>>;
    apply_match(op_id: String, result: MatchResult, now_ms: i64) -> Task<Result<(), StoreError>>;
    submit_score(op_id: String, board: String, player: PlayerId, score: i64) -> Task<Result<(), StoreError>>;
    put_checkpoint(room: space3d_replication::RoomId, bytes: Vec<u8>, now_ms: i64) -> Task<Result<(), StoreError>>;
    get_checkpoint(room: space3d_replication::RoomId) -> Task<Result<Option<StoredCheckpoint>, StoreError>>;
    bans() -> Task<Result<Vec<Ban>, StoreError>>;
    add_ban(ban: Ban) -> Task<Result<Ban, StoreError>>;
    remove_ban(id: u64) -> Task<Result<bool, StoreError>>;
    audit(actor: String, action: String, detail: serde_json::Value, now_ms: i64) -> Task<Result<(), StoreError>>;
}
