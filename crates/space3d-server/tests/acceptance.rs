//! Acceptance tests for the dedicated server + netcode, end to end:
//!
//! * **Lossy network** (`TestCluster`, in process, manual clocks): one player service and a
//!   game server; 4 clients join through guest login → quick play → signed ticket →
//!   gatekeeper, over the conditioned memory transport with WebSocket (stream) semantics at
//!   100 ms RTT, ±20 ms jitter per direction and 5 % loss per direction:
//!   - every client converges to the server state (all pawns, positions within 1 mm once
//!     input stops; interpolated remote pawns track the server's past positions while moving);
//!   - the local pawn's prediction error after reconciliation stays small;
//!   - a client whose link drops resumes its session (same slot, pawn and `NetId`);
//!   - downstream / upstream bandwidth per client stays under budget, and a 300-entity room
//!     is held to the configured per-client byte budget.
//! * **Separate processes** (real sockets): a `player-service` App and a `game-server` App
//!   talking only over localhost (fleet WebSocket); a client logs in, obtains a join ticket and
//!   connects; the ticket is single-use.

use serde::{Deserialize, Serialize};
use space3d_core::prelude::*;
use space3d_replication::api::{QuickPlayRequest, SessionTokens};
use space3d_replication::bots::{install_bot, scripted};
use space3d_replication::client::{
    ClientConfig, ClientReplicationPlugin, ConnectTarget, ConnectionEvent, ConnectionState, FailReason, Mispredicted,
    NetClient,
};
use space3d_replication::harness::ClientHandle;
use space3d_replication::prelude::*;
use space3d_replication::protocol::handshake::RejectCode;
use space3d_replication::server::NetServer;
use space3d_replication::services::{DeviceCredentials, PlayerServiceClient};
use space3d_replication::transport::conditioner::{LinkConditions, LinkProfile, Semantics};
use space3d_replication::{Controlled, NetEntity, NetId, RoomId};
use space3d_server::config::{QueueConfig, Role, ServerConfig};
use space3d_server::game::{GameDefinition, RoomSpec};
use space3d_server::gs::GameServerHandle;
use space3d_server::health::HealthState;
use space3d_server::ps::PlayerServiceHandle;
use space3d_server::security::Secrets;
use space3d_server::testing::{TestCluster, TestClusterConfig};
use space3d_server::{BuildOptions, ServerMain};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

// ---- test game -------------------------------------------------------------------------------

const GAME_ID: &str = "acceptance";
/// Pawn speed (m/s) at full input.
const SPEED: f32 = 4.0;

/// Held movement (state, not edges).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Move {
    x: i8,
    z: i8,
}

impl NetInput for Move {}

/// Marks server-driven NPCs (interpolated everywhere).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Npc(u16);

fn walk(ctx: &mut SystemCtx) {
    let dt = ctx.resources.expect::<SimTime>().dt_secs();
    for (t, input) in ctx
        .world
        .query_mut::<(&mut Transform, &PlayerInput<Move>)>()
        .with::<&Simulated>()
    {
        let d = Vec3::new(f32::from(input.0.x), 0.0, f32::from(input.0.z));
        t.translation += d.normalize_or_zero() * SPEED * dt;
    }
}

struct Protocol;

impl Plugin for Protocol {
    fn build(&self, app: &mut App) {
        app.add_plugin(ReplicationPlugin::default())
            .replicate::<Npc>("acceptance.npc")
            .add_input::<Move>("acceptance.move")
            .add_sim_system("acceptance::walk", walk);
    }
}

/// A pawn per player; `npcs` room param spawns server-driven movers on circles.
struct Rules {
    npcs: u16,
}

