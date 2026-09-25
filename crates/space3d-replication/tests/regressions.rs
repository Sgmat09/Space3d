//! Regression scenarios from the netcode / spec reviews: parked entity references, rate-limit /
//! strike / slow-consumer kicks and schema rejects on a real room `App`, the byte budget
//! (owned entities, entities larger than the burst, budget-deferred interpolation), datagram
//! RTT and reordering, oversized records, visibility-override cleanup, and the debug checks for
//! plugin order and sim discipline.

mod common;

use common::*;
use space3d_core::prelude::*;
use space3d_replication::harness::{ClientHandle, ClientSpec, HarnessConfig, NetHarness};
use space3d_replication::mapping::{DecodeCtx, EncodeCtx};
use space3d_replication::prelude::*;
use space3d_replication::protocol::handshake::{ClientCaps, DisconnectCode, Hello, HelloAuth, RejectCode};
use space3d_replication::protocol::{
    ChannelId, DataWriter, MAGIC, PROTOCOL_VERSION, Packet, WireLimits, decode_packet,
};
use space3d_replication::server::{JoinRequest, RoomInbound, RoomSender, SlotState, Visibility, room_channel};
use space3d_replication::time::{ManualClock, NetClockHandle};
use space3d_replication::transport::conditioner::{LinkConditions, LinkProfile, Semantics};
use space3d_replication::transport::{CloseCode, Lane, LinkTx, PeerInfo, TransportCaps, TransportError};
use space3d_replication::wire::Writer;
use space3d_replication::{LinkId, Owner, ReplicationRegistry};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---- helpers --------------------------------------------------------------------------------

fn with_rules(config: HarnessConfig) -> NetHarness {
    NetHarness::new(config, |app| {
        app.add_plugin(TestProtocol).add_plugin(TestRules);
    })
}

fn observer(h: &mut NetHarness, name: &str) -> ClientHandle {
    h.add_client(ClientSpec::named(name), |app| {
        app.add_plugin(TestProtocol);
    })
}

fn perfect(latency_ms: u64) -> LinkProfile {
    LinkProfile::symmetric(LinkConditions {
        latency: Duration::from_millis(latency_ms),
        ..LinkConditions::perfect()
    })
}

/// A moving NPC (server-only system moves it every tick).
struct Mover;

fn move_movers(ctx: &mut SystemCtx) {
    for (t, _) in ctx.world.query_mut::<(&mut Transform, &Mover)>() {
        t.translation.x += 0.05;
    }
}

// ---- 1. parked entity references --------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
struct Aim(Entity);

fn aim_to_wire(a: &Aim, ctx: &EncodeCtx<'_>) -> u32 {
    ctx.net_id(a.0).map_or(0, |n| n.0)
}

fn aim_from_wire(id: u32, ctx: &mut DecodeCtx<'_>) -> Option<Aim> {
    ctx.entity(NetId(id)).map(Aim)
}

struct AimProtocol;

impl Plugin for AimProtocol {
    fn build(&self, app: &mut App) {
        app.add_plugin(ReplicationPlugin::default())
            .replicate_with::<Aim>("test.aim", ComponentRule::mapped(aim_to_wire, aim_from_wire));
    }
}

fn client_aim(h: &NetHarness, c: ClientHandle, holder: Entity) -> Option<Entity> {
    let ce = h.client_entity(c, holder)?;
    h.client(c).world.get::<&Aim>(ce).ok().map(|a| a.0)
}

/// `(harness, client, holder, visible target y, hidden target x)`; `Aim(x)` is parked on the
/// client because x is hidden from it.
fn parked_aim() -> (NetHarness, ClientHandle, Entity, Entity, Entity) {
    let mut h = NetHarness::new(HarnessConfig::default(), |app| {
        app.add_plugin(AimProtocol);
    });
    let a = h.add_client(ClientSpec::named("alice"), |app| {
        app.add_plugin(AimProtocol);
    });
    assert!(h.run_until(600, |h| h.client_net(a).is_connected()));
    let alice = h.player(a);
    let (holder, y, x) = {
        let w = &mut h.server_mut().world;
        (
            w.spawn((Replicated, Transform::default())),
            w.spawn((Replicated, Transform::from_xyz(1.0, 0.0, 0.0))),
            w.spawn((Replicated, Transform::from_xyz(2.0, 0.0, 0.0))),
        )
    };
    h.server_mut()
        .resources
        .expect_mut::<NetServer>()
        .set_visibility(alice, x, Visibility::Hidden);
    h.step_ticks(20);
    assert!(h.client_entity(a, holder).is_some() && h.client_entity(a, y).is_some());
    assert!(h.client_entity(a, x).is_none(), "x is hidden");
    h.server_mut().world.insert_one(holder, Aim(x)).unwrap();
    h.step_ticks(10);
    assert_eq!(client_aim(&h, a, holder), None, "Aim(x) is parked (x unknown)");
    (h, a, holder, y, x)
}

fn reveal(h: &mut NetHarness, c: ClientHandle, x: Entity) {
    let player = h.player(c);
    h.server_mut()
        .resources
        .expect_mut::<NetServer>()
        .set_visibility(player, x, Visibility::Default);
    h.step_ticks(20);
    assert!(h.client_entity(c, x).is_some(), "x replicated once visible");
}

/// A value parked for an unresolved reference must never overwrite a newer value applied
/// directly, even when the old target shows up within the pending window.
#[test]
fn parked_reference_never_overwrites_a_newer_value() {
    let (mut h, a, holder, y, x) = parked_aim();
    *h.server_mut().world.get::<&mut Aim>(holder).unwrap() = Aim(y);
    h.step_ticks(10);
    let cy = h.client_entity(a, y).unwrap();
    assert_eq!(client_aim(&h, a, holder), Some(cy), "newer value applied");
    reveal(&mut h, a, x);
    h.step_ticks(120);
    assert_eq!(
        client_aim(&h, a, holder),
        Some(cy),
        "stale parked Aim(x) was re-applied"
    );
    assert!(h.assert_converged(a).is_ok(), "{:?}", h.assert_converged(a));
}

/// Removal variant: a parked value must not re-insert a component the server removed.
#[test]
fn parked_reference_never_resurrects_a_removed_component() {
    let (mut h, a, holder, _y, x) = parked_aim();
    h.server_mut().world.remove_one::<Aim>(holder).unwrap();
    h.step_ticks(10);
    reveal(&mut h, a, x);
    h.step_ticks(60);
    assert_eq!(client_aim(&h, a, holder), None, "removed component came back");
    assert!(h.assert_converged(a).is_ok(), "{:?}", h.assert_converged(a));
}

/// The parked value itself still resolves once its target arrives.
#[test]
fn parked_reference_resolves_when_the_target_arrives() {
    let (mut h, a, holder, _y, x) = parked_aim();
    reveal(&mut h, a, x);
    let cx = h.client_entity(a, x).unwrap();
    assert_eq!(client_aim(&h, a, holder), Some(cx));
}

// ---- 2. room-level enforcement (fake link) ---------------------------------------------------

#[derive(Default)]
struct FakeTx {
    sent: Mutex<Vec<Vec<u8>>>,
    closed: Mutex<Option<(u16, String)>>,
    backlog: AtomicUsize,
}

impl LinkTx for FakeTx {
    fn caps(&self) -> TransportCaps {
        TransportCaps::stream(1 << 20)
    }
    fn send(&self, _lane: Lane, packet: Vec<u8>) -> Result<(), TransportError> {
        self.sent.lock().unwrap().push(packet);
        Ok(())
    }
    fn backlog_bytes(&self) -> usize {
        self.backlog.load(Ordering::Relaxed)
    }
    fn close(&self, code: CloseCode, reason: &str) {
        let mut c = self.closed.lock().unwrap();
        if c.is_none() {
            *c = Some((code.0, reason.to_owned()));
        }
    }
    fn is_open(&self) -> bool {
        self.closed.lock().unwrap().is_none()
    }
}

