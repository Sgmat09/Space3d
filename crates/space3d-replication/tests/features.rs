//! Replication rules the scenario suites did not exercise (§7.3, §8.1, §8.5, §8.7): input
//! sanitizing (strike + neutral input + kick), `SendRule::{Interval, Always}`,
//! `Audience::{Team, ExceptOwner, Custom}`, `NetRelevance::{OwnerOnly, Manual, Spatial}` and
//! `VisibilityRule`s, `Target::{ObserversOf, Team}`, `SetRate` / `InterestHint`,
//! `rollback_when` and `NetPriority`.

use serde::{Deserialize, Serialize};
use space3d_core::prelude::*;
use space3d_replication::client::NetClient;
use space3d_replication::harness::{ClientHandle, ClientSpec, HarnessConfig, NetHarness};
use space3d_replication::prelude::*;
use space3d_replication::protocol::handshake::DisconnectCode;
use space3d_replication::server::{InterestPolicy, NetServer, Visibility};
use space3d_replication::transport::conditioner::{LinkConditions, LinkProfile};
use space3d_replication::{ControlledBy, InterestSource, InvalidInput, NetEntity, NetTeam, Owner};
use std::time::Duration;

// ---- the test protocol ------------------------------------------------------------------------

/// Input with a sanity rule: `cheat` inputs are invalid, axes are clamped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Cmd {
    x: i8,
    cheat: bool,
}

