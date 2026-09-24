//! Harness scenarios beyond the basics in `harness.rs`: session lifecycle (resume expiry,
//! supersede), prediction snaps, spatial interest hysteresis, bandwidth budget and starvation,
//! flow control on a slow stream, render-timed components and messages, server-side bots,
//! time sync, and a property test replaying random world edits through the whole pipeline.

mod common;

use common::*;
use proptest::prelude::*;
use serde::{Deserialize, Serialize};
use space3d_core::prelude::*;
use space3d_net::ReconnectPolicy;
use space3d_replication::bots::{install_bot, scripted};
use space3d_replication::client::{ConnectionState, FailReason, Mispredicted, RemoteDespawned};
use space3d_replication::harness::{ClientHandle, ClientSpec, HarnessConfig, NetHarness};
use space3d_replication::prelude::*;
use space3d_replication::protocol::handshake::DisconnectCode;
use space3d_replication::server::inputs::brain;
use space3d_replication::server::{InterestPolicy, SlotState};
use space3d_replication::transport::conditioner::{LinkConditions, LinkProfile, Semantics};
use space3d_replication::{InterestSource, Owner};
use std::time::Duration;

fn with_rules(config: HarnessConfig) -> NetHarness {
    NetHarness::new(config, |app| {
        app.add_plugin(TestProtocol).add_plugin(TestRules);
    })
}

fn link(latency_ms: u64) -> LinkProfile {
    LinkProfile::symmetric(LinkConditions {
        latency: Duration::from_millis(latency_ms),
        ..LinkConditions::perfect()
    })
}

fn walker(h: &mut NetHarness, name: &str, script: fn(Tick) -> Move) -> ClientHandle {
    h.add_client(ClientSpec::named(name), move |app| {
        app.add_plugin(TestProtocol);
        install_bot(app, scripted(script));
    })
}

fn observer(h: &mut NetHarness, name: &str) -> ClientHandle {
    h.add_client(ClientSpec::named(name), |app| {
        app.add_plugin(TestProtocol);
    })
}

fn connect(h: &mut NetHarness, c: ClientHandle) {
    assert!(
        h.run_until(600, |h| h.client_net(c).is_connected()
            && controlled(h.client(c)).is_some()),
        "state {:?}",
        h.client_net(c).state()
    );
}

fn read<E: Clone + Send + Sync + 'static>(app: &App, reader: &mut EventReader) -> Vec<E> {
    reader.read(app.resources.expect::<Events<E>>()).cloned().collect()
}

fn converged(h: &mut NetHarness, c: ClientHandle, ticks: u64) {
    assert!(
        h.run_until(ticks, |h| h.assert_converged(c).is_ok()),
        "{:?}",
        h.assert_converged(c)
    );
}

/// A rollback longer than `max_rollback_ticks` snaps to the server state instead of replaying,
/// and prediction recovers (one follow-up replay) once the link is healthy again.
#[test]
fn rollback_beyond_max_snaps_then_recovers() {
    let mut config = HarnessConfig {
        seed: 31,
        link: link(20),
        ..Default::default()
    };
    config.client.prediction.max_rollback_ticks = 8;
    let mut h = with_rules(config);
    let a = walker(&mut h, "alice", |_| Move { x: 1, z: 0 });
    connect(&mut h, a);
    h.step_ticks(90);
    let mut reader = EventReader::default();
    let _ = read::<Mispredicted>(h.client(a), &mut reader);
    // Downstream stalls for ~400 ms while the server applies an impulse.
    h.set_link(
        a,
        LinkProfile {
            down: LinkConditions {
                latency: Duration::from_millis(400),
                ..LinkConditions::perfect()
            },
            ..link(20)
        },
    );
    let pawn_e = pawn(h.server(), h.player(a)).unwrap();
    h.server_mut()
        .world
        .get::<&mut Transform>(pawn_e)
        .unwrap()
        .translation
        .z += 5.0;
    let mut events = Vec::new();
    for _ in 0..15 {
        h.step();
        events.extend(read::<Mispredicted>(h.client(a), &mut reader));
    }
    h.set_link(a, link(20));
    // The stall inflated the RTT variance, so the pacer's target lead exceeds
    // `max_rollback_ticks` for a while: every correction snaps until the lead shrinks back.
    for _ in 0..300 {
        h.step();
        events.extend(read::<Mispredicted>(h.client(a), &mut reader));
    }
    assert!(
        events.iter().any(|m| m.replayed == 0),
        "expected a snap, got {events:?}"
    );
    assert!(h.client_net(a).is_connected());
    // Healthy again: no further mispredictions, and the client matches the server.
    let mut late = 0;
    for _ in 0..120 {
        h.step();
        late += read::<Mispredicted>(h.client(a), &mut reader).len();
    }
    assert_eq!(late, 0, "prediction did not recover");
    let ce = controlled(h.client(a)).unwrap();
    let cz = h.client(a).world.get::<&Transform>(ce).unwrap().translation.z;
    assert!((cz - 5.0).abs() < 0.01, "client z {cz}");
    converged(&mut h, a, 60);
}