impl FakeTx {
    fn packets(&self) -> Vec<Packet<'static>> {
        let limits = WireLimits {
            max_packet: 1 << 20,
            ..WireLimits::default()
        };
        self.sent
            .lock()
            .unwrap()
            .iter()
            .filter_map(|p| {
                let p: &'static [u8] = Box::leak(p.clone().into_boxed_slice());
                decode_packet(p, &limits).ok()
            })
            .collect()
    }

    fn disconnect(&self) -> Option<(DisconnectCode, bool)> {
        self.packets().into_iter().find_map(|p| match p {
            Packet::Disconnect(d) => Some((d.code, d.resume_allowed)),
            _ => None,
        })
    }

    fn reject(&self) -> Option<RejectCode> {
        self.packets().into_iter().find_map(|p| match p {
            Packet::Reject(r) => Some(r.code),
            _ => None,
        })
    }

    fn is_closed(&self) -> bool {
        self.closed.lock().unwrap().is_some()
    }
}

struct Room {
    app: App,
    clock: ManualClock,
    tx: RoomSender,
    step: Duration,
}

impl Room {
    fn new() -> Self {
        let clock = ManualClock::new();
        let mut app = App::new().with_tickrate(60.0);
        app.insert_resource(NetClockHandle(Arc::new(clock.clone())));
        let (tx, inbox) = room_channel(1 << 16);
        app.insert_resource(inbox);
        app.add_plugin(ServerReplicationPlugin::default());
        app.add_plugin(TestProtocol).add_plugin(TestRules);
        let mut r = Room {
            app,
            clock,
            tx,
            step: Duration::from_secs_f64(1.0 / 60.0),
        };
        r.tick();
        r
    }

    fn tick(&mut self) {
        self.clock.advance(self.step);
        self.app.update(self.step);
    }

    fn net(&self) -> &NetServer {
        self.app.resources.expect::<NetServer>()
    }

    fn join_with(&mut self, link: u64, name: &str, schema_hash: Option<u64>) -> Arc<FakeTx> {
        let schema = schema_hash.unwrap_or_else(|| self.app.resources.expect::<ReplicationRegistry>().schema_hash());
        let fake = Arc::new(FakeTx::default());
        let hello = Hello {
            magic: MAGIC,
            protocol: PROTOCOL_VERSION,
            game_id: "test".into(),
            game_version: "0".into(),
            schema_hash: schema,
            auth: HelloAuth::Dev { name: name.into() },
            caps: ClientCaps::default(),
            client_time_us: 0,
        };
        let req = JoinRequest {
            link: LinkId(link),
            tx: fake.clone(),
            peer: PeerInfo {
                addr: None,
                client_ip: None,
                origin: None,
                user_agent: None,
                transport: "memory",
            },
            hello,
            identity: Some(PlayerIdentity::dev(name)),
        };
        assert!(self.tx.try_send(RoomInbound::Join(Box::new(req))).is_ok());
        self.tick();
        fake
    }

    fn join(&mut self, link: u64, name: &str) -> Arc<FakeTx> {
        self.join_with(link, name, None)
    }

    fn send(&self, link: u64, bytes: Vec<u8>) {
        let _ = self.tx.try_send(RoomInbound::Packet {
            link: LinkId(link),
            bytes,
        });
    }
}

/// An empty (valid) Data packet.
fn empty_data() -> Vec<u8> {
    DataWriter::new(None, None, Tick(0)).finish()
}

/// A token bucket refills continuously, so under a flood some packets always pass; overflow is
/// judged per 1 s window, and 3 s of it kicks with `RateLimited` (no resume).
#[test]
fn sustained_packet_flood_is_kicked_as_rate_limited() {
    let mut r = Room::new();
    let fake = r.join(1, "flooder");
    let mut kicked_at = None;
    for t in 0..360 {
        // 300 packets/s: over the 90/s allowance, under the one-window hard limit.
        for _ in 0..5 {
            r.send(1, empty_data());
        }
        r.tick();
        if kicked_at.is_none() && fake.is_closed() {
            kicked_at = Some(t);
        }
    }
    let t = kicked_at.expect("a sustained flood must be kicked");
    assert!((170..=260).contains(&t), "kicked after {t} ticks (expected ≈ 3 s)");
    assert_eq!(fake.disconnect(), Some((DisconnectCode::RateLimited, false)));
    let stats = r.net().room_stats();
    assert!(stats.rate_limited_packets > 0);
    assert_eq!(stats.disconnects_with(DisconnectCode::RateLimited), 1);
    assert!(r.net().clients().is_empty(), "slot freed");
}

/// Ten seconds' worth of the allowance dropped within one window kicks at once.
#[test]
fn extreme_flood_is_kicked_within_a_second() {
    let mut r = Room::new();
    let fake = r.join(1, "flooder");
    for _ in 0..3 {
        for _ in 0..2_000 {
            r.send(1, empty_data());
        }
        r.tick();
    }
    assert!(fake.is_closed());
    assert_eq!(fake.disconnect(), Some((DisconnectCode::RateLimited, false)));
}

/// Control: a client sending a packet every tick (plus a periodic burst) is never kicked.
#[test]
fn honest_traffic_is_never_rate_limited() {
    let mut r = Room::new();
    let fake = r.join(1, "honest");
    for t in 0..900 {
        r.send(1, empty_data());
        if t % 120 == 0 {
            // A queued burst after a hiccup.
            for _ in 0..20 {
                r.send(1, empty_data());
            }
        }
        r.tick();
    }
    assert!(!fake.is_closed(), "{:?}", fake.closed.lock().unwrap());
    assert_eq!(r.net().room_stats().rate_limited_packets, 0);
}

/// Message-rate drops count toward the kick like packet overflow.
#[test]
fn sustained_message_flood_is_kicked_as_rate_limited() {
    let mut r = Room::new();
    let fake = r.join(1, "spammer");
    let (kind, _) = r
        .app
        .resources
        .expect::<ReplicationRegistry>()
        .message_kind::<Say>()
        .expect("registered");
    let body = postcard::to_stdvec(&Say("hi".into())).unwrap();
    let mut payload = Writer::new();
    payload.varu16(kind);
    payload.raw(&body);
    let payload = payload.into_vec();
    for _ in 0..360 {
        // One packet per tick (within the packet limit), 10 messages each (over 5/s).
        let mut dw = DataWriter::new(None, None, Tick(0));
        for _ in 0..10 {
            dw.block_bytes(ChannelId::MsgReliable, &payload);
        }
        r.send(1, dw.finish());
        r.tick();
        if fake.is_closed() {
            break;
        }
    }
    assert!(fake.is_closed(), "message flood not kicked");
    assert_eq!(fake.disconnect(), Some((DisconnectCode::RateLimited, false)));
    assert!(r.net().room_stats().rate_limited_messages > 0);
}

/// `strikes.max` violations within the window → `Disconnect(ProtocolViolation)`.
#[test]
fn protocol_strikes_kick_with_protocol_violation() {
    let mut r = Room::new();
    let fake = r.join(1, "garbler");
    for _ in 0..10 {
        // A Data packet with an unknown channel byte.
        r.send(1, vec![0x04, 0x00, 0x09, 0xFF]);
        r.tick();
    }
    assert!(fake.is_closed());
    assert_eq!(fake.disconnect(), Some((DisconnectCode::ProtocolViolation, false)));
    let stats = r.net().room_stats();
    assert!(stats.strikes >= 5);
    assert_eq!(stats.disconnects_with(DisconnectCode::ProtocolViolation), 1);
}

/// A Hello with another schema hash is refused with `Reject(SchemaMismatch)`.
#[test]
fn wrong_schema_hash_is_rejected() {
    let mut r = Room::new();
    let fake = r.join_with(1, "old-build", Some(0xdead_beef));
    assert_eq!(fake.reject(), Some(RejectCode::SchemaMismatch));
    assert!(fake.is_closed());
    assert!(r.net().clients().is_empty());
    assert_eq!(r.net().room_stats().rejects, 1);
}

