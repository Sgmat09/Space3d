//! Authoritative arena rules. Add to every room after `ServerReplicationPlugin` and
//! [`ArenaProtocol`](crate::ArenaProtocol):
//!
//! * `PlayerJoined` → spawn a pawn (`Replicated`, `Simulated`, `ControlledBy`, lag-compensated
//!   capsule); `PlayerLeft` → despawn it (a disconnected player keeps the pawn for the resume
//!   window and it stands still on neutral input).
//! * Fire: every increment of `ArenaInput::fire_seq` spends a round and hit-scans against the
//!   hitboxes *as the shooter saw them* (`LagCompensation::raycast`). Hits cost [`DAMAGE`];
//!   a kill respawns the victim and scores for the shooter. Rounds reload over time.
//! * `Say` → `Chat` to everyone.

use crate::{
    Ammo, ArenaInput, Chat, DAMAGE, Health, Hit, MAX_AMMO, MAX_HEALTH, PlayerColor, RANGE, Say, Score, forward,
    spawn_point,
};
use space3d_core::prelude::*;
use space3d_replication::prelude::*;
use space3d_replication::server::lagcomp::{HitShape, LagCompensated, LagCompensation};
use space3d_replication::{InterestSource, NetClientId};

/// Ticks per reloaded round.
pub const RELOAD_TICKS: u16 = 30;

/// Server-only state of a pawn.
#[derive(Clone, Debug)]
pub struct Pawn {
    /// Owner.
    pub player: PlayerId,
    /// Display name.
    pub name: String,
    /// Spawn index (spawn point, colour).
    pub index: u32,
    /// Last `fire_seq` seen (`None` until the first input: a fresh pawn adopts the client's
    /// counter instead of firing).
    pub last_fire: Option<u8>,
    /// Reload progress.
    pub reload: u16,
}

/// Monotonic spawn counter (resource).
#[derive(Default)]
struct Spawns(u32);

/// The authoritative rules plugin (see the module docs).
#[derive(Clone, Copy, Debug, Default)]
pub struct ArenaRules;

impl Plugin for ArenaRules {
    fn build(&self, app: &mut App) {
        app.init_resource::<Spawns>();
        let mut joined = EventReader::default();
        let mut left = EventReader::default();
        app.add_system_named(Stage::Update, "arena::lifecycle", move |ctx| {
            let events: Vec<PlayerJoined> = joined
                .read(ctx.resources.expect::<Events<PlayerJoined>>())
                .cloned()
                .collect();
            for j in events {
                let spawns = ctx.resources.expect_mut::<Spawns>();
                let index = spawns.0;
                spawns.0 += 1;
                let p = spawn_point(index);
                ctx.world.spawn((
                    Replicated,
                    Simulated,
                    space3d_replication::ControlledBy(j.identity.player),
                    Transform::from_xyz(p.x, p.y, p.z),
                    Name(j.identity.display_name.clone()),
                    PlayerColor((index % 8) as u8),
                    Health(MAX_HEALTH),
                    Ammo(MAX_AMMO),
                    Score(0),
                    InterestSource { radius: 0.0 },
                    LagCompensated {
                        shape: HitShape::Capsule {
                            radius: 0.5,
                            half_height: 0.5,
                        },
                    },
                    Pawn {
                        player: j.identity.player,
                        name: j.identity.display_name.clone(),
                        index,
                        last_fire: None,
                        reload: 0,
                    },
                ));
            }
            let gone: Vec<PlayerId> = left
                .read(ctx.resources.expect::<Events<PlayerLeft>>())
                .map(|l| l.player)
                .collect();
            if !gone.is_empty() {
                let pawns: Vec<Entity> = ctx
                    .world
                    .query::<(Entity, &Pawn)>()
                    .iter()
                    .filter(|(_, p)| gone.contains(&p.player))
                    .map(|(e, _)| e)
                    .collect();
                for e in pawns {
                    let _ = ctx.world.despawn(e);
                }
            }
        });
        app.add_system_named(Stage::Update, "arena::shoot", shoot);
        let mut says = EventReader::default();
        app.add_system_named(Stage::Update, "arena::chat", move |ctx| {
            let lines: Vec<(PlayerId, String)> = says
                .read(ctx.resources.expect::<Events<FromClient<Say>>>())
                .map(|m| (m.player, m.message.0.clone()))
                .collect();
            for (player, text) in lines {
                let from = ctx
                    .world
                    .query::<&Pawn>()
                    .iter()
                    .find(|p| p.player == player)
                    .map(|p| p.name.clone())
                    .unwrap_or_default();
                ctx.resources
                    .expect_mut::<NetServer>()
                    .send(Target::All, &Chat { from, text });
            }
        });
    }
}

