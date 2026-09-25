//! Shared test game for the server integration tests: one pawn per player, a duel that ends
//! when a client sends `Finish`, and a `SetHealth` command so tests can put state into a room
//! and watch it survive checkpoints, crashes and server loss.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use space3d_core::prelude::*;
use space3d_replication::prelude::*;
use space3d_replication::{NetEntity, Remote};
use space3d_server::game::{GameDefinition, RoomSpec};
use space3d_server::gs::room::{MatchResult, PlayerDataLoaded, PlayerResult, RoomContext, RoomControl};

/// Game id of the test game.
pub const GAME_ID: &str = "duel-test";

/// Held movement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Move {
    pub x: i8,
    pub z: i8,
}

impl NetInput for Move {}

/// Replicated to everyone.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Health(pub u16);

/// Client → server: end the match with the sender as the winner.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Finish;
impl NetMessage for Finish {}

/// Client → server: set the sender's pawn health.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SetHealth(pub u16);
impl NetMessage for SetHealth {}

/// Client → server: the room loads these player-data keys of the sender
/// (`RoomControl::load_player_data`; answers land in the room's [`Loaded`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoadData(pub Vec<String>);
impl NetMessage for LoadData {}

/// Client → server: the room submits this score for the sender on the `kills` board
/// (`RoomControl::submit_score`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Score(pub i64);
impl NetMessage for Score {}

/// Room resource: every `PlayerDataLoaded` the room received.
#[derive(Default)]
pub struct Loaded(pub Vec<PlayerDataLoaded>);

/// Shared sim: move pawns.
pub fn walk(ctx: &mut SystemCtx) {
    let dt = ctx.resources.expect::<SimTime>().dt_secs();
    for (t, input) in ctx
        .world
        .query_mut::<(&mut Transform, &PlayerInput<Move>)>()
        .with::<&Simulated>()
    {
        t.translation.x += f32::from(input.0.x) * 3.0 * dt;
        t.translation.z += f32::from(input.0.z) * 3.0 * dt;
    }
}

/// Shared protocol (server and clients).
pub struct DuelProtocol;

impl Plugin for DuelProtocol {
    fn build(&self, app: &mut App) {
        app.add_plugin(ReplicationPlugin::default())
            .replicate::<Health>("duel.health")
            .add_input::<Move>("duel.move")
            .add_message::<Finish>("duel.finish", MessageConfig::reliable(Direction::ClientToServer))
            .add_message::<SetHealth>("duel.set_health", MessageConfig::reliable(Direction::ClientToServer))
            .add_message::<LoadData>("duel.load_data", MessageConfig::reliable(Direction::ClientToServer))
            .add_message::<Score>("duel.score", MessageConfig::reliable(Direction::ClientToServer))
            .add_sim_system("duel::walk", walk);
    }
}

/// Server rules.
pub struct DuelRules;

fn pawn_of(world: &World, player: PlayerId) -> Option<Entity> {
    world
        .query::<(Entity, &ControlledBy)>()
        .iter()
        .find(|(_, c)| c.0 == player)
        .map(|(e, _)| e)
}

