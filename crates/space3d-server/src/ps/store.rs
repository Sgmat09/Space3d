//! Player storage behind a trait (SQLite via `space3d-db` today; Postgres later for a
//! multi-instance player service). Internal to the server builder: shape may evolve.
//!
//! Every call returns a pollable [`Task`]; SQLite work runs on the `space3d-db` worker thread.
//! Writes from game servers are idempotent: each op runs in **one transaction** that checks
//! `applied_ops(op_id)`, applies, and records the op id. Any error rolls the transaction back.

use crate::gs::fleet::DataWrite;
use crate::gs::room::MatchResult;
use crate::security::bans::{Ban, BanTarget};
use crate::security::netaddr::Cidr;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use space3d_db::rusqlite::{self, Connection, OptionalExtension, params};
use space3d_db::{Db, DbError};
use space3d_replication::api::{AccountKind, LeaderboardRow, PlayerDataEntry};
use space3d_replication::{PlayerId, RoomId};
use space3d_tasks::Task;
use std::sync::Arc;

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

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        let db: DbError = e.into();
        db.into()
    }
}

impl From<DbError> for StoreError {
    fn from(e: DbError) -> Self {
        if e.is_constraint_violation() {
            StoreError::Conflict(e.to_string())
        } else {
            StoreError::Db(e.to_string())
        }
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        StoreError::Db(e.to_string())
    }
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
    ReuseDetected {
        /// The revoked family.
        family: [u8; 16],
    },
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
    /// Create a password account (or upgrade the `upgrade` guest).
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
    /// Accounts whose display name or username starts with `prefix` (admin search).
    fn search_accounts(&self, prefix: String, limit: u32) -> Task<Result<Vec<Account>, StoreError>>;
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
    /// Revoke the family of a refresh token (logout); returns the family if the token existed.
    fn revoke_token(&self, token_hash: [u8; 32]) -> Task<Result<Option<[u8; 16]>, StoreError>>;
    /// Revoked families with unexpired tokens (warms the access-token revocation cache).
    fn revoked_families(&self, now_ms: i64) -> Task<Result<Vec<[u8; 16]>, StoreError>>;
    /// Player data by keys (missing keys are absent).
    fn get_data(&self, player: PlayerId, keys: Vec<String>) -> Task<Result<Vec<PlayerDataEntry>, StoreError>>;
    /// Idempotent data writes (op id). `expected_version`: `None` = last writer wins,
    /// `Some(0)` = must not exist, `Some(v)` = must be at version `v`.
    fn apply_writes(
        &self,
        op_id: String,
        player: PlayerId,
        writes: Vec<DataWrite>,
        now_ms: i64,
    ) -> Task<Result<Vec<PlayerDataEntry>, StoreError>>;
    /// Idempotent match result: history, ratings (Elo), leaderboard scores.
    fn apply_match(&self, op_id: String, result: MatchResult, now_ms: i64) -> Task<Result<(), StoreError>>;
    /// Idempotent leaderboard submission (keeps the best score).
    fn submit_score(
        &self,
        op_id: String,
        board: String,
        player: PlayerId,
        score: i64,
        now_ms: i64,
    ) -> Task<Result<(), StoreError>>;
    /// Top `n` of a board, with display names.
    fn leaderboard_top(&self, board: String, n: u32) -> Task<Result<Vec<LeaderboardRow>, StoreError>>;
    /// A player's row on a board.
    fn leaderboard_rank(&self, board: String, player: PlayerId) -> Task<Result<Option<LeaderboardRow>, StoreError>>;
    /// Save a room checkpoint (split mode).
    fn put_checkpoint(&self, room: RoomId, bytes: Vec<u8>, now_ms: i64) -> Task<Result<(), StoreError>>;
    /// Load a room checkpoint.
    fn get_checkpoint(&self, room: RoomId) -> Task<Result<Option<StoredCheckpoint>, StoreError>>;
    /// Delete a room checkpoint (room closed normally).
    fn delete_checkpoint(&self, room: RoomId) -> Task<Result<(), StoreError>>;
    /// Rooms with checkpoints saved at or after `min_saved_ms`.
    fn recent_checkpoints(&self, min_saved_ms: i64) -> Task<Result<Vec<RoomId>, StoreError>>;
    /// All bans.
    fn bans(&self) -> Task<Result<Vec<Ban>, StoreError>>;
    /// Add a ban (`id == 0` assigns one; returns it with its id).
    fn add_ban(&self, ban: Ban, now_ms: i64) -> Task<Result<Ban, StoreError>>;
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
    /// `VACUUM INTO path` (online backup).
    fn backup(&self, path: String) -> Task<Result<(), StoreError>>;
}

/// SQLite implementation.
#[derive(Clone)]
pub struct SqliteStore {
    /// Database (worker thread).
    pub db: Db,
}

