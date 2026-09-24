//! Backend-agnostic operations behind the high-level stores.

use crate::error::Result;
use crate::stores::{LeaderboardEntry, NewSave, SaveGame, SaveMeta, ScoreOrder, Submission};

/// Implemented by `rusqlite::Connection` (native) and the web storage backend.
/// All timestamps are unix milliseconds supplied by the caller.
pub(crate) trait Store {
    fn kv_get(&self, ns: &str, key: &str) -> Result<Option<String>>;
    fn kv_set(&self, ns: &str, key: &str, json: &str, now: i64) -> Result<()>;
    fn kv_delete(&self, ns: &str, key: &str) -> Result<bool>;
    /// Sorted keys of `ns`.
    fn kv_keys(&self, ns: &str) -> Result<Vec<String>>;
    fn kv_clear(&self, ns: &str) -> Result<usize>;

    fn save_put(&self, slot: &str, save: NewSave, now: i64) -> Result<SaveMeta>;
    fn save_get(&self, slot: &str) -> Result<Option<SaveGame>>;
    /// Most recently saved first.
    fn save_list(&self) -> Result<Vec<SaveMeta>>;
    fn save_delete(&self, slot: &str) -> Result<bool>;
    fn save_thumbnail(&self, slot: &str) -> Result<Option<Vec<u8>>>;

    fn lb_submit(&self, board: &str, order: ScoreOrder, player: &str, score: i64, now: i64) -> Result<Submission>;
    fn lb_top(&self, board: &str, order: ScoreOrder, n: usize) -> Result<Vec<LeaderboardEntry>>;
    fn lb_rank(&self, board: &str, order: ScoreOrder, player: &str) -> Result<Option<LeaderboardEntry>>;
    fn lb_count(&self, board: &str) -> Result<u64>;
    fn lb_remove(&self, board: &str, player: &str) -> Result<bool>;
    fn lb_clear(&self, board: &str) -> Result<usize>;
}

#[cfg(not(target_arch = "wasm32"))]
mod sqlite {
    use super::*;
    use crate::error::DbError;
    use rusqlite::{Connection, OptionalExtension, params};

    fn desc(order: ScoreOrder) -> (&'static str, &'static str) {
        // (ORDER BY direction, "strictly better than" operator)
        match order {
            ScoreOrder::HigherIsBetter => ("DESC", ">"),
            ScoreOrder::LowerIsBetter => ("ASC", "<"),
        }
    }

    const SAVE_META_COLS: &str = "slot, label, created_at, updated_at, metadata, length(data), thumbnail IS NOT NULL";

    fn save_meta(r: &rusqlite::Row<'_>) -> rusqlite::Result<(SaveMeta, String)> {
        Ok((
            SaveMeta {
                slot: r.get(0)?,
                label: r.get(1)?,
                created_at: r.get(2)?,
                updated_at: r.get(3)?,
                metadata: serde_json::Value::Null,
                size_bytes: r.get::<_, i64>(5)? as u64,
                has_thumbnail: r.get(6)?,
            },
            r.get(4)?,
        ))
    }

    fn with_metadata((mut meta, json): (SaveMeta, String)) -> Result<SaveMeta> {
        meta.metadata = serde_json::from_str(&json)?;
        Ok(meta)
    }

    fn entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<LeaderboardEntry> {
        Ok(LeaderboardEntry {
            rank: r.get::<_, i64>(0)? as u64,
            player: r.get(1)?,
            score: r.get(2)?,
            achieved_at: r.get(3)?,
        })
    }

    impl Store for Connection {
        fn kv_get(&self, ns: &str, key: &str) -> Result<Option<String>> {
            Ok(self
                .prepare_cached("SELECT value FROM space3d_kv WHERE namespace = ?1 AND key = ?2")?
                .query_row(params![ns, key], |r| r.get(0))
                .optional()?)
        }