impl Plugin for DuelRules {
    fn build(&self, app: &mut App) {
        let mut joined = EventReader::default();
        let mut left = EventReader::default();
        let mut finish = EventReader::default();
        let mut set_health = EventReader::default();
        let mut load = EventReader::default();
        let mut score = EventReader::default();
        let mut loaded = EventReader::default();
        app.init_resource::<Loaded>();
        app.add_system_named(Stage::Update, "duel::data", move |ctx| {
            let loads: Vec<FromClient<LoadData>> = load
                .read(ctx.resources.expect::<Events<FromClient<LoadData>>>())
                .cloned()
                .collect();
            let scores: Vec<FromClient<Score>> = score
                .read(ctx.resources.expect::<Events<FromClient<Score>>>())
                .cloned()
                .collect();
            let control = ctx.resources.expect_mut::<RoomControl>();
            for l in loads {
                let keys: Vec<&str> = l.message.0.iter().map(String::as_str).collect();
                control.load_player_data(l.player, &keys);
            }
            for s in scores {
                control.submit_score("kills", s.player, s.message.0);
            }
            let answers: Vec<PlayerDataLoaded> = loaded
                .read(ctx.resources.expect::<Events<PlayerDataLoaded>>())
                .cloned()
                .collect();
            ctx.resources.expect_mut::<Loaded>().0.extend(answers);
        });
        app.add_system_named(Stage::Update, "duel::rules", move |ctx| {
            let events: Vec<PlayerJoined> = joined
                .read(ctx.resources.expect::<Events<PlayerJoined>>())
                .cloned()
                .collect();
            for j in events {
                // A restored room already has the pawn (checkpoint): re-use it.
                if pawn_of(ctx.world, j.identity.player).is_some() {
                    continue;
                }
                let n = ctx.world.query::<&ControlledBy>().iter().count() as f32;
                ctx.world.spawn((
                    Replicated,
                    Simulated,
                    ControlledBy(j.identity.player),
                    Transform::from_xyz(n * 2.0, 0.0, 0.0),
                    Health(100),
                ));
            }
            let events: Vec<PlayerLeft> = left
                .read(ctx.resources.expect::<Events<PlayerLeft>>())
                .cloned()
                .collect();
            for l in events {
                if matches!(l.reason, LeaveReason::RoomClosed) {
                    continue;
                }
                if let Some(e) = pawn_of(ctx.world, l.player) {
                    let _ = ctx.world.despawn(e);
                }
            }
            let sets: Vec<FromClient<SetHealth>> = set_health
                .read(ctx.resources.expect::<Events<FromClient<SetHealth>>>())
                .cloned()
                .collect();
            for s in sets {
                if let Some(e) = pawn_of(ctx.world, s.player)
                    && let Ok(mut h) = ctx.world.get::<&mut Health>(e)
                {
                    *h = Health(s.message.0);
                }
            }
            let done: Vec<FromClient<Finish>> = finish
                .read(ctx.resources.expect::<Events<FromClient<Finish>>>())
                .cloned()
                .collect();
            if let Some(f) = done.first() {
                let spec = ctx.resources.expect::<RoomContext>().spec.clone();
                let team_of = |p: PlayerId| spec.teams.iter().position(|t| t.contains(&p)).map(|t| t as u16);
                let players: Vec<PlayerId> = ctx.world.query::<&ControlledBy>().iter().map(|c| c.0).collect();
                let result = MatchResult {
                    match_id: spec.match_id.unwrap_or_default(),
                    room_id: spec.room_id,
                    mode: spec.mode.clone(),
                    started_ms: 0,
                    ended_ms: 0,
                    winner_team: team_of(f.player),
                    players: players
                        .iter()
                        .map(|p| PlayerResult {
                            player: *p,
                            team: team_of(*p),
                            score: i64::from(*p == f.player),
                            placement: Some(if *p == f.player { 1 } else { 2 }),
                            stats: serde_json::Value::Null,
                        })
                        .collect(),
                    scores: vec![("wins".into(), f.player, 1)],
                    data: serde_json::Value::Null,
                };
                let control = ctx.resources.expect_mut::<RoomControl>();
                control.end_match(result);
                control.write_player_data(f.player, "progress", serde_json::json!({ "wins": 1 }));
            }
        });
    }
}

/// The game definition.
#[derive(Clone, Copy, Debug, Default)]
pub struct DuelGame;

impl GameDefinition for DuelGame {
    fn game_id(&self) -> &str {
        GAME_ID
    }

    fn protocol(&self, app: &mut App) {
        app.add_plugin(DuelProtocol);
    }

    fn build_room(&self, app: &mut App, _spec: &RoomSpec) {
        app.add_plugin(DuelRules);
    }

    fn validate_spec(&self, spec: &RoomSpec) -> Result<(), String> {
        if spec.mode == "forbidden" {
            return Err("unknown mode".into());
        }
        Ok(())
    }
}

/// Replicated entities a client currently sees.
pub fn remote_count(app: &App) -> usize {
    app.world.query::<(&NetEntity, &Remote)>().iter().count()
}

/// Health values a client sees, sorted.
pub fn healths(app: &App) -> Vec<u16> {
    let mut v: Vec<u16> = app.world.query::<&Health>().iter().map(|h| h.0).collect();
    v.sort_unstable();
    v
}

/// Pawns in a room app.
pub fn pawns(app: &App) -> usize {
    app.world.query::<&ControlledBy>().iter().count()
}

/// Health of `player`'s pawn in a room app.
pub fn pawn_health(app: &App, player: PlayerId) -> Option<u16> {
    let e = pawn_of(&app.world, player)?;
    app.world.get::<&Health>(e).ok().map(|h| h.0)
}