/// A downstream backlog above `backlog_kick_bytes` for 5 s → `Disconnect(SlowConsumer)`
/// (resumable); below it nothing happens.
#[test]
fn sustained_backlog_kicks_a_slow_consumer() {
    let mut r = Room::new();
    let fake = r.join(1, "slow");
    let kick = r.net().config().backlog_kick_bytes;
    // Keep the link alive (idle timeout) while the backlog is high.
    let run = |r: &mut Room, ticks: u32| {
        for _ in 0..ticks {
            r.send(1, empty_data());
            r.tick();
        }
    };
    fake.backlog.store(kick - 1, Ordering::Relaxed);
    run(&mut r, 400);
    assert!(!fake.is_closed(), "below the kick threshold");
    fake.backlog.store(kick + 1, Ordering::Relaxed);
    run(&mut r, 280);
    assert!(!fake.is_closed(), "not yet 5 s");
    run(&mut r, 60);
    assert!(fake.is_closed(), "5 s above the threshold");
    assert_eq!(fake.disconnect(), Some((DisconnectCode::SlowConsumer, true)));
    let net = r.net();
    assert!(
        matches!(net.clients()[0].state, SlotState::Disconnected { .. }),
        "resumable"
    );
    assert_eq!(net.room_stats().disconnects_with(DisconnectCode::SlowConsumer), 1);
}

// ---- 3. byte budget -------------------------------------------------------------------------

/// Owned (not controlled) entities compete for the budget: a client owning hundreds of movers
/// stays near its budget and still receives everything else.
#[test]
fn owned_entities_share_the_budget() {
    const CAP: u32 = 12 * 1024;
    let mut config = HarnessConfig::default();
    config.server.client_bandwidth = CAP;
    let mut h = NetHarness::new(config, |app| {
        app.add_plugin(TestProtocol).add_plugin(TestRules);
        app.add_system(Stage::FixedUpdate, move_movers);
    });
    let a = observer(&mut h, "alice");
    assert!(h.run_until(900, |h| h.client_net(a).is_connected()));
    let player = h.player(a);
    for i in 0..400 {
        h.server_mut().world.spawn((
            Replicated,
            Mover,
            Owner(player),
            Transform::from_xyz(0.0, 0.0, i as f32),
        ));
    }
    let others: Vec<Entity> = (0..20)
        .map(|i| {
            h.server_mut()
                .world
                .spawn((Replicated, Mover, Transform::from_xyz(0.0, 1.0, i as f32)))
        })
        .collect();
    h.step_ticks(300);
    let bytes0 = h.client_net(a).stats().bytes_in;
    h.step_ticks(300);
    let rate = (h.client_net(a).stats().bytes_in - bytes0) as f64 / 5.0;
    assert!(rate <= f64::from(CAP) * 1.1, "{rate:.0} B/s for a {CAP} B/s budget");
    let known = others.iter().filter(|e| h.client_entity(a, **e).is_some()).count();
    assert_eq!(known, others.len(), "non-owned entities starved");
}

fn big_entity_delivered_after(name_len: usize, keep_pawn: bool) -> Option<u64> {
    let mut config = HarnessConfig {
        seed: 5,
        link: perfect(20),
        ..Default::default()
    };
    config.server.client_bandwidth = 12 * 1024;
    let mut h = with_rules(config);
    let a = observer(&mut h, "alice");
    assert!(h.run_until(900, |h| h.client_net(a).is_connected()));
    h.step_ticks(60);
    if !keep_pawn {
        let p = pawn(h.server(), h.player(a)).unwrap();
        h.server_mut().world.despawn(p).unwrap();
        h.step_ticks(30);
    }
    let big = h.server_mut().world.spawn((
        Replicated,
        Transform::from_xyz(1.0, 0.0, 1.0),
        Name("x".repeat(name_len)),
    ));
    (0..1200u64).find(|_| {
        h.step();
        h.client_entity(a, big).is_some()
    })
}

/// An entity larger than the per-snapshot budget (or the whole burst) is still delivered,
/// also while the client's own pawn is written every snapshot.
#[test]
fn entities_larger_than_the_budget_are_delivered() {
    for (len, keep_pawn) in [(800usize, true), (1000, true), (4000, true), (4000, false)] {
        let t = big_entity_delivered_after(len, keep_pawn);
        assert!(
            t.is_some_and(|t| t < 120),
            "{len} B entity (pawn: {keep_pawn}) delivered after {t:?} ticks"
        );
    }
}

/// An entity larger than the burst that exists at join time does not block the initial sync.
#[test]
fn big_entity_does_not_block_the_initial_sync() {
    let mut config = HarnessConfig::default();
    config.server.client_bandwidth = 12 * 1024;
    let mut h = NetHarness::new(config, |app| {
        app.add_plugin(TestProtocol).add_plugin(TestRules);
        app.world
            .spawn((Replicated, Transform::from_xyz(1.0, 0.0, 1.0), Name("x".repeat(4000))));
    });
    let a = observer(&mut h, "alice");
    assert!(
        h.run_until(600, |h| h.client_net(a).is_connected()),
        "state {:?}",
        h.client_net(a).state()
    );
    let slot = h.server_net().clients()[0].state;
    assert_eq!(slot, SlotState::Connected);
}

/// NPCs the budget updates only every few snapshots keep moving smoothly on the client (the
/// `DEFERRED` bit suppresses the rest → move hold, and the entity is drawn further back)
/// instead of freezing and jumping.
#[test]
fn budget_deferred_movers_move_smoothly() {
    const V: f32 = 0.05;
    let mut config = HarnessConfig {
        seed: 9,
        link: perfect(20),
        ..Default::default()
    };
    config.server.client_bandwidth = 12 * 1024;
    let mut h = NetHarness::new(config, |app| {
        app.add_plugin(TestProtocol).add_plugin(TestRules);
        for i in 0..300 {
            app.world
                .spawn((Replicated, Mover, Transform::from_xyz(0.0, 0.0, i as f32)));
        }
        app.add_system(Stage::FixedUpdate, move_movers);
    });
    let a = observer(&mut h, "alice");
    assert!(h.run_until(900, |h| h.client_net(a).is_connected()));
    h.step_ticks(300);
    let movers: Vec<Entity> = h
        .server()
        .world
        .query::<(Entity, &Mover)>()
        .iter()
        .map(|(e, _)| e)
        .take(10)
        .collect();
    let mut last: Vec<Option<f32>> = vec![None; movers.len()];
    let (mut frames, mut frozen, mut max_step) = (0u32, 0u32, 0f32);
    for _ in 0..600 {
        h.step();
        for (i, &se) in movers.iter().enumerate() {
            let Some(ce) = h.client_entity(a, se) else { continue };
            let x = h.client(a).world.get::<&Transform>(ce).unwrap().translation.x;
            if let Some(prev) = last[i] {
                frames += 1;
                let dx = (x - prev).abs();
                if dx < V * 0.1 {
                    frozen += 1;
                }
                max_step = max_step.max(dx);
            }
            last[i] = Some(x);
        }
    }
    assert!(frames > 5_000, "movers not replicated ({frames} frames)");
    let frozen_pct = f64::from(frozen) * 100.0 / f64::from(frames);
    assert!(frozen_pct < 10.0, "{frozen}/{frames} frames frozen ({frozen_pct:.0}%)");
    assert!(
        max_step < V * 4.0,
        "max per-frame step {max_step:.3} m (speed {V} m/tick)"
    );
}

// ---- 4. datagram links ------------------------------------------------------------------------

