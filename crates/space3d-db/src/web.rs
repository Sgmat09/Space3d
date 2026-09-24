//! Web backend: the high-level stores over a string key/value storage
//! (`localStorage` in the browser, or process memory for `:memory:`).
//!
//! Layout (all keys are prefixed with `space3d:<db path>:`):
//! * `kv:<ns len>:<ns>:<key>` → JSON value
//! * `save:<slot>` → JSON [`StoredSave`] (payload/thumbnail base64-encoded)
//! * `save-seq` → monotonically increasing save counter
//! * `lb:<board>` → JSON array of [`StoredEntry`]
//!
//! `localStorage` is small (typically ~5 MB per origin) and synchronous; writes
//! that exceed the quota fail with [`DbError::Storage`].

use crate::db::BackendKind;
#[cfg(target_arch = "wasm32")]
use crate::db::DbConfig;
use crate::error::{DbError, Result};
use crate::store::Store;
use crate::stores::{LeaderboardEntry, NewSave, SaveGame, SaveMeta, ScoreOrder, Submission};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// Minimal synchronous string storage.
pub(crate) trait StringStorage: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>>;
    fn set(&self, key: &str, value: &str) -> Result<()>;
    fn remove(&self, key: &str) -> Result<()>;
    /// All keys starting with `prefix`.
    fn keys_with_prefix(&self, prefix: &str) -> Result<Vec<String>>;
}

#[derive(Default)]
pub(crate) struct MemoryStorage(Mutex<BTreeMap<String, String>>);

impl MemoryStorage {
    fn map(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, String>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl StringStorage for MemoryStorage {
    fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.map().get(key).cloned())
    }
    fn set(&self, key: &str, value: &str) -> Result<()> {
        self.map().insert(key.to_owned(), value.to_owned());
        Ok(())
    }
    fn remove(&self, key: &str) -> Result<()> {
        self.map().remove(key);
        Ok(())
    }
    fn keys_with_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        Ok(self
            .map()
            .range(prefix.to_owned()..)
            .map(|(k, _)| k)
            .take_while(|k| k.starts_with(prefix))
            .cloned()
            .collect())
    }
}

/// `window.localStorage`. Looked up on every call so the handle stays `Send + Sync`.
#[cfg(target_arch = "wasm32")]
pub(crate) struct LocalStorage;

#[cfg(target_arch = "wasm32")]
impl LocalStorage {
    fn storage() -> Result<web_sys::Storage> {
        web_sys::window()
            .ok_or_else(|| DbError::Storage("no `window` (not running in a browser main thread)".into()))?
            .local_storage()
            .map_err(|e| DbError::Storage(format!("localStorage unavailable: {e:?}")))?
            .ok_or_else(|| DbError::Storage("localStorage is disabled".into()))
    }
}

#[cfg(target_arch = "wasm32")]
impl StringStorage for LocalStorage {
    fn get(&self, key: &str) -> Result<Option<String>> {
        Self::storage()?
            .get_item(key)
            .map_err(|e| DbError::Storage(format!("{e:?}")))
    }
    fn set(&self, key: &str, value: &str) -> Result<()> {
        Self::storage()?
            .set_item(key, value)
            .map_err(|e| DbError::Storage(format!("write failed (quota exceeded?): {e:?}")))
    }
    fn remove(&self, key: &str) -> Result<()> {
        Self::storage()?
            .remove_item(key)
            .map_err(|e| DbError::Storage(format!("{e:?}")))
    }
    fn keys_with_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        let s = Self::storage()?;
        let n = s.length().map_err(|e| DbError::Storage(format!("{e:?}")))?;
        let mut keys = Vec::new();
        for i in 0..n {
            if let Ok(Some(k)) = s.key(i)
                && k.starts_with(prefix)
            {
                keys.push(k);
            }
        }
        Ok(keys)
    }
}

pub(crate) struct WebBackend {
    storage: Box<dyn StringStorage>,
    prefix: String,
    kind: BackendKind,
}

