//! Room checkpoints for crash recovery and graceful restarts.
//!
//! A checkpoint = replicated state (`space3d_replication::server::full_state::FullState`,
//! codec bytes, fast) + optional server-only state (the game's `ComponentRegistry` →
//! `space3d_scene` JSON) + an optional game blob, written every `checkpoint_interval`, on phase
//! changes, on drain and on request. Capture runs on the room's thread right after a tick;
//! serialization runs on the compute pool and the upload on the I/O runtime; at most one upload
//! per room is in flight (a newer checkpoint replaces a queued one).

use crate::clock::UnixClock;
use crate::game::RoomSpec;
use crate::ps::store::PlayerStore;
use serde::{Deserialize, Serialize};
use space3d_db::rusqlite::{OptionalExtension, params};
use space3d_db::{Db, Migration};
use space3d_net::{Http, HttpRequest, NetError};
use space3d_replication::RoomId;
use space3d_replication::server::full_state::FullState;
use space3d_tasks::Task;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Format version of [`Checkpoint`].
pub const CHECKPOINT_VERSION: u16 = 1;

/// Largest accepted checkpoint (bytes).
pub const MAX_CHECKPOINT_BYTES: usize = 16 << 20;

/// A room checkpoint.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// [`CHECKPOINT_VERSION`].
    pub version: u16,
    /// Room.
    pub room_id: RoomId,
    /// Spec (to rebuild the room).
    pub spec: RoomSpec,
    /// Replicated state (tick, epoch, schema hash inside).
    pub state: FullState,
    /// Server-only components (scene JSON from `GameDefinition::checkpoint_registry`).
    pub server_only: Option<String>,
    /// Game-defined blob (resources).
    pub game_blob: Option<Vec<u8>>,
    /// Save time (unix ms).
    pub saved_ms: i64,
}

impl Checkpoint {
    /// postcard bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("checkpoint serializes")
    }

    /// Inverse of [`Self::to_bytes`] (checks the version).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_CHECKPOINT_BYTES {
            return Err(format!("checkpoint of {} bytes exceeds the limit", bytes.len()));
        }
        let cp: Checkpoint = postcard::from_bytes(bytes).map_err(|e| e.to_string())?;
        if cp.version != CHECKPOINT_VERSION {
            return Err(format!(
                "checkpoint version {} (expected {CHECKPOINT_VERSION})",
                cp.version
            ));
        }
        Ok(cp)
    }
}

/// Checkpoint store failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("checkpoint store: {0}")]
pub struct StoreError(pub String);

/// Where checkpoints live. Every call returns a pollable `Task` (never blocks a tick).
pub trait CheckpointStore: Send + Sync + 'static {
    /// Save (replace) a room's checkpoint.
    fn put(&self, room: RoomId, bytes: Vec<u8>) -> Task<Result<(), StoreError>>;
    /// Latest checkpoint of a room.
    fn get(&self, room: RoomId) -> Task<Result<Option<Vec<u8>>, StoreError>>;
    /// Rooms with checkpoints newer than `min_saved_ms`.
    fn recent(&self, min_saved_ms: i64) -> Task<Result<Vec<RoomId>, StoreError>>;
    /// Delete (room closed normally).
    fn delete(&self, room: RoomId) -> Task<Result<(), StoreError>>;
    /// `false` for [`NoCheckpoints`]: rooms skip capture entirely.
    fn enabled(&self) -> bool {
        true
    }
}

fn ready<T: Send + 'static>(v: T) -> Task<T> {
    space3d_tasks::spawn(async move { v })
}

fn flatten<T: Send + 'static, E: std::fmt::Display + Send + 'static>(
    task: Task<Result<T, E>>,
) -> Task<Result<T, StoreError>> {
    space3d_tasks::spawn(async move {
        match task.wait().await {
            Some(r) => r.map_err(|e| StoreError(e.to_string())),
            None => Err(StoreError("worker gone".into())),
        }
    })
}