/// With no RTT sample yet the RTO is conservative, and a late ack of a presumed-lost record
/// still samples the RTT: sync completes on long paths instead of resending forever.
#[test]
fn datagram_sync_completes_on_long_paths() {
    for ms in [35u64, 60, 100] {
        let mut h = NetHarness::new(
            HarnessConfig {
                seed: 3,
                semantics: Semantics::Datagram,
                link: perfect(ms),
                ..Default::default()
            },
            |app| {
                app.add_plugin(TestProtocol).add_plugin(TestRules);
            },
        );
        let a = observer(&mut h, "alice");
        assert!(
            h.run_until(900, |h| h.client_net(a).is_connected()),
            "one-way {ms} ms: {:?}",
            h.client_net(a).state()
        );
        h.step_ticks(120);
        let c = h.client_net(a).client_id().unwrap();
        let rtt = h.server_net().stats(c).unwrap().rtt;
        let expect = Duration::from_millis(2 * ms);
        assert!(
            rtt >= expect.mul_f32(0.8) && rtt <= expect * 3,
            "one-way {ms} ms: server RTT {rtt:?}"
        );
    }
}

/// Reordered datagrams never lose an update: records are written against the acked baseline,
/// so a newer record supersedes an older one the client then skips.
#[test]
fn datagram_reordering_never_loses_updates() {
    for seed in 1..7 {
        let cond = LinkConditions {
            latency: Duration::from_millis(20),
            jitter: Duration::from_millis(12),
            reorder: 0.5,
            reorder_delay: Duration::from_millis(36),
            ..LinkConditions::perfect()
        };
        let mut h = NetHarness::new(
            HarnessConfig {
                seed,
                semantics: Semantics::Datagram,
                link: LinkProfile::symmetric(cond),
                ..Default::default()
            },
            |app| {
                app.add_plugin(TestProtocol).add_plugin(TestRules);
            },
        );
        let a = observer(&mut h, "alice");
        assert!(h.run_until(900, |h| h.client_net(a).is_connected()));
        let crates: Vec<Entity> = (0..8)
            .map(|i| {
                h.server_mut()
                    .world
                    .spawn((Replicated, Transform::from_xyz(i as f32, 0.0, 0.0), Health(1)))
            })
            .collect();
        assert!(h.run_until(600, |h| h.assert_converged(a).is_ok()));
        if h.server_net().tick().0.is_multiple_of(2) {
            h.step();
        }
        for round in 0..40u16 {
            let e = crates[usize::from(round) % crates.len()];
            // Interval k: only Health changes; interval k + 1: only Transform.
            h.server_mut().world.get::<&mut Health>(e).unwrap().0 = 1000 + round;
            h.step_ticks(2);
            h.server_mut().world.get::<&mut Transform>(e).unwrap().translation.y += 1.0;
            h.step_ticks(12);
        }
        assert!(
            h.run_until(1200, |h| h.assert_converged(a).is_ok()),
            "seed {seed}: {:?}",
            h.assert_converged(a)
        );
    }
}

// ---- 5. oversized records -------------------------------------------------------------------

/// A single entity record larger than 16 KiB goes out alone (the downstream cap is 1 MiB), and
/// the client receives it.
#[test]
fn entity_larger_than_a_stream_packet_is_delivered() {
    let mut h = with_rules(HarnessConfig::default());
    let a = observer(&mut h, "alice");
    assert!(h.run_until(900, |h| h.client_net(a).is_connected()));
    let player = h.player(a);
    let big = h
        .server_mut()
        .world
        .spawn((Replicated, Owner(player), Name("x".repeat(20_000))));
    assert!(
        h.run_until(600, |h| h.client_entity(a, big).is_some()),
        "never delivered"
    );
    assert!(h.assert_converged(a).is_ok(), "{:?}", h.assert_converged(a));
    assert_eq!(h.server_net().room_stats().oversize_dropped, 0);
}

// ---- 6. visibility overrides --------------------------------------------------------------

/// Per-player overrides are forgotten when the entity despawns (also before it was ever
/// replicated) and when the player leaves.
#[test]
fn visibility_overrides_are_forgotten() {
    let mut h = with_rules(HarnessConfig::default());
    let a = observer(&mut h, "alice");
    assert!(h.run_until(900, |h| h.client_net(a).is_connected()));
    let alice = h.player(a);
    let other = PlayerId(77);
    let shots: Vec<Entity> = (0..50)
        .map(|_| h.server_mut().world.spawn((Replicated, Transform::default())))
        .collect();
    let never_replicated = h.server_mut().world.spawn((Transform::default(),));
    {
        let net = h.server_mut().resources.expect_mut::<NetServer>();
        for &e in &shots {
            net.set_visibility(alice, e, Visibility::Hidden);
            net.set_visibility(other, e, Visibility::Visible);
        }
        net.set_visibility(alice, never_replicated, Visibility::Hidden);
    }
    h.step_ticks(10);
    assert_eq!(h.server_net().visibility_overrides(), 101);
    for &e in &shots {
        h.server_mut().world.despawn(e).unwrap();
    }
    h.server_mut().world.despawn(never_replicated).unwrap();
    h.step_ticks(4);
    assert_eq!(h.server_net().visibility_overrides(), 1, "despawned entities forgotten");
    h.step_ticks(64 * 2);
    assert_eq!(
        h.server_net().visibility_overrides(),
        0,
        "never-replicated entity swept"
    );
    // Leaving forgets the player's overrides.
    let keep = h.server_mut().world.spawn((Replicated, Transform::default()));
    h.server_mut()
        .resources
        .expect_mut::<NetServer>()
        .set_visibility(alice, keep, Visibility::Hidden);
    h.client_mut(a).resources.expect_mut::<NetClient>().disconnect();
    assert!(h.run_until(120, |h| h.server_net().clients().is_empty()));
    assert_eq!(h.server_net().visibility_overrides(), 0);
}

// ---- 7. debug checks --------------------------------------------------------------------------

/// Game systems registered in `PreUpdate`/`FixedUpdate` before a replication plugin are found.
#[test]
fn plugin_order_check_finds_early_game_systems() {
    let mut app = App::new();
    app.add_system_named(Stage::PreUpdate, "tasks::drain_completed", |_| {});
    assert!(space3d_replication::systems_before_replication(&app).is_empty());
    app.add_system_named(Stage::FixedUpdate, "game::step_npcs", |_| {});
    assert_eq!(
        space3d_replication::systems_before_replication(&app),
        vec!["game::step_npcs"]
    );
    app.add_plugin(ServerReplicationPlugin::default());
    // Systems registered after the plugin are not "before" it for the next check either way.
    assert_eq!(space3d_replication::systems_before_replication(&app).len(), 1);
}

/// Debug builds count predicted components changed outside the sim.
#[test]
fn predicted_changes_outside_the_sim_are_detected() {
    let run = |cheat: bool| {
        let mut h = with_rules(HarnessConfig::default());
        let a = h.add_client(ClientSpec::named("alice"), move |app| {
            app.add_plugin(TestProtocol);
            space3d_replication::bots::install_bot(app, space3d_replication::bots::scripted(|_| Move { x: 1, z: 0 }));
            if cheat {
                app.add_system(Stage::Update, |ctx| {
                    for t in ctx.world.query_mut::<&mut Transform>().with::<&Predicted>() {
                        t.translation.y += 0.25;
                    }
                });
            }
        });
        assert!(h.run_until(900, |h| h.client_net(a).is_connected()
            && controlled(h.client(a)).is_some()));
        h.step_ticks(120);
        h.client_net(a).sim_discipline_violations()
    };
    assert_eq!(run(false), 0, "a sim-only game has no violations");
    if cfg!(debug_assertions) {
        assert!(
            run(true) > 0,
            "Update-stage writes to a predicted Transform went unnoticed"
        );
    }
}

/// Keep the unused-helper lint quiet for helpers only some tests use.
#[allow(dead_code)]
fn _unused(p: PlayerId) -> Owner {
    _owner(p)
}

// ---- prediction: a hitch behind held snapshots ------------------------------------------------

