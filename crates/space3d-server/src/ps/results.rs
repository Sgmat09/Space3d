//! Match results → ratings. Elo with K = 32 (team Elo: team average vs. team average, each
//! player moved by the team delta; with more than two teams every pair of teams is scored and
//! the deltas are averaged); Glicko-2 deferred.

use space3d_replication::PlayerId;
use std::collections::BTreeMap;

/// Default K factor.
pub const ELO_K: f64 = 32.0;

/// Expected score of rating `a` against `b`.
pub fn expected(a: f64, b: f64) -> f64 {
    1.0 / (1.0 + 10f64.powf((b - a) / 400.0))
}

/// New ratings after a match: `players` are `(player, rating, team)`, `winner` the winning
/// team (`None` = draw). Players are returned in input order. A match with a single team
/// changes nothing.
pub fn elo_update(players: &[(PlayerId, f64, u16)], winner: Option<u16>, k: f64) -> Vec<(PlayerId, f64)> {
    let mut teams: BTreeMap<u16, (f64, usize)> = BTreeMap::new();
    for (_, rating, team) in players {
        let t = teams.entry(*team).or_insert((0.0, 0));
        t.0 += rating;
        t.1 += 1;
    }
    let avg: BTreeMap<u16, f64> = teams.iter().map(|(t, (sum, n))| (*t, sum / *n as f64)).collect();
    let others = avg.len().saturating_sub(1);
    let delta: BTreeMap<u16, f64> = avg
        .iter()
        .map(|(t, r)| {
            if others == 0 {
                return (*t, 0.0);
            }
            let sum: f64 = avg
                .iter()
                .filter(|(o, _)| *o != t)
                .map(|(o, ro)| {
                    let actual = match winner {
                        Some(w) if w == *t => 1.0,
                        Some(w) if w == *o => 0.0,
                        _ => 0.5,
                    };
                    actual - expected(*r, *ro)
                })
                .sum();
            (*t, k * sum / others as f64)
        })
        .collect();
    players
        .iter()
        .map(|(p, rating, team)| (*p, rating + delta[team]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_duel_moves_sixteen_points() {
        let out = elo_update(&[(PlayerId(1), 1500.0, 0), (PlayerId(2), 1500.0, 1)], Some(0), ELO_K);
        assert!((out[0].1 - 1516.0).abs() < 1e-9);
        assert!((out[1].1 - 1484.0).abs() < 1e-9);
    }

    #[test]
    fn upsets_move_more_and_draws_pull_together() {
        let upset = elo_update(&[(PlayerId(1), 1200.0, 0), (PlayerId(2), 1800.0, 1)], Some(0), ELO_K);
        assert!(upset[0].1 - 1200.0 > 30.0);
        let draw = elo_update(&[(PlayerId(1), 1200.0, 0), (PlayerId(2), 1800.0, 1)], None, ELO_K);
        assert!(draw[0].1 > 1200.0 && draw[1].1 < 1800.0);
        // Zero-sum for equal team sizes.
        let sum: f64 = draw.iter().map(|(_, r)| r).sum();
        assert!((sum - 3000.0).abs() < 1e-9);
    }

    #[test]
    fn team_average_and_ffa() {
        let out = elo_update(
            &[
                (PlayerId(1), 1600.0, 0),
                (PlayerId(2), 1400.0, 0),
                (PlayerId(3), 1500.0, 1),
                (PlayerId(4), 1500.0, 1),
            ],
            Some(1),
            ELO_K,
        );
        assert!((out[0].1 - 1584.0).abs() < 1e-9, "each member moves by the team delta");
        assert!((out[2].1 - 1516.0).abs() < 1e-9);
        let ffa = elo_update(
            &[
                (PlayerId(1), 1500.0, 0),
                (PlayerId(2), 1500.0, 1),
                (PlayerId(3), 1500.0, 2),
            ],
            Some(2),
            ELO_K,
        );
        assert!(ffa[2].1 > 1500.0 && ffa[0].1 < 1500.0 && (ffa[0].1 - ffa[1].1).abs() < 1e-9);
        assert_eq!(elo_update(&[(PlayerId(1), 1500.0, 0)], Some(0), ELO_K)[0].1, 1500.0);
    }
}