impl SqliteStore {
    /// Store over `db` (migrations must have been applied: [`super::schema::migrations`]).
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Run `f` in one immediate transaction; any `Err` rolls back and is returned typed.
    fn tx<U: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> Result<U, StoreError> + Send + 'static,
    ) -> Task<Result<U, StoreError>> {
        let slot: Arc<Mutex<Option<StoreError>>> = Arc::default();
        let inner = slot.clone();
        let task = self.db.transaction(move |tx| {
            f(tx.connection()).map_err(|e| {
                *inner.lock() = Some(e);
                DbError::other("rolled back")
            })
        });
        space3d_tasks::spawn(async move {
            match task.wait().await {
                Some(Ok(v)) => Ok(v),
                Some(Err(e)) => Err(slot.lock().take().unwrap_or_else(|| e.into())),
                None => Err(StoreError::Db("database worker gone".into())),
            }
        })
    }
}

const ACCOUNT_COLS: &str = "id, kind, display_name, username, rating, created_ms, banned_until_ms";

fn account_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Account> {
    let kind: String = r.get(1)?;
    Ok(Account {
        id: PlayerId(r.get::<_, i64>(0)? as u64),
        kind: if kind == "password" {
            AccountKind::Password
        } else {
            AccountKind::Guest
        },
        display_name: r.get(2)?,
        username: r.get(3)?,
        rating: r.get(4)?,
        created_ms: r.get(5)?,
        banned_until_ms: r.get(6)?,
    })
}

fn load_account(c: &Connection, id: PlayerId) -> Result<Option<Account>, StoreError> {
    Ok(
        c.prepare_cached(&format!("SELECT {ACCOUNT_COLS} FROM accounts WHERE id = ?1"))?
            .query_row([id.0 as i64], account_row)
            .optional()?,
    )
}

fn new_player_id(c: &Connection) -> Result<PlayerId, StoreError> {
    loop {
        let id = u64::from_le_bytes(crate::security::random_bytes::<8>()) & (u64::MAX >> 1);
        if id == 0 {
            continue;
        }
        let taken: Option<i64> = c
            .prepare_cached("SELECT id FROM accounts WHERE id = ?1")?
            .query_row([id as i64], |r| r.get(0))
            .optional()?;
        if taken.is_none() {
            return Ok(PlayerId(id));
        }
    }
}

fn op_applied(c: &Connection, op_id: &str) -> Result<bool, StoreError> {
    Ok(c.prepare_cached("SELECT 1 FROM applied_ops WHERE op_id = ?1")?
        .query_row([op_id], |_| Ok(()))
        .optional()?
        .is_some())
}

fn mark_applied(c: &Connection, op_id: &str, now_ms: i64) -> Result<(), StoreError> {
    c.prepare_cached("INSERT INTO applied_ops(op_id, applied_ms) VALUES (?1, ?2)")?
        .execute(params![op_id, now_ms])?;
    Ok(())
}

fn read_data(c: &Connection, player: PlayerId, keys: &[String]) -> Result<Vec<PlayerDataEntry>, StoreError> {
    let mut out = Vec::with_capacity(keys.len());
    let mut stmt = c.prepare_cached("SELECT value, version FROM player_data WHERE account_id = ?1 AND key = ?2")?;
    for key in keys {
        let row: Option<(String, i64)> = stmt
            .query_row(params![player.0 as i64, key], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        if let Some((value, version)) = row {
            out.push(PlayerDataEntry {
                key: key.clone(),
                value: serde_json::from_str(&value)?,
                version,
            });
        }
    }
    Ok(out)
}

/// Keep the best (highest) score — same table and semantics as `space3d_db::Leaderboard`.
fn lb_submit(c: &Connection, board: &str, player: PlayerId, score: i64, now_ms: i64) -> Result<(), StoreError> {
    let key = player.to_string();
    let previous: Option<i64> = c
        .prepare_cached("SELECT score FROM space3d_leaderboard WHERE board = ?1 AND player = ?2")?
        .query_row(params![board, key], |r| r.get(0))
        .optional()?;
    if previous.is_none_or(|p| score > p) {
        c.prepare_cached(
            "INSERT INTO space3d_leaderboard (board, player, score, achieved_at, seq)
             VALUES (?1, ?2, ?3, ?4, (SELECT COALESCE(MAX(seq), 0) + 1 FROM space3d_leaderboard))
             ON CONFLICT (board, player) DO UPDATE SET
                score = excluded.score, achieved_at = excluded.achieved_at, seq = excluded.seq",
        )?
        .execute(params![board, key, score, now_ms])?;
    }
    Ok(())
}

fn display_name(c: &Connection, player: &str) -> Result<String, StoreError> {
    let Ok(id) = player.parse::<PlayerId>() else {
        return Ok(player.to_owned());
    };
    Ok(load_account(c, id)?.map_or_else(|| player.to_owned(), |a| a.display_name))
}

/// `bans` row: `(id, account, cidr, until_ms, reason)`.
type BanRow = (i64, Option<i64>, Option<String>, Option<i64>, String);

fn ban_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<BanRow> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
}