/// Regression: after a frame hitch on a low-latency link, the client's next predicted tick was
/// behind snapshots it already held; the snap wrote the confirmed state (which includes those
/// ticks) and then simulated them again, so the pawn ran ahead of the server by the overlap.
/// With a constant input every prediction (even of a repeated input) is exact: the raw
/// predicted position at tick T must equal the server's at T.
#[test]
fn a_hitch_behind_held_snapshots_never_applies_inputs_twice() {
    for (latency_ms, hitch_ms) in [(1u64, 150u64), (1, 300), (5, 200), (20, 300)] {
        let mut h = with_rules(HarnessConfig {
            seed: 11,
            semantics: Semantics::Stream,
            link: perfect(latency_ms),
            ..Default::default()
        });
        let w = h.add_client(ClientSpec::named("walker"), |app| {
            app.add_plugin(TestProtocol);
            space3d_replication::bots::install_bot(
                app,
                space3d_replication::bots::scripted(|_t: Tick| Move { x: 1, z: 0 }),
            );
        });
        assert!(h.run_until(900, |h| h.client_net(w).is_connected()
            && controlled(h.client(w)).is_some()));
        h.step_ticks(300);
        let sp = pawn(h.server(), h.player(w)).unwrap();
        let mut server_x = std::collections::HashMap::new();
        let mut observed = Vec::new();
        for t in 0..240u32 {
            if t == 60 {
                h.stall(w, Duration::from_millis(hitch_ms));
            }
            h.step();
            let st = h.server_net().tick().0;
            server_x.insert(st, h.server().world.get::<&Transform>(sp).unwrap().translation.x);
            let Some(next) = h.client_net(w).predicted_tick() else {
                continue;
            };
            let ce = controlled(h.client(w)).unwrap();
            let x = h.client(w).world.get::<&Transform>(ce).unwrap().translation.x;
            let correction = h
                .client(w)
                .world
                .get::<&space3d_replication::client::prediction::PredictionError>(ce)
                .map_or(0.0, |p| p.translation.x);
            observed.push((next.0.saturating_sub(1), x - correction));
        }
        h.step_ticks(60);
        let mut compared = 0;
        let mut worst = (0.0f32, 0u32);
        for (tick, raw) in observed {
            if let Some(sx) = server_x.get(&tick) {
                compared += 1;
                let err = (raw - sx).abs();
                if err > worst.0 {
                    worst = (err, tick);
                }
            }
        }
        assert!(compared > 150, "{compared}");
        assert!(
            worst.0 < 1e-3,
            "latency {latency_ms} ms, hitch {hitch_ms} ms: client ran {:.3} m off the server at tick {}",
            worst.0,
            worst.1
        );
    }
}

// ---- datagram reordering: no ghosts -------------------------------------------------------------

/// Regression: on datagram links a spawn record delayed (or duplicated) past the despawn of a
/// short-lived entity recreated it on the client, and nothing ever removed it (the server had
/// its despawn acked). The client now remembers despawned `NetId`s.
///
/// Second regression (lifetimes past the RTO): a spawn delayed past the RTO was presumed lost,
/// which reset its "sent" mark, so when the entity then went away no despawn was sent at all —
/// and the late spawn created a ghost with no tombstone to stop it. A despawn now follows every
/// entity that was ever put on the wire.
#[test]
fn reordered_spawns_never_resurrect_despawned_entities() {
    for (seed, lifetime, reorder_ms) in [
        (0u64, 4u32, 120u64),
        (1, 4, 120),
        (2, 4, 120),
        (3, 4, 120),
        (0, 6, 120),
        (1, 6, 120),
        (2, 6, 120),
        (3, 6, 120),
        (0, 12, 250),
        (1, 12, 250),
    ] {
        let down = LinkConditions {
            latency: Duration::from_millis(20),
            jitter: Duration::from_millis(4),
            reorder: 0.3,
            reorder_delay: Duration::from_millis(reorder_ms),
            duplicate: 0.1,
            ..LinkConditions::perfect()
        };
        let mut h = with_rules(HarnessConfig {
            seed,
            semantics: Semantics::Datagram,
            link: LinkProfile {
                up: LinkConditions::lan(),
                down,
            },
            ..Default::default()
        });
        let a = observer(&mut h, "alice");
        assert!(h.run_until(900, |h| h.client_net(a).is_connected()));
        // A projectile every 3 ticks, each living `lifetime` ticks.
        let mut live: Vec<(Entity, u32)> = Vec::new();
        for t in 0..600u32 {
            if t % 3 == 0 {
                let e = h
                    .server_mut()
                    .world
                    .spawn((Replicated, Transform::from_xyz(t as f32, 0.0, 0.0), Health(1)));
                live.push((e, t + lifetime));
            }
            let (dead, keep): (Vec<_>, Vec<_>) = live.into_iter().partition(|(_, until)| *until <= t);
            live = keep;
            for (e, _) in dead {
                let _ = h.server_mut().world.despawn(e);
            }
            h.step();
        }
        for (e, _) in live.drain(..) {
            let _ = h.server_mut().world.despawn(e);
        }
        h.set_link(a, LinkProfile::symmetric(LinkConditions::lan()));
        let ok = h.run_until(1200, |h| h.assert_converged(a).is_ok());
        assert!(
            ok,
            "seed {seed}, lifetime {lifetime}, reorder {reorder_ms} ms: {:?} (client entities {})",
            h.assert_converged(a),
            h.client_net(a).net_ids().len()
        );
    }
}

// ---- byte budget: a large controlled set ----------------------------------------------------------

/// Run the "RTS" scenario: `units` controlled entities (moving with the player's input), a
/// budget of `budget` B/s, then a second player joins. Returns (ticks until the rts player
/// sees bob's pawn, rts downstream B/s over 20 s).
fn controlled_units_run(units: usize, budget: u32, moving: bool) -> (Option<u64>, f64) {
    let mut config = HarnessConfig {
        seed: 3,
        link: perfect(20),
        ..Default::default()
    };
    config.client.max_down_bytes_per_sec = budget;
    config.server.client_bandwidth = budget;
    let mut h = with_rules(config);
    let a = h.add_client(ClientSpec::named("rts"), move |app| {
        app.add_plugin(TestProtocol);
        space3d_replication::bots::install_bot(
            app,
            space3d_replication::bots::scripted(move |t: Tick| Move {
                x: if !moving {
                    0
                } else if (t.0 / 60).is_multiple_of(2) {
                    1
                } else {
                    -1
                },
                z: 0,
            }),
        );
    });
    assert!(h.run_until(900, |h| h.client_net(a).is_connected()));
    let player = h.player(a);
    for i in 0..units {
        h.server_mut().world.spawn((
            Replicated,
            Simulated,
            ControlledBy(player),
            Transform::from_xyz(i as f32, 0.0, 5.0),
        ));
    }
    h.step_ticks(120);
    let bob = observer(&mut h, "bob");
    let bytes0 = h.client_net(a).stats().bytes_in;
    let mut seen_after = None;
    for t in 0..1200u64 {
        h.step();
        if seen_after.is_none()
            && let Some(p) = pawn(h.server(), h.player(bob))
            && h.client_entity(a, p).is_some()
        {
            seen_after = Some(t);
        }
    }
    let rate = (h.client_net(a).stats().bytes_in - bytes0) as f64 / 20.0;
    (seen_after, rate)
}

/// Regression: controlled entities (and other always-sent bytes) were charged to the budget
/// with no floor, so once they exceeded the refill nothing else was ever sent — another
/// player's spawn included — and the client's rate was exceeded without limit.
#[test]
fn a_large_controlled_set_never_starves_other_entities() {
    for (units, budget, moving) in [
        (115usize, 48 * 1024u32, true),
        (130, 48 * 1024, true),
        (20, 4096, true),
        (40, 8192, false),
    ] {
        let (seen, rate) = controlled_units_run(units, budget, moving);
        let seen =
            seen.unwrap_or_else(|| panic!("{units} units at {budget} B/s: bob's pawn never reached the rts player"));
        assert!(
            seen < 120,
            "{units} units at {budget} B/s: bob's pawn took {seen} ticks"
        );
        if !moving {
            // Idle controlled units: re-confirmations thin out, the budget holds.
            assert!(
                rate < f64::from(budget) * 1.3,
                "{units} idle units: {rate:.0} B/s for a {budget} B/s budget"
            );
        }
        eprintln!("{units} units, budget {budget}: bob seen after {seen} ticks, {rate:.0} B/s");
    }
}

