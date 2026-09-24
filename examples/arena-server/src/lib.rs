//! Arena example: the dedicated, authoritative game server.
//!
//! [`ArenaGame`] plugs the shared protocol ([`arena_shared::ArenaProtocol`]) and the
//! authoritative rules ([`arena_shared::rules::ArenaRules`]) into `space3d-server`, and adds the
//! server-only match flow:
//!
//! * **Matches** (`duel` / `team` rooms from matchmaking, `RoomSpec::match_id` set) end at
//!   `score_limit` kills or after `time_limit_s` (room params, from the queue's `params` in the
//!   config). The result — placements, winning team, per-player kills — goes through the
//!   durable outbox to the player service (match history, Elo, `arena.kills` leaderboard), then
//!   the room closes.
//! * **Drop-in rooms** (`ffa` quick play, `dev`) never end; a leaving player's kills are
//!   submitted to the `arena.ffa` leaderboard.
//! * **Recovery**: a room restored from a checkpoint (crash or server loss) still has the
//!   replicated pawns but not their server-only `Pawn` state; a rejoining player's old pawn is
//!   replaced by a fresh one that keeps its score.
//!
//! Run it: `cargo run -p arena-server -- --config server.toml` (see
//! `crates/space3d-server/deploy/server.example.toml`; `role = "all-in-one"` for a single
//! process, `"game-server"` next to a stock `space3d-server` player service).
#![cfg(not(target_arch = "wasm32"))]

pub mod bot;

use arena_shared::rules::{ArenaRules, Pawn};
use arena_shared::{ArenaProtocol, GAME_ID, Score};
use space3d_core::prelude::*;
use space3d_replication::prelude::*;
use space3d_server::game::{GameDefinition, RecoveryPolicy, RoomSpec};
use space3d_server::gs::room::{MatchResult, PlayerResult, RoomContext, RoomControl};
use std::collections::BTreeMap;

/// Game modes this server hosts.
pub const MODES: &[&str] = &["dev", "ffa", "duel", "team"];
/// Default kill limit of a match.
pub const DEFAULT_SCORE_LIMIT: u32 = 10;
/// Default match length (seconds).
pub const DEFAULT_TIME_LIMIT_S: u32 = 300;
/// Seconds between the end of a match and the room closing.
pub const CLOSE_AFTER_S: u32 = 5;

/// The arena's server-side definition.
#[derive(Clone, Copy, Debug, Default)]
pub struct ArenaGame;

impl GameDefinition for ArenaGame {
    fn game_id(&self) -> &str {
        GAME_ID
    }

    fn protocol(&self, app: &mut App) {
        app.add_plugin(ArenaProtocol);
    }

    fn build_room(&self, app: &mut App, spec: &RoomSpec) {
        // Stage order is registration order: adoption runs before the rules spawn pawns, the
        // match flow after them.
        app.add_plugin(ArenaRecovery);
        app.add_plugin(ArenaRules);
        app.add_plugin(ArenaMatch::from_spec(spec));
    }

    fn validate_spec(&self, spec: &RoomSpec) -> Result<(), String> {
        if !MODES.contains(&spec.mode.as_str()) {
            return Err(format!("unknown mode `{}`", spec.mode));
        }
        for key in ["score_limit", "time_limit_s"] {
            if let Some(v) = spec.params.get(key)
                && v.parse::<u32>().is_err()
            {
                return Err(format!("param `{key}` must be an integer"));
            }
        }
        Ok(())
    }

    fn recovery_policy(&self, _spec: &RoomSpec) -> RecoveryPolicy {
        RecoveryPolicy::Restore
    }
}

/// Rejoining players get a fresh pawn with their restored score.
struct ArenaRecovery;

/// Scores taken from restored pawns, applied once the rules spawned the replacements.
#[derive(Default)]
struct Adopted(BTreeMap<PlayerId, u32>);

