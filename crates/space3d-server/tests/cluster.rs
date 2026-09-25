//! Split-topology integration tests on the in-process `TestCluster`: one player service and
//! separate game servers (fleet links, tickets, gatekeepers, rooms, outbox, checkpoints),
//! clients over the memory network, manual clocks.

mod common;

use common::{DuelGame, DuelProtocol, Finish, SetHealth, healths, pawn_health, pawns, remote_count};
use space3d_net::{Http, HttpRequest};
use space3d_replication::api::SessionTokens;
use space3d_replication::client::{ConnectionState, FailReason, NetClient};
use space3d_replication::harness::ClientHandle;
use space3d_replication::protocol::handshake::DisconnectCode;
use space3d_replication::{PlayerId, RoomId};
use space3d_server::gs::checkpoint::Checkpoint;
use space3d_server::ps::fleet::ServerState;
use space3d_server::testing::{TEST_ADMIN_TOKEN, TestCluster, TestClusterConfig};
use std::time::Duration;

fn cluster(game_servers: usize) -> TestCluster {
    TestCluster::new(
        DuelGame,
        TestClusterConfig {
            game_servers,
            ..Default::default()
        },
    )
}

fn player(t: &SessionTokens) -> PlayerId {
    t.profile.player_id
}

fn connected_pair(c: &mut TestCluster, a: &SessionTokens, b: &SessionTokens) -> (ClientHandle, ClientHandle, RoomId) {
    let found = c.matchmake(&[a, b], "duel");
    assert_eq!(found[0].room_id, found[1].room_id, "a duel shares one room");
    assert_eq!(found[0].server_id, found[1].server_id);
    let ca = c.connect(&found[0], |app| {
        app.add_plugin(DuelProtocol);
    });
    let cb = c.connect(&found[1], |app| {
        app.add_plugin(DuelProtocol);
    });
    let ok = c.run_until(600, |c| {
        c.client_net(ca).is_connected()
            && c.client_net(cb).is_connected()
            && remote_count(c.client(ca)) == 2
            && remote_count(c.client(cb)) == 2
    });
    assert!(ok, "both players connected and see both pawns");
    (ca, cb, found[0].room_id)
}

fn send<M: space3d_replication::NetMessage>(c: &mut TestCluster, client: ClientHandle, msg: &M) {
    c.client_mut(client)
        .resources
        .expect_mut::<NetClient>()
        .send(msg)
        .expect("send");
}

fn failed_with(c: &TestCluster, client: ClientHandle, code: DisconnectCode) -> bool {
    matches!(
        c.client_net(client).state(),
        ConnectionState::Failed(FailReason::Disconnected(got, _)) if *got == code
    )
}

fn admin(
    c: &TestCluster,
    node: &str,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
    token: bool,
) -> (u16, String) {
    let mut req = HttpRequest::new(method.parse().unwrap(), format!("{}{path}", c.admin_url(node)));
    if token {
        req = req.header("authorization", format!("Bearer {TEST_ADMIN_TOKEN}"));
    }
    if let Some(b) = body {
        req = req.json(&b).unwrap();
    }
    let res = Http::new().request(req).block().unwrap().unwrap();
    (res.status, res.text())
}

#[test]
fn matchmaking_play_and_results_flow_to_the_player_service() {
    let mut c = cluster(2);
    let alice = c.guest("alice");
    let bob = c.guest("bob");
    let (ca, _cb, room) = connected_pair(&mut c, &alice, &bob);
    let server = c.room_server(room).expect("room hosted").to_owned();
    assert!(c.game_servers().contains(&server));
    assert_eq!(pawns(c.room(room).unwrap()), 2);

    // alice wins: the room reports the result through the outbox → fleet link → store.
    send(&mut c, ca, &Finish);
    let board = c.api(&alice);
    let applied = c.run_until(600, |_| {
        board
            .leaderboard("wins", 10)
            .block()
            .unwrap()
            .is_ok_and(|rows| rows.len() == 1)
    });
    assert!(applied, "match result reached the leaderboard");
    let rows = board.leaderboard("wins", 10).block().unwrap().unwrap();
    assert_eq!(rows[0].player_id, player(&alice));
    assert_eq!(rows[0].score, 1);
    let alice_me = c.api(&alice).me().block().unwrap().unwrap();
    let bob_me = c.api(&bob).me().block().unwrap().unwrap();
    assert!(
        alice_me.rating > bob_me.rating,
        "Elo moved: {} vs {}",
        alice_me.rating,
        bob_me.rating
    );
    assert!((alice_me.rating + bob_me.rating - 2.0 * alice.profile.rating).abs() < 1e-6);
    let settled = c.settle(Duration::from_secs(5), |c| {
        c.api(&alice)
            .get_data("progress")
            .block()
            .unwrap()
            .is_ok_and(|e| e.value == serde_json::json!({ "wins": 1 }))
    });
    assert!(settled, "room-written player data persisted");
}