/// Game-server local migrations (outbox is 201, local checkpoints 202).
pub fn local_migrations() -> Vec<Migration> {
    vec![Migration::new(
        202,
        "gs_local_checkpoints",
        "CREATE TABLE IF NOT EXISTS gs_checkpoints(room_id INTEGER PRIMARY KEY, saved_ms INTEGER NOT NULL, data BLOB NOT NULL);",
    )]
}

/// Local SQLite (`space3d_db::Db` on the game server's volume; single-host setups).
pub struct LocalCheckpoints {
    /// Database.
    pub db: Db,
    /// Wall clock.
    pub clock: Arc<dyn UnixClock>,
}

impl LocalCheckpoints {
    /// Open (applies the migration; resolves when ready).
    pub fn open(db: Db, clock: Arc<dyn UnixClock>) -> Task<Result<Self, StoreError>> {
        let migrated = db.migrate(local_migrations());
        space3d_tasks::spawn(async move {
            match migrated.wait().await {
                Some(Ok(_)) => Ok(Self { db, clock }),
                Some(Err(e)) => Err(StoreError(e.to_string())),
                None => Err(StoreError("worker gone".into())),
            }
        })
    }
}

impl CheckpointStore for LocalCheckpoints {
    fn put(&self, room: RoomId, bytes: Vec<u8>) -> Task<Result<(), StoreError>> {
        let now = self.clock.now_ms();
        flatten(self.db.execute(
            "INSERT INTO gs_checkpoints(room_id, saved_ms, data) VALUES (?1, ?2, ?3)
             ON CONFLICT(room_id) DO UPDATE SET saved_ms = excluded.saved_ms, data = excluded.data",
            (room.0 as i64, now, bytes),
        ))
        .map_unit()
    }

    fn get(&self, room: RoomId) -> Task<Result<Option<Vec<u8>>, StoreError>> {
        flatten(self.db.with_connection(move |c| {
            Ok(c.prepare_cached("SELECT data FROM gs_checkpoints WHERE room_id = ?1")?
                .query_row(params![room.0 as i64], |r| r.get::<_, Vec<u8>>(0))
                .optional()?)
        }))
    }

    fn recent(&self, min_saved_ms: i64) -> Task<Result<Vec<RoomId>, StoreError>> {
        flatten(self.db.with_connection(move |c| {
            let mut stmt =
                c.prepare_cached("SELECT room_id FROM gs_checkpoints WHERE saved_ms >= ?1 ORDER BY room_id")?;
            let rows = stmt
                .query_map(params![min_saved_ms], |r| r.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows.into_iter().map(|r| RoomId(r as u64)).collect())
        }))
    }

    fn delete(&self, room: RoomId) -> Task<Result<(), StoreError>> {
        flatten(
            self.db
                .execute("DELETE FROM gs_checkpoints WHERE room_id = ?1", (room.0 as i64,)),
        )
        .map_unit()
    }
}

trait MapUnit {
    fn map_unit(self) -> Task<Result<(), StoreError>>;
}

impl MapUnit for Task<Result<usize, StoreError>> {
    fn map_unit(self) -> Task<Result<(), StoreError>> {
        space3d_tasks::spawn(async move {
            match self.wait().await {
                Some(r) => r.map(|_| ()),
                None => Err(StoreError("worker gone".into())),
            }
        })
    }
}

/// Player-service storage (`PUT/GET/DELETE /internal/v1/rooms/{id}/checkpoint`, fleet token):
/// any game server can restore a room after a server loss. Default in split mode.
pub struct RemoteCheckpoints {
    /// HTTP client.
    pub http: Http,
    /// Player service base URL.
    pub base_url: String,
    /// Fleet bearer token.
    pub token: String,
}

impl RemoteCheckpoints {
    fn url(&self, room: RoomId) -> String {
        format!(
            "{}/internal/v1/rooms/{room}/checkpoint",
            self.base_url.trim_end_matches('/')
        )
    }

    fn auth(&self, req: HttpRequest) -> HttpRequest {
        req.header("authorization", format!("Bearer {}", self.token))
            .timeout(std::time::Duration::from_secs(30))
    }
}