/// A dropped client that never comes back is removed when the resume window expires.
#[test]
fn resume_window_expiry_emits_player_left_timeout() {
    let mut config = HarnessConfig {
        seed: 32,
        link: link(10),
        ..Default::default()
    };
    config.server.resume_window = Duration::from_secs(1);
    config.client.reconnect = ReconnectPolicy::default().max_attempts(0);
    let mut h = with_rules(config);
    let a = walker(&mut h, "alice", |_| Move { x: 1, z: 0 });
    connect(&mut h, a);
    let player = h.player(a);
    let mut disc_r = EventReader::default();
    let mut left_r = EventReader::default();
    h.partition(a, Duration::from_millis(200));
    let (mut disconnected, mut left) = (Vec::new(), Vec::new());
    let mut disconnected_at = None;
    for i in 0..150 {
        h.step();
        let d = read::<PlayerDisconnected>(h.server(), &mut disc_r);
        if !d.is_empty() && disconnected_at.is_none() {
            disconnected_at = Some(i);
            // Slot kept, pawn kept, neutral input.
            let info = h.server_net().clients();
            assert!(matches!(info[0].state, SlotState::Disconnected { .. }));
            assert!(pawn(h.server(), player).is_some());
        }
        disconnected.extend(d);
        left.extend(read::<PlayerLeft>(h.server(), &mut left_r));
    }
    assert_eq!(disconnected.len(), 1);
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].player, player);
    assert_eq!(left[0].reason, LeaveReason::Timeout);
    assert!(h.server_net().clients().is_empty());
    assert!(pawn(h.server(), player).is_none());
    assert!(matches!(
        h.client_net(a).state(),
        ConnectionState::Failed(FailReason::Unreachable(_))
    ));
}

/// The same player joining on a second link takes over the slot; the old link is closed with
/// `Superseded` and does not reconnect.
#[test]
fn same_player_on_a_new_link_supersedes_the_old_one() {
    let mut h = with_rules(HarnessConfig {
        seed: 33,
        link: link(10),
        ..Default::default()
    });
    let a = walker(&mut h, "alice", |_| Move { x: 1, z: 0 });
    connect(&mut h, a);
    let server_pawn = pawn(h.server(), h.player(a)).unwrap();
    let mut resumed_r = EventReader::default();
    let mut left_r = EventReader::default();
    let b = walker(&mut h, "alice", |_| Move { x: 0, z: 1 });
    let (mut resumed, mut left) = (0, 0);
    for _ in 0..120 {
        h.step();
        resumed += read::<PlayerResumed>(h.server(), &mut resumed_r).len();
        left += read::<PlayerLeft>(h.server(), &mut left_r).len();
    }
    assert!(h.client_net(b).is_connected() && controlled(h.client(b)).is_some());
    assert_eq!((resumed, left), (1, 0));
    assert_eq!(h.server_net().clients().len(), 1);
    assert!(
        matches!(
            h.client_net(a).state(),
            ConnectionState::Failed(FailReason::Disconnected(DisconnectCode::Superseded, _))
        ),
        "{:?}",
        h.client_net(a).state()
    );
    // The new link controls the same pawn (same NetId), and moves it along z.
    let ce = controlled(h.client(b)).unwrap();
    assert_eq!(h.client_net(b).net_id(ce), h.server_net().net_id(server_pawn));
    assert_eq!(pawn(h.server(), h.player(b)), Some(server_pawn));
    let z0 = h.server().world.get::<&Transform>(server_pawn).unwrap().translation.z;
    h.step_ticks(60);
    let z1 = h.server().world.get::<&Transform>(server_pawn).unwrap().translation.z;
    assert!(z1 > z0 + 1.0, "pawn not driven by the new link: {z0} → {z1}");
    converged(&mut h, b, 60);
}

