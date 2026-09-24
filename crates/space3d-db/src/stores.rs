//! High-level stores built on [`Db`]: [`KvStore`], [`SaveSlots`], [`Leaderboard`].
//!
//! They work on every backend (SQLite on native, `localStorage` on web) and,
//! like the rest of the crate, return [`Task`]s.

use crate::db::{Db, Pending};
use crate::error::{DbError, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use space3d_tasks::Task;
use std::fmt;
use std::sync::Arc;

fn ready<R: Send + 'static>(r: Result<R>) -> Task<Result<R>> {
    Pending::Ready(r).into_task()
}

// ---------------------------------------------------------------- KvStore

/// Namespaced, typed key/value store. Values are stored as JSON.
///
/// ```
/// # use space3d_db::Db;
/// let db = Db::open_in_memory();
/// let settings = db.kv("settings");
/// settings.set("volume", &0.8f32).block().unwrap().unwrap();
/// let v: Option<f32> = settings.get("volume").block().unwrap().unwrap();
/// assert_eq!(v, Some(0.8));
/// ```
#[derive(Clone)]
pub struct KvStore {
    db: Db,
    namespace: Arc<str>,
}

impl fmt::Debug for KvStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KvStore").field("namespace", &self.namespace).finish()
    }
}

impl KvStore {
    pub(crate) fn new(db: Db, namespace: String) -> Self {
        Self {
            db,
            namespace: namespace.into(),
        }
    }

    /// The namespace keys live in.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Read and deserialize `key`; `None` if absent.
    pub fn get<T: DeserializeOwned + Send + 'static>(&self, key: impl Into<String>) -> Task<Result<Option<T>>> {
        let (ns, key) = (self.namespace.clone(), key.into());
        self.db.store(move |s| s.kv_get(&ns, &key)).map_task(|raw| {
            raw.map(|json| serde_json::from_str(&json).map_err(DbError::from))
                .transpose()
        })
    }

    /// Serialize and store `value` under `key` (insert or replace).
    pub fn set<T: Serialize + ?Sized>(&self, key: impl Into<String>, value: &T) -> Task<Result<()>> {
        let json = match serde_json::to_string(value) {
            Ok(j) => j,
            Err(e) => return ready(Err(e.into())),
        };
        let (ns, key, now) = (self.namespace.clone(), key.into(), crate::now_ms());
        self.db.store(move |s| s.kv_set(&ns, &key, &json, now)).into_task()
    }

    /// Remove `key`; resolves to `true` if it existed.
    pub fn delete(&self, key: impl Into<String>) -> Task<Result<bool>> {
        let (ns, key) = (self.namespace.clone(), key.into());
        self.db.store(move |s| s.kv_delete(&ns, &key)).into_task()
    }

    /// `true` if `key` is present.
    pub fn contains(&self, key: impl Into<String>) -> Task<Result<bool>> {
        let (ns, key) = (self.namespace.clone(), key.into());
        self.db
            .store(move |s| s.kv_get(&ns, &key).map(|v| v.is_some()))
            .into_task()
    }

    /// All keys in this namespace, sorted.
    pub fn keys(&self) -> Task<Result<Vec<String>>> {
        let ns = self.namespace.clone();
        self.db.store(move |s| s.kv_keys(&ns)).into_task()
    }

    /// Delete every key in this namespace; resolves to the number removed.
    pub fn clear(&self) -> Task<Result<usize>> {
        let ns = self.namespace.clone();
        self.db.store(move |s| s.kv_clear(&ns)).into_task()
    }
}

// ---------------------------------------------------------------- SaveSlots

/// Metadata describing a save slot (cheap to list; excludes the payload).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SaveMeta {
    /// Slot identifier (e.g. `"autosave"`, `"slot-1"`).
    pub slot: String,
    /// Display label (e.g. `"Chapter 3 — The Docks"`).
    pub label: String,
    /// Unix time in milliseconds when the slot was first written.
    pub created_at: i64,
    /// Unix time in milliseconds of the most recent save.
    pub updated_at: i64,
    /// Arbitrary game-defined metadata (play time, level, party, ...).
    pub metadata: serde_json::Value,
    /// Size of the save payload in bytes.
    pub size_bytes: u64,
    /// Whether a thumbnail PNG is stored.
    pub has_thumbnail: bool,
}