impl Plugin for Rules {
    fn build(&self, app: &mut App) {
        for i in 0..self.npcs {
            let a = f32::from(i) * 0.37;
            app.world.spawn((
                Replicated,
                Npc(i),
                Transform::from_xyz(a.cos() * 30.0, 0.0, a.sin() * 30.0),
            ));
        }
        app.add_system_named(Stage::FixedUpdate, "acceptance::npcs", |ctx| {
            let tick = ctx.resources.expect::<NetServer>().tick().0 as f32 / 60.0;
            for (t, npc) in ctx.world.query_mut::<(&mut Transform, &Npc)>() {
                let a = f32::from(npc.0) * 0.37 + tick * 0.2;
                t.translation = Vec3::new(a.cos() * 30.0, 0.0, a.sin() * 30.0);
            }
        });
        let mut joined = EventReader::default();
        let mut left = EventReader::default();
        app.add_system_named(Stage::Update, "acceptance::pawns", move |ctx| {
            let joins: Vec<PlayerJoined> = joined
                .read(ctx.resources.expect::<Events<PlayerJoined>>())
                .cloned()
                .collect();
            for j in joins {
                let n = ctx.world.query::<&ControlledBy>().iter().count() as f32;
                ctx.world.spawn((
                    Replicated,
                    Simulated,
                    ControlledBy(j.identity.player),
                    Transform::from_xyz(n * 3.0, 0.0, 0.0),
                ));
            }
            let leaves: Vec<PlayerLeft> = left
                .read(ctx.resources.expect::<Events<PlayerLeft>>())
                .cloned()
                .collect();
            for l in leaves {
                let gone: Vec<Entity> = ctx
                    .world
                    .query::<(Entity, &ControlledBy)>()
                    .iter()
                    .filter(|(_, c)| c.0 == l.player)
                    .map(|(e, _)| e)
                    .collect();
                for e in gone {
                    let _ = ctx.world.despawn(e);
                }
            }
        });
    }
}

struct Game;

impl GameDefinition for Game {
    fn game_id(&self) -> &str {
        GAME_ID
    }

    fn protocol(&self, app: &mut App) {
        app.add_plugin(Protocol);
    }

    fn build_room(&self, app: &mut App, spec: &RoomSpec) {
        let npcs = spec.params.get("npcs").and_then(|n| n.parse().ok()).unwrap_or(0);
        app.add_plugin(Rules { npcs });
    }

    fn validate_spec(&self, spec: &RoomSpec) -> Result<(), String> {
        match spec.mode.as_str() {
            "ffa" | "crowd" | "dev" => Ok(()),
            m => Err(format!("unknown mode {m}")),
        }
    }
}

// ---- cluster helpers ---------------------------------------------------------------------------

/// 100 ms RTT, ±20 ms jitter and 5 % loss in each direction; on a stream a lost segment stalls
/// everything behind it for the 200 ms retransmission timeout (TCP head-of-line blocking).
fn lossy() -> LinkProfile {
    LinkProfile::symmetric(LinkConditions {
        latency: Duration::from_millis(50),
        jitter: Duration::from_millis(20),
        loss: 0.05,
        ..LinkConditions::perfect()
    })
}

fn cluster(seed: u64, budget: Option<u64>) -> TestCluster {
    let mut config = TestClusterConfig {
        seed,
        link: lossy(),
        semantics: Semantics::Stream,
        ..Default::default()
    };
    config.server.matchmaking.queues.push(QueueConfig {
        name: "crowd".into(),
        mode: "crowd".into(),
        teams: 1,
        team_size: 4,
        min_players: 1,
        params: BTreeMap::from([("npcs".into(), "300".into())]),
        ..Default::default()
    });
    if let Some(b) = budget {
        config.server.limits.client_bandwidth = b;
    }
    TestCluster::new(Game, config)
}

/// Direction pattern of bot `i`: a new heading every 0.75 s, a pause every fourth leg.
fn pattern(i: usize, t: Tick) -> Move {
    const DIRS: [(i8, i8); 8] = [(1, 0), (1, 1), (0, 1), (-1, 1), (-1, 0), (-1, -1), (0, -1), (1, -1)];
    let leg = (t.0 / 45) as usize + i * 3;
    if leg % 4 == 3 {
        return Move::default();
    }
    let (x, z) = DIRS[(leg * 5 + i) % DIRS.len()];
    Move { x, z }
}