/// Spatial interest: an entity enters within `radius`, stays while it wanders inside
/// `radius × hysteresis`, and leaves (out of scope) beyond it — no flicker at the edge.
#[test]
fn spatial_interest_enters_leaves_with_hysteresis_and_never_flickers() {
    let mut config = HarnessConfig {
        seed: 34,
        link: link(10),
        ..Default::default()
    };
    config.server.interest = InterestPolicy::Spatial {
        cell: 8.0,
        radius: 20.0,
        hysteresis: 1.2,
    };
    let mut h = with_rules(config);
    let probe = h
        .server_mut()
        .world
        .spawn((Replicated, Transform::from_xyz(30.0, 0.0, 0.0), Health(1)));
    let a = walker(&mut h, "alice", |_| Move::default());
    connect(&mut h, a);
    let pawn_e = pawn(h.server(), h.player(a)).unwrap();
    h.server_mut()
        .world
        .insert_one(pawn_e, InterestSource { radius: 0.0 })
        .unwrap();
    h.step_ticks(60);
    assert!(h.client_entity(a, probe).is_none(), "visible at 30 m");
    let mut despawn_r = EventReader::default();
    let _ = read::<RemoteDespawned>(h.client(a), &mut despawn_r);
    let mut despawns = Vec::new();
    let set_x = |h: &mut NetHarness, x: f32| {
        h.server_mut().world.get::<&mut Transform>(probe).unwrap().translation.x = x;
    };
    // Approach: 30 → 10 m at 6 m/s.
    let mut entered_at = None;
    let mut x = 30.0f32;
    while x > 10.0 {
        x -= 0.1;
        set_x(&mut h, x);
        h.step();
        let visible = h.client_entity(a, probe).is_some();
        if visible && entered_at.is_none() {
            entered_at = Some(x);
        }
        if entered_at.is_some() {
            assert!(visible, "flickered out at {x}");
        }
    }
    let entered_at = entered_at.expect("never entered");
    assert!((18.0..=20.0).contains(&entered_at), "entered at {entered_at} m");
    // Wander inside the hysteresis band (19–23 m): stays visible, no despawn.
    for i in 0..300 {
        let x = 21.0 + 2.0 * (i as f32 / 12.0).sin();
        set_x(&mut h, x);
        h.step();
        assert!(h.client_entity(a, probe).is_some(), "dropped at {x} m");
        despawns.extend(read::<RemoteDespawned>(h.client(a), &mut despawn_r));
    }
    assert!(despawns.is_empty());
    // Leave: 21 → 40 m.
    let mut left_at = None;
    let mut x = 21.0f32;
    while x < 40.0 {
        x += 0.1;
        set_x(&mut h, x);
        h.step();
        despawns.extend(read::<RemoteDespawned>(h.client(a), &mut despawn_r));
        let visible = h.client_entity(a, probe).is_some();
        if !visible && left_at.is_none() {
            left_at = Some(x);
        }
        if left_at.is_some() {
            assert!(!visible, "flickered back in at {x}");
        }
    }
    let left_at = left_at.expect("never left");
    assert!((24.0..=26.0).contains(&left_at), "left at {left_at} m");
    assert_eq!(despawns.len(), 1);
    assert!(despawns[0].out_of_scope);
    converged(&mut h, a, 60);
}