#[derive(Serialize, Deserialize)]
struct StoredSave {
    meta: SaveMeta,
    seq: u64,
    data: String,
    thumbnail: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredEntry {
    player: String,
    score: i64,
    achieved_at: i64,
    seq: u64,
}

fn b64_decode(s: &str) -> Result<Vec<u8>> {
    B64.decode(s)
        .map_err(|e| DbError::Serde(format!("invalid base64 in storage: {e}")))
}

impl WebBackend {
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn for_config(config: &DbConfig) -> Self {
        let name = config.path.to_string_lossy();
        if config.is_in_memory() {
            Self::memory(&name)
        } else {
            Self {
                storage: Box::new(LocalStorage),
                prefix: format!("space3d:{name}:"),
                kind: BackendKind::LocalStorage,
            }
        }
    }

    pub(crate) fn memory(name: &str) -> Self {
        Self {
            storage: Box::new(MemoryStorage::default()),
            prefix: format!("space3d:{name}:"),
            kind: BackendKind::Memory,
        }
    }

    pub(crate) fn kind(&self) -> BackendKind {
        self.kind
    }

    fn kv_prefix(&self, ns: &str) -> String {
        format!("{}kv:{}:{ns}:", self.prefix, ns.len())
    }

    fn save_key(&self, slot: &str) -> String {
        format!("{}save:{slot}", self.prefix)
    }

    fn lb_key(&self, board: &str) -> String {
        format!("{}lb:{board}", self.prefix)
    }

    fn load_json<T: for<'de> Deserialize<'de>>(&self, key: &str) -> Result<Option<T>> {
        self.storage
            .get(key)?
            .map(|s| serde_json::from_str(&s).map_err(DbError::from))
            .transpose()
    }

    fn store_json<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        self.storage.set(key, &serde_json::to_string(value)?)
    }

    fn next_seq(&self, name: &str) -> Result<u64> {
        let key = format!("{}{name}", self.prefix);
        let next = self.load_json::<u64>(&key)?.unwrap_or(0) + 1;
        self.store_json(&key, &next)?;
        Ok(next)
    }

    fn board(&self, board: &str) -> Result<Vec<StoredEntry>> {
        Ok(self.load_json(&self.lb_key(board))?.unwrap_or_default())
    }

    /// Sorted best-first (ties: earliest achiever first) with competition ranks.
    fn ranked(entries: &mut [StoredEntry], order: ScoreOrder) -> Vec<LeaderboardEntry> {
        entries.sort_by(|a, b| {
            let by_score = a.score.cmp(&b.score);
            let by_score = if order == ScoreOrder::HigherIsBetter {
                by_score.reverse()
            } else {
                by_score
            };
            by_score.then(a.seq.cmp(&b.seq))
        });
        let mut out: Vec<LeaderboardEntry> = Vec::with_capacity(entries.len());
        for (i, e) in entries.iter().enumerate() {
            let rank = match out.last() {
                Some(prev) if prev.score == e.score => prev.rank,
                _ => i as u64 + 1,
            };
            out.push(LeaderboardEntry {
                rank,
                player: e.player.clone(),
                score: e.score,
                achieved_at: e.achieved_at,
            });
        }
        out
    }
}

impl Store for WebBackend {
    fn kv_get(&self, ns: &str, key: &str) -> Result<Option<String>> {
        self.storage.get(&format!("{}{key}", self.kv_prefix(ns)))
    }

    fn kv_set(&self, ns: &str, key: &str, json: &str, _now: i64) -> Result<()> {
        self.storage.set(&format!("{}{key}", self.kv_prefix(ns)), json)
    }

    fn kv_delete(&self, ns: &str, key: &str) -> Result<bool> {
        let full = format!("{}{key}", self.kv_prefix(ns));
        let existed = self.storage.get(&full)?.is_some();
        self.storage.remove(&full)?;
        Ok(existed)
    }

    fn kv_keys(&self, ns: &str) -> Result<Vec<String>> {
        let prefix = self.kv_prefix(ns);
        let mut keys: Vec<String> = self
            .storage
            .keys_with_prefix(&prefix)?
            .into_iter()
            .map(|k| k[prefix.len()..].to_owned())
            .collect();
        keys.sort();
        Ok(keys)
    }

    fn kv_clear(&self, ns: &str) -> Result<usize> {
        let keys = self.storage.keys_with_prefix(&self.kv_prefix(ns))?;
        for k in &keys {
            self.storage.remove(k)?;
        }
        Ok(keys.len())
    }