struct Player {
    handle: ClientHandle,
    tokens: SessionTokens,
}

/// Guests → quick play (one room) → connect as scripted bots that stop when `stop` is set.
fn join_four(c: &mut TestCluster, mode: &str, stop: &Arc<AtomicBool>) -> (Vec<Player>, RoomId) {
    let mut players = Vec::new();
    let mut room = None;
    for i in 0..4 {
        let tokens = c.guest(&format!("p{i}"));
        let a = c.quick_play(&tokens, mode).expect("quick play");
        assert_eq!(*room.get_or_insert(a.room_id), a.room_id, "one room");
        let stop = stop.clone();
        let handle = c.connect(&a, move |app| {
            app.add_plugin(Protocol);
            install_bot(
                app,
                scripted(move |t| {
                    if stop.load(Ordering::Relaxed) {
                        Move::default()
                    } else {
                        pattern(i, t)
                    }
                }),
            );
        });
        players.push(Player { handle, tokens });
    }
    let room = room.unwrap();
    let ok = c.run_until(1800, |c| {
        players.iter().all(|p| {
            c.client_net(p.handle).is_connected()
                && pawn_count(c.client(p.handle)) == 4
                && own_pawn(c.client(p.handle)).is_some()
        })
    });
    let states: Vec<_> = players.iter().map(|p| c.client_net(p.handle).state().clone()).collect();
    assert!(ok, "4 clients connected and see 4 pawns: {states:?}");
    (players, room)
}

fn pawn_count(app: &App) -> usize {
    app.world
        .query::<(&NetEntity, &Transform)>()
        .without::<&Npc>()
        .iter()
        .count()
}

fn own_pawn(app: &App) -> Option<Entity> {
    app.world.query::<(Entity, &Controlled)>().iter().map(|(e, _)| e).next()
}

/// `(NetId, position)` of every player pawn in the room.
fn server_pawns(room: &App) -> Vec<(NetId, Vec3)> {
    let server = room.resources.expect::<NetServer>();
    room.world
        .query::<(Entity, &Transform, &ControlledBy)>()
        .iter()
        .filter_map(|(e, t, _)| server.net_id(e).map(|id| (id, t.translation)))
        .collect()
}

