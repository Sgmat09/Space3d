//! End-to-end replication tests over the deterministic in-memory harness.

mod common;

use common::*;
use space3d_core::prelude::*;
use space3d_replication::bots::{install_bot, scripted};
use space3d_replication::client::{ConnectionEvent, Mispredicted};
use space3d_replication::harness::{ClientHandle, ClientSpec, HarnessConfig, NetHarness};
use space3d_replication::prelude::*;
use space3d_replication::transport::conditioner::{LinkConditions, LinkProfile, Semantics};
use std::time::Duration;

fn harness(seed: u64, semantics: Semantics, link: LinkConditions) -> NetHarness {
    NetHarness::new(
        HarnessConfig {
            seed,
            semantics,
            link: LinkProfile::symmetric(link),
            ..Default::default()
        },
        |app| {
            app.add_plugin(TestProtocol).add_plugin(TestRules);
        },
    )
}

fn add_walker(h: &mut NetHarness, name: &str, script: fn(Tick) -> Move) -> ClientHandle {
    h.add_client(ClientSpec::named(name), move |app| {
        app.add_plugin(TestProtocol);
        install_bot(app, scripted(script));
    })
}

fn count<E: Send + Sync + 'static>(app: &App, reader: &mut EventReader) -> usize {
    reader.read(app.resources.expect::<Events<E>>()).count()
}

fn connect(h: &mut NetHarness, c: ClientHandle) {
    assert!(
        h.run_until(600, |h| h.client_net(c).is_connected()
            && controlled(h.client(c)).is_some()),
        "state {:?}",
        h.client_net(c).state()
    );
}

#[test]
fn client_connects_syncs_and_sees_its_pawn() {
    let mut h = harness(1, Semantics::Stream, LinkConditions::lan());
    let a = h.add_client(ClientSpec::named("alice"), |app| {
        app.add_plugin(TestProtocol);
    });
    connect(&mut h, a);
    let e = controlled(h.client(a)).unwrap();
    let app = h.client(a);
    assert!(app.world.satisfies::<&Predicted>(e));
    assert!(app.world.satisfies::<&Simulated>(e));
    assert_eq!(app.world.get::<&Health>(e).unwrap().0, 100);
    assert_eq!(app.world.get::<&Ammo>(e).unwrap().0, 30);
    assert_eq!(app.world.get::<&Name>(e).unwrap().0, "alice");
    h.step_ticks(30);
    assert!(
        h.run_until(120, |h| h.assert_converged(a).is_ok()),
        "{:?}",
        h.assert_converged(a)
    );
}

/// Deterministic sim at 100 ms RTT: the prediction never disagrees with the server.
#[test]
fn deterministic_prediction_has_no_mispredictions() {
    let rtt_50 = LinkConditions {
        latency: Duration::from_millis(50),
        ..LinkConditions::perfect()
    };
    let mut h = harness(2, Semantics::Stream, rtt_50);
    let a = add_walker(&mut h, "alice", |t| Move {
        x: if (t.0 / 90) % 2 == 0 { 1 } else { -1 },
        z: if (t.0 / 50) % 3 == 0 { 1 } else { 0 },
    });
    connect(&mut h, a);
    h.step_ticks(60); // settle the pacer
    let mut reader = EventReader::default();
    let _ = count::<Mispredicted>(h.client(a), &mut reader);
    let rollbacks = h.client_net(a).stats().rollbacks;
    let mut mispredicted = 0;
    for _ in 0..600 {
        h.step();
        mispredicted += count::<Mispredicted>(h.client(a), &mut reader);
    }
    assert_eq!(
        mispredicted,
        0,
        "rollbacks {}",
        h.client_net(a).stats().rollbacks - rollbacks
    );
    // The pawn actually moved on both sides and the client leads the server.
    let server_pawn = pawn(h.server(), h.player(a)).unwrap();
    let sx = h.server().world.get::<&Transform>(server_pawn).unwrap().translation;
    assert!(sx.length() > 1.0, "server pawn did not move: {sx}");
    let net = h.client_net(a);
    let lead = net.predicted_tick().unwrap().0 as i64 - h.server_net().tick().0 as i64;
    assert!((3..=12).contains(&lead), "prediction lead {lead}");
    assert_eq!(h.server_net().room_stats().inputs_missing, 0);
    assert!(
        h.run_until(60, |h| h.assert_converged(a).is_ok()),
        "{:?}",
        h.assert_converged(a)
    );
}