    fn save_put(&self, slot: &str, save: NewSave, now: i64) -> Result<SaveMeta> {
        let key = self.save_key(slot);
        let created_at = self.load_json::<StoredSave>(&key)?.map_or(now, |s| s.meta.created_at);
        let meta = SaveMeta {
            slot: slot.to_owned(),
            label: save.label,
            created_at,
            updated_at: now,
            metadata: save.metadata,
            size_bytes: save.data.len() as u64,
            has_thumbnail: save.thumbnail_png.is_some(),
        };
        let stored = StoredSave {
            meta: meta.clone(),
            seq: self.next_seq("save-seq")?,
            data: B64.encode(&save.data),
            thumbnail: save.thumbnail_png.as_deref().map(|t| B64.encode(t)),
        };
        self.store_json(&key, &stored)?;
        Ok(meta)
    }

    fn save_get(&self, slot: &str) -> Result<Option<SaveGame>> {
        self.load_json::<StoredSave>(&self.save_key(slot))?
            .map(|s| {
                Ok(SaveGame {
                    meta: s.meta,
                    data: b64_decode(&s.data)?,
                    thumbnail_png: s.thumbnail.as_deref().map(b64_decode).transpose()?,
                })
            })
            .transpose()
    }

    fn save_list(&self) -> Result<Vec<SaveMeta>> {
        let mut saves = Vec::new();
        for key in self.storage.keys_with_prefix(&self.save_key(""))? {
            if let Some(s) = self.load_json::<StoredSave>(&key)? {
                saves.push((s.meta, s.seq));
            }
        }
        saves.sort_by(|(a, sa), (b, sb)| b.updated_at.cmp(&a.updated_at).then(sb.cmp(sa)));
        Ok(saves.into_iter().map(|(m, _)| m).collect())
    }

    fn save_delete(&self, slot: &str) -> Result<bool> {
        let key = self.save_key(slot);
        let existed = self.storage.get(&key)?.is_some();
        self.storage.remove(&key)?;
        Ok(existed)
    }

    fn save_thumbnail(&self, slot: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.save_get(slot)?.and_then(|s| s.thumbnail_png))
    }

    fn lb_submit(&self, board: &str, order: ScoreOrder, player: &str, score: i64, now: i64) -> Result<Submission> {
        let mut entries = self.board(board)?;
        let existing = entries.iter().position(|e| e.player == player);
        let improved = existing.is_none_or(|i| order.better(score, entries[i].score));
        if improved {
            let seq = self.next_seq("lb-seq")?;
            let e = StoredEntry {
                player: player.to_owned(),
                score,
                achieved_at: now,
                seq,
            };
            match existing {
                Some(i) => entries[i] = e,
                None => entries.push(e),
            }
            self.store_json(&self.lb_key(board), &entries)?;
        }
        let best = Self::ranked(&mut entries, order)
            .into_iter()
            .find(|e| e.player == player)
            .ok_or_else(|| DbError::other("leaderboard entry vanished during submit"))?;
        Ok(Submission { improved, best })
    }

    fn lb_top(&self, board: &str, order: ScoreOrder, n: usize) -> Result<Vec<LeaderboardEntry>> {
        let mut ranked = Self::ranked(&mut self.board(board)?, order);
        ranked.truncate(n);
        Ok(ranked)
    }

    fn lb_rank(&self, board: &str, order: ScoreOrder, player: &str) -> Result<Option<LeaderboardEntry>> {
        Ok(Self::ranked(&mut self.board(board)?, order)
            .into_iter()
            .find(|e| e.player == player))
    }

    fn lb_count(&self, board: &str) -> Result<u64> {
        Ok(self.board(board)?.len() as u64)
    }

    fn lb_remove(&self, board: &str, player: &str) -> Result<bool> {
        let mut entries = self.board(board)?;
        let before = entries.len();
        entries.retain(|e| e.player != player);
        if entries.len() == before {
            return Ok(false);
        }
        self.store_json(&self.lb_key(board), &entries)?;
        Ok(true)
    }

    fn lb_clear(&self, board: &str) -> Result<usize> {
        let n = self.board(board)?.len();
        self.storage.remove(&self.lb_key(board))?;
        Ok(n)
    }
}