fn percentile(mut v: Vec<f32>, q: f64) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f32::total_cmp);
    v[((q * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1]
}

/// Server positions by tick, for looking up what a client should show for a (render) tick.
#[derive(Default)]
struct History(BTreeMap<u32, BTreeMap<NetId, Vec3>>);

impl History {
    fn record(&mut self, room: &App) {
        let tick = room.resources.expect::<NetServer>().tick().0;
        self.0.insert(tick, server_pawns(room).into_iter().collect());
    }

    fn at(&self, id: NetId, tick: u32) -> Option<Vec3> {
        self.0.get(&tick).and_then(|m| m.get(&id)).copied()
    }

    /// Linear interpolation at a fractional tick (both neighbours must be known).
    fn lerp(&self, id: NetId, tick: f64) -> Option<Vec3> {
        let a = tick.floor() as u32;
        let (pa, pb) = (self.at(id, a)?, self.at(id, a + 1)?);
        Some(pa.lerp(pb, (tick - f64::from(a)) as f32))
    }
}

// ---- lossy network -------------------------------------------------------------------------------

/// All four clients converge, remote pawns track the server's past, and the local pawn's
/// prediction is exact after reconciliation whenever the server had the client's inputs.
#[test]
fn four_clients_converge_and_predict_under_100ms_rtt_jitter_and_5pct_loss() {
    let mut c = cluster(11, None);
    let stop = Arc::new(AtomicBool::new(false));
    let (players, room) = join_four(&mut c, "ffa", &stop);
    let mut history = History::default();
    let mut mis_readers: Vec<EventReader> = players.iter().map(|_| EventReader::default()).collect();
    for (p, r) in players.iter().zip(&mut mis_readers) {
        let _ = r
            .read(c.client(p.handle).resources.expect::<Events<Mispredicted>>())
            .count();
    }
    let slots: Vec<_> = players
        .iter()
        .map(|p| c.client_net(p.handle).client_id().unwrap())
        .collect();
    let own_ids: Vec<NetId> = players
        .iter()
        .map(|p| {
            let net = c.client_net(p.handle);
            net.net_id(own_pawn(c.client(p.handle)).unwrap()).unwrap()
        })
        .collect();
    let missing_count = |room: &App, i: usize| {
        room.resources
            .expect::<NetServer>()
            .stats(slots[i])
            .map_or(0, |s| s.input_missing)
    };
    // Warm up (pacer and interpolation delay settle), then measure 10 s of play.
    for _ in 0..120 {
        c.step();
        history.record(c.room(room).unwrap());
    }
    // Server ticks at which the server had no input from client i (it repeated the last one).
    let mut missing: Vec<Vec<u32>> = vec![Vec::new(); players.len()];
    // (client, predicted tick P, newest snapshot tick S when P was simulated, simulated
    // position, rendered position) and (client, NetId, render tick, drawn position).
    let mut own: Vec<(usize, u32, u32, Vec3, Vec3)> = Vec::new();
    let mut remote: Vec<(NetId, f64, Vec3)> = Vec::new();
    let mut last_predicted = vec![0u32; players.len()];
    let (mut rollbacks, mut snaps) = (0, 0);
    for _ in 0..600 {
        let before: Vec<u64> = (0..players.len())
            .map(|i| missing_count(c.room(room).unwrap(), i))
            .collect();
        c.step();
        let r = c.room(room).unwrap();
        history.record(r);
        let tick = r.resources.expect::<NetServer>().tick().0;
        for i in 0..players.len() {
            if missing_count(r, i) > before[i] {
                missing[i].push(tick);
            }
        }
        for (i, p) in players.iter().enumerate() {
            let app = c.client(p.handle);
            let net = app.resources.expect::<NetClient>();
            for m in mis_readers[i].read(app.resources.expect::<Events<Mispredicted>>()) {
                rollbacks += 1;
                if m.replayed == 0 {
                    snaps += 1;
                }
            }
            let e = own_pawn(app).unwrap();
            let rendered = app.world.get::<&Transform>(e).unwrap().translation;
            let offset = app
                .world
                .get::<&space3d_replication::client::prediction::PredictionError>(e)
                .map_or(Vec3::ZERO, |p| p.translation);
            let predicted = net.predicted_tick().unwrap().0 - 1;
            if predicted > last_predicted[i] {
                last_predicted[i] = predicted;
                own.push((i, predicted, net.last_snapshot_tick().0, rendered - offset, rendered));
            }
            let render_tick = net.render_tick().unwrap();
            for (t, id) in app
                .world
                .query::<(&Transform, &NetEntity)>()
                .without::<&Controlled>()
                .iter()
                .map(|(t, n)| (t.translation, n.0))
            {
                remote.push((id, render_tick, t));
            }
        }
    }

    // Remote pawns are drawn where the server had them at the client's render tick. TCP
    // head-of-line stalls (a lost segment holds everything behind it for 200 ms) show up in
    // the tail: the entity is extrapolated for ≤ 100 ms, then holds, then catches up.
    let remote_err: Vec<f32> = remote
        .iter()
        .filter_map(|(id, rt, drawn)| history.lerp(*id, *rt).map(|s| (s - *drawn).length()))
        .collect();
    // Local pawn, predicted position for tick P vs the server's position for P:
    // * all ticks: mispredictions happen when an input stalled behind a lost segment and the
    //   server had to repeat the previous one;
    // * after reconciliation: P was predicted from the newest confirmed state S, so when the
    //   server had the client's real inputs for every tick in (S, P] the prediction is exact.
    let mut all_err = Vec::new();
    let mut reconciled_err = Vec::new();
    let mut rendered_err = Vec::new();
    for (i, p, s, sim, shown) in &own {
        let Some(truth) = history.at(own_ids[*i], *p) else {
            continue;
        };
        let err = (truth - *sim).length();
        all_err.push(err);
        rendered_err.push((truth - *shown).length());
        if !missing[*i].iter().any(|t| t > s && t <= p) {
            reconciled_err.push(err);
        }
    }
    let exact = |v: &[f32]| v.iter().filter(|e| **e < 1e-3).count() as f64 / v.len().max(1) as f64;
    let missing_total: usize = missing.iter().map(Vec::len).sum();
    eprintln!(
        "remote interpolation error (m): n={} p50={:.3} p95={:.3} p99={:.3} max={:.3}\n\
         prediction error, all ticks (m): n={} exact={:.1}% p95={:.3} p99={:.3} max={:.3}\n\
         prediction error after reconciliation (m): n={} exact={:.2}% max={:.4}\n\
         rendered own error (m): p95={:.3} max={:.3} | rollbacks={rollbacks} snaps={snaps} \
         missing inputs={missing_total}",
        remote_err.len(),
        percentile(remote_err.clone(), 0.5),
        percentile(remote_err.clone(), 0.95),
        percentile(remote_err.clone(), 0.99),
        percentile(remote_err.clone(), 1.0),
        all_err.len(),
        exact(&all_err) * 100.0,
        percentile(all_err.clone(), 0.95),
        percentile(all_err.clone(), 0.99),
        percentile(all_err.clone(), 1.0),
        reconciled_err.len(),
        exact(&reconciled_err) * 100.0,
        percentile(reconciled_err.clone(), 1.0),
        percentile(rendered_err.clone(), 0.95),
        percentile(rendered_err.clone(), 1.0),
    );
    assert!(remote_err.len() > 4 * 3 * 500, "remote samples: {}", remote_err.len());
    assert!(percentile(remote_err.clone(), 0.95) < 0.05, "remote p95");
    assert!(percentile(remote_err.clone(), 0.99) < 1.0, "remote p99");
    assert!(all_err.len() > 4 * 500, "own samples: {}", all_err.len());
    assert!(exact(&all_err) > 0.9, "most ticks predicted exactly");
    assert!(percentile(all_err.clone(), 0.95) < 0.1, "misprediction p95");
    assert!(
        reconciled_err.len() as f64 > all_err.len() as f64 * 0.5,
        "most ticks had their inputs on time"
    );
    assert!(
        percentile(reconciled_err.clone(), 1.0) < 1e-3,
        "after reconciliation the prediction is exact"
    );
    assert!(percentile(rendered_err.clone(), 0.95) < 0.25, "smoothed correction p95");
    assert_eq!(snaps, 0, "every correction was replayed, none snapped");

    // Stop, settle, and compare every client with the server.
    stop.store(true, Ordering::Relaxed);
    c.step_ticks(240);
    let truth = server_pawns(c.room(room).unwrap());
    for p in &players {
        let app = c.client(p.handle);
        let net = app.resources.expect::<NetClient>();
        assert_eq!(pawn_count(app), 4);
        for (id, pos) in &truth {
            let e = net.entity(*id).expect("pawn known to every client");
            let shown = app.world.get::<&Transform>(e).unwrap().translation;
            assert!((shown - *pos).length() < 1e-3, "{id:?}: client {shown} vs server {pos}");
        }
    }
}

// ---- resume ----------------------------------------------------------------------------------

#[test]
fn dropped_link_resumes_the_same_slot_and_pawn_under_loss() {
    let mut c = cluster(23, None);
    let stop = Arc::new(AtomicBool::new(false));
    let (players, room) = join_four(&mut c, "ffa", &stop);
    let victim = &players[1];
    let player_id = victim.tokens.profile.player_id;
    let (slot, link, pawn_id) = {
        let r = c.room(room).unwrap();
        let server = r.resources.expect::<NetServer>();
        let info = server
            .clients()
            .into_iter()
            .find(|i| i.identity.player == player_id)
            .unwrap();
        let pawn = r
            .world
            .query::<(Entity, &ControlledBy)>()
            .iter()
            .find(|(_, cb)| cb.0 == player_id)
            .map(|(e, _)| e)
            .unwrap();
        (info.client, info.link.unwrap(), server.net_id(pawn).unwrap())
    };
    let server_id = c.client_server(victim.handle).to_owned();
    let mut reader = EventReader::default();
    let _ = reader
        .read(c.client(victim.handle).resources.expect::<Events<ConnectionEvent>>())
        .count();
    // The link goes dark for 3 s (idle timeout on both sides, then reconnect + resume).
    c.network(&server_id).partition(link, Duration::from_secs(3));
    let mut dropped = false;
    let mut resumed = false;
    let back = c.run_until(60 * 20, |c| {
        for e in reader.read(c.client(victim.handle).resources.expect::<Events<ConnectionEvent>>()) {
            match e {
                ConnectionEvent::Disconnected {
                    will_reconnect: true, ..
                }
                | ConnectionEvent::Reconnecting { .. } => dropped = true,
                ConnectionEvent::Connected { resumed: true, .. } => resumed = true,
                _ => {}
            }
        }
        resumed && c.client_net(victim.handle).is_connected()
    });
    assert!(
        back && dropped,
        "resumed: {:?} (dropped={dropped})",
        c.client_net(victim.handle).state()
    );
    let r = c.room(room).unwrap();
    let server = r.resources.expect::<NetServer>();
    let info = server
        .clients()
        .into_iter()
        .find(|i| i.identity.player == player_id)
        .unwrap();
    assert_eq!(info.client, slot, "same slot");
    assert_ne!(info.link, Some(link), "on a fresh link");
    let pawns: Vec<NetId> = r
        .world
        .query::<(Entity, &ControlledBy)>()
        .iter()
        .filter(|(_, cb)| cb.0 == player_id)
        .filter_map(|(e, _)| server.net_id(e))
        .collect();
    assert_eq!(pawns, vec![pawn_id], "same pawn, no duplicate");
    // The resumed client controls the same entity again and everyone still sees four pawns.
    assert!(c.run_until(600, |c| {
        let app = c.client(victim.handle);
        own_pawn(app).and_then(|e| c.client_net(victim.handle).net_id(e)) == Some(pawn_id)
            && players.iter().all(|p| pawn_count(c.client(p.handle)) == 4)
    }));
    for p in &players {
        assert!(
            c.client_net(p.handle).is_connected(),
            "{:?}",
            c.client_net(p.handle).state()
        );
    }
}

// ---- bandwidth -----------------------------------------------------------------------------------

/// Per-client application bytes/s over `secs` of cluster time.
fn rates(c: &mut TestCluster, players: &[Player], secs: u64) -> Vec<(f64, f64)> {
    let before: Vec<(u64, u64)> = players
        .iter()
        .map(|p| {
            let s = c.client_net(p.handle).stats();
            (s.bytes_in, s.bytes_out)
        })
        .collect();
    c.step_ticks(secs * 60);
    players
        .iter()
        .zip(before)
        .map(|(p, (i0, o0))| {
            let s = c.client_net(p.handle).stats();
            (
                (s.bytes_in - i0) as f64 / secs as f64,
                (s.bytes_out - o0) as f64 / secs as f64,
            )
        })
        .collect()
}

#[test]
fn bandwidth_per_client_stays_under_budget() {
    // Four movers: small rooms cost a few KB/s per client in each direction.
    let mut c = cluster(31, None);
    let stop = Arc::new(AtomicBool::new(false));
    let (players, _) = join_four(&mut c, "ffa", &stop);
    c.step_ticks(120);
    let small = rates(&mut c, &players, 5);
    eprintln!("4 movers: (down, up) bytes/s per client = {small:.0?}");
    for (down, up) in &small {
        assert!(*down < 4_000.0, "down {down:.0} B/s");
        assert!(*up < 2_000.0, "up {up:.0} B/s");
    }

    // 300 server-driven movers against a 12 KiB/s per-client budget: priority and the byte
    // budget decide what is sent; the rate stays at the budget, not at the room's demand.
    let budget = 12 * 1024;
    let mut c = cluster(37, Some(budget));
    let (players, room) = join_four(&mut c, "crowd", &stop);
    assert_eq!(
        c.room(room).unwrap().world.query::<&Npc>().iter().count(),
        300,
        "crowd room"
    );
    c.step_ticks(300);
    let crowd = rates(&mut c, &players, 5);
    eprintln!("300 movers, {budget} B/s budget: (down, up) bytes/s per client = {crowd:.0?}");
    for (down, _) in &crowd {
        // The budget covers snapshots; control / time traffic and packet headers ride on top.
        assert!(
            *down <= budget as f64 * 1.1,
            "down {down:.0} B/s over the {budget} B/s budget"
        );
        assert!(
            *down >= budget as f64 * 0.5,
            "the budget is what limits the rate: {down:.0}"
        );
    }
    // Nothing starves: every client knows every NPC.
    for p in &players {
        let seen = c.client(p.handle).world.query::<(&Npc, &NetEntity)>().iter().count();
        assert_eq!(seen, 300);
    }
}

// ---- separate processes over localhost -----------------------------------------------------------

fn secrets() -> Secrets {
    static KEYS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let spec = KEYS.get_or_init(|| space3d_server::security::token::generate_key_spec("k1"));
    let env = |k: &str| match k {
        "SPACE3D_KEYS" => Some(spec.clone()),
        "SPACE3D_FLEET_TOKEN" => Some("acceptance-fleet-token-0123456789".to_owned()),
        "SPACE3D_ADMIN_TOKENS" => Some("acceptance-admin-token".to_owned()),
        _ => None,
    };
    Secrets::load(&ServerConfig::default(), &env).expect("secrets")
}

fn node_config(dir: &std::path::Path, role: Role, id: &str) -> ServerConfig {
    let mut c = ServerConfig {
        role,
        server_id: id.into(),
        ..Default::default()
    };
    c.admin.bind = "127.0.0.1:0".parse().unwrap();
    c.persistence.data_dir = dir.to_path_buf();
    c.player_service.http_rps_per_ip = 10_000;
    c.player_service.login_attempts_per_min = 10_000;
    c.fleet.heartbeat = Duration::from_millis(300);
    c
}

fn pump_until(apps: &mut [&mut App], deadline: Duration, mut pred: impl FnMut(&[&mut App]) -> bool) -> bool {
    let end = Instant::now() + deadline;
    let mut last = Instant::now();
    while Instant::now() < end {
        let now = Instant::now();
        for a in apps.iter_mut() {
            a.update(now - last);
        }
        last = now;
        if pred(apps) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(4));
    }
    false
}

