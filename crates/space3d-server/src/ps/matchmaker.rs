//! Pure, deterministic matchmaker (driven by the supervisor every `matchmaking.interval`;
//! unit-tested by calling [`Matchmaker::tick`] directly).
//!
//! Per queue and region: tickets oldest-first; a match forms when `teams × team_size` players
//! fit one rating window (starts at `rating_window`, grows by `rating_window_growth`/s waited)
//! and share a region within `region_ping_ms` (widening to `region_ping_max_ms` after 30 s).
//! After `max_wait`, starts with ≥ `min_players` (games may backfill with server bots) or
//! expires. `backfill` queues first fill open rooms with free seats (fleet heartbeats).
//! Allocation failures put the players back at the front of the queue.

use crate::config::QueueConfig;
use space3d_replication::PlayerId;
use space3d_replication::api::TicketId;
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
    /// Region RTTs.
    pub region_pings: BTreeMap<String, u32>,
    /// Enqueue time (unix ms).
    pub enqueued_ms: i64,
}

/// A formed match.
#[derive(Clone, Debug, PartialEq)]
pub struct MatchFormed {
    /// Queue.
    pub queue: String,
    /// Mode.
    pub mode: String,
    /// Chosen region.
    pub region: String,
    /// Teams.
    pub teams: Vec<Vec<QueueEntry>>,
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
    /// Matched; assignment pending allocation or ready.
    Matched(Option<space3d_replication::api::Assignment>),
    /// Expired.
    Expired,
    /// Cancelled.
    Cancelled,
}

/// Queues.
#[derive(Debug, Default)]
pub struct Matchmaker {
    queues: BTreeMap<String, (QueueConfig, Vec<QueueEntry>)>,
    tickets: BTreeMap<TicketId, TicketState>,
}

impl Matchmaker {
    /// Build from config.
    pub fn new(queues: &[QueueConfig]) -> Self {
        todo!()
    }

    /// Enqueue.
    pub fn enqueue(&mut self, queue: &str, entry: QueueEntry) -> Result<(), MatchmakingError> {
        todo!()
    }

    /// Cancel; `false` if unknown or already matched.
    pub fn cancel(&mut self, ticket: &TicketId) -> bool {
        todo!()
    }

    /// Poll a ticket.
    pub fn state(&self, ticket: &TicketId, now_ms: i64) -> Option<TicketState> {
        todo!()
    }

    /// Form matches at `now_ms`.
    pub fn tick(&mut self, now_ms: i64) -> Vec<MatchFormed> {
        todo!()
    }

    /// Attach assignments to matched tickets (after allocation).
    pub fn resolve(&mut self, ticket: &TicketId, assignment: space3d_replication::api::Assignment) {
        todo!()
    }

    /// Allocation failed: requeue at the front.
    pub fn requeue(&mut self, formed: MatchFormed) {
        todo!()
    }
}
