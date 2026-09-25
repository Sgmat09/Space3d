//! Pure, deterministic matchmaker (driven by the supervisor every `matchmaking.interval`;
//! unit-tested by calling [`Matchmaker::tick`] directly).
//!
//! Per queue: tickets oldest-first. The oldest waiting ticket anchors a candidate match: a
//! match forms when `teams × team_size` players fit the anchor's rating window (starts at
//! `rating_window`, grows by `rating_window_growth` per second waited) and share a region
//! within their ping threshold (`region_ping_ms`, widening to `region_ping_max_ms` after 30 s).
//! After `max_wait` the anchor accepts any rating and starts short with ≥ `min_players`, or
//! expires. Teams are balanced by a rating snake draft. `backfill` queues first fill open
//! rooms with free seats (done by the supervisor with [`Matchmaker::searching`] +
//! [`Matchmaker::assign_direct`]). Allocation failures put the players back at the front of
//! the queue ([`Matchmaker::requeue`]).

use crate::config::QueueConfig;
use space3d_replication::PlayerId;
use space3d_replication::api::{Assignment, TicketId};
use std::collections::BTreeMap;

/// A queued player.
#[derive(Clone, Debug, PartialEq)]
pub struct QueueEntry {
    /// Ticket.
    pub ticket: TicketId,
    /// Player.
    pub player: PlayerId,
    /// Name.
    pub display_name: String,
    /// Rating.
    pub rating: f64,
    /// Region RTTs (empty = no preference).
    pub region_pings: BTreeMap<String, u32>,
    /// Enqueue time (unix ms).
    pub enqueued_ms: i64,
    /// Client build: players are only matched with the same build (they must share a
    /// compatible game server).
    pub build: crate::ps::fleet::Build,
}

/// A formed match.
#[derive(Clone, Debug, PartialEq)]
pub struct MatchFormed {
    /// Queue.
    pub queue: String,
    /// Mode.
    pub mode: String,
    /// Chosen region (`""` = any).
    pub region: String,
    /// Teams.
    pub teams: Vec<Vec<QueueEntry>>,
}

impl MatchFormed {
    /// All players, team by team.
    pub fn players(&self) -> impl Iterator<Item = (u16, &QueueEntry)> {
        self.teams
            .iter()
            .enumerate()
            .flat_map(|(t, members)| members.iter().map(move |e| (t as u16, e)))
    }
}

/// Matchmaking failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MatchmakingError {
    /// Unknown queue.
    #[error("unknown queue {0}")]
    UnknownQueue(String),
    /// Player already queued.
    #[error("already queued")]
    AlreadyQueued,
}

/// Ticket state as seen by the poller.
#[derive(Clone, Debug, PartialEq)]
pub enum TicketState {
    /// Waiting.
    Searching {
        /// Waited ms.
        waited_ms: i64,
    },
    /// Matched; assignment pending allocation (`None`) or ready.
    Matched(Option<Assignment>),
    /// Expired.
    Expired,
    /// Cancelled.
    Cancelled,
}

#[derive(Clone, Debug)]
struct TicketInfo {
    queue: String,
    player: PlayerId,
    state: TicketState,
    enqueued_ms: i64,
    /// When the ticket reached a terminal state (pruned 2 minutes later).
    finished_ms: Option<i64>,
}

/// How long finished tickets stay pollable.
const FINISHED_TTL_MS: i64 = 120_000;
/// Region thresholds widen after this long.
const REGION_WIDEN_MS: i64 = 30_000;

/// Queues.
#[derive(Debug, Default)]
pub struct Matchmaker {
    queues: BTreeMap<String, (QueueConfig, Vec<QueueEntry>)>,
    tickets: BTreeMap<TicketId, TicketInfo>,
}

