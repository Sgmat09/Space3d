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

/// Replay correctness: with changing inputs and a server-side impulse, every tick the client
/// predicts after the correction equals the server's authoritative state for that tick
/// (bit-for-bit after quantization), and so does every tick before the impulse.
#[test]
fn replayed_prediction_matches_the_server_timeline_exactly() {
    let mut config = HarnessConfig {
        seed: 43,
        link: link(40),
        ..Default::default()
    };
    // No visual smoothing: the rendered transform is the simulated one.
    config.client.prediction.smoothing_half_life = Duration::ZERO;
    let mut h = with_rules(config);
    let a = walker(&mut h, "alice", |t| Move {
        x: [1, 1, 0, -1, -1, 0, 1][(t.0 / 7 % 7) as usize],
        z: [0, 1, 1, 0, -1, -1][(t.0 / 11 % 6) as usize],
    });
    connect(&mut h, a);
    h.step_ticks(60);
    let server_pawn = pawn(h.server(), h.player(a)).unwrap();
    let mut client_pos = std::collections::BTreeMap::new();
    let mut server_pos = std::collections::BTreeMap::new();
    let mut mis_r = EventReader::default();
    let _ = read::<Mispredicted>(h.client(a), &mut mis_r);
    let mut impulse_tick = None;
    let mut corrected_after = None;
    for i in 0..240 {
        if i == 60 {
            h.server_mut()
                .world
                .get::<&mut Transform>(server_pawn)
                .unwrap()
                .translation
                .z += 3.0;
            impulse_tick = Some(h.server_net().tick().0 + 1);
        }
        h.step();
        let events = read::<Mispredicted>(h.client(a), &mut mis_r);
        let predicted = h.client_net(a).predicted_tick().unwrap().0 - 1;
        if !events.is_empty() {
            assert!(events.iter().all(|m| m.replayed > 0), "{events:?}");
            corrected_after = Some(predicted);
        }
        let ce = controlled(h.client(a)).unwrap();
        client_pos.insert(predicted, h.client(a).world.get::<&Transform>(ce).unwrap().translation);
        server_pos.insert(
            h.server_net().tick().0,
            h.server().world.get::<&Transform>(server_pawn).unwrap().translation,
        );
    }
    let (impulse, corrected) = (impulse_tick.unwrap(), corrected_after.expect("no rollback"));
    let mut compared = 0;
    for (t, c) in &client_pos {
        let Some(s) = server_pos.get(t) else { continue };
        if *t < impulse || *t > corrected {
            assert!((*c - *s).abs().max_element() < 1e-6, "tick {t}: client {c} server {s}");
            compared += 1;
        }
    }
    assert!(compared > 150, "compared {compared} ticks");
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

/// Interpolation timing: at every client frame (144 Hz, phase-shifted against the 60 Hz server,
/// 30 Hz snapshots), a remote pawn moving at constant speed is drawn exactly where the server
/// had it at the client's render tick; the render tick advances smoothly and never backwards.
#[test]
fn remote_pawn_is_drawn_at_the_server_position_of_the_render_tick() {
    let mut h = with_rules(HarnessConfig {
        seed: 40,
        link: LinkProfile::symmetric(LinkConditions {
            latency: Duration::from_millis(30),
            jitter: Duration::from_millis(4),
            ..LinkConditions::perfect()
        }),
        ..Default::default()
    });
    let mover = walker(&mut h, "mover", |_| Move { x: 1, z: 0 });
    let viewer = h.add_client(
        ClientSpec::named("viewer")
            .with_frame_dt(Duration::from_secs_f64(1.0 / 144.0))
            .with_phase(Duration::from_millis(3)),
        |app| {
            app.add_plugin(TestProtocol);
        },
    );
    connect(&mut h, mover);
    connect(&mut h, viewer);
    h.step_ticks(120);
    let server_pawn = pawn(h.server(), h.player(mover)).unwrap();
    // Server x per tick (state after the tick).
    let mut history: Vec<(u32, f32)> = Vec::new();
    let mut last_render = f64::MIN;
    let mut worst = 0.0f32;
    for _ in 0..300 {
        h.step();
        let tick = h.server_net().tick().0;
        let x = h.server().world.get::<&Transform>(server_pawn).unwrap().translation.x;
        history.push((tick, x));
        let render = h.client_net(viewer).render_tick().unwrap();
        assert!(render >= last_render, "render tick went backwards");
        last_render = render;
        let (Some(a), Some(b)) = (
            history.iter().rev().find(|(t, _)| f64::from(*t) <= render),
            history.iter().find(|(t, _)| f64::from(*t) > render),
        ) else {
            continue;
        };
        let expected = a.1 + (b.1 - a.1) * ((render - f64::from(a.0)) / f64::from(b.0 - a.0)) as f32;
        let remote = h.client_entity(viewer, server_pawn).unwrap();
        let drawn = h.client(viewer).world.get::<&Transform>(remote).unwrap().translation.x;
        worst = worst.max((drawn - expected).abs());
    }
    // 1 mm position quantization; no extrapolation or stutter on a clean link.
    assert!(worst < 0.002, "drawn position off by {worst} m");
    // The render delay stays within its bounds (35–250 ms) and above one send interval.
    let delay = h.client_net(viewer).stats().interp_delay_ticks;
    assert!((2.0..=15.0).contains(&delay), "interp delay {delay} ticks");
}

/// Move → stop → teleport → rest: the remote copy never slides before the move starts, never
/// overshoots a stop by more than a snapshot interval of motion, and settles exactly (no
/// extrapolation into places the entity never went) once it rests.
#[test]
fn remote_entity_settles_after_stops_and_teleports() {
    let mut h = with_rules(HarnessConfig {
        seed: 44,
        link: link(30),
        ..Default::default()
    });
    let probe = h
        .server_mut()
        .world
        .spawn((Replicated, Transform::from_xyz(0.0, 0.0, 5.0), Health(1)));
    let a = observer(&mut h, "alice");
    connect(&mut h, a);
    h.step_ticks(60);
    let set_x = |h: &mut NetHarness, x: f32| {
        h.server_mut().world.get::<&mut Transform>(probe).unwrap().translation.x = x;
    };
    let drawn = |h: &NetHarness| {
        let e = h.client_entity(a, probe).unwrap();
        h.client(a).world.get::<&Transform>(e).unwrap().translation
    };
    // Rest, then move at 6 m/s for 1 s, then stop at 6 m.
    for _ in 0..30 {
        h.step();
        assert_eq!(drawn(&h).x, 0.0, "slid while at rest");
    }
    let mut x = 0.0;
    for _ in 0..60 {
        x += 0.1;
        set_x(&mut h, x);
        h.step();
        assert!(drawn(&h).x <= x + 1e-3);
    }
    let stop = x;
    let mut peak = 0.0f32;
    for _ in 0..60 {
        h.step();
        peak = peak.max(drawn(&h).x);
    }
    assert!(peak <= stop + 0.25, "overshot the stop: {peak} > {stop}");
    assert!((drawn(&h).x - stop).abs() < 1e-3, "did not settle: {}", drawn(&h).x);
    // Teleport far away and rest there.
    set_x(&mut h, -15.0);
    let mut lowest = f32::MAX;
    for _ in 0..60 {
        h.step();
        lowest = lowest.min(drawn(&h).x);
    }
    assert!(
        lowest >= -15.0 - 1e-3,
        "extrapolated past the teleport target: {lowest}"
    );
    assert!((drawn(&h).x + 15.0).abs() < 1e-3);
    converged(&mut h, a, 30);
}

/// A 500 ms client hitch: the client falls behind the server, resyncs its predicted tick and
/// is back to exact prediction within a second.
#[test]
fn client_stall_resyncs_and_recovers() {
    let mut h = with_rules(HarnessConfig {
        seed: 41,
        link: link(30),
        ..Default::default()
    });
    let a = walker(&mut h, "alice", |t| Move {
        x: if (t.0 / 50).is_multiple_of(2) { 1 } else { -1 },
        z: 1,
    });
    connect(&mut h, a);
    h.step_ticks(120);
    let missing0 = h.server_net().room_stats().inputs_missing;
    let mut mis_r = EventReader::default();
    let _ = read::<Mispredicted>(h.client(a), &mut mis_r);
    h.stall(a, Duration::from_millis(500));
    let mut during = Vec::new();
    for _ in 0..90 {
        h.step();
        during.extend(read::<Mispredicted>(h.client(a), &mut mis_r));
    }
    assert!(h.client_net(a).is_connected());
    assert!(
        h.server_net().room_stats().inputs_missing > missing0,
        "the server should have run short of inputs during the stall"
    );
    let mut after = 0;
    for _ in 0..120 {
        h.step();
        after += read::<Mispredicted>(h.client(a), &mut mis_r).len();
    }
    assert_eq!(after, 0, "still mispredicting after the stall (during: {during:?})");
    let lead = i64::from(h.client_net(a).predicted_tick().unwrap().0) - i64::from(h.server_net().tick().0);
    assert!((2..=12).contains(&lead), "prediction lead {lead}");
    converged(&mut h, a, 60);
}

/// Snapshots queued behind a downstream stall arrive together, all disagreeing with the
/// prediction (the server moved the pawn during the stall). The client keeps only the newest
/// confirmation, so it replays from that tick exactly once: no snap, no second correction, and
/// every tick it predicts afterwards equals the server's.
#[test]
fn snapshot_burst_after_a_stall_replays_from_the_newest_confirmation() {
    let mut config = HarnessConfig {
        seed: 47,
        link: link(30),
        ..Default::default()
    };
    config.client.prediction.smoothing_half_life = Duration::ZERO;
    let mut h = with_rules(config);
    let a = walker(&mut h, "alice", |t| Move {
        x: [1, 0, -1, 0][(t.0 / 20 % 4) as usize],
        z: 1,
    });
    connect(&mut h, a);
    h.step_ticks(120);
    let server_pawn = pawn(h.server(), h.player(a)).unwrap();
    let mut mis_r = EventReader::default();
    let _ = read::<Mispredicted>(h.client(a), &mut mis_r);
    // ~20 ticks of snapshots pile up behind a 330 ms downstream stall.
    h.set_link(
        a,
        LinkProfile {
            down: LinkConditions {
                latency: Duration::from_millis(330),
                ..LinkConditions::perfect()
            },
            ..link(30)
        },
    );
    let mut client_pos = std::collections::BTreeMap::new();
    let mut server_pos = std::collections::BTreeMap::new();
    let mut record = |h: &NetHarness| {
        let predicted = h.client_net(a).predicted_tick().unwrap().0 - 1;
        let ce = controlled(h.client(a)).unwrap();
        client_pos.insert(predicted, h.client(a).world.get::<&Transform>(ce).unwrap().translation);
        server_pos.insert(
            h.server_net().tick().0,
            h.server().world.get::<&Transform>(server_pawn).unwrap().translation,
        );
    };
    for i in 0..3 {
        if i == 1 {
            h.server_mut()
                .world
                .get::<&mut Transform>(server_pawn)
                .unwrap()
                .translation
                .x += 2.0;
        }
        h.step();
        record(&h);
    }
    h.set_link(a, link(30));
    let mut events = Vec::new();
    let mut first_fix = None;
    for _ in 0..180 {
        h.step();
        let got = read::<Mispredicted>(h.client(a), &mut mis_r);
        if !got.is_empty() && first_fix.is_none() {
            first_fix = Some(h.client_net(a).predicted_tick().unwrap().0 - 1);
        }
        events.extend(got);
        record(&h);
    }
    assert!(!events.is_empty(), "the teleport was corrected");
    assert!(events.iter().all(|m| m.replayed > 0), "no snap: {events:?}");
    assert_eq!(
        events.len(),
        1,
        "one correction from the newest confirmation: {events:?}"
    );
    let fixed = first_fix.unwrap();
    let mut compared = 0;
    for (t, c) in client_pos.range(fixed..) {
        let Some(s) = server_pos.get(t) else { continue };
        assert!((*c - *s).abs().max_element() < 1e-6, "tick {t}: client {c} server {s}");
        compared += 1;
    }
    assert!(compared > 100, "compared {compared} ticks");
}

/// CI smoke load: 50 walking bots in one room for 10 s on broadband links. Everyone stays
/// connected and converged, every client's downstream stays within its budget and inputs
/// arrive on time. (Bytes and counts, not timing.)
#[test]
fn fifty_bots_in_one_room() {
    let mut h = with_rules(HarnessConfig {
        seed: 42,
        link: LinkProfile::symmetric(LinkConditions::broadband()),
        server: space3d_replication::server::ServerNetConfig {
            max_clients: 64,
            ..Default::default()
        },
        ..Default::default()
    });
    let clients: Vec<ClientHandle> = (0..50u32)
        .map(|i| {
            h.add_client(
                ClientSpec::named(&format!("bot-{i}")).with_phase(Duration::from_micros(u64::from(i) * 331)),
                move |app| {
                    app.add_plugin(TestProtocol);
                    install_bot(
                        app,
                        scripted(move |t: Tick| Move {
                            x: if ((t.0 + i * 7) / 60).is_multiple_of(2) { 1 } else { -1 },
                            z: if ((t.0 + i * 13) / 90).is_multiple_of(2) { 1 } else { -1 },
                        }),
                    );
                },
            )
        })
        .collect();
    assert!(
        h.run_until(900, |h| clients.iter().all(|c| h.client_net(*c).is_connected())),
        "not everyone connected"
    );
    h.step_ticks(120);
    let bytes0: Vec<u64> = clients.iter().map(|c| h.client_net(*c).stats().bytes_in).collect();
    let stats0 = h.server_net().room_stats().clone();
    h.step_ticks(600);
    let stats = h.server_net().room_stats().clone();
    let budget = f64::from(space3d_replication::server::ServerNetConfig::default().client_bandwidth);
    for (i, c) in clients.iter().enumerate() {
        let rate = (h.client_net(*c).stats().bytes_in - bytes0[i]) as f64 / 10.0;
        assert!(rate <= budget * 1.05, "client {i}: {rate:.0} B/s");
        assert!(h.client_net(*c).is_connected());
    }
    // Broadband has 0.1 % loss: on a stream each lost packet is a ≥ 200 ms retransmission stall
    // (head-of-line blocking), i.e. ~12 missing inputs; ~30 such stalls are expected here.
    let ticks = 600 * 50;
    let missing = stats.inputs_missing - stats0.inputs_missing;
    assert!(missing * 50 <= ticks, "{missing} of {ticks} inputs missing");
    assert_eq!(stats.kicks, 0);
    assert_eq!(h.server_net().connected_count(), 50);
    for c in &clients {
        converged(&mut h, *c, 300);
    }
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