impl Plugin for ArenaRecovery {
    fn build(&self, app: &mut App) {
        app.init_resource::<Adopted>();
        let mut joined = EventReader::default();
        app.add_system_named(Stage::Update, "arena::adopt_restored", move |ctx| {
            let players: Vec<PlayerId> = joined
                .read(ctx.resources.expect::<Events<PlayerJoined>>())
                .map(|j| j.identity.player)
                .collect();
            for player in players {
                // Restored pawns carry `ControlledBy` + `Score` but no server-only `Pawn`.
                let restored: Vec<(Entity, u32)> = ctx
                    .world
                    .query::<(Entity, &ControlledBy, &Score)>()
                    .without::<&Pawn>()
                    .iter()
                    .filter(|(_, c, _)| c.0 == player)
                    .map(|(e, _, s)| (e, s.0))
                    .collect();
                for (e, score) in restored {
                    let _ = ctx.world.despawn(e);
                    ctx.resources.expect_mut::<Adopted>().0.insert(player, score);
                }
            }
        });
    }
}

/// Server-only match flow (see the crate docs).
#[derive(Clone, Debug)]
pub struct ArenaMatch {
    /// Kills that end a match.
    pub score_limit: u32,
    /// Match length (seconds).
    pub time_limit_s: u32,
}

impl ArenaMatch {
    /// Limits from the room params (`score_limit`, `time_limit_s`).
    pub fn from_spec(spec: &RoomSpec) -> Self {
        let param = |k: &str, d: u32| spec.params.get(k).and_then(|v| v.parse().ok()).unwrap_or(d);
        Self {
            score_limit: param("score_limit", DEFAULT_SCORE_LIMIT),
            time_limit_s: param("time_limit_s", DEFAULT_TIME_LIMIT_S),
        }
    }
}

#[derive(Default)]
struct MatchState {
    started_tick: Option<u32>,
    started_ms: i64,
    ended_tick: Option<u32>,
    last_scores: BTreeMap<PlayerId, u32>,
}

/// Wall time for the result's informational timestamps (ticks drive the match itself).
fn now_ms() -> i64 {
    use space3d_server::clock::UnixClock;
    space3d_server::clock::SystemUnixClock.now_ms()
}

impl Plugin for ArenaMatch {
    fn build(&self, app: &mut App) {
        let limits = self.clone();
        let mut state = MatchState::default();
        let mut left = EventReader::default();
        app.add_system_named(Stage::Update, "arena::match", move |ctx| {
            // Restored scores → replacement pawns.
            let adopted = std::mem::take(&mut ctx.resources.expect_mut::<Adopted>().0);
            for (player, score) in adopted {
                for (c, s) in ctx.world.query_mut::<(&ControlledBy, &mut Score)>() {
                    if c.0 == player {
                        s.0 = s.0.max(score);
                    }
                }
            }
            let spec = ctx.resources.expect::<RoomContext>().spec.clone();
            let tick = ctx.resources.expect::<NetServer>().tick().0;
            let hz = ctx.resources.expect::<TickRate>().hz.max(1.0);
            let scores: BTreeMap<PlayerId, u32> = ctx
                .world
                .query::<(&ControlledBy, &Score)>()
                .iter()
                .map(|(c, s)| (c.0, s.0))
                .collect();
            let leavers: Vec<PlayerId> = left
                .read(ctx.resources.expect::<Events<PlayerLeft>>())
                .map(|l| l.player)
                .collect();
            if spec.match_id.is_none() {
                // Drop-in room: bank a leaver's kills on the ffa board.
                for p in leavers {
                    if let Some(&kills) = state.last_scores.get(&p)
                        && kills > 0
                    {
                        ctx.resources
                            .expect_mut::<RoomControl>()
                            .submit_score("arena.ffa", p, i64::from(kills));
                    }
                }
                state.last_scores = scores;
                return;
            }
            if let Some(ended) = state.ended_tick {
                if f64::from(tick.saturating_sub(ended)) >= f64::from(CLOSE_AFTER_S) * hz {
                    ctx.resources.expect_mut::<RoomControl>().close("match over");
                }
                return;
            }
            let Some(started) = state.started_tick else {
                if !scores.is_empty() {
                    state.started_tick = Some(tick);
                    state.started_ms = now_ms();
                }
                return;
            };
            let elapsed_s = f64::from(tick.saturating_sub(started)) / hz;
            let top = scores.values().copied().max().unwrap_or(0);
            if top < limits.score_limit && elapsed_s < f64::from(limits.time_limit_s) {
                return;
            }
            state.ended_tick = Some(tick);
            let result = match_result(&spec, &scores, state.started_ms);
            let control = ctx.resources.expect_mut::<RoomControl>();
            control.end_match(result);
        });
    }
}