/// A server-only change to a predicted component gives exactly one rollback and converges.
#[test]
fn server_impulse_gives_one_misprediction() {
    let link = LinkConditions {
        latency: Duration::from_millis(40),
        ..LinkConditions::perfect()
    };
    let mut h = harness(3, Semantics::Stream, link);
    let a = add_walker(&mut h, "alice", |_| Move { x: 1, z: 0 });
    connect(&mut h, a);
    h.step_ticks(90);
    let mut reader = EventReader::default();
    let _ = count::<Mispredicted>(h.client(a), &mut reader);
    let server_pawn = pawn(h.server(), h.player(a)).unwrap();
    h.server_mut()
        .world
        .get::<&mut Transform>(server_pawn)
        .unwrap()
        .translation
        .z += 5.0;
    let mut events = 0;
    for _ in 0..60 {
        h.step();
        events += count::<Mispredicted>(h.client(a), &mut reader);
    }
    assert_eq!(events, 1);
    // Converged: the client's pawn is where the server's is plus the prediction lead.
    let ce = controlled(h.client(a)).unwrap();
    let cz = h.client(a).world.get::<&Transform>(ce).unwrap().translation.z;
    assert!((cz - 5.0).abs() < 0.01, "client z {cz}");
    assert!(
        h.run_until(60, |h| h.assert_converged(a).is_ok()),
        "{:?}",
        h.assert_converged(a)
    );
}

/// Two clients see each other; the remote pawn is interpolated and `Audience::Owner` never leaks.
#[test]
fn remote_pawns_interpolate_and_owner_state_stays_private() {
    let mut h = harness(4, Semantics::Stream, LinkConditions::broadband());
    let a = add_walker(&mut h, "alice", |_| Move { x: 1, z: 0 });
    let b = add_walker(&mut h, "bob", |_| Move { x: 0, z: 0 });
    connect(&mut h, a);
    connect(&mut h, b);
    let alice_on_b = || {};
    let _ = alice_on_b;
    let mut last_render = f64::MIN;
    for _ in 0..240 {
        h.step();
        let app = h.client(b);
        for (e, _) in app.world.query::<(Entity, &space3d_replication::NetEntity)>().iter() {
            if !app.world.satisfies::<&space3d_replication::Controlled>(e) {
                assert!(app.world.get::<&Ammo>(e).is_err(), "Audience::Owner leaked");
                assert!(app.world.satisfies::<&space3d_replication::Interpolated>(e));
            }
        }
        let r = h.client_net(b).render_tick().unwrap_or(f64::MIN);
        assert!(r >= last_render, "render tick went backwards");
        last_render = r;
    }
    let server_alice = pawn(h.server(), h.player(a)).unwrap();
    let remote = h.client_entity(b, server_alice).expect("bob sees alice");
    let rx = h.client(b).world.get::<&Transform>(remote).unwrap().translation.x;
    let sx = h.server().world.get::<&Transform>(server_alice).unwrap().translation.x;
    // Interpolated in the past, but close.
    assert!(rx < sx && sx - rx < 1.5, "remote x {rx} vs server {sx}");
    assert!(
        h.run_until(120, |h| h.assert_converged(b).is_ok()),
        "{:?}",
        h.assert_converged(b)
    );
}

fn churn(h: &mut NetHarness, step: u32) {
    // Spawn / insert / remove / despawn server-side entities with a fixed schedule.
    let world = &mut h.server_mut().world;
    let mut crates: Vec<Entity> = world
        .query::<(Entity, &Health)>()
        .without::<&space3d_replication::ControlledBy>()
        .iter()
        .map(|(e, _)| e)
        .collect();
    crates.sort();
    match step % 4 {
        0 => {
            world.spawn((
                Replicated,
                Health(step as u16),
                Transform::from_xyz(step as f32, 0.0, 1.0),
            ));
        }
        1 => {
            if let Some(e) = crates.first() {
                let _ = world.insert_one(*e, Name(format!("crate{step}")));
            }
        }
        2 => {
            if let Some(e) = crates.get(1) {
                let _ = world.remove_one::<Name>(*e);
                world.get::<&mut Health>(*e).unwrap().0 += 1;
            }
        }
        _ => {
            if crates.len() > 3 {
                let _ = world.despawn(crates[0]);
            }
        }
    }
}

