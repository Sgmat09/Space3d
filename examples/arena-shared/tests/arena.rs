//! The arena protocol and rules over the deterministic harness: two bots on a mobile-like
//! link, one hunting the other with lag-compensated shots.

use arena_shared::rules::ArenaRules;
use arena_shared::*;
use space3d_core::prelude::*;
use space3d_replication::bots::install_bot;
use space3d_replication::client::Mispredicted;
use space3d_replication::harness::{ClientHandle, ClientSpec, HarnessConfig, NetHarness};
use space3d_replication::prelude::*;
use space3d_replication::transport::conditioner::{LinkConditions, LinkProfile};
use space3d_replication::{Controlled, NetEntity};
use std::time::Duration;

fn arena(seed: u64) -> NetHarness {
    let mut config = HarnessConfig {
        seed,
        tick_hz: TICK_HZ,
        link: LinkProfile::symmetric(LinkConditions {
            latency: Duration::from_millis(45),
            jitter: Duration::from_millis(5),
            ..LinkConditions::perfect()
        }),
        ..Default::default()
    };
    config.server.game_id = GAME_ID.into();
    NetHarness::new(config, |app| {
        app.add_plugin(ArenaProtocol).add_plugin(ArenaRules);
    })
}

fn own_pawn(world: &World) -> Option<Vec3> {
    world
        .query::<(&Transform, &Controlled)>()
        .iter()
        .map(|(t, _)| t.translation)
        .next()
}

/// Aims at the nearest other pawn and pulls the trigger every 20 ticks while strafing.
fn hunter(world: &World, tick: Tick) -> ArenaInput {
    let me = own_pawn(world).unwrap_or_default();
    let target = world
        .query::<(&Transform, &PlayerColor)>()
        .without::<&Controlled>()
        .iter()
        .map(|(t, _)| t.translation)
        .min_by(|a, b| a.distance(me).total_cmp(&b.distance(me)));
    let yaw = target.map_or(0.0, |t| {
        let d = t - me;
        (-d.x).atan2(-d.z)
    });
    ArenaInput {
        move_x: if (tick.0 / 45).is_multiple_of(2) { 40 } else { -40 },
        move_z: 0,
        yaw: ArenaInput::yaw_from_radians(yaw),
        buttons: 0,
        fire_seq: (tick.0 / 20) as u8,
    }
}

fn add(h: &mut NetHarness, name: &str, bot: fn(&World, Tick) -> ArenaInput) -> ClientHandle {
    h.add_client(ClientSpec::named(name), move |app| {
        app.add_plugin(ArenaProtocol);
        install_bot(app, bot);
    })
}

#[test]
fn hunter_scores_with_lag_compensation_and_prediction_stays_exact() {
    let mut h = arena(7);
    let a = add(&mut h, "hunter", hunter);
    let b = add(&mut h, "target", |_, _| ArenaInput::default());
    for c in [a, b] {
        assert!(
            h.run_until(600, |h| h.client_net(c).is_connected()),
            "{:?}",
            h.client_net(c).state()
        );
    }
    h.step_ticks(120);
    let (mut hits_a, mut hits_b, mut mis) = (Vec::new(), Vec::new(), 0);
    let (mut ra, mut rb, mut rm) = (EventReader::default(), EventReader::default(), EventReader::default());
    for _ in 0..900 {
        h.step();
        hits_a.extend(
            ra.read(h.client(a).resources.expect::<Events<FromServer<Hit>>>())
                .map(|m| m.message),
        );
        hits_b.extend(
            rb.read(h.client(b).resources.expect::<Events<FromServer<Hit>>>())
                .map(|m| m.message),
        );
        mis += rm.read(h.client(a).resources.expect::<Events<Mispredicted>>()).count();
    }
    // 45 shots in 15 s against a standing target from ~22 m: most land (4 hits per kill).
    assert!(hits_a.len() >= 20, "only {} hits", hits_a.len());
    assert_eq!(hits_a, hits_b, "both clients see the same hits");
    assert!(hits_a.iter().any(|h| h.kill));
    assert_eq!(mis, 0, "the hunter's own movement is predicted exactly");
    // Score replicated; ammo is private to its owner.
    let (score, ammo_visible_to_target) = {
        let app = h.client(b);
        let mut score = None;
        let mut ammo = false;
        for (e, _) in app.world.query::<(Entity, &NetEntity)>().iter() {
            if !app.world.satisfies::<&Controlled>(e) {
                score = app.world.get::<&Score>(e).ok().map(|s| s.0);
                ammo |= app.world.get::<&Ammo>(e).is_ok();
            }
        }
        (score, ammo)
    };
    assert!(score.is_some_and(|s| s >= 1), "hunter score {score:?}");
    assert!(!ammo_visible_to_target);
    for c in [a, b] {
        assert!(
            h.run_until(120, |h| h.assert_converged(c).is_ok()),
            "{:?}",
            h.assert_converged(c)
        );
    }
}

#[test]
fn chat_round_trip_and_validation() {
    let mut h = arena(8);
    let a = add(&mut h, "alice", |_, _| ArenaInput::default());
    assert!(h.run_until(600, |h| h.client_net(a).is_connected()));
    let net = h.client_mut(a).resources.expect_mut::<NetClient>();
    net.send(&Say("hello arena".into())).unwrap();
    net.send(&Say("x".repeat(200))).unwrap(); // passes the size cap, fails `validate`
    let mut got = Vec::new();
    let mut r = EventReader::default();
    for _ in 0..60 {
        h.step();
        got.extend(
            r.read(h.client(a).resources.expect::<Events<FromServer<Chat>>>())
                .map(|m| m.message.clone()),
        );
    }
    assert_eq!(
        got,
        vec![Chat {
            from: "alice".into(),
            text: "hello arena".into()
        }]
    );
    assert_eq!(h.server_net().room_stats().strikes, 1);
}