// Restored rooms: `full_state::restore` pre-maps the captured `NetId`s and the change cache
// adopts those entities on its first collect, so (re)joining clients receive the restored state.

#[test]
fn room_crash_is_contained_and_players_rejoin_restored_state() {
    room_crash_scenario();
}

fn room_crash_scenario() {
    let mut c = cluster(1);
    let alice = c.guest("alice");
    let bob = c.guest("bob");
    let (ca, cb, room) = connected_pair(&mut c, &alice, &bob);
    send(&mut c, ca, &SetHealth(42));
    assert!(c.run_until(120, |c| pawn_health(c.room(room).unwrap(), player(&alice)) == Some(42)));
    // Let the 1 s checkpoint cadence capture the new state.
    c.step_ticks(90);

    c.crash_room(room);
    // The crashed room disconnects everyone (`RoomCrashed`, no resume): clients give up on the
    // link and rejoin through the player service.
    let dropped = c.run_until(120, |c| {
        failed_with(c, ca, DisconnectCode::RoomCrashed) && failed_with(c, cb, DisconnectCode::RoomCrashed)
    });
    assert!(
        dropped,
        "clients told to rejoin: {:?} / {:?}",
        c.client_net(ca).state(),
        c.client_net(cb).state()
    );
    assert_eq!(c.room_crashes(), 1);
    let restored = c.room(room).expect("room rebuilt, not closed");
    assert_eq!(pawns(restored), 2, "pawns restored from the checkpoint");
    assert_eq!(pawn_health(restored, player(&alice)), Some(42));

    for (client, tokens) in [(ca, &alice), (cb, &bob)] {
        let a = c.rejoin(tokens).unwrap().expect("room still exists");
        assert_eq!(a.room_id, room);
        c.reconnect(client, &a);
    }
    let ok = c.run_until(600, |c| {
        c.client_net(ca).is_connected() && c.client_net(cb).is_connected()
    });
    assert!(ok, "players back in the rebuilt room");
    assert_eq!(pawns(c.room(room).unwrap()), 2, "rejoin re-used the restored pawns");
    let synced = c.run_until(300, |c| {
        healths(c.client(ca)) == vec![42, 100] && healths(c.client(cb)) == vec![42, 100]
    });
    assert!(synced, "clients see the restored state: {:?}", healths(c.client(ca)));
}

#[test]
fn game_server_loss_restores_the_room_on_another_server() {
    server_loss_scenario();
}

fn server_loss_scenario() {
    let mut c = cluster(2);
    let alice = c.guest("alice");
    let bob = c.guest("bob");
    let (ca, cb, room) = connected_pair(&mut c, &alice, &bob);
    let lost = c.room_server(room).unwrap().to_owned();
    send(&mut c, ca, &SetHealth(7));
    assert!(c.run_until(120, |c| pawn_health(c.room(room).unwrap(), player(&alice)) == Some(7)));
    let set_at = c.unix_clock_ms();
    // A checkpoint captured after the change must reach the player service's store. Judge it
    // by the blob's own capture time: an older capture that was still uploading may land
    // later (regression: the store used to stamp arrival time, so a stale blob passed this
    // wait and the room was restored with health 100).
    let stored = c.run_until(600, |c| {
        c.player_service()
            .store
            .get_checkpoint(room)
            .block()
            .unwrap()
            .unwrap()
            .is_some_and(|(saved, bytes)| {
                let cp = Checkpoint::from_bytes(&bytes).expect("stored checkpoint decodes");
                assert_eq!(saved, cp.saved_ms, "row stamped with the capture time");
                cp.saved_ms > set_at
            })
    });
    assert!(stored, "checkpoint uploaded to the player service");

    c.kill_game_server(&lost);
    assert!(c.room(room).is_none());
    let lease = c.player_service().config.fleet.lease_ttl;
    let ticks = (lease.as_secs_f64() * 60.0) as u64 + 180;
    let dead = c.run_until(ticks, |c| {
        c.player_service()
            .fleet
            .lock()
            .server(&lost)
            .is_none_or(|s| s.state == ServerState::Dead)
    });
    assert!(dead, "lease expired");

    let mut target = None;
    for (client, tokens) in [(ca, &alice), (cb, &bob)] {
        let a = c.rejoin(tokens).unwrap().expect("orphaned room can be restored");
        assert_eq!(a.room_id, room);
        assert_ne!(a.server_id, lost);
        target.get_or_insert(a.server_id.clone());
        assert_eq!(
            target.as_deref(),
            Some(a.server_id.as_str()),
            "both players go to the same server"
        );
        c.reconnect(client, &a);
    }
    let ok = c.run_until(600, |c| {
        c.client_net(ca).is_connected() && c.client_net(cb).is_connected()
    });
    assert!(ok, "players reconnected to the restored room");
    assert_eq!(c.room_server(room), target.as_deref());
    let restored = c.room(room).unwrap();
    assert_eq!(pawns(restored), 2);
    assert_eq!(pawn_health(restored, player(&alice)), Some(7));
    let synced = c.run_until(300, |c| {
        healths(c.client(ca)) == vec![7, 100] && healths(c.client(cb)) == vec![7, 100]
    });
    assert!(synced, "clients see the restored state: {:?}", healths(c.client(ca)));
}

