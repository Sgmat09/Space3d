//! Match results → ratings. Elo with K = 32 (team Elo: team average vs. team average, each
//! player moved by the team delta); Glicko-2 deferred.

use space3d_replication::PlayerId;

/// New ratings after a match: `players` are `(player, rating, team)`, `winner` the winning
/// team (`None` = draw).
pub fn elo_update(players: &[(PlayerId, f64, u16)], winner: Option<u16>, k: f64) -> Vec<(PlayerId, f64)> {
    todo!()
}
