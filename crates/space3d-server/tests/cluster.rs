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
    // A checkpoint taken after the change must reach the player service's store.
    let stored = c.run_until(600, |c| {
        c.player_service()
            .store
            .get_checkpoint(room)
            .block()
            .unwrap()
            .unwrap()
            .is_some_and(|(saved, _)| saved > set_at)
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
    let draining = c.run_until(600, |c| {
        c.player_service()
            .fleet
            .lock()
            .server("gs-1")
            .is_some_and(|s| s.state == ServerState::Draining)
    });
    assert!(draining, "heartbeat reported draining");
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
    let kicked = c.run_until(300, |c| {
        failed_with(c, cb, DisconnectCode::Kicked) && pawns(c.room(room).unwrap()) == 1
    });
    assert!(kicked, "the ban reached the game server and kicked bob");
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