fn converge_under(semantics: Semantics, seed: u64) -> u64 {
    let mut h = harness(seed, semantics, LinkConditions::bad_wifi());
    let a = add_walker(&mut h, "alice", |t| Move {
        x: ((t.0 / 40) % 3) as i8 - 1,
        z: 0,
    });
    let b = add_walker(&mut h, "bob", |_| Move::default());
    connect(&mut h, a);
    connect(&mut h, b);
    for i in 0..40 {
        churn(&mut h, i);
        h.step_ticks(9);
    }
    // Stop the churn; everything must converge.
    let ok = h.run_until(900, |h| h.assert_converged(a).is_ok() && h.assert_converged(b).is_ok());
    assert!(ok, "a: {:?} b: {:?}", h.assert_converged(a), h.assert_converged(b));
    h.wire_digest()
}

#[test]
fn spawn_insert_remove_despawn_converge_on_bad_wifi_stream() {
    converge_under(Semantics::Stream, 11);
}

#[test]
fn spawn_insert_remove_despawn_converge_on_bad_wifi_datagram() {
    converge_under(Semantics::Datagram, 12);
}

#[test]
fn same_seed_same_bytes() {
    assert_eq!(
        converge_under(Semantics::Datagram, 21),
        converge_under(Semantics::Datagram, 21)
    );
}

#[test]
fn messages_both_ways() {
    let mut h = harness(5, Semantics::Stream, LinkConditions::lan());
    let a = add_walker(&mut h, "alice", |_| Move::default());
    connect(&mut h, a);
    // Client → server.
    h.client_mut(a)
        .resources
        .expect_mut::<NetClient>()
        .send(&Say("hello".into()))
        .unwrap();
    assert!(
        h.client_mut(a)
            .resources
            .expect_mut::<NetClient>()
            .send(&Chat("server only".into()))
            .is_err()
    );
    let mut got = Vec::new();
    let mut reader = EventReader::default();
    for _ in 0..30 {
        h.step();
        for m in reader.read(h.server().resources.expect::<Events<FromClient<Say>>>()) {
            got.push((m.player, m.message.clone()));
        }
    }
    assert_eq!(got, vec![(h.player(a), Say("hello".into()))]);
    // Server → client.
    h.server_mut()
        .resources
        .expect_mut::<NetServer>()
        .send(Target::All, &Chat("welcome".into()));
    let mut got = Vec::new();
    let mut reader = EventReader::default();
    for _ in 0..30 {
        h.step();
        for m in reader.read(h.client(a).resources.expect::<Events<FromServer<Chat>>>()) {
            got.push(m.message.clone());
        }
    }
    assert_eq!(got, vec![Chat("welcome".into())]);
}

#[test]
fn partition_then_resume_keeps_entity_and_net_id() {
    let mut h = harness(6, Semantics::Stream, LinkConditions::lan());
    let a = add_walker(&mut h, "alice", |_| Move { x: 1, z: 0 });
    connect(&mut h, a);
    let before = controlled(h.client(a)).unwrap();
    let net_id = h.client_net(a).net_id(before).unwrap();
    let mut resumed_r = EventReader::default();
    let mut disc_r = EventReader::default();
    let mut conn_r = EventReader::default();
    let _ = count::<PlayerResumed>(h.server(), &mut resumed_r);
    let _ = count::<PlayerDisconnected>(h.server(), &mut disc_r);
    let _ = count::<ConnectionEvent>(h.client(a), &mut conn_r);
    h.partition(a, Duration::from_secs(2));
    let (mut resumed, mut disconnected) = (0, 0);
    let mut client_events = Vec::new();
    for _ in 0..240 {
        h.step();
        resumed += count::<PlayerResumed>(h.server(), &mut resumed_r);
        disconnected += count::<PlayerDisconnected>(h.server(), &mut disc_r);
        client_events.extend(
            conn_r
                .read(h.client(a).resources.expect::<Events<ConnectionEvent>>())
                .cloned(),
        );
    }
    assert_eq!((disconnected, resumed), (1, 1));
    assert!(h.client_net(a).is_connected());
    let after = controlled(h.client(a)).unwrap();
    assert_eq!(after, before, "entity was recreated");
    assert_eq!(h.client_net(a).net_id(after), Some(net_id));
    assert_eq!(h.server_net().connected_count(), 1);
    assert!(client_events.iter().any(|e| matches!(
        e,
        ConnectionEvent::Disconnected {
            will_reconnect: true,
            ..
        }
    )));
    assert!(
        client_events
            .iter()
            .any(|e| matches!(e, ConnectionEvent::Connected { resumed: true, .. }))
    );
    assert!(
        h.run_until(120, |h| h.assert_converged(a).is_ok()),
        "{:?}",
        h.assert_converged(a)
    );
}