// ---- clocks across a stream stall -----------------------------------------------------------------

/// Largest backward step of the render tick, number of predicted-tick jumps (a frame advancing
/// it by more than 2 or moving it back) and the largest interpolation delay, across a
/// head-of-line stall of the observer's uplink (`up = true`) or downlink.
fn stall_run(seed: u64, up: bool) -> (f64, usize, f32) {
    let base = LinkConditions {
        latency: Duration::from_millis(40),
        jitter: Duration::from_millis(3),
        ..LinkConditions::perfect()
    };
    let mut h = with_rules(HarnessConfig {
        seed,
        semantics: Semantics::Stream,
        link: LinkProfile::symmetric(base.clone()),
        ..Default::default()
    });
    let w = h.add_client(ClientSpec::named("walker"), |app| {
        app.add_plugin(TestProtocol);
        space3d_replication::bots::install_bot(
            app,
            space3d_replication::bots::scripted(|_t: Tick| Move { x: 1, z: 0 }),
        );
    });
    let o = h.add_client(ClientSpec::named("obs"), |app| {
        app.add_plugin(TestProtocol);
        space3d_replication::bots::install_bot(app, space3d_replication::bots::scripted(|_t: Tick| Move::default()));
    });
    assert!(h.run_until(900, |h| h.client_net(w).is_connected()
        && h.client_net(o).is_connected()));
    h.step_ticks(300);
    let stalled = LinkConditions {
        loss: 0.9,
        ..base.clone()
    };
    h.set_link(
        o,
        if up {
            LinkProfile {
                up: stalled,
                down: base.clone(),
            }
        } else {
            LinkProfile {
                up: base.clone(),
                down: stalled,
            }
        },
    );
    let (mut back, mut jumps, mut delay) = (0.0f64, 0usize, 0.0f32);
    let (mut last_render, mut last_pred): (Option<f64>, Option<u32>) = (None, None);
    for t in 0..600u32 {
        if t == 36 {
            h.set_link(o, LinkProfile::symmetric(base.clone()));
        }
        h.step();
        let net = h.client_net(o);
        if let Some(r) = net.render_tick() {
            if let Some(l) = last_render {
                back = back.max(l - r);
            }
            last_render = Some(r);
        }
        if let Some(p) = net.predicted_tick() {
            if let Some(l) = last_pred {
                let d = i64::from(p.0) - i64::from(l);
                if !(0..=2).contains(&d) {
                    jumps += 1;
                }
            }
            last_pred = Some(p.0);
        }
        if t > 120 {
            delay = delay.max(net.stats().interp_delay_ticks);
        }
    }
    (back, jumps, delay)
}

/// Regression: pacing and the render clock used the RFC 6298 `srtt`, which one head-of-line
/// stall inflates several-fold: the render tick snapped backwards (remote entities froze), the
/// interpolation delay ballooned to ~20 ticks and the clock-based resync fought the pacer
/// (several forward/backward prediction jumps). They now use the stall-resistant path RTT of
/// the server clock, and the render clock never runs backwards.
#[test]
fn a_stream_stall_neither_rewinds_the_render_clock_nor_jumps_prediction() {
    for seed in 0..3u64 {
        for up in [true, false] {
            let (back, jumps, delay) = stall_run(seed, up);
            let which = if up { "uplink" } else { "downlink" };
            assert!(
                back <= 0.0,
                "seed {seed} {which}: render tick went back {back:.2} ticks"
            );
            assert!(jumps <= 1, "seed {seed} {which}: {jumps} predicted-tick jumps");
            assert!(
                delay < 8.0,
                "seed {seed} {which}: interpolation delay grew to {delay:.1} ticks"
            );
        }
    }
}

/// Regression: after a lasting RTT increase (one-way 20 → 130 ms) the stall-resistant path RTT
/// kept its old value until most of its 16-sample window had turned over (~3 s), and the
/// jitter — the spread of the window around its median — counted the step itself as jitter
/// while the window straddled it. The clock-based resync then jumped the prediction forward
/// and back, and inputs went missing again in the 4th second. The path RTT now follows a
/// lasting rise after ~1.25 s and the jitter comes from consecutive samples.
#[test]
fn a_lasting_rtt_increase_is_followed_without_a_relapse() {
    for (up_ms, down_ms) in [(130u64, 130u64), (130, 20)] {
        let mut missing = [0u64; 8];
        for seed in 1..=4u64 {
            let lat = |ms: u64| LinkConditions {
                latency: Duration::from_millis(ms),
                jitter: Duration::from_millis(2),
                ..LinkConditions::perfect()
            };
            let mut h = with_rules(HarnessConfig {
                seed,
                semantics: Semantics::Stream,
                link: LinkProfile::symmetric(lat(20)),
                ..Default::default()
            });
            let w = h.add_client(ClientSpec::named("walker"), |app| {
                app.add_plugin(TestProtocol);
                space3d_replication::bots::install_bot(
                    app,
                    space3d_replication::bots::scripted(|_t: Tick| Move { x: 1, z: 0 }),
                );
            });
            assert!(h.run_until(900, |h| h.client_net(w).is_connected()));
            h.step_ticks(600);
            h.set_link(
                w,
                LinkProfile {
                    up: lat(up_ms),
                    down: lat(down_ms),
                },
            );
            for second in &mut missing {
                let before = h.server_net().room_stats().inputs_missing;
                h.step_ticks(60);
                *second += h.server_net().room_stats().inputs_missing - before;
            }
        }
        let settled = missing.iter().position(|m| *m == 0).unwrap_or(missing.len());
        assert!(
            settled <= 2 && missing[settled..].iter().all(|m| *m == 0),
            "up {up_ms} ms, down {down_ms} ms: missing inputs per second (4 seeds) {missing:?}"
        );
    }
}

// ---- late inputs are counted -------------------------------------------------------------------

/// Regression: `inputs_late` / `NetStats::input_late` stayed 0 even when many inputs arrived
/// after their tick (every simulated tick counted as "consumed"). On a lossy stream (TCP
/// head-of-line stalls) inputs go missing and then arrive late; both are counted now.
#[test]
fn late_inputs_are_counted() {
    let mut h = with_rules(HarnessConfig {
        seed: 5,
        semantics: Semantics::Stream,
        link: LinkProfile::symmetric(LinkConditions {
            latency: Duration::from_millis(50),
            jitter: Duration::from_millis(30),
            loss: 0.15,
            ..LinkConditions::perfect()
        }),
        ..Default::default()
    });
    let w = h.add_client(ClientSpec::named("walker"), |app| {
        app.add_plugin(TestProtocol);
        space3d_replication::bots::install_bot(
            app,
            space3d_replication::bots::scripted(|t: Tick| Move {
                x: if (t.0 / 30).is_multiple_of(2) { 1 } else { -1 },
                z: 0,
            }),
        );
    });
    assert!(h.run_until(1800, |h| h.client_net(w).is_connected()));
    h.step_ticks(1200);
    let room = h.server_net().room_stats();
    assert!(room.inputs_missing > 0, "the lossy stream stalls inputs");
    assert!(room.inputs_late > 0, "late inputs counted: {room:?}");
    assert!(room.inputs_late <= room.inputs_missing, "{room:?}");
    let slot = h.server_net().stats(h.client_net(w).client_id().unwrap()).unwrap();
    assert_eq!(slot.input_late, room.inputs_late);
}

