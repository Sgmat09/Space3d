//! Durable outbox (game server): results, player-data writes and scores are written to the
//! local `space3d-db` (`gs_outbox(op_id PK, payload, created_ms)`) before being sent over the
//! fleet link, and deleted only on `FleetDown::Ack { op_id }`. Survives player-service outages
//! and graceful restarts; replayed on the next start with the same `server_id` and volume
//! (StatefulSet PVC). On an ephemeral volume a hard crash loses unsent entries — documented
//! trade-off.
//!
//! `op_id`s are unique per room instance (`"<server_id>:<room_id>:<nonce>:<counter>"`, or
//! `"<server_id>:<room_id>:match:<match_id>"` for results), so retries are idempotent on the
//! player service (`applied_ops`). Unacked entries are re-sent after every (re)registration of
//! the fleet link and every [`RESEND_AFTER_MS`].

use super::fleet::{FleetLink, FleetUp};
use crate::clock::UnixClock;
use space3d_db::{Db, DbError, Migration};
use space3d_tasks::{Task, TaskState};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Unacked entries are sent again after this long.
pub const RESEND_AFTER_MS: i64 = 10_000;

/// Bound of the room → outbox channel.
pub const OUTBOX_CHANNEL: usize = 16_384;

/// The outbox channel is full.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("outbox full")]
pub struct OutboxFull;

/// Non-blocking handle rooms use to enqueue (bounded channel into the outbox service).
#[derive(Clone, Debug)]
pub struct OutboxSender {
    tx: async_channel::Sender<FleetUp>,
}

impl OutboxSender {
    /// Queue a message (the outbox assigns an op id if it has an empty one).
    /// `Err(OutboxFull)` if the outbox service is not keeping up (the room retries next tick).
    pub fn push(&self, msg: FleetUp) -> Result<(), OutboxFull> {
        self.tx.try_send(msg).map_err(|_| OutboxFull)
    }

    /// A sender whose messages go nowhere (tests / rooms built without a server).
    pub fn detached() -> Self {
        let (tx, _rx) = async_channel::bounded(1);
        Self { tx }
    }
}

struct Entry {
    msg: FleetUp,
    durable: bool,
    sent_ms: Option<i64>,
}

/// The outbox service (owned by the supervisor).
pub struct Outbox {
    db: Db,
    rx: async_channel::Receiver<FleetUp>,
    tx: async_channel::Sender<FleetUp>,
    server_id: String,
    clock: Arc<dyn UnixClock>,
    entries: BTreeMap<String, Entry>,
    persisting: Vec<(String, Task<Result<usize, DbError>>)>,
    last_session: u64,
    nonce: u64,
    counter: u64,
}

/// Outbox table (version 201; see `ps::schema` for the numbering).
pub fn migrations() -> Vec<Migration> {
    vec![Migration::new(
        201,
        "gs_outbox",
        "CREATE TABLE IF NOT EXISTS gs_outbox(op_id TEXT PRIMARY KEY, payload TEXT NOT NULL, created_ms INTEGER NOT NULL);",
    )]
}

impl Outbox {
    /// Open (runs the outbox migration and loads unacked entries) — resolves when ready.
    pub fn open(db: Db, server_id: &str, clock: Arc<dyn UnixClock>) -> Task<Result<Self, DbError>> {
        let migrated = db.migrate(migrations());
        let server_id = server_id.to_owned();
        space3d_tasks::spawn(async move {
            migrated.wait().await.ok_or(DbError::WorkerGone)??;
            let rows: Vec<(String, String)> = db
                .query("SELECT op_id, payload FROM gs_outbox ORDER BY created_ms, op_id", ())
                .wait()
                .await
                .ok_or(DbError::WorkerGone)??;
            let mut entries = BTreeMap::new();
            for (op_id, payload) in rows {
                match serde_json::from_str::<FleetUp>(&payload) {
                    Ok(msg) => {
                        entries.insert(
                            op_id,
                            Entry {
                                msg,
                                durable: true,
                                sent_ms: None,
                            },
                        );
                    }
                    Err(e) => tracing::error!(%op_id, error = %e, "dropping unreadable outbox entry"),
                }
            }
            if !entries.is_empty() {
                tracing::info!(pending = entries.len(), "replaying outbox from the previous run");
            }
            let (tx, rx) = async_channel::bounded(OUTBOX_CHANNEL);
            Ok(Self {
                db,
                rx,
                tx,
                server_id,
                clock,
                entries,
                persisting: Vec::new(),
                last_session: 0,
                nonce: u64::from_le_bytes(crate::security::random_bytes::<8>()),
                counter: 0,
            })
        })
    }

    /// Sender for rooms.
    pub fn sender(&self) -> OutboxSender {
        OutboxSender { tx: self.tx.clone() }
    }

    fn accept(&mut self, mut msg: FleetUp, now: i64) {
        let Some(op) = msg.op_id_mut() else {
            tracing::warn!(kind = msg.kind(), "non-op message pushed to the outbox; ignored");
            return;
        };
        if op.is_empty() {
            self.counter += 1;
            *op = format!("{}:ob:{:016x}:{}", self.server_id, self.nonce, self.counter);
        }
        let op_id = op.clone();
        if self.entries.contains_key(&op_id) {
            return;
        }
        match serde_json::to_string(&msg) {
            Ok(payload) => {
                let task = self.db.execute(
                    "INSERT OR IGNORE INTO gs_outbox(op_id, payload, created_ms) VALUES (?1, ?2, ?3)",
                    (op_id.clone(), payload, now),
                );
                self.persisting.push((op_id.clone(), task));
            }
            Err(e) => tracing::error!(%op_id, error = %e, "outbox entry does not serialize"),
        }
        self.entries.insert(
            op_id,
            Entry {
                msg,
                durable: false,
                sent_ms: None,
            },
        );
    }