        fn kv_set(&self, ns: &str, key: &str, json: &str, now: i64) -> Result<()> {
            self.prepare_cached(
                "INSERT INTO space3d_kv (namespace, key, value, updated_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (namespace, key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            )?
            .execute(params![ns, key, json, now])?;
            Ok(())
        }

        fn kv_delete(&self, ns: &str, key: &str) -> Result<bool> {
            Ok(self
                .prepare_cached("DELETE FROM space3d_kv WHERE namespace = ?1 AND key = ?2")?
                .execute(params![ns, key])?
                > 0)
        }

        fn kv_keys(&self, ns: &str) -> Result<Vec<String>> {
            let mut stmt = self.prepare_cached("SELECT key FROM space3d_kv WHERE namespace = ?1 ORDER BY key")?;
            let keys = stmt.query_map([ns], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            Ok(keys)
        }

        fn kv_clear(&self, ns: &str) -> Result<usize> {
            Ok(self
                .prepare_cached("DELETE FROM space3d_kv WHERE namespace = ?1")?
                .execute([ns])?)
        }

        fn save_put(&self, slot: &str, save: NewSave, now: i64) -> Result<SaveMeta> {
            let metadata = serde_json::to_string(&save.metadata)?;
            let tx = self.unchecked_transaction()?;
            tx.prepare_cached(
                "INSERT INTO space3d_saves (slot, label, metadata, data, thumbnail, created_at, updated_at, seq)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, (SELECT COALESCE(MAX(seq), 0) + 1 FROM space3d_saves))
                 ON CONFLICT (slot) DO UPDATE SET
                    label = excluded.label, metadata = excluded.metadata, data = excluded.data,
                    thumbnail = excluded.thumbnail, updated_at = excluded.updated_at, seq = excluded.seq",
            )?
            .execute(params![slot, save.label, metadata, save.data, save.thumbnail_png, now])?;
            let meta = tx
                .prepare_cached(&format!("SELECT {SAVE_META_COLS} FROM space3d_saves WHERE slot = ?1"))?
                .query_row([slot], save_meta)?;
            tx.commit()?;
            with_metadata(meta)
        }

        fn save_get(&self, slot: &str) -> Result<Option<SaveGame>> {
            let row = self
                .prepare_cached(&format!(
                    "SELECT {SAVE_META_COLS}, data, thumbnail FROM space3d_saves WHERE slot = ?1"
                ))?
                .query_row([slot], |r| Ok((save_meta(r)?, r.get::<_, Vec<u8>>(7)?, r.get(8)?)))
                .optional()?;
            row.map(|(meta, data, thumbnail_png)| {
                Ok(SaveGame {
                    meta: with_metadata(meta)?,
                    data,
                    thumbnail_png,
                })
            })
            .transpose()
        }

        fn save_list(&self) -> Result<Vec<SaveMeta>> {
            let mut stmt = self.prepare_cached(&format!(
                "SELECT {SAVE_META_COLS} FROM space3d_saves ORDER BY updated_at DESC, seq DESC"
            ))?;
            let rows = stmt.query_map([], save_meta)?.collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().map(with_metadata).collect()
        }

        fn save_delete(&self, slot: &str) -> Result<bool> {
            Ok(self
                .prepare_cached("DELETE FROM space3d_saves WHERE slot = ?1")?
                .execute([slot])?
                > 0)
        }

        fn save_thumbnail(&self, slot: &str) -> Result<Option<Vec<u8>>> {
            Ok(self
                .prepare_cached("SELECT thumbnail FROM space3d_saves WHERE slot = ?1")?
                .query_row([slot], |r| r.get::<_, Option<Vec<u8>>>(0))
                .optional()?
                .flatten())
        }

        fn lb_submit(&self, board: &str, order: ScoreOrder, player: &str, score: i64, now: i64) -> Result<Submission> {
            let tx = self.unchecked_transaction()?;
            let previous: Option<i64> = tx
                .prepare_cached("SELECT score FROM space3d_leaderboard WHERE board = ?1 AND player = ?2")?
                .query_row([board, player], |r| r.get(0))
                .optional()?;
            let improved = previous.is_none_or(|p| order.better(score, p));
            if improved {
                tx.prepare_cached(
                    "INSERT INTO space3d_leaderboard (board, player, score, achieved_at, seq)
                     VALUES (?1, ?2, ?3, ?4, (SELECT COALESCE(MAX(seq), 0) + 1 FROM space3d_leaderboard))
                     ON CONFLICT (board, player) DO UPDATE SET
                        score = excluded.score, achieved_at = excluded.achieved_at, seq = excluded.seq",
                )?
                .execute(params![board, player, score, now])?;
            }
            let best = tx
                .lb_rank(board, order, player)?
                .ok_or_else(|| DbError::other("leaderboard entry vanished during submit"))?;
            tx.commit()?;
            Ok(Submission { improved, best })
        }

        fn lb_top(&self, board: &str, order: ScoreOrder, n: usize) -> Result<Vec<LeaderboardEntry>> {
            let (dir, _) = desc(order);
            let mut stmt = self.prepare_cached(&format!(
                "SELECT RANK() OVER (ORDER BY score {dir}), player, score, achieved_at
                 FROM space3d_leaderboard WHERE board = ?1
                 ORDER BY score {dir}, seq ASC LIMIT ?2"
            ))?;
            let limit = i64::try_from(n).unwrap_or(i64::MAX);
            let rows = stmt
                .query_map(params![board, limit], entry)?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        }

        fn lb_rank(&self, board: &str, order: ScoreOrder, player: &str) -> Result<Option<LeaderboardEntry>> {
            let (_, better) = desc(order);
            Ok(self
                .prepare_cached(&format!(
                    "SELECT 1 + (SELECT COUNT(*) FROM space3d_leaderboard o
                                 WHERE o.board = e.board AND o.score {better} e.score),
                            player, score, achieved_at
                     FROM space3d_leaderboard e WHERE board = ?1 AND player = ?2"
                ))?
                .query_row([board, player], entry)
                .optional()?)
        }

        fn lb_count(&self, board: &str) -> Result<u64> {
            let n: i64 = self
                .prepare_cached("SELECT COUNT(*) FROM space3d_leaderboard WHERE board = ?1")?
                .query_row([board], |r| r.get(0))?;
            Ok(n as u64)
        }

        fn lb_remove(&self, board: &str, player: &str) -> Result<bool> {
            Ok(self
                .prepare_cached("DELETE FROM space3d_leaderboard WHERE board = ?1 AND player = ?2")?
                .execute([board, player])?
                > 0)
        }

        fn lb_clear(&self, board: &str) -> Result<usize> {
            Ok(self
                .prepare_cached("DELETE FROM space3d_leaderboard WHERE board = ?1")?
                .execute([board])?)
        }
    }
}
