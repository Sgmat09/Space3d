//! Player-service SQLite schema (`space3d_db::Migration`s, applied at startup).
//!
//! ```sql
//! -- v1
//! CREATE TABLE accounts(id INTEGER PRIMARY KEY /* PlayerId as i64 bits */, kind TEXT NOT NULL,
//!   display_name TEXT NOT NULL, username TEXT UNIQUE, password_phc TEXT, device_hash BLOB UNIQUE,
//!   rating REAL NOT NULL DEFAULT 1500, created_ms INTEGER NOT NULL, last_login_ms INTEGER,
//!   banned_until_ms INTEGER, ban_reason TEXT);
//! CREATE TABLE refresh_tokens(token_hash BLOB PRIMARY KEY, account_id INTEGER NOT NULL,
//!   family BLOB NOT NULL, expires_ms INTEGER NOT NULL, rotated INTEGER NOT NULL DEFAULT 0,
//!   revoked INTEGER NOT NULL DEFAULT 0);
//! CREATE INDEX refresh_family ON refresh_tokens(family);
//! CREATE TABLE player_data(account_id INTEGER NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL,
//!   version INTEGER NOT NULL, updated_ms INTEGER NOT NULL, PRIMARY KEY(account_id, key)) WITHOUT ROWID;
//! CREATE TABLE applied_ops(op_id TEXT PRIMARY KEY, applied_ms INTEGER NOT NULL) WITHOUT ROWID;
//! CREATE TABLE matches(id INTEGER PRIMARY KEY, room_id INTEGER, mode TEXT, started_ms INTEGER,
//!   ended_ms INTEGER, result TEXT NOT NULL);
//! CREATE TABLE match_players(match_id INTEGER, account_id INTEGER, team INTEGER, score INTEGER,
//!   placement INTEGER, stats TEXT, PRIMARY KEY(match_id, account_id));
//! CREATE TABLE room_checkpoints(room_id INTEGER PRIMARY KEY, server_id TEXT, saved_ms INTEGER NOT NULL,
//!   data BLOB NOT NULL);
//! CREATE TABLE bans(id INTEGER PRIMARY KEY, account_id INTEGER, cidr TEXT, until_ms INTEGER,
//!   reason TEXT NOT NULL, created_by TEXT, created_ms INTEGER NOT NULL);
//! CREATE TABLE audit_log(id INTEGER PRIMARY KEY, at_ms INTEGER NOT NULL, actor TEXT NOT NULL,
//!   action TEXT NOT NULL, detail TEXT);
//! -- leaderboards: space3d_db::Leaderboard tables (player key = PlayerId hex)
//! ```

use space3d_db::Migration;

/// All migrations in order.
pub fn migrations() -> Vec<Migration> {
    todo!()
}