    /// Persist newly queued messages and send everything durable and unacked over `link`
    /// (supervisor tick; never blocks).
    pub fn pump(&mut self, link: &dyn FleetLink) {
        let now = self.clock.now_ms();
        while let Ok(msg) = self.rx.try_recv() {
            self.accept(msg, now);
        }
        let entries = &mut self.entries;
        self.persisting.retain_mut(|(op, task)| match task.state() {
            TaskState::Pending => true,
            TaskState::Ready | TaskState::Cancelled => {
                if let Some(Err(e)) = task.poll() {
                    tracing::error!(op_id = %op, error = %e, "outbox persist failed; delivering from memory");
                }
                if let Some(e) = entries.get_mut(op) {
                    e.durable = true;
                }
                false
            }
        });
        let session = link.session();
        if session != self.last_session {
            self.last_session = session;
            for e in self.entries.values_mut() {
                e.sent_ms = None;
            }
        }
        if !link.is_connected() {
            return;
        }
        for e in self.entries.values_mut() {
            if !e.durable || e.sent_ms.is_some_and(|t| now.saturating_sub(t) < RESEND_AFTER_MS) {
                continue;
            }
            if link.send(e.msg.clone()).is_err() {
                break;
            }
            e.sent_ms = Some(now);
        }
    }

    /// Mark an op applied (deletes it).
    pub fn ack(&mut self, op_id: &str) {
        if self.entries.remove(op_id).is_some() {
            // Fire and forget: the worker runs the job even if the task is dropped.
            drop(
                self.db
                    .execute("DELETE FROM gs_outbox WHERE op_id = ?1", (op_id.to_owned(),)),
            );
        }
    }

    /// Entries not yet acked.
    pub fn pending(&self) -> usize {
        self.entries.len() + self.rx.len()
    }

    /// Whether every entry was acked.
    pub fn is_flushed(&self) -> bool {
        self.pending() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualUnixClock;
    use crate::gs::fleet::{FleetDown, FleetError};
    use parking_lot::Mutex;
    use space3d_replication::PlayerId;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    #[derive(Default)]
    struct FakeLink {
        sent: Mutex<Vec<FleetUp>>,
        up: AtomicBool,
        session: AtomicU64,
    }

    impl FleetLink for FakeLink {
        fn send(&self, msg: FleetUp) -> Result<(), FleetError> {
            if !self.up.load(Ordering::SeqCst) {
                return Err(FleetError::Down);
            }
            self.sent.lock().push(msg);
            Ok(())
        }
        fn try_recv(&self) -> Option<FleetDown> {
            None
        }
        fn is_connected(&self) -> bool {
            self.up.load(Ordering::SeqCst)
        }
        fn session(&self) -> u64 {
            self.session.load(Ordering::SeqCst)
        }
    }

    fn score(op: &str) -> FleetUp {
        FleetUp::SubmitScore {
            op_id: op.into(),
            board: "b".into(),
            player: PlayerId(1),
            score: 5,
        }
    }

    fn pump_until(outbox: &mut Outbox, link: &FakeLink, n: usize) {
        for _ in 0..500 {
            outbox.pump(link);
            if link.sent.lock().len() >= n {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("outbox did not send {n} entries");
    }

    #[test]
    fn persists_sends_acks_and_replays_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gs.db");
        let clock = ManualUnixClock::at(1_000);
        let link = FakeLink::default();
        {
            let db = Db::open(&path);
            let mut outbox = Outbox::open(db, "gs-1", Arc::new(clock.clone()))
                .block()
                .unwrap()
                .unwrap();
            let tx = outbox.sender();
            tx.push(score("")).unwrap();
            tx.push(score("fixed-op")).unwrap();
            tx.push(score("fixed-op")).unwrap();
            // Link down: nothing is sent, nothing is lost.
            for _ in 0..20 {
                outbox.pump(&link);
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            assert!(link.sent.lock().is_empty());
            assert_eq!(outbox.pending(), 2, "duplicate op ids collapse");
            link.up.store(true, Ordering::SeqCst);
            link.session.store(1, Ordering::SeqCst);
            pump_until(&mut outbox, &link, 2);
            let ops: Vec<String> = link.sent.lock().iter().map(|m| m.op_id().unwrap().to_owned()).collect();
            assert!(ops.iter().any(|o| o.starts_with("gs-1:ob:")));
            outbox.ack("fixed-op");
            assert_eq!(outbox.pending(), 1);
            // Not resent before the resend interval...
            outbox.pump(&link);
            assert_eq!(link.sent.lock().len(), 2);
            // ...but after it.
            clock.advance_ms(RESEND_AFTER_MS);
            outbox.pump(&link);
            assert_eq!(link.sent.lock().len(), 3);
            // The worker is FIFO: this completes after the fire-and-forget delete.
            outbox.db.blocking().query_one::<i64>("SELECT 1", ()).unwrap();
        }
        // Restart: the unacked entry is replayed from disk.
        let db = Db::open(&path);
        let mut outbox = Outbox::open(db, "gs-1", Arc::new(clock.clone()))
            .block()
            .unwrap()
            .unwrap();
        assert_eq!(outbox.pending(), 1);
        let link2 = FakeLink::default();
        link2.up.store(true, Ordering::SeqCst);
        link2.session.store(1, Ordering::SeqCst);
        pump_until(&mut outbox, &link2, 1);
        let op = link2.sent.lock()[0].op_id().unwrap().to_owned();
        outbox.ack(&op);
        assert!(outbox.is_flushed());
    }
}