#[test]
fn draining_server_gets_no_new_rooms_and_admin_api_is_guarded() {
    let mut c = cluster(2);
    // Admin API: token required, metrics public.
    assert_eq!(admin(&c, "gs-1", "GET", "/admin/v1/status", None, false).0, 401);
    let (status, body) = admin(&c, "gs-1", "GET", "/admin/v1/status", None, true);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"registered\":true"), "{body}");
    let (status, metrics) = admin(&c, "gs-1", "GET", "/metrics", None, false);
    assert_eq!(status, 200);
    assert!(metrics.contains("space3d_rooms"), "{metrics}");
    let (status, schema) = admin(&c, "gs-2", "GET", "/admin/v1/schema", None, true);
    assert_eq!(status, 200);
    assert!(schema.contains("duel.health"), "{schema}");

    // Drain gs-1 through its admin API; the player service stops allocating to it.
    let (status, _) = admin(&c, "gs-1", "POST", "/admin/v1/drain", None, true);
    assert_eq!(status, 202);
    // It reports draining; with no rooms the drain completes at once and it deregisters
    // (dead right away, not after the lease).
    let draining = c.run_until(600, |c| {
        c.player_service()
            .fleet
            .lock()
            .server("gs-1")
            .is_none_or(|s| s.state != ServerState::Ready)
    });
    assert!(draining, "heartbeat reported draining");
    let lease_ticks = (c.player_service().config.fleet.lease_ttl.as_secs_f64() * 60.0) as u64;
    let left = c.run_until(lease_ticks / 2, |c| {
        c.player_service()
            .fleet
            .lock()
            .server("gs-1")
            .is_none_or(|s| s.state == ServerState::Dead && s.left)
    });
    assert!(left, "deregistered (dead and forgotten) before its lease ran out");
    let (status, _) = admin(&c, "gs-1", "GET", "/readyz", None, false);
    assert_eq!(status, 503, "a draining server is not ready");
    for name in ["p1", "p2", "p3"] {
        let t = c.guest(name);
        let a = c.quick_play(&t, "ffa").expect("quick play");
        assert_eq!(a.server_id, "gs-2");
    }
}

#[test]
fn bans_kick_players_and_block_their_tickets() {
    let mut c = cluster(1);
    let alice = c.guest("alice");
    let bob = c.guest("bob");
    let (ca, cb, room) = connected_pair(&mut c, &alice, &bob);

    let (status, body) = admin(
        &c,
        "ps",
        "POST",
        "/admin/v1/bans",
        Some(serde_json::json!({ "player": player(&bob), "reason": "cheating" })),
        true,
    );
    assert_eq!(status, 201, "{body}");
    // The room removes a banned player itself once its server's ban list changes.
    let kicked = c.run_until(300, |c| {
        failed_with(c, cb, DisconnectCode::Banned) && pawns(c.room(room).unwrap()) == 1
    });
    assert!(
        kicked,
        "the ban reached the game server and removed bob: {:?}",
        c.client_net(cb).state()
    );
    assert!(c.client_net(ca).is_connected(), "alice unaffected");
    let err = c.api(&bob).me().block().unwrap().unwrap_err();
    assert_eq!(err.status(), Some(403));
    assert_eq!(err.code(), Some("banned"));
    let err = c.rejoin(&bob).unwrap_err();
    assert_eq!(err.code(), Some("banned"));
    // Unknown admin token.
    let res = Http::new()
        .request(
            HttpRequest::get(format!("{}/admin/v1/bans", c.admin_url("ps"))).header("authorization", "Bearer nope"),
        )
        .block()
        .unwrap()
        .unwrap();
    assert_eq!(res.status, 401);
}