fn to_ban((id, account, cidr, until_ms, reason): BanRow) -> Option<Ban> {
    let target = match (account, cidr) {
        (Some(a), _) => BanTarget::Player(PlayerId(a as u64)),
        (None, Some(c)) => BanTarget::Ip(Cidr::parse(&c).ok()?),
        _ => return None,
    };
    Some(Ban {
        id: id as u64,
        target,
        until_ms,
        reason,
    })
}

fn family_of(bytes: Vec<u8>) -> [u8; 16] {
    let mut f = [0u8; 16];
    let n = bytes.len().min(16);
    f[..n].copy_from_slice(&bytes[..n]);
    f
}

impl PlayerStore for SqliteStore {
    fn guest(
        &self,
        device_hash: [u8; 32],
        display_name: Option<String>,
        now_ms: i64,
    ) -> Task<Result<Account, StoreError>> {
        self.tx(move |c| {
            let existing: Option<i64> = c
                .prepare_cached("SELECT id FROM accounts WHERE device_hash = ?1")?
                .query_row([&device_hash[..]], |r| r.get(0))
                .optional()?;
            let id = match existing {
                Some(id) => {
                    c.prepare_cached("UPDATE accounts SET last_login_ms = ?2 WHERE id = ?1")?
                        .execute(params![id, now_ms])?;
                    PlayerId(id as u64)
                }
                None => {
                    let id = new_player_id(c)?;
                    let name = display_name.unwrap_or_else(|| format!("Guest-{}", &id.to_string()[..6]));
                    c.prepare_cached(
                        "INSERT INTO accounts(id, kind, display_name, device_hash, created_ms, last_login_ms)
                         VALUES (?1, 'guest', ?2, ?3, ?4, ?4)",
                    )?
                    .execute(params![id.0 as i64, name, &device_hash[..], now_ms])?;
                    id
                }
            };
            load_account(c, id)?.ok_or(StoreError::NotFound)
        })
    }

    fn create_password(
        &self,
        username: String,
        phc: String,
        display_name: String,
        upgrade: Option<PlayerId>,
        now_ms: i64,
    ) -> Task<Result<Account, StoreError>> {
        self.tx(move |c| {
            let taken: Option<i64> = c
                .prepare_cached("SELECT id FROM accounts WHERE username = ?1")?
                .query_row([&username], |r| r.get(0))
                .optional()?;
            if taken.is_some() {
                return Err(StoreError::Conflict("username taken".into()));
            }
            let id = match upgrade {
                Some(id) => {
                    let n = c
                        .prepare_cached(
                            "UPDATE accounts SET kind = 'password', username = ?2, password_phc = ?3,
                             display_name = ?4, last_login_ms = ?5 WHERE id = ?1 AND kind = 'guest'",
                        )?
                        .execute(params![id.0 as i64, username, phc, display_name, now_ms])?;
                    if n == 0 {
                        return Err(StoreError::Conflict("account is not an upgradable guest".into()));
                    }
                    id
                }
                None => {
                    let id = new_player_id(c)?;
                    c.prepare_cached(
                        "INSERT INTO accounts(id, kind, display_name, username, password_phc, created_ms, last_login_ms)
                         VALUES (?1, 'password', ?2, ?3, ?4, ?5, ?5)",
                    )?
                    .execute(params![id.0 as i64, display_name, username, phc, now_ms])?;
                    id
                }
            };
            load_account(c, id)?.ok_or(StoreError::NotFound)
        })
    }