/// A fully loaded save: metadata, payload and optional thumbnail.
#[derive(Clone, Debug, PartialEq)]
pub struct SaveGame {
    /// Slot metadata.
    pub meta: SaveMeta,
    /// Raw save payload.
    pub data: Vec<u8>,
    /// PNG-encoded thumbnail, if one was saved.
    pub thumbnail_png: Option<Vec<u8>>,
}

impl SaveGame {
    /// Decode a payload written with [`NewSave::from_state`].
    pub fn decode<T: DeserializeOwned>(&self) -> Result<T> {
        Ok(serde_json::from_slice(&self.data)?)
    }
}

/// Input for [`SaveSlots::save`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NewSave {
    /// Display label.
    pub label: String,
    /// Raw payload (any format the game likes).
    pub data: Vec<u8>,
    /// Arbitrary metadata shown in load menus. `Null` is stored as `{}`.
    pub metadata: serde_json::Value,
    /// Optional PNG thumbnail (e.g. a downscaled screenshot).
    pub thumbnail_png: Option<Vec<u8>>,
}

impl NewSave {
    /// Save with a raw payload.
    pub fn new(label: impl Into<String>, data: impl Into<Vec<u8>>) -> Self {
        Self {
            label: label.into(),
            data: data.into(),
            ..Default::default()
        }
    }

    /// Save any serializable game state (JSON payload; read back with [`SaveGame::decode`]).
    pub fn from_state<T: Serialize + ?Sized>(label: impl Into<String>, state: &T) -> Result<Self> {
        Ok(Self::new(label, serde_json::to_vec(state)?))
    }

    /// Attach metadata.
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = metadata;
        self
    }

    /// Attach a PNG thumbnail.
    pub fn with_thumbnail(mut self, png: impl Into<Vec<u8>>) -> Self {
        self.thumbnail_png = Some(png.into());
        self
    }
}

/// Game save slots with metadata, timestamps and thumbnails.
///
/// ```
/// # use space3d_db::{Db, NewSave};
/// let db = Db::open_in_memory();
/// let saves = db.save_slots();
/// saves.save("slot-1", NewSave::new("Level 1", b"state".to_vec())).block().unwrap().unwrap();
/// let list = saves.list().block().unwrap().unwrap();
/// assert_eq!(list[0].slot, "slot-1");
/// ```
#[derive(Clone, Debug)]
pub struct SaveSlots {
    db: Db,
}

impl SaveSlots {
    pub(crate) fn new(db: Db) -> Self {
        Self { db }
    }

    /// Write (or overwrite) `slot`. Overwriting keeps `created_at` and bumps `updated_at`.
    pub fn save(&self, slot: impl Into<String>, mut save: NewSave) -> Task<Result<SaveMeta>> {
        if save.metadata.is_null() {
            save.metadata = serde_json::Value::Object(Default::default());
        }
        let (slot, now) = (slot.into(), crate::now_ms());
        self.db.store(move |s| s.save_put(&slot, save, now)).into_task()
    }

    /// Load a slot including payload and thumbnail.
    pub fn load(&self, slot: impl Into<String>) -> Task<Result<Option<SaveGame>>> {
        let slot = slot.into();
        self.db.store(move |s| s.save_get(&slot)).into_task()
    }

    /// All slots, most recently saved first.
    pub fn list(&self) -> Task<Result<Vec<SaveMeta>>> {
        self.db.store(|s| s.save_list()).into_task()
    }

    /// Delete a slot; resolves to `true` if it existed.
    pub fn delete(&self, slot: impl Into<String>) -> Task<Result<bool>> {
        let slot = slot.into();
        self.db.store(move |s| s.save_delete(&slot)).into_task()
    }

    /// Only the thumbnail PNG of a slot (for load menus).
    pub fn thumbnail(&self, slot: impl Into<String>) -> Task<Result<Option<Vec<u8>>>> {
        let slot = slot.into();
        self.db.store(move |s| s.save_thumbnail(&slot)).into_task()
    }
}