trait UnixMs {
    fn unix_clock_ms(&self) -> i64;
}

impl UnixMs for TestCluster {
    fn unix_clock_ms(&self) -> i64 {
        use space3d_server::clock::UnixClock;
        self.unix_clock().now_ms()
    }
}

#[test]
fn network_partition_resumes_the_same_slot_through_the_gatekeeper() {
    use space3d_core::{EventReader, Events};
    use space3d_replication::client::ConnectionEvent;
    let mut c = cluster(1);
    let alice = c.guest("alice");
    let bob = c.guest("bob");
    let (ca, cb, room) = connected_pair(&mut c, &alice, &bob);
    let server = c.client_server(ca).to_owned();
    // Alice's link is the first one opened on her game server's network.
    let link = c.network(&server).links()[0];
    let mut reader = EventReader::default();
    c.network(&server).partition(link, Duration::from_secs(2));
    let mut resumed = false;
    let back = c.run_until(1200, |c| {
        if let Some(events) = c.client(ca).resources.get::<Events<ConnectionEvent>>() {
            for e in reader.read(events) {
                if let ConnectionEvent::Connected { resumed: true, .. } = e {
                    resumed = true;
                }
            }
        }
        resumed && c.client_net(ca).is_connected()
    });
    assert!(back, "alice resumed: {:?}", c.client_net(ca).state());
    assert!(c.client_net(cb).is_connected(), "bob unaffected");
    assert_eq!(pawns(c.room(room).unwrap()), 2, "same slot and pawn, no duplicate");
    assert!(c.run_until(120, |c| remote_count(c.client(ca)) == 2));
}

#[test]
fn concurrent_quick_plays_fill_one_room() {
    use space3d_replication::api::QuickPlayRequest;
    let mut c = cluster(1);
    let players: Vec<SessionTokens> = (0..8).map(|i| c.guest(&format!("q{i}"))).collect();
    // All eight requests are in flight at once: finding an open room and allocating a new one
    // happen under one fleet lock, so they share one room (capacity 16) instead of racing
    // into several.
    let tasks: Vec<_> = players
        .iter()
        .map(|t| {
            c.api(t).quick_play(&QuickPlayRequest {
                mode: "ffa".into(),
                ..Default::default()
            })
        })
        .collect();
    let rooms: std::collections::BTreeSet<RoomId> = tasks
        .into_iter()
        .map(|t| t.block().unwrap().expect("quick play").room_id)
        .collect();
    assert_eq!(rooms.len(), 1, "{rooms:?}");
}

/// [`TestCluster::run_until`] that also gives background work real time (≥ 2 ms per tick):
/// checkpoint uploads, database writes and the player service's async answers finish in real
/// time, while cluster time can race ahead of them.
fn run_until_bg(c: &mut TestCluster, max_ticks: u64, mut pred: impl FnMut(&TestCluster) -> bool) -> bool {
    for _ in 0..max_ticks {
        if pred(c) {
            return true;
        }
        c.step();
        std::thread::sleep(Duration::from_millis(2));
    }
    pred(c)
}

fn persistent_cluster(game_servers: usize) -> TestCluster {
    use space3d_server::config::QueueConfig;
    let mut config = TestClusterConfig {
        game_servers,
        ..Default::default()
    };
    // Only the drain checkpoint captures state changed right before the drain.
    config.server.persistence.checkpoint_interval = Duration::from_secs(600);
    config.server.matchmaking.queues.push(QueueConfig {
        name: "world".into(),
        mode: "world".into(),
        teams: 1,
        team_size: 8,
        min_players: 1,
        backfill: true,
        params: [("persistent".to_owned(), "true".to_owned())].into(),
        ..Default::default()
    });
    TestCluster::new(DuelGame, config)
}

fn saw_notice(
    c: &TestCluster,
    client: ClientHandle,
    reader: &mut space3d_core::EventReader,
    want: &space3d_replication::protocol::control::Notice,
) -> bool {
    use space3d_replication::client::ConnectionEvent;
    let events = c
        .client(client)
        .resources
        .expect::<space3d_core::Events<ConnectionEvent>>();
    reader
        .read(events)
        .any(|e| matches!(e, ConnectionEvent::Notice(n) if n == want))
}

