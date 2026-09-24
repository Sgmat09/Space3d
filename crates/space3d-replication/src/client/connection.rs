//! Connection state machine (internal to the replication builder):
//!
//! ```text
//! Idle ─connect─▶ Connecting ─open─▶ Handshaking ─Welcome─▶ Syncing ─SYNC_COMPLETE─▶ Connected
//!                     ▲                   │ Reject → Failed(Rejected)                  │ link lost / idle timeout
//!                     └──── Reconnecting{attempt} (backoff; HelloAuth::Resume if we hold a token) ◀┘
//! Disconnect{resume_allowed: false} → Failed(Disconnected)
//! ```
//!
//! Every attempt opens a **fresh** transport via the `Connector` (no buffered stale inputs).
//! A resume that fails with `ResumeExpired` falls back to the original ticket once (tickets
//! are single-use, so this only succeeds for `Dev` targets) and otherwise fails, letting the
//! game ask the player service for a new assignment (`/v1/sessions/rejoin`).

use crate::protocol::handshake::{HelloAuth, Welcome};
use crate::time::{RttEstimator, ServerClock, TickPacer};
use crate::transport::ClientTransport;
use std::time::Duration;

/// Live connection data.
pub struct Connection {
    /// Transport of the current attempt.
    pub transport: Option<Box<dyn ClientTransport>>,
    /// Original credentials.
    pub auth: HelloAuth,
    /// URL.
    pub url: String,
    /// Last Welcome.
    pub welcome: Option<Welcome>,
    /// RTT.
    pub rtt: RttEstimator,
    /// Server tick estimate.
    pub clock: ServerClock,
    /// Pacing.
    pub pacer: TickPacer,
    /// Next reconnect attempt time.
    pub retry_at: Option<Duration>,
    /// Last packet received.
    pub last_rx: Duration,
}
