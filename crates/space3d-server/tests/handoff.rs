//! Drain handoff of a persistent room against a checkpoint store that is slow, unreachable or
//! answers out of order: the room itself (`build_room_app` / `run_tick`, as the shards drive
//! it) with a manual wall clock and a store the test opens and closes.

mod common;

use common::{DuelGame, Health};
use parking_lot::Mutex;
use space3d_core::prelude::*;
use space3d_replication::{Replicated, RoomId};
use space3d_server::clock::ManualUnixClock;
use space3d_server::game::RoomSpec;
use space3d_server::gs::checkpoint::{Checkpoint, CheckpointStore, MemoryCheckpoints, StoreError};
use space3d_server::gs::fleet::FleetUp;
use space3d_server::gs::room::{RoomBuild, build_room_app, run_tick, stop_hosted};
use space3d_server::gs::rooms::{RoomCommand, RoomEnv, RoomPhase, RoomStatus};
use space3d_server::testing::{TestCluster, TestClusterConfig};
use space3d_tasks::Task;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

const ROOM: RoomId = RoomId(4242);
const STEP: Duration = Duration::from_nanos(16_666_667);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Puts are stored at once.
    Up,
    /// Puts fail (store unreachable).
    Down,
    /// Puts wait until the test releases them, in order.
    Hold,
}

#[derive(Default)]
struct Inner {
    mode: Option<Mode>,
    stored: Option<Vec<u8>>,
    held: VecDeque<async_channel::Sender<()>>,
    puts: u32,
}

/// A checkpoint store the test controls (keeps the greater `(epoch, saved_ms)` key, like the
/// real stores).
#[derive(Clone, Default)]
struct TestStore(Arc<Mutex<Inner>>);

impl TestStore {
    fn set(&self, mode: Mode) {
        self.0.lock().mode = Some(mode);
    }

    /// Let the oldest held put through; `false` if none is waiting.
    fn release_one(&self) -> bool {
        let tx = self.0.lock().held.pop_front();
        tx.is_some_and(|tx| tx.try_send(()).is_ok())
    }

    fn held(&self) -> usize {
        self.0.lock().held.len()
    }

    fn stored(&self) -> Option<Checkpoint> {
        self.0
            .lock()
            .stored
            .as_ref()
            .map(|b| Checkpoint::from_bytes(b).unwrap())
    }

    fn store(inner: &Mutex<Inner>, bytes: Vec<u8>) {
        let key = Checkpoint::peek(&bytes).unwrap().1;
        let mut s = inner.lock();
        if s.stored
            .as_ref()
            .is_none_or(|old| Checkpoint::peek(old).unwrap().1 <= key)
        {
            s.stored = Some(bytes);
        }
    }
}

impl CheckpointStore for TestStore {
    fn put(&self, _room: RoomId, bytes: Vec<u8>) -> Task<Result<(), StoreError>> {
        let inner = self.0.clone();
        let mode = {
            let mut s = inner.lock();
            s.puts += 1;
            s.mode.unwrap_or(Mode::Up)
        };
        match mode {
            Mode::Up => {
                Self::store(&inner, bytes);
                space3d_tasks::spawn(async { Ok(()) })
            }
            Mode::Down => space3d_tasks::spawn(async { Err(StoreError("unreachable".into())) }),
            Mode::Hold => {
                let (tx, rx) = async_channel::bounded::<()>(1);
                inner.lock().held.push_back(tx);
                space3d_tasks::spawn(async move {
                    rx.recv().await.map_err(|_| StoreError("dropped".into()))?;
                    Self::store(&inner, bytes);
                    Ok(())
                })
            }
        }
    }

    fn get(&self, _room: RoomId) -> Task<Result<Option<Vec<u8>>, StoreError>> {
        let v = self.0.lock().stored.clone();
        space3d_tasks::spawn(async move { Ok(v) })
    }

    fn recent(&self, _min_saved_ms: i64) -> Task<Result<Vec<RoomId>, StoreError>> {
        space3d_tasks::spawn(async { Ok(Vec::new()) })
    }

    fn delete(&self, _room: RoomId) -> Task<Result<(), StoreError>> {
        let inner = self.0.clone();
        space3d_tasks::spawn(async move {
            inner.lock().stored = None;
            Ok(())
        })
    }
}

/// A persistent room whose one replicated entity's `Health` counts ticks.
struct Room {
    _cluster: TestCluster,
    app: App,
    env: RoomEnv,
    unix: ManualUnixClock,
    store: TestStore,
    rescue: Arc<MemoryCheckpoints>,
    commands: crossbeam_channel::Sender<RoomCommand>,
    status: Arc<Mutex<RoomStatus>>,
    uplink: async_channel::Receiver<(RoomId, FleetUp)>,
    fleet: Vec<FleetUp>,
    counter: Entity,
}