/// 400 entities changing every tick against a 16 KiB/s budget: the downstream stays within
/// the cap (+5 %) and priority accumulation keeps every entity fresh (no starvation).
#[test]
fn bandwidth_budget_is_respected_without_starvation() {
    const CAP: u32 = 16 * 1024;
    let mut config = HarnessConfig {
        seed: 35,
        link: link(10),
        ..Default::default()
    };
    config.server.client_bandwidth = CAP;
    let mut h = with_rules(config);
    let crates: Vec<Entity> = (0..400)
        .map(|i| {
            h.server_mut().world.spawn((
                Replicated,
                Transform::from_xyz((i % 20) as f32, 0.0, (i / 20) as f32),
                Health(0),
            ))
        })
        .collect();
    let stamp = |h: &mut NetHarness| {
        let tick = h.server_net().tick().0 as u16;
        for hp in h
            .server_mut()
            .world
            .query_mut::<&mut Health>()
            .without::<&space3d_replication::ControlledBy>()
        {
            hp.0 = tick;
        }
    };
    let a = observer(&mut h, "alice");
    connect(&mut h, a);
    for _ in 0..180 {
        stamp(&mut h);
        h.step();
    }
    let bytes0 = h.client_net(a).stats().bytes_in;
    let mut worst_age = 0u16;
    let secs = 5;
    for _ in 0..60 * secs {
        stamp(&mut h);
        h.step();
        let now = h.server_net().tick().0 as u16;
        let app = h.client(a);
        for &c in &crates {
            let e = h.client_entity(a, c).expect("crate replicated");
            let hp = app.world.get::<&Health>(e).unwrap().0;
            worst_age = worst_age.max(now.wrapping_sub(hp));
        }
    }
    let rate = (h.client_net(a).stats().bytes_in - bytes0) as f64 / f64::from(secs);
    assert!(
        rate <= f64::from(CAP) * 1.05,
        "downstream {rate:.0} B/s over the {CAP} B/s cap"
    );
    assert!(rate >= f64::from(CAP) * 0.8, "budget underused: {rate:.0} B/s");
    // ≈ 80 of 400 entities fit per snapshot → each is refreshed every ~5 snapshots.
    assert!(worst_age <= 30, "an entity went {worst_age} ticks without an update");
    let client_id = h.client_net(a).client_id().unwrap();
    let stats = h.server_net().stats(client_id).unwrap();
    assert!(stats.starved_entities > 0, "budget should be binding");
}

/// A 128 KiB/s stream cannot carry ~500 KB/s of snapshots: flow control skips snapshots and
/// halves the rate so the link backlog stays bounded (no bufferbloat), and the client stays
/// connected and converges once the load stops.
#[test]
fn flow_control_bounds_the_backlog_on_a_slow_stream() {
    let slow = LinkConditions {
        latency: Duration::from_millis(20),
        bandwidth: Some(128 * 1024),
        queue_limit: 64 << 20,
        ..LinkConditions::perfect()
    };
    let mut config = HarnessConfig {
        seed: 36,
        link: LinkProfile { down: slow, ..link(20) },
        ..Default::default()
    };
    config.server.client_bandwidth = 4 << 20;
    config.server.client_bandwidth_max = 4 << 20;
    let mut h = with_rules(config);
    for i in 0..1000 {
        h.server_mut()
            .world
            .spawn((Replicated, Transform::from_xyz(i as f32, 0.0, 0.0), Health(1)));
    }
    let a = observer(&mut h, "alice");
    connect(&mut h, a);
    let link = h.link(a).unwrap();
    let mut max_backlog = 0usize;
    for t in 0..600 {
        for (t_, _) in h
            .server_mut()
            .world
            .query_mut::<(&mut Transform, &Health)>()
            .without::<&space3d_replication::ControlledBy>()
        {
            t_.translation.y = (t as f32 * 0.05).sin();
        }
        h.step();
        max_backlog = max_backlog.max(h.network().down_backlog(link).unwrap());
    }
    let skip = 256 * 1024;
    assert!(max_backlog < skip + 64 * 1024, "backlog grew to {max_backlog} B");
    assert!(h.server_net().room_stats().snapshots_skipped > 0);
    assert!(
        h.client_net(a).is_connected(),
        "{:?} {:?}",
        h.client_net(a).state(),
        h.server_net().room_stats()
    );
    assert_eq!(h.server_net().room_stats().kicks, 0);
    converged(&mut h, a, 900);
}