impl NetInput for Cmd {
    fn sanitize(&mut self) -> Result<(), InvalidInput> {
        if self.cheat {
            return Err(InvalidInput("cheat flag"));
        }
        self.x = self.x.clamp(-1, 1);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Hp(u16);
/// Everyone but the owner.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Secret(u16);
/// Only the entity's team.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Intel(u16);
/// Only the player named by the server-only `For`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Gift(u16);
/// Server-only: who may see `Gift`.
struct For(PlayerId);
/// At most every 10 ticks.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Beacon(u32);
/// Every send tick.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Pulse(u16);
/// Predicted, with a tolerance.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Stamina(f32);
/// Predicted, exact.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Mana(f32);
/// Server-only drift of `Stamina` / `Mana` per tick (outside the shared sim).
struct Drift(f32);
/// Server-only: hidden from this player by the fog rule.
struct Fog(PlayerId);

/// Server → client note.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Note(u16);
impl NetMessage for Note {}

fn gift_visible(world: &World, e: Entity, viewer: PlayerId) -> bool {
    world.get::<&For>(e).is_ok_and(|f| f.0 == viewer)
}

fn walk(ctx: &mut SystemCtx) {
    let dt = ctx.resources.expect::<SimTime>().dt_secs();
    for (t, input) in ctx
        .world
        .query_mut::<(&mut Transform, &PlayerInput<Cmd>)>()
        .with::<&Simulated>()
    {
        t.translation.x += f32::from(input.0.x) * 3.0 * dt;
    }
}

struct Feat;

impl Plugin for Feat {
    fn build(&self, app: &mut App) {
        app.add_plugin(ReplicationPlugin::default())
            .replicate::<Hp>("f.hp")
            .replicate_with::<Secret>("f.secret", ComponentRule::serde().audience(Audience::ExceptOwner))
            .replicate_with::<Intel>("f.intel", ComponentRule::serde().audience(Audience::Team))
            .replicate_with::<Gift>(
                "f.gift",
                ComponentRule::serde().audience(Audience::Custom(gift_visible)),
            )
            .replicate_with::<Beacon>("f.beacon", ComponentRule::serde().send(SendRule::Interval(10)))
            .replicate_with::<Pulse>("f.pulse", ComponentRule::serde().send(SendRule::Always))
            .replicate_with::<Stamina>(
                "f.stamina",
                ComponentRule::serde()
                    .predict()
                    .rollback_when(|a: &Stamina, b: &Stamina| (a.0 - b.0).abs() > 0.05),
            )
            .replicate_with::<Mana>("f.mana", ComponentRule::serde().predict())
            .add_input::<Cmd>("f.cmd")
            .add_message::<Note>("f.note", MessageConfig::reliable(Direction::ServerToClient))
            .add_sim_system("f::walk", walk);
    }
}

#[derive(Default)]
struct Pawns(Vec<(PlayerId, Entity)>);

/// A pawn per player (team from the identity), and the drift of predicted values.
struct FeatRules;

impl Plugin for FeatRules {
    fn build(&self, app: &mut App) {
        app.init_resource::<Pawns>();
        let mut joined = EventReader::default();
        app.add_system(Stage::Update, move |ctx| {
            let events: Vec<PlayerJoined> = joined
                .read(ctx.resources.expect::<Events<PlayerJoined>>())
                .cloned()
                .collect();
            for j in events {
                let n = ctx.resources.expect::<Pawns>().0.len() as f32;
                let mut b = space3d_core::hecs::EntityBuilder::new();
                b.add(Replicated)
                    .add(Simulated)
                    .add(ControlledBy(j.identity.player))
                    .add(Transform::from_xyz(n * 2.0, 0.0, 0.0))
                    .add(InterestSource { radius: 0.0 })
                    .add(Hp(100))
                    .add(Secret(7))
                    .add(Intel(3));
                if let Some(team) = j.identity.team {
                    b.add(NetTeam(team));
                }
                let e = ctx.world.spawn(b.build());
                ctx.resources.expect_mut::<Pawns>().0.push((j.identity.player, e));
            }
            for (v, d) in ctx.world.query_mut::<(&mut Stamina, &Drift)>() {
                v.0 += d.0;
            }
            for (v, d) in ctx.world.query_mut::<(&mut Mana, &Drift)>() {
                v.0 += d.0;
            }
        });
    }
}

fn harness(config: HarnessConfig) -> NetHarness {
    NetHarness::new(config, |app| {
        app.add_plugin(Feat).add_plugin(FeatRules);
    })
}

fn client(h: &mut NetHarness, spec: ClientSpec, input: Cmd) -> ClientHandle {
    h.add_client(spec, move |app| {
        app.add_plugin(Feat);
        space3d_replication::bots::install_bot(app, space3d_replication::bots::scripted(move |_t: Tick| input));
    })
}

fn pawn(h: &NetHarness, c: ClientHandle) -> Entity {
    let p = h.player(c);
    h.server()
        .resources
        .expect::<Pawns>()
        .0
        .iter()
        .find(|(q, _)| *q == p)
        .map(|(_, e)| *e)
        .expect("pawn")
}

/// Component `T` of server entity `e` as client `c` sees it (`None`: absent / not replicated).
fn seen<T: space3d_core::hecs::Component + Copy>(h: &NetHarness, c: ClientHandle, e: Entity) -> Option<T> {
    let ce = h.client_entity(c, e)?;
    h.client(c).world.get::<&T>(ce).ok().map(|v| *v)
}

fn visible(h: &NetHarness, c: ClientHandle, e: Entity) -> bool {
    h.client_entity(c, e).is_some()
}

fn connect_all(h: &mut NetHarness, clients: &[ClientHandle]) {
    assert!(
        h.run_until(900, |h| clients.iter().all(|c| h.client_net(*c).is_connected())),
        "clients connect"
    );
    h.step_ticks(60);
}

// ---- tests -------------------------------------------------------------------------------------

/// A raw client link (the room is driven directly, so the client can send what a modified
/// client would: the normal client sanitizes before sending).
#[derive(Default)]
struct RawTx {
    closed: std::sync::Mutex<bool>,
    sent: std::sync::Mutex<Vec<Vec<u8>>>,
}

impl space3d_replication::transport::LinkTx for RawTx {
    fn caps(&self) -> space3d_replication::transport::TransportCaps {
        space3d_replication::transport::TransportCaps::stream(1 << 20)
    }
    fn send(
        &self,
        _lane: space3d_replication::transport::Lane,
        packet: Vec<u8>,
    ) -> Result<(), space3d_replication::transport::TransportError> {
        self.sent.lock().unwrap().push(packet);
        Ok(())
    }
    fn backlog_bytes(&self) -> usize {
        0
    }
    fn close(&self, _code: space3d_replication::transport::CloseCode, _reason: &str) {
        *self.closed.lock().unwrap() = true;
    }
    fn is_open(&self) -> bool {
        !*self.closed.lock().unwrap()
    }
}

impl RawTx {
    fn disconnect(&self) -> Option<DisconnectCode> {
        use space3d_replication::protocol::{Packet, WireLimits, decode_packet};
        let limits = WireLimits {
            max_packet: 1 << 20,
            ..WireLimits::default()
        };
        self.sent
            .lock()
            .unwrap()
            .iter()
            .find_map(|p| match decode_packet(p, &limits) {
                Ok(Packet::Disconnect(d)) => Some(d.code),
                _ => None,
            })
    }
}

/// An input packet carrying `cmd` for `tick` (and the previous tick, as the client does).
fn input_packet(tick: Tick, cmd: Cmd) -> Vec<u8> {
    use space3d_replication::protocol::input::InputWindow;
    use space3d_replication::protocol::{ChannelId, DataWriter};
    let bytes = postcard::to_stdvec(&cmd).unwrap();
    let mut w = space3d_replication::wire::Writer::new();
    InputWindow::encode(
        &mut w,
        &[(tick, bytes.as_slice()), (Tick(tick.0 - 1), bytes.as_slice())],
        0,
    );
    let mut dw = DataWriter::new(None, None, Tick(0));
    dw.block_bytes(ChannelId::Input, w.as_slice());
    dw.finish()
}

/// §7.3: an input failing `NetInput::sanitize` on the server is a strike and its tick runs on
/// the neutral input; a client that keeps sending them is kicked (`ProtocolViolation`) after
/// `strikes.max`. (The normal client sanitizes before sending; a modified one does not.)
#[test]
fn invalid_inputs_strike_run_neutral_and_kick() {
    use space3d_replication::protocol::handshake::{ClientCaps, Hello, HelloAuth};
    use space3d_replication::server::{JoinRequest, PlayerIdentity, RoomInbound, room_channel};
    use space3d_replication::time::{ManualClock, NetClockHandle};
    use space3d_replication::transport::PeerInfo;
    let clock = ManualClock::new();
    let mut app = App::new().with_tickrate(60.0);
    app.insert_resource(NetClockHandle(std::sync::Arc::new(clock.clone())));
    let (tx, inbox) = room_channel(1 << 16);
    app.insert_resource(inbox);
    app.add_plugin(space3d_replication::server::ServerReplicationPlugin::default());
    app.add_plugin(Feat).add_plugin(FeatRules);
    let step = Duration::from_secs_f64(1.0 / 60.0);
    let tick = |app: &mut App| {
        clock.advance(step);
        app.update(step);
    };
    tick(&mut app);
    let schema = app
        .resources
        .expect::<space3d_replication::ReplicationRegistry>()
        .schema_hash();
    let join = |link: u64, name: &str| {
        let fake = std::sync::Arc::new(RawTx::default());
        let hello = Hello::new(
            "test",
            "0",
            schema,
            HelloAuth::Dev { name: name.into() },
            ClientCaps::default(),
        );
        let req = JoinRequest {
            link: space3d_replication::LinkId(link),
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
        assert!(tx.try_send(RoomInbound::Join(Box::new(req))).is_ok());
        fake
    };
    let cheater = join(1, "cheater");
    let honest = join(2, "honest");
    tick(&mut app);
    tick(&mut app);
    let pawn_of = |app: &App, name: &str| {
        let p = PlayerIdentity::dev(name).player;
        app.resources
            .expect::<Pawns>()
            .0
            .iter()
            .find(|(q, _)| *q == p)
            .map(|(_, e)| *e)
            .expect("pawn")
    };
    let (cp, hp) = (pawn_of(&app, "cheater"), pawn_of(&app, "honest"));
    let x = |app: &App, e: Entity| {
        app.world
            .get::<&Transform>(e)
            .map(|t| t.translation.x)
            .unwrap_or(f32::NAN)
    };
    let (cx0, hx0) = (x(&app, cp), x(&app, hp));
    let mut kicked_at = None;
    for i in 0..120u32 {
        let next = app.resources.expect::<NetServer>().tick().0 + 2;
        let send = |link: u64, cmd: Cmd| {
            let _ = tx.try_send(RoomInbound::Packet {
                link: space3d_replication::LinkId(link),
                bytes: input_packet(Tick(next), cmd),
            });
        };
        send(1, Cmd { x: 1, cheat: true });
        send(2, Cmd { x: 1, cheat: false });
        tick(&mut app);
        if kicked_at.is_none() && !space3d_replication::transport::LinkTx::is_open(&*cheater) {
            kicked_at = Some(i);
        }
    }
    assert!(kicked_at.is_some(), "the cheater was kicked");
    assert_eq!(cheater.disconnect(), Some(DisconnectCode::ProtocolViolation));
    assert!(
        space3d_replication::transport::LinkTx::is_open(&*honest),
        "the honest client stays"
    );
    let stats = app.resources.expect::<NetServer>().room_stats().clone();
    assert!(stats.inputs_invalid >= 5 && stats.strikes >= 5, "{stats:?}");
    // The cheater's ticks ran on the neutral input (never moved); the honest pawn walked.
    if app.world.contains(cp) {
        assert_eq!(x(&app, cp), cx0, "neutral input used");
    }
    assert!(x(&app, hp) > hx0 + 0.5, "the honest input was applied");
}

/// §8.1: `Audience::ExceptOwner`, `Audience::Team` (per the viewer's identity team) and
/// `Audience::Custom` decide per component who receives it.
#[test]
fn team_except_owner_and_custom_audiences() {
    let mut h = harness(HarnessConfig::default());
    let idle = Cmd::default();
    let alice = client(&mut h, ClientSpec::named("alice").with_team(1), idle);
    let bob = client(&mut h, ClientSpec::named("bob").with_team(1), idle);
    let carol = client(&mut h, ClientSpec::named("carol").with_team(2), idle);
    connect_all(&mut h, &[alice, bob, carol]);
    let (pa, pb, pc) = (pawn(&h, alice), pawn(&h, bob), pawn(&h, carol));
    // A gift on bob's pawn meant for carol only.
    let carol_id = h.player(carol);
    h.server_mut().world.insert(pb, (Gift(9), For(carol_id))).unwrap();
    h.step_ticks(30);
    // ExceptOwner: everyone else sees the secret, the owner does not.
    assert_eq!(seen::<Secret>(&h, alice, pa), None, "owner");
    assert_eq!(seen::<Secret>(&h, bob, pa), Some(Secret(7)));
    assert_eq!(seen::<Secret>(&h, carol, pa), Some(Secret(7)));
    // Team: alice and bob (team 1) see each other's intel, carol (team 2) sees only her own.
    assert_eq!(seen::<Intel>(&h, alice, pb), Some(Intel(3)));
    assert_eq!(seen::<Intel>(&h, bob, pa), Some(Intel(3)));
    assert_eq!(seen::<Intel>(&h, carol, pa), None);
    assert_eq!(seen::<Intel>(&h, carol, pc), Some(Intel(3)));
    assert_eq!(seen::<Intel>(&h, alice, pc), None);
    // Custom: only carol gets the gift.
    assert_eq!(seen::<Gift>(&h, carol, pb), Some(Gift(9)));
    assert_eq!(seen::<Gift>(&h, alice, pb), None);
    assert_eq!(seen::<Gift>(&h, bob, pb), None);
    // Re-targeting the rule removes it from carol and gives it to alice.
    let alice_id = h.player(alice);
    h.server_mut().world.insert_one(pb, For(alice_id)).unwrap();
    // `Custom` is evaluated when the component is (re)sent: touch it.
    h.server_mut().world.insert_one(pb, Gift(10)).unwrap();
    h.step_ticks(30);
    assert_eq!(seen::<Gift>(&h, alice, pb), Some(Gift(10)));
    assert_eq!(seen::<Gift>(&h, carol, pb), None);
}

/// §8.1: `SendRule::Interval(10)` sends a component changing every tick at most every 10
/// ticks; `SendRule::Always` sends an unchanged component every send tick.
#[test]
fn interval_and_always_send_rules() {
    let mut h = harness(HarnessConfig::default());
    let obs = client(&mut h, ClientSpec::named("obs"), Cmd::default());
    connect_all(&mut h, &[obs]);
    let beacon = h.server_mut().world.spawn((Replicated, Beacon(0)));
    h.step_ticks(10);
    let mut changes = 0;
    let mut last = None;
    for _ in 0..300 {
        if let Ok(mut b) = h.server_mut().world.get::<&mut Beacon>(beacon) {
            b.0 += 1;
        }
        h.step();
        let now = seen::<Beacon>(&h, obs, beacon);
        if now.is_some() && now != last {
            changes += 1;
            last = now;
        }
    }
    assert!(
        (25..=32).contains(&changes),
        "Interval(10) over 300 ticks: {changes} updates"
    );

    // Always vs OnChange: bytes for an unchanged component.
    let bytes = |always: bool| {
        let mut h = harness(HarnessConfig::default());
        let obs = client(&mut h, ClientSpec::named("obs"), Cmd::default());
        connect_all(&mut h, &[obs]);
        for i in 0..20 {
            if always {
                h.server_mut().world.spawn((Replicated, Pulse(i)));
            } else {
                h.server_mut().world.spawn((Replicated, Hp(i)));
            }
        }
        h.step_ticks(60);
        let before = h.client_net(obs).stats().bytes_in;
        h.step_ticks(300);
        h.client_net(obs).stats().bytes_in - before
    };
    let (always, on_change) = (bytes(true), bytes(false));
    assert!(
        always > on_change * 3,
        "Always resends unchanged values: {always} B vs {on_change} B"
    );
}

/// §8.5: per-entity relevance overrides and visibility rules in a spatial room —
/// `OwnerOnly`, `Manual` (+ `set_visibility`), `Spatial { radius }`, `Global`, and a
/// `VisibilityRule` (fog of war) entering and leaving scope.
#[test]
fn relevance_overrides_and_visibility_rules() {
    let mut config = HarnessConfig::default();
    config.server.interest = InterestPolicy::Spatial {
        cell: 16.0,
        radius: 50.0,
        hysteresis: 1.2,
    };
    config.server.interest_every = 1;
    let mut h = harness(config);
    let alice = client(&mut h, ClientSpec::named("alice"), Cmd::default());
    let bob = client(&mut h, ClientSpec::named("bob"), Cmd::default());
    connect_all(&mut h, &[alice, bob]);
    let (alice_id, bob_id) = (h.player(alice), h.player(bob));
    // Pawns sit at x = 0 and x = 2.
    h.server_mut().resources.expect_mut::<NetServer>().add_visibility_rule(
        move |world: &World, viewer: PlayerId, e: Entity| !world.get::<&Fog>(e).is_ok_and(|f| f.0 == viewer),
    );
    let world = &mut h.server_mut().world;
    let owner_only = world.spawn((
        Replicated,
        Owner(alice_id),
        NetRelevance::OwnerOnly,
        Transform::from_xyz(1.0, 0.0, 0.0),
        Hp(1),
    ));
    let manual = world.spawn((
        Replicated,
        NetRelevance::Manual,
        Transform::from_xyz(1.0, 0.0, 0.0),
        Hp(2),
    ));
    let near_only = world.spawn((
        Replicated,
        NetRelevance::Spatial { radius: Some(3.0) },
        Transform::from_xyz(7.0, 0.0, 0.0),
        Hp(3),
    ));
    let global = world.spawn((
        Replicated,
        NetRelevance::Global,
        Transform::from_xyz(1000.0, 0.0, 0.0),
        Hp(4),
    ));
    let far = world.spawn((Replicated, Transform::from_xyz(1000.0, 0.0, 0.0), Hp(5)));
    let fogged = world.spawn((Replicated, Fog(bob_id), Transform::from_xyz(1.0, 0.0, 0.0), Hp(6)));
    h.step_ticks(20);
    assert!(
        visible(&h, alice, owner_only) && !visible(&h, bob, owner_only),
        "OwnerOnly"
    );
    assert!(
        !visible(&h, alice, manual) && !visible(&h, bob, manual),
        "Manual: nobody by default"
    );
    assert!(
        !visible(&h, alice, near_only) && !visible(&h, bob, near_only),
        "5 m > radius 3"
    );
    assert!(
        visible(&h, alice, global) && visible(&h, bob, global),
        "Global ignores distance"
    );
    assert!(!visible(&h, alice, far), "spatial default hides far entities");
    assert!(visible(&h, alice, fogged) && !visible(&h, bob, fogged), "fog rule");
    // Enter scope.
    h.server_mut()
        .resources
        .expect_mut::<NetServer>()
        .set_visibility(bob_id, manual, Visibility::Visible);
    h.server_mut()
        .world
        .get::<&mut Transform>(near_only)
        .unwrap()
        .translation
        .x = 4.0;
    let _ = h.server_mut().world.remove_one::<Fog>(fogged);
    h.step_ticks(20);
    assert!(
        visible(&h, bob, manual) && !visible(&h, alice, manual),
        "set_visibility"
    );
    assert!(visible(&h, bob, near_only), "within 3 m of bob (x = 2)");
    assert!(!visible(&h, alice, near_only), "4 m from alice");
    assert!(visible(&h, bob, fogged), "fog lifted");
    // Leave scope again.
    h.server_mut()
        .resources
        .expect_mut::<NetServer>()
        .set_visibility(bob_id, manual, Visibility::Default);
    h.server_mut()
        .world
        .get::<&mut Transform>(near_only)
        .unwrap()
        .translation
        .x = 20.0;
    h.server_mut().world.insert_one(fogged, Fog(bob_id)).unwrap();
    h.step_ticks(20);
    assert!(!visible(&h, bob, manual));
    assert!(!visible(&h, bob, near_only));
    assert!(!visible(&h, bob, fogged));
    let _ = alice_id;
}

/// §8.7: `Target::ObserversOf(entity)` reaches exactly the clients that have it in scope;
/// `Target::Team(t)` the players of team `t`.
#[test]
fn observers_of_and_team_targets() {
    let mut h = harness(HarnessConfig::default());
    let idle = Cmd::default();
    let alice = client(&mut h, ClientSpec::named("alice").with_team(1), idle);
    let bob = client(&mut h, ClientSpec::named("bob").with_team(1), idle);
    let carol = client(&mut h, ClientSpec::named("carol").with_team(2), idle);
    connect_all(&mut h, &[alice, bob, carol]);
    let carol_id = h.player(carol);
    let hidden_from_carol = h.server_mut().world.spawn((Replicated, Hp(1)));
    h.server_mut()
        .resources
        .expect_mut::<NetServer>()
        .set_visibility(carol_id, hidden_from_carol, Visibility::Hidden);
    h.step_ticks(20);
    let mut readers: Vec<EventReader> = (0..3).map(|_| EventReader::default()).collect();
    // Events live for two frames: read them every step.
    let run = |h: &mut NetHarness, readers: &mut Vec<EventReader>, ticks: u32| -> Vec<Vec<u16>> {
        let mut got = vec![Vec::new(); 3];
        for _ in 0..ticks {
            h.step();
            for (i, c) in [alice, bob, carol].iter().enumerate() {
                got[i].extend(
                    readers[i]
                        .read(h.client(*c).resources.expect::<Events<FromServer<Note>>>())
                        .map(|n| n.message.0),
                );
            }
        }
        got
    };
    let _ = run(&mut h, &mut readers, 1);
    h.server_mut()
        .resources
        .expect_mut::<NetServer>()
        .send(Target::ObserversOf(hidden_from_carol), &Note(1));
    assert_eq!(run(&mut h, &mut readers, 20), vec![vec![1], vec![1], vec![]]);
    h.server_mut()
        .resources
        .expect_mut::<NetServer>()
        .send(Target::Team(2), &Note(2));
    h.server_mut()
        .resources
        .expect_mut::<NetServer>()
        .send(Target::Team(1), &Note(3));
    assert_eq!(run(&mut h, &mut readers, 20), vec![vec![3], vec![3], vec![2]]);
}

/// `ControlMsg::SetRate` (`NetClient::set_rate`) changes the measured downstream rate;
/// `ControlMsg::InterestHint` (`NetClient::set_interest_hint`) adds an interest centre.
#[test]
fn set_rate_and_interest_hint() {
    let mut config = HarnessConfig::default();
    config.server.client_bandwidth = 64 * 1024;
    config.server.interest = InterestPolicy::Spatial {
        cell: 16.0,
        radius: 50.0,
        hysteresis: 1.2,
    };
    config.server.interest_every = 1;
    let mut h = harness(config);
    let obs = client(&mut h, ClientSpec::named("obs"), Cmd::default());
    connect_all(&mut h, &[obs]);
    // 150 entities changing every tick near the observer: more than any budget below.
    let movers: Vec<Entity> = (0..150)
        .map(|i| {
            h.server_mut().world.spawn((
                Replicated,
                Transform::from_xyz(i as f32 * 0.1, 0.0, 1.0),
                Beacon(0),
                Hp(i),
            ))
        })
        .collect();
    let run = |h: &mut NetHarness, ticks: u32| {
        let before = h.client_net(obs).stats().bytes_in;
        for _ in 0..ticks {
            for e in &movers {
                if let Ok(mut t) = h.server_mut().world.get::<&mut Transform>(*e) {
                    t.translation.z += 0.01;
                }
            }
            h.step();
        }
        (h.client_net(obs).stats().bytes_in - before) as f64 / (f64::from(ticks) / 60.0)
    };
    run(&mut h, 120);
    let fast = run(&mut h, 300);
    h.client_mut(obs)
        .resources
        .expect_mut::<NetClient>()
        .set_rate(8 * 1024)
        .unwrap();
    run(&mut h, 120);
    let slow = run(&mut h, 300);
    assert!(fast > 30_000.0, "unthrottled: {fast:.0} B/s");
    assert!(slow < 8.0 * 1024.0 * 1.3, "after SetRate(8 KiB/s): {slow:.0} B/s");

    // Interest hint: a far entity comes into scope around the hinted centre.
    let far = h
        .server_mut()
        .world
        .spawn((Replicated, Transform::from_xyz(500.0, 0.0, 0.0), Hp(99)));
    h.step_ticks(20);
    assert!(!visible(&h, obs, far));
    h.client_mut(obs)
        .resources
        .expect_mut::<NetClient>()
        .set_interest_hint(space3d_core::glam::Vec3::new(500.0, 0.0, 0.0), 10.0)
        .unwrap();
    assert!(h.run_until(300, |h| visible(h, obs, far)), "hinted area in scope");
}

/// §8.1: `rollback_when` decides what counts as a misprediction: a predicted value drifting
/// on the server by less than the tolerance causes no rollback; an exact rule does.
#[test]
fn rollback_when_tolerates_small_differences() {
    let rollbacks = |tolerant: bool| {
        let mut h = harness(HarnessConfig {
            link: LinkProfile::symmetric(LinkConditions {
                latency: Duration::from_millis(20),
                ..LinkConditions::perfect()
            }),
            ..Default::default()
        });
        let c = client(&mut h, ClientSpec::named("p"), Cmd::default());
        connect_all(&mut h, &[c]);
        let p = pawn(&h, c);
        // Drift 0.0001 per tick outside the shared sim: the client cannot predict it.
        if tolerant {
            h.server_mut().world.insert(p, (Stamina(1.0), Drift(0.0001))).unwrap();
        } else {
            h.server_mut().world.insert(p, (Mana(1.0), Drift(0.0001))).unwrap();
        }
        h.step_ticks(60);
        let before = h.client_net(c).stats().rollbacks;
        h.step_ticks(240);
        h.client_net(c).stats().rollbacks - before
    };
    let (tolerant, exact) = (rollbacks(true), rollbacks(false));
    assert!(tolerant <= 1, "within tolerance: {tolerant} rollbacks");
    assert!(exact >= 20, "exact rule: {exact} rollbacks");
}

/// §8.1: `NetPriority` weights the byte budget: under a tight budget, a high-priority entity
/// is updated far more often than an equal one with the default priority.
#[test]
fn net_priority_weights_the_budget() {
    let mut config = HarnessConfig::default();
    config.server.client_bandwidth = 4096;
    config.client.max_down_bytes_per_sec = 4096;
    let mut h = harness(config);
    let obs = client(&mut h, ClientSpec::named("obs"), Cmd::default());
    connect_all(&mut h, &[obs]);
    let mut movers = Vec::new();
    for i in 0..60 {
        let e = h
            .server_mut()
            .world
            .spawn((Replicated, Transform::from_xyz(i as f32, 0.0, 3.0)));
        movers.push(e);
    }
    let vip = movers[30];
    let plain = movers[31];
    h.server_mut().world.insert_one(vip, NetPriority(20.0)).unwrap();
    h.step_ticks(240);
    let (mut vip_updates, mut plain_updates) = (0, 0);
    let (mut last_vip, mut last_plain) = (None, None);
    for _ in 0..600 {
        for e in &movers {
            if let Ok(mut t) = h.server_mut().world.get::<&mut Transform>(*e) {
                t.translation.z += 0.05;
            }
        }
        h.step();
        let raw = |h: &NetHarness, e: Entity| {
            let ce = h.client_entity(obs, e)?;
            h.client(obs)
                .world
                .get::<&NetEntity>(ce)
                .ok()
                .and_then(|n| h.client_net(obs).last_applied(n.0))
        };
        let v = raw(&h, vip);
        if v.is_some() && v != last_vip {
            vip_updates += 1;
            last_vip = v;
        }
        let p = raw(&h, plain);
        if p.is_some() && p != last_plain {
            plain_updates += 1;
            last_plain = p;
        }
    }
    assert!(
        vip_updates >= plain_updates * 3,
        "NetPriority(20): {vip_updates} updates vs {plain_updates} for priority 1"
    );
}