impl Matchmaker {
    /// Build from config.
    pub fn new(queues: &[QueueConfig]) -> Self {
        Self {
            queues: queues
                .iter()
                .map(|q| (q.name.clone(), (q.clone(), Vec::new())))
                .collect(),
            tickets: BTreeMap::new(),
        }
    }

    /// Queue config.
    pub fn queue_config(&self, queue: &str) -> Option<&QueueConfig> {
        self.queues.get(queue).map(|(c, _)| c)
    }

    /// Queue names.
    pub fn queue_names(&self) -> Vec<String> {
        self.queues.keys().cloned().collect()
    }

    /// Enqueue.
    pub fn enqueue(&mut self, queue: &str, entry: QueueEntry) -> Result<(), MatchmakingError> {
        if !self.queues.contains_key(queue) {
            return Err(MatchmakingError::UnknownQueue(queue.to_owned()));
        }
        let busy = self.tickets.values().any(|t| {
            t.player == entry.player && matches!(t.state, TicketState::Searching { .. } | TicketState::Matched(None))
        });
        if busy || self.tickets.contains_key(&entry.ticket) {
            return Err(MatchmakingError::AlreadyQueued);
        }
        self.tickets.insert(
            entry.ticket.clone(),
            TicketInfo {
                queue: queue.to_owned(),
                player: entry.player,
                state: TicketState::Searching { waited_ms: 0 },
                enqueued_ms: entry.enqueued_ms,
                finished_ms: None,
            },
        );
        self.queues.get_mut(queue).expect("checked").1.push(entry);
        Ok(())
    }

    /// Cancel; `false` if unknown or already matched.
    pub fn cancel(&mut self, ticket: &TicketId) -> bool {
        let Some(info) = self.tickets.get_mut(ticket) else {
            return false;
        };
        if !matches!(info.state, TicketState::Searching { .. }) {
            return false;
        }
        info.state = TicketState::Cancelled;
        info.finished_ms = Some(info.enqueued_ms);
        if let Some((_, q)) = self.queues.get_mut(&info.queue) {
            q.retain(|e| &e.ticket != ticket);
        }
        true
    }

    /// Owner of a ticket.
    pub fn owner(&self, ticket: &TicketId) -> Option<PlayerId> {
        self.tickets.get(ticket).map(|t| t.player)
    }

    /// Poll a ticket.
    pub fn state(&self, ticket: &TicketId, now_ms: i64) -> Option<TicketState> {
        self.tickets.get(ticket).map(|t| match &t.state {
            TicketState::Searching { .. } => TicketState::Searching {
                waited_ms: now_ms.saturating_sub(t.enqueued_ms).max(0),
            },
            other => other.clone(),
        })
    }

    /// Entries waiting in `queue` (oldest first).
    pub fn searching(&self, queue: &str) -> Vec<QueueEntry> {
        self.queues.get(queue).map(|(_, q)| q.clone()).unwrap_or_default()
    }

    /// Queued players per `(queue, best region)`.
    pub fn queue_sizes(&self) -> BTreeMap<(String, String), usize> {
        let mut out = BTreeMap::new();
        for (name, (_, entries)) in &self.queues {
            out.entry((name.clone(), "any".to_owned())).or_insert(0);
            for e in entries {
                let region = e
                    .region_pings
                    .iter()
                    .min_by_key(|(_, p)| **p)
                    .map_or_else(|| "any".to_owned(), |(r, _)| r.clone());
                *out.entry((name.clone(), region)).or_insert(0) += 1;
            }
        }
        out
    }

    /// Hand a searching ticket straight to an assignment (backfill into an open room).
    pub fn assign_direct(&mut self, ticket: &TicketId, assignment: Assignment, now_ms: i64) -> bool {
        let Some(info) = self.tickets.get_mut(ticket) else {
            return false;
        };
        if !matches!(info.state, TicketState::Searching { .. }) {
            return false;
        }
        info.state = TicketState::Matched(Some(assignment));
        info.finished_ms = Some(now_ms);
        if let Some((_, q)) = self.queues.get_mut(&info.queue) {
            q.retain(|e| &e.ticket != ticket);
        }
        true
    }