// --- Render-timed state -------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Mood(u8);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Boom(u32);
impl NetMessage for Boom {}

struct TimedProtocol;
impl Plugin for TimedProtocol {
    fn build(&self, app: &mut App) {
        app.add_plugin(ReplicationPlugin::default())
            .replicate_with::<Mood>("timed.mood", ComponentRule::serde().delayed())
            .add_input::<Move>("timed.move")
            .add_message::<Boom>(
                "timed.boom",
                MessageConfig::reliable(Direction::ServerToClient).timing(EventTiming::Interpolated),
            );
    }
}

/// `Delayed` components and `EventTiming::Interpolated` messages are released when the
/// client's render clock reaches their tick (in sync with interpolated poses), not on arrival.
#[test]
fn delayed_components_and_interpolated_messages_follow_the_render_clock() {
    let mut h = NetHarness::new(
        HarnessConfig {
            seed: 37,
            link: link(40),
            ..Default::default()
        },
        |app| {
            app.add_plugin(TimedProtocol);
            let mut joined = EventReader::default();
            app.add_system(Stage::Update, move |ctx| {
                let events: Vec<PlayerJoined> = joined
                    .read(ctx.resources.expect::<Events<PlayerJoined>>())
                    .cloned()
                    .collect();
                for j in events {
                    ctx.world.spawn((
                        Replicated,
                        space3d_replication::ControlledBy(j.identity.player),
                        Mood(1),
                    ));
                }
            });
        },
    );
    let a = h.add_client(ClientSpec::named("alice"), |app| {
        app.add_plugin(TimedProtocol);
    });
    connect(&mut h, a);
    h.step_ticks(60);
    let server_e = h
        .server()
        .world
        .query::<(Entity, &Mood)>()
        .iter()
        .map(|(e, _)| e)
        .next()
        .unwrap();
    // Change the mood and fire a message on the same tick.
    h.server_mut().world.get::<&mut Mood>(server_e).unwrap().0 = 2;
    h.server_mut()
        .resources
        .expect_mut::<NetServer>()
        .send(Target::All, &Boom(7));
    h.step();
    let tick = h.server_net().tick();
    let mut boom_r = EventReader::default();
    let _ = read::<FromServer<Boom>>(h.client(a), &mut boom_r);
    let (mut mood_at, mut boom_at) = (None, None);
    let client_e = h.client_entity(a, server_e).unwrap();
    for _ in 0..60 {
        h.step();
        let render = h.client_net(a).render_tick().unwrap();
        let mood = h.client(a).world.get::<&Mood>(client_e).unwrap().0;
        if mood == 2 && mood_at.is_none() {
            mood_at = Some(render);
        }
        let booms = read::<FromServer<Boom>>(h.client(a), &mut boom_r);
        if !booms.is_empty() {
            assert_eq!(booms[0].message, Boom(7));
            assert_eq!(booms[0].tick, tick);
            boom_at = Some(render);
        }
    }
    let (mood_at, boom_at) = (mood_at.expect("mood applied"), boom_at.expect("boom delivered"));
    let t = f64::from(tick.0);
    // Released when the render clock reached the tick (within one frame), not ~RTT/2 earlier.
    for at in [mood_at, boom_at] {
        assert!(at >= t && at < t + 1.5, "released at render tick {at}, state tick {t}");
    }
}