/// §2.4.2 regression: draining a server moves its persistent room. The room checkpoints at the
/// drain, closes once that checkpoint is stored (`Notice::Reconnect`, then
/// `Disconnect(ServerShutdown)`), and its players' rejoin restores exactly that state on
/// another server — never an older checkpoint, never two live copies.
#[test]
fn draining_hands_a_persistent_room_to_another_server() {
    use space3d_replication::protocol::control::Notice;
    let mut c = persistent_cluster(2);
    let alice = c.guest("alice");
    let bob = c.guest("bob");
    let a = c.quick_play(&alice, "world").expect("quick play");
    let b = c.quick_play(&bob, "world").expect("quick play");
    assert_eq!(a.room_id, b.room_id, "one open world room");
    let (room, host) = (a.room_id, a.server_id.clone());
    let ca = c.connect(&a, |app| {
        app.add_plugin(DuelProtocol);
    });
    let cb = c.connect(&b, |app| {
        app.add_plugin(DuelProtocol);
    });
    assert!(c.run_until(600, |c| {
        c.client_net(ca).is_connected() && c.client_net(cb).is_connected() && remote_count(c.client(ca)) == 2
    }));
    let spec = c
        .room(room)
        .unwrap()
        .resources
        .expect::<space3d_server::gs::room::RoomContext>()
        .spec
        .clone();
    assert!(spec.persistent);
    // Let the join checkpoint land, then change state only the drain checkpoint captures.
    c.step_ticks(30);
    send(&mut c, ca, &SetHealth(9));
    assert!(c.run_until(120, |c| pawn_health(c.room(room).unwrap(), player(&alice)) == Some(9)));

    let (status, _) = admin(&c, &host, "POST", "/admin/v1/drain", None, true);
    assert_eq!(status, 202);
    let mut reader = space3d_core::EventReader::default();
    let mut warned = false;
    let moved = run_until_bg(&mut c, 900, |c| {
        warned |= saw_notice(c, ca, &mut reader, &Notice::Reconnect);
        failed_with(c, ca, DisconnectCode::ServerShutdown) && failed_with(c, cb, DisconnectCode::ServerShutdown)
    });
    assert!(
        moved,
        "room handed off: {:?} / {:?}",
        c.client_net(ca).state(),
        c.client_net(cb).state()
    );
    assert!(warned, "clients were told the room moves");
    // The frozen room closes once its final checkpoint is stored.
    let handed_off = run_until_bg(&mut c, 900, |c| {
        c.room(room).is_none() && c.player_service().fleet.lock().room(room).is_some_and(|e| e.orphaned)
    });
    assert!(handed_off, "no live copy left on the draining server");
    let entry = c.player_service().fleet.lock().room(room).cloned().expect("room kept");
    assert!(entry.orphaned && !entry.moving, "kept restorable");

    let mut target = None;
    for (client, tokens) in [(ca, &alice), (cb, &bob)] {
        let a = c.rejoin(tokens).unwrap().expect("the world can be restored");
        assert_eq!(a.room_id, room);
        assert_ne!(a.server_id, host, "restored on another server");
        target.get_or_insert(a.server_id.clone());
        assert_eq!(target.as_deref(), Some(a.server_id.as_str()));
        c.reconnect(client, &a);
    }
    assert!(c.run_until(600, |c| {
        c.client_net(ca).is_connected() && c.client_net(cb).is_connected()
    }));
    let restored = c.room(room).unwrap();
    assert_eq!(
        pawn_health(restored, player(&alice)),
        Some(9),
        "the drain checkpoint's state"
    );
    assert!(c.run_until(300, |c| healths(c.client(ca)) == vec![9, 100]));
}