// ---------------------------------------------------------------- Leaderboard

/// Which direction of score is better.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ScoreOrder {
    /// Points: bigger is better.
    #[default]
    HigherIsBetter,
    /// Times / strokes: smaller is better (store times as integer milliseconds).
    LowerIsBetter,
}

impl ScoreOrder {
    /// `true` if `a` beats `b`.
    pub fn better(self, a: i64, b: i64) -> bool {
        match self {
            ScoreOrder::HigherIsBetter => a > b,
            ScoreOrder::LowerIsBetter => a < b,
        }
    }
}

/// One ranked leaderboard row. Ties share a rank ("1224" competition ranking)
/// and are listed in the order they were achieved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaderboardEntry {
    /// 1-based rank.
    pub rank: u64,
    /// Player identifier.
    pub player: String,
    /// Player's best score.
    pub score: i64,
    /// Unix time in milliseconds when the best score was achieved.
    pub achieved_at: i64,
}

/// Result of [`Leaderboard::submit`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submission {
    /// `true` if the submitted score became the player's new best.
    pub improved: bool,
    /// The player's (possibly unchanged) best entry with its current rank.
    pub best: LeaderboardEntry,
}

/// Local leaderboard keeping each player's best score.
///
/// ```
/// # use space3d_db::Db;
/// let db = Db::open_in_memory();
/// let board = db.leaderboard("arcade");
/// board.submit("ada", 300).block().unwrap().unwrap();
/// board.submit("bob", 500).block().unwrap().unwrap();
/// let top = board.top(10).block().unwrap().unwrap();
/// assert_eq!(top[0].player, "bob");
/// ```
#[derive(Clone)]
pub struct Leaderboard {
    db: Db,
    board: Arc<str>,
    order: ScoreOrder,
}

impl fmt::Debug for Leaderboard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Leaderboard")
            .field("board", &self.board)
            .field("order", &self.order)
            .finish()
    }
}

impl Leaderboard {
    pub(crate) fn new(db: Db, board: String) -> Self {
        Self {
            db,
            board: board.into(),
            order: ScoreOrder::default(),
        }
    }

    /// Set the score direction. Use the same order every time you open a given board.
    pub fn with_order(mut self, order: ScoreOrder) -> Self {
        self.order = order;
        self
    }

    /// Board name.
    pub fn name(&self) -> &str {
        &self.board
    }

    /// Score direction.
    pub fn order(&self) -> ScoreOrder {
        self.order
    }

    /// Submit a score; only kept if it beats the player's previous best.
    pub fn submit(&self, player: impl Into<String>, score: i64) -> Task<Result<Submission>> {
        let (board, order, player, now) = (self.board.clone(), self.order, player.into(), crate::now_ms());
        self.db
            .store(move |s| s.lb_submit(&board, order, &player, score, now))
            .into_task()
    }

    /// The best `n` entries.
    pub fn top(&self, n: usize) -> Task<Result<Vec<LeaderboardEntry>>> {
        let (board, order) = (self.board.clone(), self.order);
        self.db.store(move |s| s.lb_top(&board, order, n)).into_task()
    }

    /// A player's entry and rank (`None` if they never submitted).
    pub fn rank(&self, player: impl Into<String>) -> Task<Result<Option<LeaderboardEntry>>> {
        let (board, order, player) = (self.board.clone(), self.order, player.into());
        self.db.store(move |s| s.lb_rank(&board, order, &player)).into_task()
    }

    /// Number of players on the board.
    pub fn count(&self) -> Task<Result<u64>> {
        let board = self.board.clone();
        self.db.store(move |s| s.lb_count(&board)).into_task()
    }

    /// Remove a player; resolves to `true` if they were on the board.
    pub fn remove(&self, player: impl Into<String>) -> Task<Result<bool>> {
        let (board, player) = (self.board.clone(), player.into());
        self.db.store(move |s| s.lb_remove(&board, &player)).into_task()
    }

    /// Remove every entry; resolves to the number removed.
    pub fn clear(&self) -> Task<Result<usize>> {
        let board = self.board.clone();
        self.db.store(move |s| s.lb_clear(&board)).into_task()
    }
}
