//! Abuse and failure paths on the in-process `TestCluster`: a flooding client is cut at the
//! gateway without ejecting anyone else, IP bans end live connections, room kicks show up in
//! `space3d_disconnects_total{reason}`, a player-service outage keeps rooms running and the
//! server ready (results buffered, then flushed), and `RecoveryPolicy::Abandon` voids an
//! orphaned room instead of restoring it.

mod common;

use common::{DuelGame, DuelProtocol, Finish, GAME_ID, remote_count};
use space3d_core::App;
use space3d_net::{Http, HttpRequest};
use space3d_replication::api::SessionTokens;
use space3d_replication::client::{ConnectionState, FailReason, NetClient};
use space3d_replication::harness::ClientHandle;
use space3d_replication::protocol::control::{Ping, TimeMsg};
use space3d_replication::protocol::handshake::{ClientCaps, DisconnectCode, Hello, HelloAuth};
use space3d_replication::protocol::{ChannelId, DataWriter, PacketKind, encode_handshake, peek};
use space3d_replication::transport::conditioner::LinkProfile;
use space3d_replication::transport::{ClientTransport, Lane, LinkState};
use space3d_replication::{PlayerId, RoomId, Tick};
use space3d_server::clock::UnixClock;
use space3d_server::game::{GameDefinition, RecoveryPolicy, RoomSpec};
use space3d_server::health::HealthState;
use space3d_server::metrics::Metrics;
use space3d_server::ps::fleet::ServerState;
use space3d_server::testing::{TEST_ADMIN_TOKEN, TestCluster, TestClusterConfig};

fn cluster(game_servers: usize) -> TestCluster {
    TestCluster::new(
        DuelGame,
        TestClusterConfig {
            game_servers,
            ..Default::default()
        },
    )
}

fn connected_pair(c: &mut TestCluster, a: &SessionTokens, b: &SessionTokens) -> (ClientHandle, ClientHandle, RoomId) {
    let found = c.matchmake(&[a, b], "duel");
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
    assert!(ok, "both players connected");
    (ca, cb, found[0].room_id)
}

fn failed_with(c: &TestCluster, client: ClientHandle, code: DisconnectCode) -> bool {
    matches!(
        c.client_net(client).state(),
        ConnectionState::Failed(FailReason::Disconnected(got, _)) if *got == code
    )
}

fn ping_packet(id: u16) -> Vec<u8> {
    let mut dw = DataWriter::new(None, None, Tick(0));
    let body = postcard::to_stdvec(&TimeMsg::Ping(Ping { id, t_us: 1 })).unwrap();
    dw.block_bytes(ChannelId::Time, &body);
    dw.finish()
}

/// A raw client holding a real ticket for `found`'s room, admitted by the gatekeeper.
fn raw_client(c: &mut TestCluster, assignment: &space3d_replication::api::Assignment) -> impl ClientTransport {
    let mut t = c
        .network(&assignment.server_id)
        .connector(LinkProfile::default())
        .connect_memory();
    let (schema, _) = space3d_server::schema_of(&DuelGame);
    let hello = Hello::new(
        GAME_ID,
        "1",
        schema,
        HelloAuth::Ticket(assignment.join_ticket.clone()),
        ClientCaps::default(),
    );
    c.step();
    t.send(Lane::Reliable, encode_handshake(PacketKind::Hello, &hello))
        .unwrap();
    let mut welcomed = false;
    for _ in 0..300 {
        c.step();
        while let Some(p) = t.recv() {
            welcomed |= matches!(peek(&p), Some((PacketKind::Welcome, _)));
        }
        if welcomed {
            break;
        }
    }
    assert!(welcomed, "raw client admitted");
    t
}

fn metrics_text(c: &TestCluster, node: &str) -> String {
    c.game_server_app(node).unwrap().resources.expect::<Metrics>().render()
}