    /// Form matches at `now_ms` (also expires overdue tickets and prunes old finished ones).
    pub fn tick(&mut self, now_ms: i64) -> Vec<MatchFormed> {
        self.tickets
            .retain(|_, t| t.finished_ms.is_none_or(|f| now_ms.saturating_sub(f) < FINISHED_TTL_MS));
        let mut formed = Vec::new();
        let mut expired = Vec::new();
        for (name, (cfg, entries)) in &mut self.queues {
            entries.sort_by(|a, b| a.enqueued_ms.cmp(&b.enqueued_ms).then_with(|| a.ticket.cmp(&b.ticket)));
            let need = usize::from(cfg.teams) * usize::from(cfg.team_size);
            let max_wait_ms = cfg.max_wait.as_millis() as i64;
            let mut i = 0;
            while i < entries.len() {
                let anchor = &entries[i];
                let waited = now_ms.saturating_sub(anchor.enqueued_ms).max(0);
                let overdue = waited >= max_wait_ms;
                let picked = pick(cfg, entries, i, now_ms, overdue, need);
                let size = picked.as_ref().map_or(0, |(_, idx)| idx.len());
                if size >= need || (overdue && size >= usize::from(cfg.min_players.max(1))) {
                    let (region, idx) = picked.expect("size > 0");
                    let mut chosen: Vec<QueueEntry> = idx.iter().map(|&j| entries[j].clone()).collect();
                    let mut sorted = idx.clone();
                    sorted.sort_unstable_by(|a, b| b.cmp(a));
                    for j in sorted {
                        entries.remove(j);
                    }
                    chosen.sort_by(|a, b| b.rating.total_cmp(&a.rating).then_with(|| a.ticket.cmp(&b.ticket)));
                    formed.push(MatchFormed {
                        queue: name.clone(),
                        mode: cfg.mode.clone(),
                        region,
                        teams: snake_draft(chosen, usize::from(cfg.teams)),
                    });
                    // Do not advance `i`: the entry now at `i` is the next oldest.
                } else if overdue {
                    expired.push(entries.remove(i).ticket);
                } else {
                    i += 1;
                }
            }
        }
        for m in &formed {
            for (_, e) in m.players() {
                if let Some(t) = self.tickets.get_mut(&e.ticket) {
                    t.state = TicketState::Matched(None);
                }
            }
        }
        for t in expired {
            if let Some(info) = self.tickets.get_mut(&t) {
                info.state = TicketState::Expired;
                info.finished_ms = Some(now_ms);
            }
        }
        formed
    }

    /// Attach assignments to matched tickets (after allocation).
    pub fn resolve(&mut self, ticket: &TicketId, assignment: Assignment) {
        if let Some(t) = self.tickets.get_mut(ticket) {
            // Pollable until two minutes after the join ticket expires.
            t.finished_ms = Some(assignment.expires_ms);
            t.state = TicketState::Matched(Some(assignment));
        }
    }

    /// Allocation failed: requeue at the front (original enqueue times are kept, so the
    /// players stay the oldest).
    pub fn requeue(&mut self, formed: MatchFormed) {
        for (_, e) in formed.players() {
            if let Some(t) = self.tickets.get_mut(&e.ticket) {
                if !matches!(t.state, TicketState::Matched(None)) {
                    continue;
                }
                t.state = TicketState::Searching { waited_ms: 0 };
            }
            if let Some((_, q)) = self.queues.get_mut(&formed.queue) {
                q.insert(0, e.clone());
            }
        }
    }
}