fn net_err(e: NetError) -> StoreError {
    StoreError(e.to_string())
}

impl CheckpointStore for RemoteCheckpoints {
    fn put(&self, room: RoomId, bytes: Vec<u8>) -> Task<Result<(), StoreError>> {
        let fut = self.http.fetch(
            self.auth(HttpRequest::put(self.url(room)))
                .header("content-type", "application/octet-stream")
                .body(bytes),
        );
        space3d_tasks::spawn(async move {
            fut.await
                .and_then(|r| r.error_for_status())
                .map(|_| ())
                .map_err(net_err)
        })
    }

    fn get(&self, room: RoomId) -> Task<Result<Option<Vec<u8>>, StoreError>> {
        let fut = self.http.fetch(self.auth(HttpRequest::get(self.url(room))));
        space3d_tasks::spawn(async move {
            let res = fut.await.map_err(net_err)?;
            if res.status == 404 {
                return Ok(None);
            }
            res.error_for_status().map(|r| Some(r.body)).map_err(net_err)
        })
    }

    fn recent(&self, min_saved_ms: i64) -> Task<Result<Vec<RoomId>, StoreError>> {
        let url = format!(
            "{}/internal/v1/checkpoints?since={min_saved_ms}",
            self.base_url.trim_end_matches('/')
        );
        let fut = self.http.fetch(self.auth(HttpRequest::get(url)));
        space3d_tasks::spawn(async move {
            fut.await
                .and_then(|r| r.error_for_status())
                .and_then(|r| r.json::<Vec<RoomId>>())
                .map_err(net_err)
        })
    }

    fn delete(&self, room: RoomId) -> Task<Result<(), StoreError>> {
        let fut = self.http.fetch(self.auth(HttpRequest::delete(self.url(room))));
        space3d_tasks::spawn(async move {
            let res = fut.await.map_err(net_err)?;
            if res.status == 404 {
                return Ok(());
            }
            res.error_for_status().map(|_| ()).map_err(net_err)
        })
    }
}

/// The player service's own store (all-in-one mode: no HTTP hop).
pub struct PlayerServiceCheckpoints {
    /// Store.
    pub store: Arc<dyn PlayerStore>,
    /// Wall clock.
    pub clock: Arc<dyn UnixClock>,
}

impl CheckpointStore for PlayerServiceCheckpoints {
    fn put(&self, room: RoomId, bytes: Vec<u8>) -> Task<Result<(), StoreError>> {
        flatten(self.store.put_checkpoint(room, bytes, self.clock.now_ms()))
    }

    fn get(&self, room: RoomId) -> Task<Result<Option<Vec<u8>>, StoreError>> {
        let t = flatten(self.store.get_checkpoint(room));
        space3d_tasks::spawn(async move {
            match t.wait().await {
                Some(r) => r.map(|o| o.map(|(_, b)| b)),
                None => Err(StoreError("worker gone".into())),
            }
        })
    }

    fn recent(&self, min_saved_ms: i64) -> Task<Result<Vec<RoomId>, StoreError>> {
        flatten(self.store.recent_checkpoints(min_saved_ms))
    }

    fn delete(&self, room: RoomId) -> Task<Result<(), StoreError>> {
        flatten(self.store.delete_checkpoint(room))
    }
}

/// In-memory (tests).
#[derive(Default)]
pub struct MemoryCheckpoints {
    /// Stored bytes with their `saved_ms`.
    pub map: parking_lot::Mutex<BTreeMap<RoomId, (i64, Vec<u8>)>>,
}

impl CheckpointStore for MemoryCheckpoints {
    fn put(&self, room: RoomId, bytes: Vec<u8>) -> Task<Result<(), StoreError>> {
        let saved = Checkpoint::from_bytes(&bytes).map_or(0, |c| c.saved_ms);
        self.map.lock().insert(room, (saved, bytes));
        ready(Ok(()))
    }

    fn get(&self, room: RoomId) -> Task<Result<Option<Vec<u8>>, StoreError>> {
        ready(Ok(self.map.lock().get(&room).map(|(_, b)| b.clone())))
    }