/// Regression: a resync (the clock-based hard resync after a long hitch, or the pacer's
/// `Resync`) moved the predicted tick but left the world at the old timeline's state whenever
/// the prediction for the newest confirmed tick was right: a forward jump skipped ticks (the
/// pawn fell behind) and a backward one simulated ticks a second time (inputs applied twice).
/// The backward jumps came from the hitch's fixed-step backlog running on top of the jump.
/// With a constant input every simulated tick — fresh or replayed — must equal the server's
/// state at that tick, and the predicted tick never moves backwards after the hitch.
#[test]
fn a_hitch_resync_rebuilds_the_timeline_without_skipping_or_repeating_ticks() {
    for latency_ms in [60u64, 100, 150] {
        for hitch_ms in [200u64, 400] {
            for fps in [60u32, 144] {
                for seed in 1..=3u64 {
                    let mut h = with_rules(HarnessConfig {
                        seed,
                        semantics: Semantics::Stream,
                        link: LinkProfile::symmetric(LinkConditions {
                            latency: Duration::from_millis(latency_ms),
                            jitter: Duration::from_millis(latency_ms / 5),
                            ..LinkConditions::perfect()
                        }),
                        ..Default::default()
                    });
                    let sims: Arc<Mutex<Vec<(u32, bool, f32)>>> = Default::default();
                    let record = sims.clone();
                    let w = h.add_client(
                        ClientSpec::named("walker").with_frame_dt(Duration::from_secs_f64(1.0 / f64::from(fps))),
                        |app| {
                            app.add_plugin(TestProtocol);
                            app.add_sim_system("test::record", move |ctx| {
                                let time = *ctx.resources.expect::<SimTime>();
                                for (t, _) in ctx.world.query_mut::<(&Transform, &space3d_replication::Controlled)>() {
                                    record
                                        .lock()
                                        .unwrap()
                                        .push((time.tick.0, time.resimulating, t.translation.x));
                                }
                            });
                            space3d_replication::bots::install_bot(
                                app,
                                space3d_replication::bots::scripted(|_t: Tick| Move { x: 1, z: 0 }),
                            );
                        },
                    );
                    assert!(h.run_until(900, |h| h.client_net(w).is_connected()
                        && controlled(h.client(w)).is_some()));
                    h.step_ticks(300);
                    sims.lock().unwrap().clear();
                    let sp = pawn(h.server(), h.player(w)).unwrap();
                    let mut server_x = std::collections::HashMap::new();
                    let mut previous = None;
                    for t in 0..240u32 {
                        if t == 60 {
                            h.stall(w, Duration::from_millis(hitch_ms));
                        }
                        h.step();
                        let st = h.server_net().tick().0;
                        server_x.insert(st, h.server().world.get::<&Transform>(sp).unwrap().translation.x);
                        let next = h.client_net(w).predicted_tick().unwrap();
                        assert!(
                            previous.is_none_or(|p| next >= p),
                            "latency {latency_ms} ms, hitch {hitch_ms} ms, {fps} fps, seed {seed}: \
                             predicted tick went back from {previous:?} to {next:?} (t = {t})"
                        );
                        previous = Some(next);
                    }
                    let sims = sims.lock().unwrap();
                    let mut compared = 0;
                    for &(tick, resimulating, x) in sims.iter() {
                        let Some(sx) = server_x.get(&tick) else { continue };
                        compared += 1;
                        assert!(
                            (x - sx).abs() < 1e-3,
                            "latency {latency_ms} ms, hitch {hitch_ms} ms, {fps} fps, seed {seed}: tick {tick} \
                             (resimulating: {resimulating}) predicted x = {x:.3}, server {sx:.3}"
                        );
                    }
                    assert!(compared > 150, "{compared}");
                }
            }
        }
    }
}

// ---- a large RTT drop ---------------------------------------------------------------------------

/// Predicted-tick jumps back / forward (a frame moving it back, or by more than 2), inputs
/// missing on the server, and fresh (not replayed) predicted ticks that differ from the server,
/// over 15 s after the walker's link drops from `from` to `to` ms one-way (`up_only`: the
/// downlink keeps `from`), in `ramp` ticks.
fn rtt_drop_run(
    seed: u64,
    semantics: Semantics,
    from: u64,
    to: u64,
    up_only: bool,
    ramp: u32,
) -> (usize, usize, u64, usize) {
    let lat = |ms: u64| LinkConditions {
        latency: Duration::from_millis(ms),
        jitter: Duration::from_millis(2),
        ..LinkConditions::perfect()
    };
    let mut h = with_rules(HarnessConfig {
        seed,
        semantics,
        link: LinkProfile::symmetric(lat(from)),
        ..Default::default()
    });
    let sims: Arc<Mutex<Vec<(u32, bool, f32)>>> = Default::default();
    let record = sims.clone();
    let w = h.add_client(ClientSpec::named("walker"), |app| {
        app.add_plugin(TestProtocol);
        app.add_sim_system("test::record", move |ctx| {
            let time = *ctx.resources.expect::<SimTime>();
            for (t, _) in ctx.world.query_mut::<(&Transform, &space3d_replication::Controlled)>() {
                record
                    .lock()
                    .unwrap()
                    .push((time.tick.0, time.resimulating, t.translation.x));
            }
        });
        // A player: the input follows the sampling time (every 7 samples it flips), not the
        // predicted tick — a tick predicted again with another input than the one the server
        // got shows up as a fresh misprediction.
        let mut samples = 0u32;
        space3d_replication::bots::install_bot(app, move |_: &World, _: Tick| {
            samples += 1;
            Move {
                x: if (samples / 7).is_multiple_of(2) { 1 } else { -1 },
                z: 0,
            }
        });
    });
    assert!(h.run_until(1200, |h| h.client_net(w).is_connected()
        && controlled(h.client(w)).is_some()));
    h.step_ticks(600);
    sims.lock().unwrap().clear();
    let sp = pawn(h.server(), h.player(w)).unwrap();
    let mut server_x = std::collections::HashMap::new();
    let missing = h.server_net().room_stats().inputs_missing;
    let mut last = h.client_net(w).predicted_tick().unwrap().0;
    let (mut back, mut forward) = (0, 0);
    for t in 0..900u32 {
        if t < ramp.max(1) {
            let ms = from - (from - to) * u64::from(t + 1) / u64::from(ramp.max(1));
            let down = if up_only { from } else { ms };
            h.set_link(
                w,
                LinkProfile {
                    up: lat(ms),
                    down: lat(down),
                },
            );
        }
        h.step();
        server_x.insert(
            h.server_net().tick().0,
            h.server().world.get::<&Transform>(sp).unwrap().translation.x,
        );
        let next = h.client_net(w).predicted_tick().unwrap().0;
        let d = i64::from(next) - i64::from(last);
        back += usize::from(d < 0);
        forward += usize::from(d > 2);
        last = next;
    }
    let bad_fresh = sims
        .lock()
        .unwrap()
        .iter()
        .filter(|(tick, resim, x)| !resim && server_x.get(tick).is_some_and(|sx| (x - sx).abs() > 1e-3))
        .count();
    (
        back,
        forward,
        h.server_net().room_stats().inputs_missing - missing,
        bad_fresh,
    )
}