/// Whether `e` accepts `region` given how long it waited.
fn region_ok(cfg: &QueueConfig, e: &QueueEntry, region: &str, now_ms: i64, overdue: bool) -> bool {
    if region.is_empty() || e.region_pings.is_empty() {
        return true;
    }
    let waited = now_ms.saturating_sub(e.enqueued_ms);
    let threshold = if overdue || waited >= REGION_WIDEN_MS {
        cfg.region_ping_max_ms
    } else {
        cfg.region_ping_ms
    };
    e.region_pings.get(region).is_some_and(|p| *p <= threshold)
}

/// Candidate regions of the anchor, best first (`""` when it has no pings).
fn anchor_regions(cfg: &QueueConfig, anchor: &QueueEntry, now_ms: i64, overdue: bool) -> Vec<String> {
    if anchor.region_pings.is_empty() {
        return vec![String::new()];
    }
    let mut regions: Vec<(&String, &u32)> = anchor
        .region_pings
        .iter()
        .filter(|(r, _)| region_ok(cfg, anchor, r, now_ms, overdue))
        .collect();
    regions.sort_by(|a, b| a.1.cmp(b.1).then_with(|| a.0.cmp(b.0)));
    let mut out: Vec<String> = regions.into_iter().map(|(r, _)| r.clone()).collect();
    if out.is_empty() {
        // Nothing under the threshold: still try the anchor's best region.
        if let Some((r, _)) = anchor.region_pings.iter().min_by_key(|(_, p)| **p) {
            out.push(r.clone());
        }
    }
    out
}

/// Best group for anchor `i`: `(region, indices incl. the anchor)` — the first region (by the
/// anchor's ping) that fills the match, else the largest group.
fn pick(
    cfg: &QueueConfig,
    entries: &[QueueEntry],
    i: usize,
    now_ms: i64,
    overdue: bool,
    need: usize,
) -> Option<(String, Vec<usize>)> {
    let anchor = &entries[i];
    let waited_s = now_ms.saturating_sub(anchor.enqueued_ms).max(0) as f64 / 1000.0;
    let window = if overdue {
        f64::INFINITY
    } else {
        cfg.rating_window + cfg.rating_window_growth * waited_s
    };
    let mut best: Option<(String, Vec<usize>)> = None;
    for region in anchor_regions(cfg, anchor, now_ms, overdue) {
        let mut idx = vec![i];
        for (j, e) in entries.iter().enumerate() {
            if idx.len() >= need {
                break;
            }
            if j == i || e.player == anchor.player || e.build != anchor.build {
                continue;
            }
            let e_window = cfg.rating_window
                + cfg.rating_window_growth * (now_ms.saturating_sub(e.enqueued_ms).max(0) as f64 / 1000.0);
            if (e.rating - anchor.rating).abs() > window.max(e_window) {
                continue;
            }
            if !region_ok(cfg, e, &region, now_ms, overdue) {
                continue;
            }
            idx.push(j);
        }
        let full = idx.len() >= need;
        if best.as_ref().is_none_or(|(_, b)| idx.len() > b.len()) {
            best = Some((region, idx));
        }
        if full {
            break;
        }
    }
    best
}