fn client_app() -> App {
    let mut app = App::new().with_tickrate(60.0);
    app.add_plugin(ClientReplicationPlugin {
        config: ClientConfig {
            game_id: GAME_ID.into(),
            ..Default::default()
        },
    });
    app.add_plugin(Protocol);
    install_bot(&mut app, scripted(|t| pattern(0, t)));
    app
}

#[test]
fn separate_player_service_and_game_server_ticket_then_connect() {
    let ps_dir = tempfile::tempdir().unwrap();
    let gs_dir = tempfile::tempdir().unwrap();
    // Player service: its own process-like App, listener and database.
    let mut ps_config = node_config(ps_dir.path(), Role::PlayerService, "players");
    ps_config.player_service.bind = "127.0.0.1:0".parse().unwrap();
    ps_config.player_service.db_path = ps_dir.path().join("players.db");
    let mut ps = ServerMain::with_config(Game, ps_config)
        .options(BuildOptions {
            secrets: Some(secrets()),
            ..Default::default()
        })
        .build()
        .expect("player service starts");
    let ps_url = ps.resources.expect::<PlayerServiceHandle>().url();
    // Game server: dials the player service's fleet WebSocket; no shared memory.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut gs_config = node_config(gs_dir.path(), Role::GameServer, "gs-a");
    gs_config.game.bind = format!("127.0.0.1:{port}").parse().unwrap();
    gs_config.game.public_url = format!("ws://127.0.0.1:{port}/play");
    gs_config.game.shards = 1;
    gs_config.fleet.player_service_url = format!("{}/internal/v1/fleet", ps_url.replacen("http://", "ws://", 1));
    gs_config.fleet.player_service_http = ps_url.clone();
    let mut gs = ServerMain::with_config(Game, gs_config)
        .options(BuildOptions {
            secrets: Some(secrets()),
            ..Default::default()
        })
        .build()
        .expect("game server starts");
    assert!(
        pump_until(&mut [&mut ps, &mut gs], Duration::from_secs(10), |apps| apps[1]
            .resources
            .expect::<HealthState>()
            .is_registered()),
        "game server registered over the fleet WebSocket"
    );

    // The client logs in and obtains a signed join ticket for the game server.
    let http = space3d_net::Http::new();
    let api = PlayerServiceClient::new(&http, &ps_url);
    let tokens = api
        .login_guest(&DeviceCredentials::generate(), Some("solo"))
        .block()
        .unwrap()
        .unwrap();
    let a = api
        .quick_play(&QuickPlayRequest {
            mode: "ffa".into(),
            ..Default::default()
        })
        .block()
        .unwrap()
        .unwrap();
    assert_eq!(a.server_id, "gs-a");
    assert_eq!(a.server_url, format!("ws://127.0.0.1:{port}/play"));
    let mut client = client_app();
    client
        .resources
        .expect_mut::<NetClient>()
        .connect(ConnectTarget::Ticket {
            url: a.server_url.clone(),
            ticket: a.join_ticket.clone(),
        });
    let playing = pump_until(&mut [&mut ps, &mut gs, &mut client], Duration::from_secs(10), |apps| {
        apps[2].resources.expect::<NetClient>().is_connected() && own_pawn(apps[2]).is_some()
    });
    assert!(playing, "{:?}", client.resources.expect::<NetClient>().state());
    let rooms = gs.resources.expect::<GameServerHandle>().rooms.clone();
    assert!(rooms.get(a.room_id).is_some(), "room created lazily from the ticket");
    assert_eq!(
        client.resources.expect::<NetClient>().player_id(),
        Some(tokens.profile.player_id),
        "the ticket carried the account"
    );
    // The pawn moves on the server as the client's inputs arrive.
    let e = own_pawn(&client).unwrap();
    let before = client.world.get::<&Transform>(e).unwrap().translation;
    assert!(pump_until(
        &mut [&mut ps, &mut gs, &mut client],
        Duration::from_secs(5),
        |apps| {
            let e = own_pawn(apps[2]).unwrap();
            (apps[2].world.get::<&Transform>(e).unwrap().translation - before).length() > 1.0
        }
    ));

    // Tickets are single-use: replaying it on a second connection is refused.
    let mut thief = client_app();
    thief
        .resources
        .expect_mut::<NetClient>()
        .connect(ConnectTarget::Ticket {
            url: a.server_url.clone(),
            ticket: a.join_ticket.clone(),
        });
    let refused = pump_until(&mut [&mut ps, &mut gs, &mut thief], Duration::from_secs(10), |apps| {
        matches!(
            apps[2].resources.expect::<NetClient>().state(),
            ConnectionState::Failed(FailReason::Rejected(r)) if r.code == RejectCode::TicketReused
        )
    });
    assert!(refused, "{:?}", thief.resources.expect::<NetClient>().state());
    assert!(
        client.resources.expect::<NetClient>().is_connected(),
        "the owner is unaffected"
    );
}