/// Regression: when the uplink latency dropped a lot (200 → 20 ms one-way), the pacer resynced
/// backwards on the server's slack feedback, then kept acting on that feedback although it
/// still described the old timeline (the client sends nothing until its prediction passes the
/// newest input already sent, and the server's smoothed slack takes ~20 samples to follow),
/// while the clock-based resync — whose path RTT lags such a change by ~1.25 s — jumped
/// forward to the old target again: `[-7, +6, -9, +11, -9, -5, +6, -4]` within 90 ticks, then
/// dozens of missing and late inputs. Ticks predicted again after a backward jump also sampled
/// new inputs the server never got. Now one backward correction, no forward jump, and every
/// fresh prediction matches the server.
#[test]
fn a_large_rtt_drop_is_one_correction_not_a_fight() {
    for semantics in [Semantics::Stream, Semantics::Datagram] {
        for (from, to, up_only, ramp) in [
            (200u64, 20u64, false, 1u32),
            (250, 60, false, 1),
            (200, 20, true, 1),
            (200, 20, false, 30),
        ] {
            for seed in 1..=2u64 {
                let (back, forward, missing, bad_fresh) = rtt_drop_run(seed, semantics, from, to, up_only, ramp);
                let case = format!("{semantics:?} {from} -> {to} ms (up only: {up_only}, ramp {ramp}), seed {seed}");
                assert!(back <= 1, "{case}: {back} backward jumps");
                assert_eq!(forward, 0, "{case}: forward jumps");
                // (Uplink-only: the downlink keeps the 400 ms feedback loop, whose slow
                // settling still lets a few inputs go missing — and those ticks mispredict.)
                if !up_only {
                    assert_eq!(missing, 0, "{case}: missing inputs");
                    assert_eq!(bad_fresh, 0, "{case}: fresh predictions that differ from the server");
                }
            }
        }
    }
}

// ---- lost snapshots are not rest -----------------------------------------------------------------

/// Regression: on a datagram link a lost snapshot leaves the same gap in a moving entity's
/// samples as rest does, and the interpolation gap fix held its previous value across it: with
/// 1–5 % downlink loss a remote walker stepped back (up to 0.125 m at 0.05 m/tick) dozens of
/// times a minute. Only a snapshot that arrived without the entity counts as rest now.
#[test]
fn lost_snapshots_never_step_a_moving_remote_entity_back() {
    for seed in 0..2u64 {
        let base = LinkConditions {
            latency: Duration::from_millis(40),
            jitter: Duration::from_millis(2),
            ..LinkConditions::perfect()
        };
        let mut h = with_rules(HarnessConfig {
            seed,
            semantics: Semantics::Datagram,
            link: LinkProfile::symmetric(base.clone()),
            ..Default::default()
        });
        let w = h.add_client(ClientSpec::named("walker"), |app| {
            app.add_plugin(TestProtocol);
            space3d_replication::bots::install_bot(
                app,
                space3d_replication::bots::scripted(|_t: Tick| Move { x: 1, z: 0 }),
            );
        });
        let o = observer(&mut h, "obs");
        assert!(h.run_until(1200, |h| h.client_net(w).is_connected()
            && h.client_net(o).is_connected()));
        h.step_ticks(300);
        h.set_link(
            o,
            LinkProfile {
                up: base.clone(),
                down: LinkConditions {
                    loss: 0.03,
                    ..base.clone()
                },
            },
        );
        h.step_ticks(60);
        let sp = pawn(h.server(), h.player(w)).unwrap();
        let (mut last, mut back, mut worst): (Option<f32>, usize, f32) = (None, 0, 0.0);
        for _ in 0..1200 {
            h.step();
            let Some(e) = h.client_entity(o, sp) else { continue };
            let x = h.client(o).world.get::<&Transform>(e).unwrap().translation.x;
            if let Some(p) = last
                && x < p - 1e-4
            {
                back += 1;
                worst = worst.max(p - x);
            }
            last = Some(x);
        }
        assert_eq!(
            back, 0,
            "seed {seed}: the walker stepped back {back} times (up to {worst:.3} m)"
        );
    }
}

// ---- datagram acks across stays in scope ------------------------------------------------------

/// A datagram client whose acks take a second to come back (`link(loss)` sets the downlink
/// loss) and one entity, synced and visible: `(harness, client, entity, link)`.
fn late_acks() -> (NetHarness, ClientHandle, Entity, impl Fn(f32) -> LinkProfile) {
    let link = |loss: f32| LinkProfile {
        up: LinkConditions {
            latency: Duration::from_millis(1_000),
            ..LinkConditions::perfect()
        },
        down: LinkConditions {
            latency: Duration::from_millis(30),
            loss,
            ..LinkConditions::perfect()
        },
    };
    let mut h = with_rules(HarnessConfig {
        seed: 7,
        semantics: Semantics::Datagram,
        link: link(0.0),
        ..Default::default()
    });
    let a = observer(&mut h, "alice");
    assert!(h.run_until(900, |h| h.client_net(a).is_connected()));
    let e = h
        .server_mut()
        .world
        .spawn((Replicated, Transform::from_xyz(1.0, 0.0, 0.0), Health(5)));
    // Everything sent so far acked.
    h.step_ticks(180);
    assert!(h.client_entity(a, e).is_some());
    (h, a, e, link)
}

fn set_visibility(h: &mut NetHarness, c: ClientHandle, e: Entity, v: Visibility) {
    let player = h.player(c);
    h.server_mut()
        .resources
        .expect_mut::<NetServer>()
        .set_visibility(player, e, v);
}

/// Regression: datagram acks were applied by `NetId` alone. An entity that left the client's
/// view and came back got fresh per-client state, but a late ack of a packet sent during its
/// earlier stay (its spawn) was then applied to that fresh state: the server marked the new
/// stay's spawn delivered while it had actually been lost, never resent it, and the client
/// never saw the entity again. Acks are now fenced by the stay they were sent in.
#[test]
fn a_late_ack_from_an_earlier_stay_in_scope_never_marks_a_respawn_delivered() {
    let (mut h, a, e, link) = late_acks();
    set_visibility(&mut h, a, e, Visibility::Hidden);
    assert!(h.run_until(30, |h| h.client_entity(a, e).is_none()), "despawned");
    h.step_ticks(120);
    // First stay: the spawn arrives; its ack is a second away.
    set_visibility(&mut h, a, e, Visibility::Default);
    assert!(h.run_until(30, |h| h.client_entity(a, e).is_some()), "spawned");
    // It leaves: the despawn arrives.
    set_visibility(&mut h, a, e, Visibility::Hidden);
    assert!(h.run_until(30, |h| h.client_entity(a, e).is_none()), "despawned");
    // Second stay, while the first spawn's ack is still on its way: its spawn is lost.
    h.set_link(a, link(1.0));
    set_visibility(&mut h, a, e, Visibility::Default);
    h.step_ticks(12);
    h.set_link(a, link(0.0));
    // The first stay's ack arrives, then the second spawn is presumed lost and sent again.
    let back = h.run_until(900, |h| h.client_entity(a, e).is_some());
    assert!(
        back,
        "the entity never came back to the client ({} entities there)",
        h.client_net(a).net_ids().len()
    );
    assert!(
        h.run_until(300, |h| h.assert_converged(a).is_ok()),
        "{:?}",
        h.assert_converged(a)
    );
}

/// The despawn side of the same regression: a late ack of an entity's first despawn (from an
/// earlier stay) confirmed a later, lost despawn, which was then never resent — the client
/// kept a ghost of the entity.
#[test]
fn a_late_despawn_ack_from_an_earlier_stay_never_confirms_a_later_despawn() {
    let (mut h, a, e, link) = late_acks();
    // It leaves (the despawn arrives; its ack is a second away) and comes back.
    set_visibility(&mut h, a, e, Visibility::Hidden);
    assert!(h.run_until(30, |h| h.client_entity(a, e).is_none()), "despawned");
    set_visibility(&mut h, a, e, Visibility::Default);
    assert!(h.run_until(30, |h| h.client_entity(a, e).is_some()), "spawned again");
    // It leaves again; this despawn is lost for as long as the first one's ack takes.
    h.set_link(a, link(1.0));
    set_visibility(&mut h, a, e, Visibility::Hidden);
    h.step_ticks(90);
    h.set_link(a, link(0.0));
    let gone = h.run_until(900, |h| h.client_entity(a, e).is_none());
    assert!(gone, "the client kept a ghost of the entity");
    assert!(
        h.run_until(300, |h| h.assert_converged(a).is_ok()),
        "{:?}",
        h.assert_converged(a)
    );
}