/// Regression: the drain handoff kept a persistent room playable while its drain checkpoint
/// uploaded, then restored that earlier capture, so every change made during the upload — which
/// the players had seen — was lost. The room now freezes (clients disconnected) at the capture,
/// and the player service holds rejoins until that capture is stored.
#[test]
fn a_drain_handoff_keeps_every_change_the_players_saw() {
    let mut c = persistent_cluster(2);
    let alice = c.guest("alice");
    let bob = c.guest("bob");
    let a = c.quick_play(&alice, "world").expect("quick play");
    let b = c.quick_play(&bob, "world").expect("quick play");
    let (room, host) = (a.room_id, a.server_id.clone());
    let ca = c.connect(&a, |app| {
        app.add_plugin(DuelProtocol);
    });
    let cb = c.connect(&b, |app| {
        app.add_plugin(DuelProtocol);
    });
    assert!(c.run_until(600, |c| {
        c.client_net(ca).is_connected() && c.client_net(cb).is_connected() && remote_count(c.client(ca)) == 2
    }));
    c.step_ticks(30);
    let (status, _) = admin(&c, &host, "POST", "/admin/v1/drain", None, true);
    assert_eq!(status, 202);
    // Alice changes her pawn every tick, as fast as the room accepts it, until the room moves.
    let mut last_in_room = 0;
    let mut last_seen = 0;
    for _ in 0..3000 {
        if c.client_net(ca).is_connected() {
            let seen = healths(c.client(ca)).into_iter().max().unwrap_or(0);
            last_seen = last_seen.max(seen);
            send(&mut c, ca, &SetHealth(seen + 1));
        }
        if let Some(app) = c.room(room) {
            last_in_room = pawn_health(app, player(&alice)).unwrap_or(last_in_room);
        }
        if c.room(room).is_none() && c.player_service().fleet.lock().room(room).is_some_and(|e| e.orphaned) {
            break;
        }
        c.step();
    }
    assert!(c.room(room).is_none(), "handed off");
    assert!(last_seen > 100, "alice changed her pawn during the drain");
    let target = c.rejoin(&alice).unwrap().expect("the world can be restored");
    assert_ne!(target.server_id, host);
    c.reconnect(ca, &target);
    assert!(c.run_until(600, |c| c.client_net(ca).is_connected()));
    let restored = pawn_health(c.room(room).unwrap(), player(&alice)).unwrap();
    assert_eq!(
        restored, last_in_room,
        "restored {restored}, but the old room reached {last_in_room} (alice saw {last_seen})"
    );
    assert!(restored >= last_seen);
}

/// A rejoin that arrives while a persistent room is frozen for its handoff waits until the
/// final checkpoint is stored, then restores the room elsewhere (it neither goes back to the
/// frozen room nor restores an older checkpoint).
#[test]
fn a_rejoin_during_a_handoff_waits_for_the_final_checkpoint() {
    let mut c = persistent_cluster(2);
    let alice = c.guest("alice");
    let a = c.quick_play(&alice, "world").expect("quick play");
    let (room, host) = (a.room_id, a.server_id.clone());
    let ca = c.connect(&a, |app| {
        app.add_plugin(DuelProtocol);
    });
    assert!(c.run_until(600, |c| c.client_net(ca).is_connected()));
    c.step_ticks(30);
    send(&mut c, ca, &SetHealth(42));
    assert!(c.run_until(120, |c| pawn_health(c.room(room).unwrap(), player(&alice)) == Some(42)));
    let (status, _) = admin(&c, &host, "POST", "/admin/v1/drain", None, true);
    assert_eq!(status, 202);
    let moving = c.run_until(600, |c| {
        c.player_service().fleet.lock().room(room).is_some_and(|e| e.moving)
    });
    assert!(moving, "the player service knows the room is moving");
    assert!(c.run_until(120, |c| failed_with(c, ca, DisconnectCode::ServerShutdown)));
    // The room only closes when the test steps it: the rejoin must wait meanwhile.
    let api = c.api(&alice);
    let rejoin = std::thread::spawn(move || api.rejoin().block());
    std::thread::sleep(Duration::from_millis(300));
    assert!(!rejoin.is_finished(), "the rejoin waits for the handoff");
    assert!(
        run_until_bg(&mut c, 900, |_| rejoin.is_finished()),
        "answered after the handoff"
    );
    let target = rejoin
        .join()
        .unwrap()
        .expect("task")
        .expect("rejoin")
        .expect("restorable");
    assert_eq!(target.room_id, room);
    assert_ne!(target.server_id, host, "restored on another server");
    c.reconnect(ca, &target);
    assert!(c.run_until(600, |c| c.client_net(ca).is_connected()));
    assert_eq!(pawn_health(c.room(room).unwrap(), player(&alice)), Some(42));
}