    fn recent(&self, min_saved_ms: i64) -> Task<Result<Vec<RoomId>, StoreError>> {
        ready(Ok(self
            .map
            .lock()
            .iter()
            .filter(|(_, (saved, _))| *saved >= min_saved_ms)
            .map(|(r, _)| *r)
            .collect()))
    }

    fn delete(&self, room: RoomId) -> Task<Result<(), StoreError>> {
        self.map.lock().remove(&room);
        ready(Ok(()))
    }
}

/// No-op store (`checkpoint_store = "none"`).
pub struct NoCheckpoints;

impl CheckpointStore for NoCheckpoints {
    fn put(&self, _room: RoomId, _bytes: Vec<u8>) -> Task<Result<(), StoreError>> {
        ready(Ok(()))
    }

    fn get(&self, _room: RoomId) -> Task<Result<Option<Vec<u8>>, StoreError>> {
        ready(Ok(None))
    }

    fn recent(&self, _min_saved_ms: i64) -> Task<Result<Vec<RoomId>, StoreError>> {
        ready(Ok(Vec::new()))
    }

    fn delete(&self, _room: RoomId) -> Task<Result<(), StoreError>> {
        ready(Ok(()))
    }

    fn enabled(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use space3d_replication::{NetId, Tick};

    pub(crate) fn sample(room: u64, saved_ms: i64) -> Checkpoint {
        Checkpoint {
            version: CHECKPOINT_VERSION,
            room_id: RoomId(room),
            spec: RoomSpec {
                room_id: RoomId(room),
                mode: "zone".into(),
                persistent: true,
                ..Default::default()
            },
            state: FullState {
                schema_hash: 0xabc,
                tick: Tick(600),
                epoch: 2,
                next_net_id: 3,
                entities: vec![(NetId(1), vec![("core.transform".into(), vec![1, 2, 3])])],
            },
            server_only: Some("{}".into()),
            game_blob: None,
            saved_ms,
        }
    }

    #[test]
    fn checkpoint_roundtrip_and_version_check() {
        let cp = sample(5, 1_000);
        let bytes = cp.to_bytes();
        assert_eq!(Checkpoint::from_bytes(&bytes).unwrap(), cp);
        let mut old = cp.clone();
        old.version = 99;
        assert!(Checkpoint::from_bytes(&old.to_bytes()).is_err());
        assert!(Checkpoint::from_bytes(&[1, 2, 3]).is_err());
    }

    fn b<T>(t: Task<T>) -> T {
        t.block().unwrap()
    }

    fn exercise(store: &dyn CheckpointStore) {
        assert_eq!(b(store.get(RoomId(5))).unwrap(), None);
        b(store.put(RoomId(5), sample(5, 1_000).to_bytes())).unwrap();
        b(store.put(RoomId(5), sample(5, 2_000).to_bytes())).unwrap();
        let back = b(store.get(RoomId(5))).unwrap().unwrap();
        assert_eq!(Checkpoint::from_bytes(&back).unwrap().saved_ms, 2_000);
        assert_eq!(b(store.recent(0)).unwrap(), vec![RoomId(5)]);
        b(store.delete(RoomId(5))).unwrap();
        assert_eq!(b(store.get(RoomId(5))).unwrap(), None);
    }

    #[test]
    fn local_memory_and_player_service_stores() {
        let clock: Arc<dyn UnixClock> = Arc::new(crate::clock::ManualUnixClock::at(2_000));
        let db = Db::open_in_memory();
        let local = LocalCheckpoints::open(db.clone(), clock.clone())
            .block()
            .unwrap()
            .unwrap();
        exercise(&local);
        exercise(&MemoryCheckpoints::default());
        db.blocking().migrate(crate::ps::schema::migrations()).unwrap();
        exercise(&PlayerServiceCheckpoints {
            store: Arc::new(crate::ps::store::SqliteStore::new(db)),
            clock,
        });
        let none = NoCheckpoints;
        assert!(!none.enabled());
        assert_eq!(none.get(RoomId(1)).block().unwrap().unwrap(), None);
    }
}