/// A flood from one admitted client is cut at the gateway (per-link budget) before it can fill
/// the room inbox every player shares: the other player never loses its link.
#[test]
fn a_flooding_client_is_cut_without_ejecting_other_players() {
    let mut c = cluster(1);
    let victim = c.guest("victim");
    let attacker = c.guest("attacker");
    let found = c.matchmake(&[&victim, &attacker], "duel");
    let cv = c.connect(&found[0], |app| {
        app.add_plugin(DuelProtocol);
    });
    let mut at = raw_client(&mut c, &found[1]);
    assert!(c.run_until(300, |c| c.client_net(cv).is_connected()));
    let mut victim_drops = 0;
    let mut id = 0u16;
    for _ in 0..120 {
        // 12 000 packets per tick: more than the room inbox (8192) holds between two ticks.
        for _ in 0..12_000 {
            id = id.wrapping_add(1);
            if at.send(Lane::Unreliable, ping_packet(id)).is_err() {
                break;
            }
        }
        c.step();
        while at.recv().is_some() {}
        if !c.client_net(cv).is_connected() {
            victim_drops += 1;
        }
    }
    assert_eq!(victim_drops, 0, "victim state {:?}", c.client_net(cv).state());
    assert!(
        matches!(at.state(), LinkState::Closed(_)),
        "flooder cut, link {:?}",
        at.state()
    );
    let gate = c.game_server(&found[1].server_id).unwrap().gate.clone();
    assert!(gate.lock().dropped_packets() > 0);
    assert!(metrics_text(&c, &found[1].server_id).contains("space3d_rate_limited_total{kind=\"link_packets\"}"));
}

/// Room-side kicks are exported per reason (here: strikes → `protocol_violation`).
#[test]
fn room_kicks_are_counted_by_reason() {
    let mut c = cluster(1);
    let a = c.guest("a");
    let b = c.guest("b");
    let found = c.matchmake(&[&a, &b], "duel");
    let _ca = c.connect(&found[0], |app| {
        app.add_plugin(DuelProtocol);
    });
    let mut bad = raw_client(&mut c, &found[1]);
    for _ in 0..10 {
        let _ = bad.send(Lane::Reliable, vec![0x04, 0x00, 0x09, 0xFF]);
        c.step();
    }
    c.step_ticks(120);
    let text = metrics_text(&c, &found[1].server_id);
    assert!(
        text.contains("space3d_disconnects_total{reason=\"protocol_violation\"} 1"),
        "{text}"
    );
    assert!(matches!(bad.state(), LinkState::Closed(_)));
}

/// An IP/CIDR ban also ends connections that are already open from the banned range.
#[test]
fn ip_bans_disconnect_connected_clients() {
    let mut c = cluster(1);
    let alice = c.guest("alice");
    let bob = c.guest("bob");
    let (ca, cb, _room) = connected_pair(&mut c, &alice, &bob);
    // TestCluster gives memory links synthetic 10.x.y.z client IPs.
    let res = Http::new()
        .request(
            HttpRequest::post(format!("{}/admin/v1/bans", c.admin_url("ps")))
                .header("authorization", format!("Bearer {TEST_ADMIN_TOKEN}"))
                .json(&serde_json::json!({ "cidr": "10.0.0.0/8", "reason": "abuse" }))
                .unwrap(),
        )
        .block()
        .unwrap()
        .unwrap();
    assert_eq!(res.status, 201, "{}", res.text());
    let both = c.run_until(300, |c| {
        failed_with(c, ca, DisconnectCode::Banned) && failed_with(c, cb, DisconnectCode::Banned)
    });
    assert!(both, "{:?} / {:?}", c.client_net(ca).state(), c.client_net(cb).state());
}

