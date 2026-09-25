//! Shared test game: a walking pawn per player, plus helpers.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use space3d_core::prelude::*;
use space3d_replication::prelude::*;
use space3d_replication::{ControlledBy, NetEntity, Owner};

/// Held movement (state, not edges).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Move {
    pub x: i8,
    pub z: i8,
}

impl NetInput for Move {}

/// Health (everyone).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Health(pub u16);

/// Ammo (owner only).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Ammo(pub u16);

/// Server → client chat.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Chat(pub String);
impl NetMessage for Chat {}

/// Client → server command.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Say(pub String);
impl NetMessage for Say {
    fn validate(&self) -> Result<(), space3d_replication::InvalidMessage> {
        if self.0.len() > 64 {
            return Err(space3d_replication::InvalidMessage("too long"));
        }
        Ok(())
    }
}

/// Metres per second per unit of input.
pub const SPEED: f32 = 3.0;

/// The shared sim system: move predicted / authoritative pawns.
pub fn walk(ctx: &mut SystemCtx) {
    let dt = ctx.resources.expect::<SimTime>().dt_secs();
    for (t, input) in ctx
        .world
        .query_mut::<(&mut Transform, &PlayerInput<Move>)>()
        .with::<&Simulated>()
    {
        t.translation.x += f32::from(input.0.x) * SPEED * dt;
        t.translation.z += f32::from(input.0.z) * SPEED * dt;
    }
}

/// The shared protocol plugin.
pub struct TestProtocol;

impl Plugin for TestProtocol {
    fn build(&self, app: &mut App) {
        app.add_plugin(ReplicationPlugin::default())
            .replicate::<Health>("test.health")
            .replicate_with::<Ammo>("test.ammo", ComponentRule::serde().audience(Audience::Owner))
            .add_input::<Move>("test.move")
            .add_message::<Chat>("test.chat", MessageConfig::reliable(Direction::ServerToClient))
            .add_message::<Say>(
                "test.say",
                MessageConfig::reliable(Direction::ClientToServer)
                    .rate_limit(5, 5)
                    .max_bytes(128),
            )
            .add_sim_system("test::walk", walk);
    }
}

/// Server rules: a pawn per player.
pub struct TestRules;

#[derive(Default)]
pub struct Pawns(pub Vec<(PlayerId, Entity)>);

impl Plugin for TestRules {
    fn build(&self, app: &mut App) {
        app.init_resource::<Pawns>();
        let mut joined = EventReader::default();
        let mut left = EventReader::default();
        app.add_system(Stage::Update, move |ctx| {
            let events: Vec<PlayerJoined> = joined
                .read(ctx.resources.expect::<Events<PlayerJoined>>())
                .cloned()
                .collect();
            for j in events {
                let x = ctx.resources.expect::<Pawns>().0.len() as f32 * 2.0;
                let e = ctx.world.spawn((
                    Replicated,
                    Simulated,
                    ControlledBy(j.identity.player),
                    Transform::from_xyz(x, 0.0, 0.0),
                    Name(j.identity.display_name.clone()),
                    Health(100),
                    Ammo(30),
                ));
                ctx.resources.expect_mut::<Pawns>().0.push((j.identity.player, e));
            }
            let events: Vec<PlayerLeft> = left
                .read(ctx.resources.expect::<Events<PlayerLeft>>())
                .cloned()
                .collect();
            for l in events {
                let pawns = &mut ctx.resources.expect_mut::<Pawns>().0;
                if let Some(i) = pawns.iter().position(|(p, _)| *p == l.player) {
                    let (_, e) = pawns.remove(i);
                    let _ = ctx.world.despawn(e);
                }
            }
        });
    }
}

/// The pawn of `player` on the server.
pub fn pawn(app: &App, player: PlayerId) -> Option<Entity> {
    app.resources
        .expect::<Pawns>()
        .0
        .iter()
        .find(|(p, _)| *p == player)
        .map(|(_, e)| *e)
}

/// Client entity with `ControlledBy`-equivalent marker (`Controlled`).
pub fn controlled(app: &App) -> Option<Entity> {
    app.world
        .query::<(Entity, &space3d_replication::Controlled, &NetEntity)>()
        .iter()
        .map(|(e, _, _)| e)
        .next()
}

/// Silence unused-import warnings for helpers only some tests use.
pub fn _owner(p: PlayerId) -> Owner {
    Owner(p)
}