fn spec() -> RoomSpec {
    RoomSpec {
        room_id: ROOM,
        mode: "world".into(),
        max_players: 8,
        persistent: true,
        ..Default::default()
    }
}

impl Room {
    fn new() -> Self {
        let cluster = TestCluster::new(DuelGame, TestClusterConfig::default());
        let mut env = cluster.game_server("gs-1").unwrap().rooms.env().clone();
        let store = TestStore::default();
        let rescue = Arc::new(MemoryCheckpoints::default());
        let unix = ManualUnixClock::at(1_900_000_000_000);
        let (up_tx, up_rx) = async_channel::unbounded();
        env.checkpoints = Arc::new(store.clone());
        env.rescue = Some(rescue.clone());
        env.unix = Arc::new(unix.clone());
        env.uplink = up_tx;
        env.checkpoint_interval = Duration::from_secs(600);
        let (_sender, inbox) = space3d_replication::server::room_channel(64);
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let status = Arc::new(Mutex::new(RoomStatus::default()));
        let mut app = build_room_app(RoomBuild {
            spec: spec(),
            env: env.clone(),
            inbox,
            commands: cmd_rx,
            status: status.clone(),
            restore: None,
            created_ms: 1_900_000_000_000,
            epoch: None,
        })
        .unwrap();
        let counter = app.world.spawn((Replicated, Health(0)));
        app.add_system(Stage::FixedUpdate, move |ctx| {
            if let Ok(mut h) = ctx.world.get::<&mut Health>(counter) {
                h.0 += 1;
            }
        });
        Self {
            _cluster: cluster,
            app,
            env,
            unix,
            store,
            rescue,
            commands: cmd_tx,
            status,
            uplink: up_rx,
            fleet: Vec::new(),
            counter,
        }
    }

    fn health(&self) -> u16 {
        self.app.world.get::<&Health>(self.counter).unwrap().0
    }

    /// One tick; `advance_ms` of wall clock first (0 = a catch-up tick in the same millisecond).
    fn tick(&mut self, advance_ms: i64) {
        self.unix.advance_ms(advance_ms);
        run_tick(&mut self.app, STEP);
        while let Ok((_, msg)) = self.uplink.try_recv() {
            self.fleet.push(msg);
        }
    }

    /// Tick with real time in between (background serialization and uploads) until `pred`.
    fn tick_until(&mut self, max: u32, mut pred: impl FnMut(&Self) -> bool) -> bool {
        for _ in 0..max {
            if pred(self) {
                return true;
            }
            self.tick(17);
            std::thread::sleep(Duration::from_millis(2));
        }
        pred(self)
    }

    fn closed(&self) -> bool {
        self.status.lock().phase == RoomPhase::Closed
    }

    fn reported_restorable(&self) -> bool {
        self.fleet
            .iter()
            .any(|m| matches!(m, FleetUp::RoomClosed { restorable: true, .. }))
    }

    fn moving(&self) -> bool {
        self.fleet.iter().any(|m| matches!(m, FleetUp::RoomMoving { .. }))
    }
}

/// `Health` of the counter entity in a checkpoint (restored into a fresh room app).
fn restored_health(env: &RoomEnv, cp: Checkpoint) -> u16 {
    let (_sender, inbox) = space3d_replication::server::room_channel(8);
    let (_tx, rx) = crossbeam_channel::unbounded();
    let app = build_room_app(RoomBuild {
        spec: spec(),
        env: env.clone(),
        inbox,
        commands: rx,
        status: Arc::default(),
        restore: Some(cp),
        created_ms: 0,
        epoch: None,
    })
    .unwrap();
    let v: Vec<u16> = app.world.query::<&Health>().iter().map(|h| h.0).collect();
    assert_eq!(v.len(), 1, "one counter entity: {v:?}");
    v[0]
}