/// Player service unreachable: rooms keep ticking, the game server stays ready (registration
/// is a readiness latch), results wait in the outbox and reach the player service once the
/// fleet link is back.
#[test]
fn player_service_outage_keeps_rooms_running_and_ready() {
    let mut c = cluster(1);
    let alice = c.guest("alice");
    let bob = c.guest("bob");
    let (ca, _cb, room) = connected_pair(&mut c, &alice, &bob);
    let gs = c.room_server(room).unwrap().to_owned();
    let health = c
        .game_server_app(&gs)
        .unwrap()
        .resources
        .expect::<HealthState>()
        .clone();
    let tick0 = room_tick(&c, room);
    c.set_fleet_outage(&gs, true);
    c.step_ticks(120);
    assert!(!health.is_fleet_connected(), "link state reported");
    // The match ends during the outage.
    c.client_mut(ca)
        .resources
        .expect_mut::<NetClient>()
        .send(&Finish)
        .unwrap();
    c.step_ticks(180);
    assert!(room_tick(&c, room) > tick0 + 250, "room kept ticking");
    let now = c.unix_clock().now_ms();
    assert!(health.is_ready(now, true), "still ready during the outage");
    let outbox = c.game_server(&gs).unwrap().rooms.env().metrics.outbox_pending.get();
    assert!(outbox > 0.0 || pending_outbox(&c, &gs) > 0, "result buffered");
    let alice_id = alice.profile.player_id;
    assert!(
        progress(&c, alice_id).is_none(),
        "nothing reached the player service yet"
    );
    c.set_fleet_outage(&gs, false);
    let flushed = c.run_until(600, |c| progress(c, alice_id).is_some());
    assert!(flushed, "the outbox flushed after the link came back");
    c.step_ticks(120);
    assert!(health.is_fleet_connected());
    assert!(health.is_ready(c.unix_clock().now_ms(), true));
}

fn room_tick(c: &TestCluster, room: RoomId) -> u32 {
    c.room(room)
        .unwrap()
        .resources
        .expect::<space3d_replication::server::NetServer>()
        .tick()
        .0
}

fn pending_outbox(c: &TestCluster, gs: &str) -> usize {
    let text = metrics_text(c, gs);
    text.lines()
        .find(|l| l.starts_with("space3d_outbox_pending "))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse::<f64>().ok())
        .map_or(0, |v| v as usize)
}

fn progress(c: &TestCluster, player: PlayerId) -> Option<serde_json::Value> {
    c.player_service()
        .store
        .get_data(player, vec!["progress".into()])
        .block()
        .unwrap()
        .unwrap()
        .into_iter()
        .next()
        .map(|e| e.value)
}

/// A duel game whose rooms are not restored after a server loss.
#[derive(Clone, Copy, Debug, Default)]
struct AbandonGame;

impl GameDefinition for AbandonGame {
    fn game_id(&self) -> &str {
        GAME_ID
    }
    fn protocol(&self, app: &mut App) {
        DuelGame.protocol(app);
    }
    fn build_room(&self, app: &mut App, spec: &RoomSpec) {
        DuelGame.build_room(app, spec);
    }
    fn recovery_policy(&self, _spec: &RoomSpec) -> RecoveryPolicy {
        RecoveryPolicy::Abandon
    }
}

/// `RecoveryPolicy::Abandon`: after a server loss the player service voids the room instead of
/// restoring it; rejoin answers "no room" and the players go back to matchmaking.
#[test]
fn abandoned_rooms_are_voided_not_restored() {
    let mut c = TestCluster::new(
        AbandonGame,
        TestClusterConfig {
            game_servers: 2,
            ..Default::default()
        },
    );
    let alice = c.guest("alice");
    let bob = c.guest("bob");
    let (_ca, _cb, room) = connected_pair(&mut c, &alice, &bob);
    let lost = c.room_server(room).unwrap().to_owned();
    // The room's policy reaches the player service with the heartbeat.
    let reported = c.run_until(300, |c| {
        c.player_service()
            .fleet
            .lock()
            .room(room)
            .and_then(|e| e.status.as_ref().map(|s| s.abandon_on_loss))
            == Some(true)
    });
    assert!(reported);
    c.kill_game_server(&lost);
    let lease = c.player_service().config.fleet.lease_ttl;
    let ticks = (lease.as_secs_f64() * 60.0) as u64 + 180;
    assert!(c.run_until(ticks, |c| {
        c.player_service()
            .fleet
            .lock()
            .server(&lost)
            .is_none_or(|s| s.state == ServerState::Dead)
    }));
    assert_eq!(c.rejoin(&alice).unwrap(), None, "no restore for an abandoned room");
    assert_eq!(c.rejoin(&bob).unwrap(), None);
    assert!(c.player_service().fleet.lock().room(room).is_none(), "room voided");
    let survivors = c.game_servers();
    assert!(
        survivors.iter().all(|s| !c.rooms_of(s).contains(&room)),
        "nothing restored"
    );
    // Back to matchmaking works.
    let again = c.matchmake(&[&alice, &bob], "duel");
    assert_ne!(again[0].room_id, room);
}