    fn by_username(&self, username: String) -> Task<Result<Option<(Account, String)>, StoreError>> {
        self.tx(move |c| {
            let row: Option<(i64, Option<String>)> = c
                .prepare_cached("SELECT id, password_phc FROM accounts WHERE username = ?1")?
                .query_row([&username], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            match row {
                Some((id, Some(phc))) => Ok(load_account(c, PlayerId(id as u64))?.map(|a| (a, phc))),
                _ => Ok(None),
            }
        })
    }

    fn account(&self, id: PlayerId) -> Task<Result<Option<Account>, StoreError>> {
        self.tx(move |c| load_account(c, id))
    }

    fn set_display_name(&self, id: PlayerId, name: String) -> Task<Result<Account, StoreError>> {
        self.tx(move |c| {
            c.prepare_cached("UPDATE accounts SET display_name = ?2 WHERE id = ?1")?
                .execute(params![id.0 as i64, name])?;
            load_account(c, id)?.ok_or(StoreError::NotFound)
        })
    }

    fn search_accounts(&self, prefix: String, limit: u32) -> Task<Result<Vec<Account>, StoreError>> {
        self.tx(move |c| {
            let pattern = format!("{}%", prefix.replace(['%', '_'], ""));
            let mut stmt = c.prepare_cached(&format!(
                "SELECT {ACCOUNT_COLS} FROM accounts WHERE display_name LIKE ?1 OR username LIKE ?1
                 ORDER BY created_ms LIMIT ?2"
            ))?;
            let rows = stmt
                .query_map(params![pattern, limit], account_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    fn insert_refresh(
        &self,
        player: PlayerId,
        family: [u8; 16],
        token_hash: [u8; 32],
        expires_ms: i64,
    ) -> Task<Result<(), StoreError>> {
        self.tx(move |c| {
            c.prepare_cached(
                "INSERT INTO refresh_tokens(token_hash, account_id, family, expires_ms) VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![&token_hash[..], player.0 as i64, &family[..], expires_ms])?;
            Ok(())
        })
    }

    fn rotate_refresh(
        &self,
        old_hash: [u8; 32],
        new_hash: [u8; 32],
        expires_ms: i64,
        now_ms: i64,
    ) -> Task<Result<Rotation, StoreError>> {
        self.tx(move |c| {
            let row: Option<(i64, Vec<u8>, i64, bool, bool)> = c
                .prepare_cached(
                    "SELECT account_id, family, expires_ms, rotated, revoked FROM refresh_tokens WHERE token_hash = ?1",
                )?
                .query_row([&old_hash[..]], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })
                .optional()?;
            let Some((account, family, expires, rotated, revoked)) = row else {
                return Ok(Rotation::Invalid);
            };
            let family = family_of(family);
            if revoked {
                return Ok(Rotation::Invalid);
            }
            if rotated {
                c.prepare_cached("UPDATE refresh_tokens SET revoked = 1 WHERE family = ?1")?
                    .execute([&family[..]])?;
                return Ok(Rotation::ReuseDetected { family });
            }
            if expires <= now_ms {
                return Ok(Rotation::Invalid);
            }
            c.prepare_cached("UPDATE refresh_tokens SET rotated = 1 WHERE token_hash = ?1")?
                .execute([&old_hash[..]])?;
            c.prepare_cached(
                "INSERT INTO refresh_tokens(token_hash, account_id, family, expires_ms) VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![&new_hash[..], account, &family[..], expires_ms])?;
            Ok(Rotation::Rotated {
                player: PlayerId(account as u64),
                family,
            })
        })
    }

    fn revoke_family(&self, family: [u8; 16]) -> Task<Result<(), StoreError>> {
        self.tx(move |c| {
            c.prepare_cached("UPDATE refresh_tokens SET revoked = 1 WHERE family = ?1")?
                .execute([&family[..]])?;
            Ok(())
        })
    }

    fn revoke_token(&self, token_hash: [u8; 32]) -> Task<Result<Option<[u8; 16]>, StoreError>> {
        self.tx(move |c| {
            let family: Option<Vec<u8>> = c
                .prepare_cached("SELECT family FROM refresh_tokens WHERE token_hash = ?1")?
                .query_row([&token_hash[..]], |r| r.get(0))
                .optional()?;
            let Some(family) = family.map(family_of) else {
                return Ok(None);
            };
            c.prepare_cached("UPDATE refresh_tokens SET revoked = 1 WHERE family = ?1")?
                .execute([&family[..]])?;
            Ok(Some(family))
        })
    }

    fn revoked_families(&self, now_ms: i64) -> Task<Result<Vec<[u8; 16]>, StoreError>> {
        self.tx(move |c| {
            let mut stmt =
                c.prepare_cached("SELECT DISTINCT family FROM refresh_tokens WHERE revoked = 1 AND expires_ms > ?1")?;
            let rows = stmt
                .query_map([now_ms], |r| r.get::<_, Vec<u8>>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows.into_iter().map(family_of).collect())
        })
    }

    fn get_data(&self, player: PlayerId, keys: Vec<String>) -> Task<Result<Vec<PlayerDataEntry>, StoreError>> {
        self.tx(move |c| read_data(c, player, &keys))
    }

    fn apply_writes(
        &self,
        op_id: String,
        player: PlayerId,
        writes: Vec<DataWrite>,
        now_ms: i64,
    ) -> Task<Result<Vec<PlayerDataEntry>, StoreError>> {
        self.tx(move |c| {
            let keys: Vec<String> = writes.iter().map(|w| w.key.clone()).collect();
            if op_applied(c, &op_id)? {
                return read_data(c, player, &keys);
            }
            let mut version_of =
                c.prepare_cached("SELECT version FROM player_data WHERE account_id = ?1 AND key = ?2")?;
            let mut current = Vec::with_capacity(writes.len());
            for w in &writes {
                let v: Option<i64> = version_of
                    .query_row(params![player.0 as i64, w.key], |r| r.get(0))
                    .optional()?;
                if let Some(expected) = w.expected_version
                    && expected != v.unwrap_or(0)
                {
                    return Err(StoreError::VersionMismatch);
                }
                current.push(v.unwrap_or(0));
            }
            drop(version_of);
            let mut upsert = c.prepare_cached(
                "INSERT INTO player_data(account_id, key, value, version, updated_ms) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(account_id, key) DO UPDATE SET value = excluded.value, version = excluded.version,
                 updated_ms = excluded.updated_ms",
            )?;
            for (w, v) in writes.iter().zip(current) {
                upsert.execute(params![player.0 as i64, w.key, w.value.to_string(), v + 1, now_ms])?;
            }
            drop(upsert);
            mark_applied(c, &op_id, now_ms)?;
            read_data(c, player, &keys)
        })
    }

    fn apply_match(&self, op_id: String, result: MatchResult, now_ms: i64) -> Task<Result<(), StoreError>> {
        self.tx(move |c| {
            if op_applied(c, &op_id)? {
                return Ok(());
            }
            c.prepare_cached(
                "INSERT OR REPLACE INTO matches(id, room_id, mode, started_ms, ended_ms, result)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![
                result.match_id.0 as i64,
                result.room_id.0 as i64,
                result.mode,
                result.started_ms,
                result.ended_ms,
                serde_json::to_string(&result)?
            ])?;
            // Ratings: team Elo over the players that have accounts (bots/dev players are skipped).
            let mut rated = Vec::new();
            let mut winner = result.winner_team;
            for (i, p) in result.players.iter().enumerate() {
                c.prepare_cached(
                    "INSERT OR REPLACE INTO match_players(match_id, account_id, team, score, placement, stats)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )?
                .execute(params![
                    result.match_id.0 as i64,
                    p.player.0 as i64,
                    p.team,
                    p.score,
                    p.placement,
                    p.stats.to_string()
                ])?;
                // Free-for-all: every player is their own team; placement 1 wins.
                let team = p.team.unwrap_or(1000 + i as u16);
                if winner.is_none() && p.team.is_none() && p.placement == Some(1) {
                    winner = Some(team);
                }
                if let Some(a) = load_account(c, p.player)? {
                    rated.push((a.id, a.rating, team));
                }
            }
            for (player, rating) in super::results::elo_update(&rated, winner, super::results::ELO_K) {
                c.prepare_cached("UPDATE accounts SET rating = ?2 WHERE id = ?1")?
                    .execute(params![player.0 as i64, rating])?;
            }
            for (board, player, score) in &result.scores {
                lb_submit(c, board, *player, *score, now_ms)?;
            }
            mark_applied(c, &op_id, now_ms)
        })
    }

    fn submit_score(
        &self,
        op_id: String,
        board: String,
        player: PlayerId,
        score: i64,
        now_ms: i64,
    ) -> Task<Result<(), StoreError>> {
        self.tx(move |c| {
            if op_applied(c, &op_id)? {
                return Ok(());
            }
            lb_submit(c, &board, player, score, now_ms)?;
            mark_applied(c, &op_id, now_ms)
        })
    }

    fn leaderboard_top(&self, board: String, n: u32) -> Task<Result<Vec<LeaderboardRow>, StoreError>> {
        self.tx(move |c| {
            let rows: Vec<(i64, String, i64)> = c
                .prepare_cached(
                    "SELECT RANK() OVER (ORDER BY score DESC), player, score FROM space3d_leaderboard
                     WHERE board = ?1 ORDER BY score DESC, seq ASC LIMIT ?2",
                )?
                .query_map(params![board, n], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            rows.into_iter()
                .filter_map(|(rank, player, score)| {
                    let id = player.parse::<PlayerId>().ok()?;
                    Some(display_name(c, &player).map(|name| LeaderboardRow {
                        rank: rank as u64,
                        player_id: id,
                        display_name: name,
                        score,
                    }))
                })
                .collect()
        })
    }

    fn leaderboard_rank(&self, board: String, player: PlayerId) -> Task<Result<Option<LeaderboardRow>, StoreError>> {
        self.tx(move |c| {
            let key = player.to_string();
            let row: Option<(i64, i64)> = c
                .prepare_cached(
                    "SELECT 1 + (SELECT COUNT(*) FROM space3d_leaderboard o WHERE o.board = e.board AND o.score > e.score),
                            score
                     FROM space3d_leaderboard e WHERE board = ?1 AND player = ?2",
                )?
                .query_row(params![board, key], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            row.map(|(rank, score)| {
                Ok(LeaderboardRow {
                    rank: rank as u64,
                    player_id: player,
                    display_name: display_name(c, &key)?,
                    score,
                })
            })
            .transpose()
        })
    }

    fn put_checkpoint(&self, room: RoomId, bytes: Vec<u8>, now_ms: i64) -> Task<Result<(), StoreError>> {
        self.tx(move |c| {
            c.prepare_cached(
                "INSERT INTO room_checkpoints(room_id, saved_ms, data) VALUES (?1, ?2, ?3)
                 ON CONFLICT(room_id) DO UPDATE SET saved_ms = excluded.saved_ms, data = excluded.data",
            )?
            .execute(params![room.0 as i64, now_ms, bytes])?;
            Ok(())
        })
    }

    fn get_checkpoint(&self, room: RoomId) -> Task<Result<Option<StoredCheckpoint>, StoreError>> {
        self.tx(move |c| {
            Ok(
                c.prepare_cached("SELECT saved_ms, data FROM room_checkpoints WHERE room_id = ?1")?
                    .query_row([room.0 as i64], |r| Ok((r.get(0)?, r.get(1)?)))
                    .optional()?,
            )
        })
    }

    fn delete_checkpoint(&self, room: RoomId) -> Task<Result<(), StoreError>> {
        self.tx(move |c| {
            c.prepare_cached("DELETE FROM room_checkpoints WHERE room_id = ?1")?
                .execute([room.0 as i64])?;
            Ok(())
        })
    }

    fn recent_checkpoints(&self, min_saved_ms: i64) -> Task<Result<Vec<RoomId>, StoreError>> {
        self.tx(move |c| {
            let rows = c
                .prepare_cached("SELECT room_id FROM room_checkpoints WHERE saved_ms >= ?1 ORDER BY room_id")?
                .query_map([min_saved_ms], |r| r.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows.into_iter().map(|r| RoomId(r as u64)).collect())
        })
    }

    fn bans(&self) -> Task<Result<Vec<Ban>, StoreError>> {
        self.tx(|c| {
            let rows = c
                .prepare_cached("SELECT id, account_id, cidr, until_ms, reason FROM bans ORDER BY id")?
                .query_map([], ban_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows.into_iter().filter_map(to_ban).collect())
        })
    }

    fn add_ban(&self, ban: Ban, now_ms: i64) -> Task<Result<Ban, StoreError>> {
        self.tx(move |c| {
            let (account, cidr) = match &ban.target {
                BanTarget::Player(p) => (Some(p.0 as i64), None),
                BanTarget::Ip(cidr) => (None, Some(cidr.to_string())),
            };
            let id = (ban.id != 0).then_some(ban.id as i64);
            c.prepare_cached(
                "INSERT OR REPLACE INTO bans(id, account_id, cidr, until_ms, reason, created_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![id, account, cidr, ban.until_ms, ban.reason, now_ms])?;
            let id = id.unwrap_or_else(|| c.last_insert_rowid());
            if let BanTarget::Player(p) = &ban.target {
                c.prepare_cached("UPDATE accounts SET banned_until_ms = ?2, ban_reason = ?3 WHERE id = ?1")?
                    .execute(params![p.0 as i64, ban.until_ms.unwrap_or(i64::MAX), ban.reason])?;
            }
            Ok(Ban { id: id as u64, ..ban })
        })
    }

    fn remove_ban(&self, id: u64) -> Task<Result<bool, StoreError>> {
        self.tx(move |c| {
            let account: Option<Option<i64>> = c
                .prepare_cached("SELECT account_id FROM bans WHERE id = ?1")?
                .query_row([id as i64], |r| r.get(0))
                .optional()?;
            let n = c
                .prepare_cached("DELETE FROM bans WHERE id = ?1")?
                .execute([id as i64])?;
            if let Some(Some(a)) = account {
                c.prepare_cached("UPDATE accounts SET banned_until_ms = NULL, ban_reason = NULL WHERE id = ?1")?
                    .execute([a])?;
            }
            Ok(n > 0)
        })
    }

    fn audit(
        &self,
        actor: String,
        action: String,
        detail: serde_json::Value,
        now_ms: i64,
    ) -> Task<Result<(), StoreError>> {
        self.tx(move |c| {
            c.prepare_cached("INSERT INTO audit_log(at_ms, actor, action, detail) VALUES (?1, ?2, ?3, ?4)")?
                .execute(params![now_ms, actor, action, detail.to_string()])?;
            Ok(())
        })
    }

    fn backup(&self, path: String) -> Task<Result<(), StoreError>> {
        let task = self.db.with_connection(move |c| {
            c.execute("VACUUM INTO ?1", [path])?;
            Ok(())
        });
        space3d_tasks::spawn(async move {
            match task.wait().await {
                Some(r) => r.map_err(StoreError::from),
                None => Err(StoreError::Db("database worker gone".into())),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gs::room::PlayerResult;
    use space3d_replication::MatchId;

    fn store() -> SqliteStore {
        let db = Db::open_in_memory();
        db.blocking().migrate(super::super::schema::migrations()).unwrap();
        SqliteStore::new(db)
    }

    fn b<T>(t: Task<T>) -> T {
        t.block().unwrap()
    }

    #[test]
    fn guest_accounts_are_bound_to_devices() {
        let s = store();
        let a = b(s.guest([1; 32], Some("alice".into()), 10)).unwrap();
        let again = b(s.guest([1; 32], None, 20)).unwrap();
        assert_eq!(a.id, again.id);
        assert_eq!(again.display_name, "alice");
        assert_eq!(a.kind, AccountKind::Guest);
        let other = b(s.guest([2; 32], None, 30)).unwrap();
        assert_ne!(other.id, a.id);
        assert!(other.display_name.starts_with("Guest-"));
        assert_eq!(
            b(s.set_display_name(a.id, "Alice".into())).unwrap().display_name,
            "Alice"
        );
        assert_eq!(b(s.search_accounts("Ali".into(), 10)).unwrap().len(), 1);
    }

    #[test]
    fn password_accounts_upgrade_and_conflicts() {
        let s = store();
        let guest = b(s.guest([1; 32], None, 0)).unwrap();
        let up = b(s.create_password("alice".into(), "$phc".into(), "Alice".into(), Some(guest.id), 1)).unwrap();
        assert_eq!(up.id, guest.id);
        assert_eq!(up.kind, AccountKind::Password);
        assert!(matches!(
            b(s.create_password("alice".into(), "x".into(), "A2".into(), None, 2)),
            Err(StoreError::Conflict(_))
        ));
        let (acc, phc) = b(s.by_username("alice".into())).unwrap().unwrap();
        assert_eq!((acc.id, phc.as_str()), (guest.id, "$phc"));
        assert!(b(s.by_username("bob".into())).unwrap().is_none());
    }

    #[test]
    fn refresh_rotation_and_reuse_detection() {
        let s = store();
        let a = b(s.guest([1; 32], None, 0)).unwrap();
        b(s.insert_refresh(a.id, [5; 16], [10; 32], 1_000)).unwrap();
        assert_eq!(
            b(s.rotate_refresh([10; 32], [11; 32], 2_000, 100)).unwrap(),
            Rotation::Rotated {
                player: a.id,
                family: [5; 16]
            }
        );
        // Presenting the old token again: theft signal, family revoked.
        assert_eq!(
            b(s.rotate_refresh([10; 32], [12; 32], 2_000, 100)).unwrap(),
            Rotation::ReuseDetected { family: [5; 16] }
        );
        assert_eq!(
            b(s.rotate_refresh([11; 32], [13; 32], 2_000, 100)).unwrap(),
            Rotation::Invalid
        );
        assert_eq!(b(s.revoked_families(0)).unwrap(), vec![[5; 16]]);
        b(s.insert_refresh(a.id, [6; 16], [20; 32], 1_000)).unwrap();
        assert_eq!(
            b(s.rotate_refresh([20; 32], [21; 32], 2_000, 5_000)).unwrap(),
            Rotation::Invalid,
            "expired"
        );
        b(s.insert_refresh(a.id, [7; 16], [30; 32], 9_000)).unwrap();
        assert_eq!(b(s.revoke_token([30; 32])).unwrap(), Some([7; 16]));
        assert_eq!(
            b(s.rotate_refresh([30; 32], [31; 32], 9_000, 100)).unwrap(),
            Rotation::Invalid
        );
    }

    #[test]
    fn data_writes_are_idempotent_and_versioned() {
        let s = store();
        let p = b(s.guest([1; 32], None, 0)).unwrap().id;
        let w = |key: &str, v: i64, expected: Option<i64>| DataWrite {
            key: key.into(),
            value: serde_json::json!({ "v": v }),
            expected_version: expected,
        };
        let out = b(s.apply_writes("op1".into(), p, vec![w("progress", 1, Some(0))], 1)).unwrap();
        assert_eq!(out[0].version, 1);
        // Retry of the same op: no double apply.
        let out = b(s.apply_writes("op1".into(), p, vec![w("progress", 1, Some(0))], 2)).unwrap();
        assert_eq!(out[0].version, 1);
        assert_eq!(
            b(s.apply_writes("op2".into(), p, vec![w("progress", 2, Some(0))], 3)),
            Err(StoreError::VersionMismatch)
        );
        let out = b(s.apply_writes("op3".into(), p, vec![w("progress", 3, Some(1)), w("inv", 9, None)], 4)).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].value["v"], 3);
        assert_eq!(out[0].version, 2);
        let got = b(s.get_data(p, vec!["inv".into(), "missing".into(), "progress".into()])).unwrap();
        assert_eq!(
            got.iter().map(|e| e.key.as_str()).collect::<Vec<_>>(),
            vec!["inv", "progress"]
        );
    }

    #[test]
    fn match_results_update_ratings_history_and_boards_once() {
        let s = store();
        let a = b(s.guest([1; 32], None, 0)).unwrap().id;
        let c = b(s.guest([2; 32], None, 0)).unwrap().id;
        let result = MatchResult {
            match_id: MatchId(77),
            room_id: RoomId(5),
            mode: "duel".into(),
            started_ms: 0,
            ended_ms: 100,
            winner_team: Some(0),
            players: vec![
                PlayerResult {
                    player: a,
                    team: Some(0),
                    score: 10,
                    placement: Some(1),
                    stats: serde_json::json!({"kills": 3}),
                },
                PlayerResult {
                    player: c,
                    team: Some(1),
                    score: 4,
                    placement: Some(2),
                    stats: serde_json::Value::Null,
                },
                PlayerResult {
                    player: PlayerId(999),
                    team: Some(1),
                    score: 0,
                    placement: None,
                    stats: serde_json::Value::Null,
                },
            ],
            scores: vec![("kills".into(), a, 3), ("kills".into(), c, 1)],
            data: serde_json::Value::Null,
        };
        b(s.apply_match("gs-1:5:match".into(), result.clone(), 100)).unwrap();
        b(s.apply_match("gs-1:5:match".into(), result, 100)).unwrap();
        let ra = b(s.account(a)).unwrap().unwrap().rating;
        let rc = b(s.account(c)).unwrap().unwrap().rating;
        assert!((ra - 1516.0).abs() < 1e-6, "applied exactly once: {ra}");
        assert!((rc - 1484.0).abs() < 1e-6);
        let top = b(s.leaderboard_top("kills".into(), 10)).unwrap();
        assert_eq!(top.len(), 2);
        assert_eq!((top[0].player_id, top[0].rank, top[0].score), (a, 1, 3));
        assert!(top[0].display_name.starts_with("Guest-"));
        b(s.submit_score("op-s".into(), "kills".into(), c, 7, 200)).unwrap();
        b(s.submit_score("op-s".into(), "kills".into(), c, 99, 201)).unwrap();
        let rank = b(s.leaderboard_rank("kills".into(), c)).unwrap().unwrap();
        assert_eq!((rank.rank, rank.score), (1, 7));
        assert!(b(s.leaderboard_rank("kills".into(), PlayerId(4))).unwrap().is_none());
    }

    #[test]
    fn checkpoints_bans_audit_backup() {
        let s = store();
        b(s.put_checkpoint(RoomId(1), vec![1, 2, 3], 50)).unwrap();
        b(s.put_checkpoint(RoomId(1), vec![4], 60)).unwrap();
        assert_eq!(b(s.get_checkpoint(RoomId(1))).unwrap(), Some((60, vec![4])));
        assert_eq!(b(s.recent_checkpoints(55)).unwrap(), vec![RoomId(1)]);
        b(s.delete_checkpoint(RoomId(1))).unwrap();
        assert_eq!(b(s.get_checkpoint(RoomId(1))).unwrap(), None);

        let p = b(s.guest([1; 32], None, 0)).unwrap().id;
        let ban = b(s.add_ban(
            Ban {
                id: 0,
                target: BanTarget::Player(p),
                until_ms: Some(1_000),
                reason: "cheating".into(),
            },
            5,
        ))
        .unwrap();
        assert!(ban.id > 0);
        assert_eq!(b(s.account(p)).unwrap().unwrap().banned_until_ms, Some(1_000));
        b(s.add_ban(
            Ban {
                id: 0,
                target: BanTarget::Ip(Cidr::parse("10.0.0.0/8").unwrap()),
                until_ms: None,
                reason: "abuse".into(),
            },
            5,
        ))
        .unwrap();
        assert_eq!(b(s.bans()).unwrap().len(), 2);
        assert!(b(s.remove_ban(ban.id)).unwrap());
        assert!(!b(s.remove_ban(ban.id)).unwrap());
        assert_eq!(b(s.account(p)).unwrap().unwrap().banned_until_ms, None);
        b(s.audit("admin".into(), "ban".into(), serde_json::json!({"id": 1}), 9)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backup.db");
        b(s.backup(path.display().to_string())).unwrap();
        assert!(path.exists());
    }
}