struct Shot {
    shooter: Entity,
    player: PlayerId,
    client: NetClientId,
    origin: Vec3,
    dir: Vec3,
}

/// Fire, reload, hit-scan with lag compensation, damage, kills.
fn shoot(ctx: &mut SystemCtx) {
    let Some(server) = ctx.resources.get::<NetServer>() else {
        return;
    };
    let tick = server.tick();
    let clients: Vec<(PlayerId, NetClientId)> = server
        .clients()
        .into_iter()
        .map(|c| (c.identity.player, c.client))
        .collect();
    let mut shots = Vec::new();
    for (e, t, input, pawn, ammo) in ctx
        .world
        .query_mut::<(Entity, &Transform, &PlayerInput<ArenaInput>, &mut Pawn, &mut Ammo)>()
    {
        let seq = input.0.fire_seq;
        let presses = pawn.last_fire.map_or(0, |last| seq.wrapping_sub(last)).min(3);
        pawn.last_fire = Some(seq);
        if ammo.0 < MAX_AMMO {
            pawn.reload += 1;
            if pawn.reload >= RELOAD_TICKS {
                pawn.reload = 0;
                ammo.0 += 1;
            }
        }
        let Some(client) = clients.iter().find(|(p, _)| *p == pawn.player).map(|(_, c)| *c) else {
            continue;
        };
        for _ in 0..presses {
            if ammo.0 == 0 {
                break;
            }
            ammo.0 -= 1;
            shots.push(Shot {
                shooter: e,
                player: pawn.player,
                client,
                origin: t.translation + Vec3::Y * 0.5,
                dir: forward(input.0.yaw_radians()),
            });
        }
    }
    if shots.is_empty() {
        return;
    }
    let hits: Vec<(Shot, Entity, Vec3)> = {
        let Some(lc) = ctx.resources.get::<LagCompensation>() else {
            return;
        };
        shots
            .into_iter()
            .filter_map(|s| {
                let hit = lc.raycast(s.client, tick, s.origin, s.dir, RANGE, Some(s.shooter))?;
                Some((s, hit.entity, hit.point))
            })
            .collect()
    };
    for (shot, victim, point) in hits {
        let Ok(target) = ctx.world.get::<&Pawn>(victim).map(|p| p.player) else {
            continue;
        };
        let kill = {
            let Ok(mut hp) = ctx.world.get::<&mut Health>(victim) else {
                continue;
            };
            hp.0 = hp.0.saturating_sub(DAMAGE);
            hp.0 == 0
        };
        if kill {
            let index = ctx.world.get::<&Pawn>(victim).map(|p| p.index).unwrap_or(0);
            if let Ok(mut t) = ctx.world.get::<&mut Transform>(victim) {
                t.translation = spawn_point(index + 3);
            }
            if let Ok(mut hp) = ctx.world.get::<&mut Health>(victim) {
                hp.0 = MAX_HEALTH;
            }
            if let Ok(mut ammo) = ctx.world.get::<&mut Ammo>(victim) {
                ammo.0 = MAX_AMMO;
            }
            if let Ok(mut score) = ctx.world.get::<&mut Score>(shot.shooter) {
                score.0 += 1;
            }
        }
        ctx.resources.expect_mut::<NetServer>().send(
            Target::All,
            &Hit {
                shooter: shot.player,
                target,
                point: point.to_array(),
                kill,
            },
        );
    }
}