/// A server-side virtual client drives its pawn with no link; humans see it move.
#[test]
fn server_bot_drives_its_pawn_and_is_replicated() {
    let mut h = with_rules(HarnessConfig {
        seed: 38,
        link: link(20),
        ..Default::default()
    });
    let a = observer(&mut h, "alice");
    connect(&mut h, a);
    let bot = PlayerIdentity::bot("bot-1");
    let slot = h
        .server_mut()
        .resources
        .expect_mut::<NetServer>()
        .add_virtual_client(bot.clone(), brain(|_, _| Move { x: 1, z: 0 }));
    assert_ne!(slot, NetClientId(u16::MAX));
    h.step_ticks(2);
    let bot_pawn = pawn(h.server(), bot.player).expect("bot pawn spawned");
    let x0 = h.server().world.get::<&Transform>(bot_pawn).unwrap().translation.x;
    h.step_ticks(120);
    let x1 = h.server().world.get::<&Transform>(bot_pawn).unwrap().translation.x;
    assert!((x1 - x0 - 2.0 * SPEED).abs() < 0.1, "bot moved {}", x1 - x0);
    let remote = h.client_entity(a, bot_pawn).expect("bot visible");
    let rx = h.client(a).world.get::<&Transform>(remote).unwrap().translation.x;
    assert!(rx > x0 + SPEED, "remote bot at {rx}");
    assert!(h.server_net().clients().iter().any(|c| c.identity.is_bot));
    converged(&mut h, a, 60);
}

/// Under 50 ± 15 ms jitter the server-tick estimate stays within a couple of ticks, the pacer
/// keeps inputs arriving before they are needed, and prediction stays exact.
#[test]
fn clock_sync_and_pacing_hold_under_jitter() {
    let jittery = LinkConditions {
        latency: Duration::from_millis(50),
        jitter: Duration::from_millis(15),
        ..LinkConditions::perfect()
    };
    let mut h = with_rules(HarnessConfig {
        seed: 39,
        link: LinkProfile::symmetric(jittery),
        ..Default::default()
    });
    let a = walker(&mut h, "alice", |t| Move {
        x: if (t.0 / 60) % 2 == 0 { 1 } else { -1 },
        z: 0,
    });
    connect(&mut h, a);
    h.step_ticks(180);
    let missing0 = h.server_net().room_stats().inputs_missing;
    let mut mis_r = EventReader::default();
    let _ = read::<Mispredicted>(h.client(a), &mut mis_r);
    let mut worst = 0.0f64;
    let mut mispredicted = 0;
    for _ in 0..600 {
        h.step();
        let est = h.client_net(a).server_tick_estimate().unwrap();
        worst = worst.max((est - f64::from(h.server_net().tick().0)).abs());
        mispredicted += read::<Mispredicted>(h.client(a), &mut mis_r).len();
    }
    assert!(worst <= 2.0, "server tick estimate off by {worst:.2} ticks");
    let missing = h.server_net().room_stats().inputs_missing - missing0;
    assert!(missing <= 6, "{missing} inputs missing in 600 ticks");
    assert_eq!(mispredicted, 0);
    let stats = h.client_net(a).stats();
    assert!((0.95..=1.05).contains(&stats.tick_scale), "{stats:?}");
}

// --- Property test: random world edits replicate exactly ----------------------------------

#[derive(Clone, Debug)]
enum Op {
    Spawn { x: i8, hp: u16, owned: bool },
    SetHealth { which: u8, hp: u16 },
    Move { which: u8, dx: i8 },
    InsertName { which: u8 },
    RemoveName { which: u8 },
    SetParent { which: u8, parent: u8 },
    ClearParent { which: u8 },
    Despawn { which: u8 },
    Wait { ticks: u8 },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (any::<i8>(), any::<u16>(), any::<bool>()).prop_map(|(x, hp, owned)| Op::Spawn { x, hp, owned }),
        3 => (any::<u8>(), any::<u16>()).prop_map(|(which, hp)| Op::SetHealth { which, hp }),
        2 => (any::<u8>(), any::<i8>()).prop_map(|(which, dx)| Op::Move { which, dx }),
        1 => any::<u8>().prop_map(|which| Op::InsertName { which }),
        1 => any::<u8>().prop_map(|which| Op::RemoveName { which }),
        1 => (any::<u8>(), any::<u8>()).prop_map(|(which, parent)| Op::SetParent { which, parent }),
        1 => any::<u8>().prop_map(|which| Op::ClearParent { which }),
        2 => any::<u8>().prop_map(|which| Op::Despawn { which }),
        2 => (1u8..8).prop_map(|ticks| Op::Wait { ticks }),
    ]
}