/// Regression: a frozen room got `RoomCommand::Shutdown` right after `Drain` (a server with
/// only persistent rooms is idle at once), and the shutdown wait gave up after 15 s of ticks:
/// the room closed with `RoomClosed { restorable: true }` while its freeze capture was never
/// stored, the capture was thrown away, and a rejoin restored an older checkpoint — everything
/// since lost. (It also made `a_drain_handoff_keeps_every_change_the_players_saw` flaky: the
/// test burns 900 ticks in a few milliseconds.) The room now waits for exactly its freeze
/// capture, however long the store takes, retrying it.
#[test]
fn a_frozen_room_is_not_restorable_until_its_freeze_capture_is_stored() {
    let mut r = Room::new();
    // An empty room in `Waiting` takes no checkpoints (the periodic one is 10 min away).
    for _ in 0..30 {
        r.tick(17);
    }
    assert_eq!(r.store.0.lock().puts, 0);
    r.store.set(Mode::Down);
    r.commands
        .send(RoomCommand::Drain {
            deadline: Duration::from_secs(30),
        })
        .unwrap();
    r.tick(17);
    let frozen_at = r.health();
    r.commands.send(RoomCommand::Shutdown).unwrap();
    // 25 s of ticks with the store unreachable (well past the 15 s shutdown wait).
    assert!(
        !r.tick_until(1_500, |r| r.closed()),
        "closed without its freeze capture"
    );
    assert!(r.moving(), "the player service was told the room is moving");
    assert!(!r.reported_restorable(), "never reported restorable meanwhile");
    assert!(r.store.0.lock().puts >= 3, "the freeze capture is retried");
    assert!(r.health() > frozen_at + 1_000, "the room kept ticking");
    // The store comes back: the retried freeze capture lands and the room closes.
    r.store.set(Mode::Up);
    assert!(r.tick_until(3_000, |r| r.closed()), "closes once the capture is stored");
    assert!(r.reported_restorable());
    let cp = r.store.stored().expect("freeze capture stored");
    assert_eq!(restored_health(&r.env, cp), frozen_at, "exactly the freeze capture");
}

/// If the process stops while the store still has not confirmed the freeze capture, the shard
/// keeps that capture in the local rescue store (uploaded on the next start) instead of
/// dropping it.
#[test]
fn an_unconfirmed_freeze_capture_goes_to_the_rescue_store_at_stop() {
    let mut r = Room::new();
    // An empty room in `Waiting` takes no checkpoints (the periodic one is 10 min away).
    for _ in 0..30 {
        r.tick(17);
    }
    assert_eq!(r.store.0.lock().puts, 0);
    r.store.set(Mode::Down);
    r.commands
        .send(RoomCommand::Drain {
            deadline: Duration::from_secs(30),
        })
        .unwrap();
    r.tick(17);
    let frozen_at = r.health();
    r.commands.send(RoomCommand::Shutdown).unwrap();
    assert!(!r.tick_until(120, |r| r.closed()));
    let write = stop_hosted(&mut r.app).expect("a rescue write");
    write.block().unwrap().unwrap();
    let bytes = r.rescue.get(ROOM).block().unwrap().unwrap().expect("rescued");
    let cp = Checkpoint::from_bytes(&bytes).unwrap();
    assert_eq!(restored_health(&r.env, cp), frozen_at);
}

/// Regression: the handoff compared capture times at millisecond granularity. Catch-up ticks
/// run back to back, so a checkpoint captured on the tick before the drain could share the
/// freeze tick's unix millisecond; once that earlier upload landed the room counted the handoff
/// as done, closed restorable and abandoned the freeze capture queued behind it — the restore
/// lost the last ticks the players saw.
#[test]
fn an_earlier_capture_in_the_same_millisecond_does_not_complete_the_handoff() {
    let mut r = Room::new();
    // An empty room in `Waiting` takes no checkpoints (the periodic one is 10 min away).
    for _ in 0..30 {
        r.tick(17);
    }
    assert_eq!(r.store.0.lock().puts, 0);
    r.store.set(Mode::Hold);
    // Tick N-1 captures (an admin checkpoint request); tick N (a catch-up tick, same unix
    // millisecond) receives the drain.
    r.commands.send(RoomCommand::Checkpoint).unwrap();
    r.tick(17);
    let before = r.health();
    r.commands
        .send(RoomCommand::Drain {
            deadline: Duration::from_secs(30),
        })
        .unwrap();
    r.tick(0);
    let frozen_at = r.health();
    assert_eq!(frozen_at, before + 1);
    let deadline = Instant::now() + Duration::from_secs(10);
    while r.store.held() == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    // Only the N-1 upload lands.
    assert!(r.store.release_one());
    assert!(
        !r.tick_until(300, |r| r.closed()),
        "the freeze capture is not stored yet"
    );
    assert!(!r.reported_restorable());
    // Then the freeze capture.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !r.closed() && Instant::now() < deadline {
        r.store.release_one();
        r.tick(17);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(r.closed() && r.reported_restorable());
    let cp = r.store.stored().expect("stored");
    assert_eq!(
        restored_health(&r.env, cp),
        frozen_at,
        "the freeze capture, not tick N-1's"
    );
}