/// Regression (split brain): a game server whose fleet link was cut for longer than its lease
/// keeps running its rooms, so a player who rejoined meanwhile got the room restored on another
/// server — and once the link healed, the stale copy kept running forever (its heartbeats even
/// flipped the room's entry back and forth). The player service now closes the stale copy
/// (`RoomSuperseded`) as soon as its server is heard from again, and its players rejoin the
/// live one.
#[test]
fn a_partitioned_server_closes_its_stale_copy_of_a_restored_room() {
    let mut c = cluster(2);
    let alice = c.guest("alice");
    let bob = c.guest("bob");
    let (ca, cb, room) = connected_pair(&mut c, &alice, &bob);
    let stale = c.room_server(room).unwrap().to_owned();
    send(&mut c, cb, &SetHealth(7));
    assert!(c.run_until(120, |c| pawn_health(c.room(room).unwrap(), player(&bob)) == Some(7)));
    let set_at = c.unix_clock_ms();
    let stored = c.run_until(600, |c| {
        c.player_service()
            .store
            .get_checkpoint(room)
            .block()
            .unwrap()
            .unwrap()
            .is_some_and(|(saved, _)| saved > set_at)
    });
    assert!(stored, "checkpoint stored");

    c.set_fleet_outage(&stale, true);
    let lease = c.player_service().config.fleet.lease_ttl;
    let dead = c.run_until((lease.as_secs_f64() * 60.0) as u64 + 180, |c| {
        c.player_service()
            .fleet
            .lock()
            .server(&stale)
            .is_none_or(|s| s.state == ServerState::Dead)
    });
    assert!(dead, "lease expired");
    assert!(
        c.rooms_of(&stale).contains(&room),
        "the partitioned server still runs the room"
    );
    // Bob's client restarted: the player service restores the room elsewhere.
    let moved = c.rejoin(&bob).unwrap().expect("restorable");
    assert_ne!(moved.server_id, stale);
    c.reconnect(cb, &moved);
    assert!(c.run_until(600, |c| c.client_net(cb).is_connected()));
    // Alice plays on in the stale copy until the partition heals.
    send(&mut c, ca, &SetHealth(55));
    c.step_ticks(60);
    c.set_fleet_outage(&stale, false);
    let closed = c.run_until(600, |c| !c.rooms_of(&stale).contains(&room));
    assert!(closed, "the stale copy was closed");
    assert!(
        c.run_until(120, |c| failed_with(c, ca, DisconnectCode::ServerShutdown)),
        "{:?}",
        c.client_net(ca).state()
    );
    let fleet = c.player_service().fleet.lock().room(room).cloned().unwrap();
    assert_eq!(fleet.server, moved.server_id, "the entry stays with the live copy");
    let back = c.run_until(600, |c| {
        c.player_service()
            .fleet
            .lock()
            .server(&stale)
            .is_some_and(|s| s.state == ServerState::Ready)
    });
    assert!(back, "the partitioned server registered again");
    let a = c.rejoin(&alice).unwrap().expect("rejoin");
    assert_eq!((a.room_id, a.server_id.as_str()), (room, moved.server_id.as_str()));
    c.reconnect(ca, &a);
    let one = c.run_until(600, |c| {
        c.client_net(ca).is_connected()
            && healths(c.client(ca)) == healths(c.client(cb))
            && healths(c.client(ca)).len() == 2
    });
    assert!(one, "{:?} vs {:?}", healths(c.client(ca)), healths(c.client(cb)));
    assert_eq!(c.rooms_of(&stale), Vec::<RoomId>::new());
}

/// Regression: a rejoin while the hosting server drains goes back to the match that still
/// runs there instead of restoring a second copy on another server.
#[test]
fn rejoin_during_a_drain_returns_to_the_running_match() {
    let mut c = cluster(2);
    let alice = c.guest("alice");
    let bob = c.guest("bob");
    let (ca, cb, room) = connected_pair(&mut c, &alice, &bob);
    let host = c.room_server(room).unwrap().to_owned();
    let (status, _) = admin(&c, &host, "POST", "/admin/v1/drain", None, true);
    assert_eq!(status, 202);
    assert!(c.run_until(600, |c| {
        c.player_service()
            .fleet
            .lock()
            .server(&host)
            .is_some_and(|s| s.state == ServerState::Draining)
    }));
    // Bob's client restarted (no resume token): rejoin through the player service.
    let a = c.rejoin(&bob).unwrap().expect("rejoin");
    assert_eq!(
        (a.room_id, a.server_id.as_str()),
        (room, host.as_str()),
        "same room, same server"
    );
    c.reconnect(cb, &a);
    assert!(
        c.run_until(600, |c| c.client_net(cb).is_connected()),
        "{:?}",
        c.client_net(cb).state()
    );
    assert!(c.client_net(ca).is_connected(), "alice unaffected");
    assert_eq!(pawns(c.room(room).unwrap()), 2, "bob took his own slot back");
    let other = c.game_servers().into_iter().find(|s| *s != host).unwrap();
    assert!(c.rooms_of(&other).is_empty(), "no second copy of the match");
    assert_eq!(
        c.player_service().fleet.lock().room(room).map(|e| e.server.clone()),
        Some(host.clone())
    );
}