/// Split rating-sorted players into `teams` by a snake draft (0,1,1,0,0,1,...).
fn snake_draft(players: Vec<QueueEntry>, teams: usize) -> Vec<Vec<QueueEntry>> {
    let teams = teams.max(1);
    let mut out = vec![Vec::new(); teams];
    for (n, p) in players.into_iter().enumerate() {
        let round = n / teams;
        let pos = n % teams;
        let t = if round.is_multiple_of(2) { pos } else { teams - 1 - pos };
        out[t].push(p);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn q(name: &str, teams: u16, team_size: u16) -> QueueConfig {
        QueueConfig {
            name: name.into(),
            mode: name.into(),
            teams,
            team_size,
            min_players: 2,
            max_wait: Duration::from_secs(60),
            rating_window: 100.0,
            rating_window_growth: 10.0,
            region_ping_ms: 80,
            region_ping_max_ms: 150,
            backfill: false,
            params: Default::default(),
        }
    }

    fn e(id: u64, rating: f64, at: i64, pings: &[(&str, u32)]) -> QueueEntry {
        QueueEntry {
            build: Default::default(),
            ticket: TicketId(format!("t{id}")),
            player: PlayerId(id),
            display_name: format!("p{id}"),
            rating,
            region_pings: pings.iter().map(|(r, p)| ((*r).to_owned(), *p)).collect(),
            enqueued_ms: at,
        }
    }

    fn assignment() -> Assignment {
        Assignment {
            server_url: "ws://gs".into(),
            server_id: "gs-1".into(),
            room_id: space3d_replication::RoomId(1),
            join_ticket: "t".into(),
            expires_ms: 61_000,
        }
    }

    /// Players of different builds are never matched together.
    #[test]
    fn matches_only_within_one_build() {
        let cfg = QueueConfig {
            name: "duel".into(),
            mode: "duel".into(),
            ..Default::default()
        };
        let mut m = Matchmaker::new(&[cfg]);
        let mut a = e(1, 1500.0, 0, &[]);
        a.build = crate::ps::fleet::Build::new("arena", 1);
        let mut b = e(2, 1500.0, 0, &[]);
        b.build = crate::ps::fleet::Build::new("arena", 2);
        m.enqueue("duel", a).unwrap();
        m.enqueue("duel", b).unwrap();
        assert!(m.tick(1_000).is_empty(), "different schemas");
        let mut c = e(3, 1500.0, 0, &[]);
        c.build = crate::ps::fleet::Build::new("arena", 2);
        m.enqueue("duel", c).unwrap();
        let formed = m.tick(2_000);
        assert_eq!(formed.len(), 1);
        let ids: Vec<u64> = formed[0].players().map(|(_, e)| e.player.0).collect();
        assert!(ids.contains(&2) && ids.contains(&3), "{ids:?}");
    }

    #[test]
    fn forms_duel_within_rating_window() {
        let mut m = Matchmaker::new(&[q("duel", 2, 1)]);
        m.enqueue("duel", e(1, 1500.0, 0, &[])).unwrap();
        m.enqueue("duel", e(2, 1900.0, 10, &[])).unwrap();
        assert!(m.tick(1_000).is_empty(), "400 apart: outside the window");
        m.enqueue("duel", e(3, 1550.0, 20, &[])).unwrap();
        let formed = m.tick(1_000);
        assert_eq!(formed.len(), 1);
        let ids: Vec<u64> = formed[0].players().map(|(_, e)| e.player.0).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&1) && ids.contains(&3));
        assert_eq!(m.state(&TicketId("t1".into()), 1_000), Some(TicketState::Matched(None)));
        assert!(matches!(
            m.state(&TicketId("t2".into()), 1_000),
            Some(TicketState::Searching { waited_ms: 990 })
        ));
        // The window grows 10/s: after 31 s player 2 (400 away) matches a newcomer at 1900.
        m.enqueue("duel", e(4, 1900.0, 30_000, &[])).unwrap();
        assert_eq!(m.tick(31_000).len(), 1);
    }

    #[test]
    fn duplicate_unknown_cancel() {
        let mut m = Matchmaker::new(&[q("duel", 2, 1)]);
        assert_eq!(
            m.enqueue("nope", e(1, 1500.0, 0, &[])),
            Err(MatchmakingError::UnknownQueue("nope".into()))
        );
        m.enqueue("duel", e(1, 1500.0, 0, &[])).unwrap();
        let mut again = e(1, 1500.0, 0, &[]);
        again.ticket = TicketId("other".into());
        assert_eq!(m.enqueue("duel", again), Err(MatchmakingError::AlreadyQueued));
        assert!(m.cancel(&TicketId("t1".into())));
        assert!(!m.cancel(&TicketId("t1".into())));
        assert_eq!(m.state(&TicketId("t1".into()), 5), Some(TicketState::Cancelled));
        assert!(m.searching("duel").is_empty());
    }

    #[test]
    fn regions_must_overlap_and_widen_after_30s() {
        let mut m = Matchmaker::new(&[q("duel", 2, 1)]);
        m.enqueue("duel", e(1, 1500.0, 0, &[("eu", 30), ("us", 120)])).unwrap();
        m.enqueue("duel", e(2, 1500.0, 0, &[("us", 20)])).unwrap();
        assert!(m.tick(1_000).is_empty(), "eu vs us under 80 ms");
        let formed = m.tick(31_000);
        assert_eq!(
            formed.len(),
            1,
            "after 30 s the 150 ms threshold allows us for player 1"
        );
        assert_eq!(formed[0].region, "us");
    }

    #[test]
    fn max_wait_starts_short_or_expires() {
        let mut cfg = q("squad", 2, 2);
        cfg.min_players = 3;
        let mut m = Matchmaker::new(&[cfg]);
        m.enqueue("squad", e(1, 1000.0, 0, &[])).unwrap();
        m.enqueue("squad", e(2, 2000.0, 0, &[])).unwrap();
        m.enqueue("squad", e(3, 3000.0, 0, &[])).unwrap();
        assert!(m.tick(59_000).is_empty());
        let formed = m.tick(60_000);
        assert_eq!(formed.len(), 1, "short match with 3 ≥ min_players, any rating");
        assert_eq!(formed[0].players().count(), 3);
        assert_eq!(formed[0].teams.len(), 2);
        m.enqueue("squad", e(4, 1000.0, 60_000, &[])).unwrap();
        assert!(m.tick(120_000).is_empty());
        assert_eq!(m.state(&TicketId("t4".into()), 120_000), Some(TicketState::Expired));
        // Finished tickets are pruned after two minutes.
        m.tick(250_000);
        assert_eq!(m.state(&TicketId("t4".into()), 250_000), None);
    }

    #[test]
    fn teams_are_balanced_and_requeue_goes_first() {
        let mut m = Matchmaker::new(&[q("2v2", 2, 2)]);
        for (id, r) in [(1, 1600.0), (2, 1500.0), (3, 1550.0), (4, 1450.0)] {
            m.enqueue("2v2", e(id, r, id as i64, &[])).unwrap();
        }
        let formed = m.tick(100);
        assert_eq!(formed.len(), 1);
        let sum = |t: &Vec<QueueEntry>| t.iter().map(|e| e.rating).sum::<f64>();
        assert_eq!(sum(&formed[0].teams[0]), 1600.0 + 1450.0);
        assert_eq!(sum(&formed[0].teams[1]), 1550.0 + 1500.0);
        m.enqueue("2v2", e(9, 1500.0, 50, &[])).unwrap();
        m.requeue(formed[0].clone());
        let order: Vec<u64> = m.searching("2v2").iter().map(|e| e.player.0).collect();
        assert_eq!(order.len(), 5);
        assert_eq!(*order.last().unwrap(), 9, "requeued players go back to the front");
        let formed = m.tick(200);
        assert_eq!(formed.len(), 1);
        m.resolve(&TicketId("t1".into()), assignment());
        assert!(matches!(
            m.state(&TicketId("t1".into()), 200),
            Some(TicketState::Matched(Some(_)))
        ));
    }

    #[test]
    fn backfill_assign_direct() {
        let mut m = Matchmaker::new(&[q("zone", 1, 16)]);
        m.enqueue("zone", e(1, 1500.0, 0, &[])).unwrap();
        assert!(m.assign_direct(&TicketId("t1".into()), assignment(), 10));
        assert!(m.searching("zone").is_empty());
        assert!(!m.assign_direct(&TicketId("t1".into()), assignment(), 10));
        assert_eq!(m.queue_sizes()[&("zone".to_owned(), "any".to_owned())], 0);
    }
}