fn apply_op(h: &mut NetHarness, op: &Op, other: PlayerId) {
    let mut crates: Vec<Entity> = h
        .server()
        .world
        .query::<(Entity, &Health)>()
        .without::<&space3d_replication::ControlledBy>()
        .iter()
        .map(|(e, _)| e)
        .collect();
    crates.sort();
    let pick = |i: u8| (!crates.is_empty()).then(|| crates[usize::from(i) % crates.len()]);
    let world = &mut h.server_mut().world;
    match *op {
        Op::Spawn { x, hp, owned } => {
            let e = world.spawn((Replicated, Transform::from_xyz(f32::from(x), 0.0, 0.0), Health(hp)));
            if owned {
                // Owner-only state of another player must never reach this client.
                world.insert(e, (Owner(other), Ammo(hp))).unwrap();
            }
        }
        Op::SetHealth { which, hp } => {
            if let Some(e) = pick(which) {
                world.get::<&mut Health>(e).unwrap().0 = hp;
            }
        }
        Op::Move { which, dx } => {
            if let Some(e) = pick(which) {
                world.get::<&mut Transform>(e).unwrap().translation.x += f32::from(dx) * 0.37;
            }
        }
        Op::InsertName { which } => {
            if let Some(e) = pick(which) {
                let _ = world.insert_one(e, Name(format!("crate-{which}")));
            }
        }
        Op::RemoveName { which } => {
            if let Some(e) = pick(which) {
                let _ = world.remove_one::<Name>(e);
            }
        }
        Op::SetParent { which, parent } => {
            if let (Some(e), Some(p)) = (pick(which), pick(parent))
                && e != p
            {
                let _ = world.insert_one(e, Parent(p));
            }
        }
        Op::ClearParent { which } => {
            if let Some(e) = pick(which) {
                let _ = world.remove_one::<Parent>(e);
            }
        }
        Op::Despawn { which } => {
            if let Some(e) = pick(which) {
                // Children lose their parent first (no dangling references on the server).
                let children: Vec<Entity> = world
                    .query::<(Entity, &Parent)>()
                    .iter()
                    .filter(|(_, p)| p.0 == e)
                    .map(|(c, _)| c)
                    .collect();
                for c in children {
                    let _ = world.remove_one::<Parent>(c);
                }
                let _ = world.despawn(e);
            }
        }
        Op::Wait { .. } => {}
    }
    let ticks = match op {
        Op::Wait { ticks } => u64::from(*ticks),
        _ => 1,
    };
    h.step_ticks(ticks);
}

fn replay_ops(ops: &[Op], semantics: Semantics, lossy: bool, seed: u64) -> Result<(), String> {
    let conditions = if lossy {
        LinkConditions::bad_wifi()
    } else {
        LinkConditions::lan()
    };
    let mut h = NetHarness::new(
        HarnessConfig {
            seed,
            semantics,
            link: LinkProfile::symmetric(conditions),
            ..Default::default()
        },
        |app| {
            app.add_plugin(TestProtocol).add_plugin(TestRules);
        },
    );
    let a = observer(&mut h, "alice");
    if !h.run_until(900, |h| h.client_net(a).is_connected()) {
        return Err(format!("never connected: {:?}", h.client_net(a).state()));
    }
    let other = PlayerIdentity::dev("mallory").player;
    for op in ops {
        apply_op(&mut h, op, other);
    }
    if h.run_until(1200, |h| h.assert_converged(a).is_ok()) {
        Ok(())
    } else {
        Err(format!("{:?}", h.assert_converged(a)))
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    /// Any sequence of spawns, inserts, removes, reparenting, value changes and despawns ends
    /// with the client holding exactly the server's replicated state (byte-identical encodings,
    /// owner-only components never leaked), over streams and datagrams, with or without loss.
    #[test]
    fn prop_random_world_edits_replicate_exactly(
        ops in proptest::collection::vec(op_strategy(), 1..48),
        datagram in any::<bool>(),
        lossy in any::<bool>(),
        seed in 0u64..1000,
    ) {
        let semantics = if datagram { Semantics::Datagram } else { Semantics::Stream };
        let result = replay_ops(&ops, semantics, lossy, seed);
        prop_assert!(result.is_ok(), "{:?} ({semantics:?}, lossy {lossy})", result);
    }
}