/// §12 regression: a room crashing 3 times within 5 minutes is closed (clients get
/// `RoomCrashed`, no rebuild) — the in-process cluster uses the shards' crash-loop guard.
#[test]
fn a_crash_looping_room_is_closed() {
    let mut c = cluster(1);
    let alice = c.guest("alice");
    let bob = c.guest("bob");
    let (ca, _cb, room) = connected_pair(&mut c, &alice, &bob);
    for crash in 1..=3u64 {
        c.crash_room(room);
        assert!(
            c.run_until(60, |c| c.room_crashes() == crash),
            "crash {crash} contained"
        );
        if crash < 3 {
            assert!(c.room(room).is_some(), "rebuilt after crash {crash}");
        }
        c.step_ticks(5);
    }
    assert!(c.room(room).is_none(), "closed after the third crash");
    assert!(failed_with(&c, ca, DisconnectCode::RoomCrashed));
    let gone = c.run_until(120, |c| c.player_service().fleet.lock().room(room).is_none());
    assert!(gone, "the player service forgot the room");
    assert!(!matches!(c.rejoin(&alice), Ok(Some(_))), "nothing to rejoin");
}

/// §12 regression: `RoomControl::load_player_data` reaches the player service and back
/// (`PlayerDataLoaded`), a fetch during a fleet outage is answered empty, and
/// `RoomControl::submit_score` reaches the leaderboard through the outbox (replayed after an
/// outage, applied once).
#[test]
fn rooms_load_player_data_and_submit_scores() {
    use common::{LoadData, Loaded, Score};
    let mut c = cluster(1);
    let alice = c.guest("alice");
    let bob = c.guest("bob");
    let (ca, _cb, room) = connected_pair(&mut c, &alice, &bob);
    let server = c.room_server(room).unwrap().to_owned();
    c.api(&alice)
        .put_data(
            "settings",
            &space3d_replication::api::PutPlayerDataRequest {
                value: serde_json::json!({ "fov": 90 }),
                expected_version: None,
            },
        )
        .block()
        .unwrap()
        .unwrap();
    let loaded = |c: &TestCluster| c.room(room).unwrap().resources.expect::<Loaded>().0.clone();
    send(&mut c, ca, &LoadData(vec!["settings".into(), "missing".into()]));
    assert!(run_until_bg(&mut c, 500, |c| !loaded(c).is_empty()), "answer arrived");
    let got = loaded(&c);
    assert_eq!(got[0].player, player(&alice));
    assert_eq!(got[0].entries.len(), 1, "missing keys are absent");
    assert_eq!(got[0].entries[0].key, "settings");
    assert_eq!(got[0].entries[0].value, serde_json::json!({ "fov": 90 }));

    // Player service unreachable: the game still gets an (empty) answer.
    c.set_fleet_outage(&server, true);
    send(&mut c, ca, &LoadData(vec!["settings".into()]));
    assert!(
        run_until_bg(&mut c, 300, |c| loaded(c).len() == 2),
        "answered during the outage"
    );
    assert!(loaded(&c)[1].entries.is_empty());

    // Score submitted during the outage: buffered, replayed, on the board once.
    send(&mut c, ca, &Score(7));
    c.step_ticks(30);
    c.set_fleet_outage(&server, false);
    let board = c.api(&alice);
    let on_board = c.run_until(600, |_| {
        board
            .leaderboard("kills", 10)
            .block()
            .unwrap()
            .is_ok_and(|rows| rows.len() == 1)
    });
    assert!(on_board, "score reached the leaderboard");
    let rows = board.leaderboard("kills", 10).block().unwrap().unwrap();
    assert_eq!((rows[0].player_id, rows[0].score), (player(&alice), 7));
}

/// A fetch the player service never answers (the link dropped after the request went out) is
/// answered empty after `FETCH_TIMEOUT`, so the game is never left waiting.
#[test]
fn unanswered_player_data_fetches_time_out() {
    use common::{LoadData, Loaded};
    let mut c = cluster(1);
    let alice = c.guest("alice");
    let bob = c.guest("bob");
    let (ca, _cb, room) = connected_pair(&mut c, &alice, &bob);
    let server = c.room_server(room).unwrap().to_owned();
    c.set_fleet_swallow(&server, true);
    send(&mut c, ca, &LoadData(vec!["settings".into()]));
    let loaded = |c: &TestCluster| c.room(room).unwrap().resources.expect::<Loaded>().0.clone();
    let ticks = (space3d_server::gs::FETCH_TIMEOUT.as_secs_f64() * 60.0) as u64 + 120;
    assert!(
        c.run_until(ticks, |c| !loaded(c).is_empty()),
        "answered after the timeout"
    );
    assert!(loaded(&c)[0].entries.is_empty());
}