/// Build the result of a finished match: placements by kills, winner team by summed kills
/// (none on a tie), `arena.kills` leaderboard submissions.
pub fn match_result(spec: &RoomSpec, scores: &BTreeMap<PlayerId, u32>, started_ms: i64) -> MatchResult {
    let team_of = |p: PlayerId| spec.teams.iter().position(|t| t.contains(&p)).map(|t| t as u16);
    let mut ranked: Vec<(PlayerId, u32)> = scores.iter().map(|(p, s)| (*p, *s)).collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut team_kills: BTreeMap<u16, u32> = BTreeMap::new();
    for (p, s) in &ranked {
        if let Some(t) = team_of(*p) {
            *team_kills.entry(t).or_insert(0) += s;
        }
    }
    let best = team_kills.values().copied().max();
    let leaders: Vec<u16> = team_kills
        .iter()
        .filter(|(_, k)| Some(**k) == best)
        .map(|(t, _)| *t)
        .collect();
    let winner_team = (leaders.len() == 1).then(|| leaders[0]);
    let mut placement = 0u16;
    let mut previous = None;
    let players = ranked
        .iter()
        .enumerate()
        .map(|(i, (p, s))| {
            if previous != Some(*s) {
                placement = i as u16 + 1;
                previous = Some(*s);
            }
            PlayerResult {
                player: *p,
                team: team_of(*p),
                score: i64::from(*s),
                placement: Some(placement),
                stats: serde_json::json!({ "kills": s }),
            }
        })
        .collect();
    MatchResult {
        match_id: spec.match_id.unwrap_or_default(),
        room_id: spec.room_id,
        mode: spec.mode.clone(),
        started_ms,
        ended_ms: now_ms(),
        winner_team,
        players,
        scores: ranked
            .iter()
            .filter(|(_, s)| *s > 0)
            .map(|(p, s)| ("arena.kills".to_owned(), *p, i64::from(*s)))
            .collect(),
        data: serde_json::Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use space3d_replication::MatchId;

    #[test]
    fn results_rank_players_and_pick_the_winning_team() {
        let (a, b, c) = (PlayerId(1), PlayerId(2), PlayerId(3));
        let spec = RoomSpec {
            mode: "team".into(),
            match_id: Some(MatchId(9)),
            teams: vec![vec![a, c], vec![b]],
            ..Default::default()
        };
        let scores = BTreeMap::from([(a, 3), (b, 5), (c, 3)]);
        let r = match_result(&spec, &scores, 0);
        assert_eq!(r.winner_team, Some(0), "6 team kills beat 5");
        let places: Vec<(PlayerId, Option<u16>)> = r.players.iter().map(|p| (p.player, p.placement)).collect();
        assert_eq!(places, vec![(b, Some(1)), (a, Some(2)), (c, Some(2))]);
        assert_eq!(r.scores.len(), 3);
        let tie = match_result(&spec, &BTreeMap::from([(a, 1), (b, 1)]), 0);
        assert_eq!(tie.winner_team, None);
    }

    #[test]
    fn specs_are_validated() {
        let ok = RoomSpec {
            mode: "duel".into(),
            params: BTreeMap::from([("time_limit_s".into(), "60".into())]),
            ..Default::default()
        };
        assert!(ArenaGame.validate_spec(&ok).is_ok());
        let bad_mode = RoomSpec {
            mode: "golf".into(),
            ..Default::default()
        };
        assert!(ArenaGame.validate_spec(&bad_mode).is_err());
        let bad_param = RoomSpec {
            params: BTreeMap::from([("score_limit".into(), "lots".into())]),
            ..ok
        };
        assert!(ArenaGame.validate_spec(&bad_param).is_err());
        assert_eq!(
            ArenaMatch::from_spec(&RoomSpec::default()).score_limit,
            DEFAULT_SCORE_LIMIT
        );
    }
}
