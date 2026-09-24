//! Sans-IO admission and routing of links (driven by the ingress task; unit-testable with a
//! manual clock).
//!
//! Per link: `Opened` → per-IP limits (`ConnectionLimiter`), IP bans, `Origin` allowlist →
//! pending (handshake deadline). First packet must be a `Hello` (≤ 4 KiB): structure
//! (`parse_hello`), draining check, then by `HelloAuth`:
//! * `Ticket` → `verify::<JoinClaims>` (kind, kid, MAC, exp/iat ± 5 s), `server_id == self`,
//!   `jti` unused (`NonceCache`), player not banned, `spec.room_id == room`,
//!   `GameDefinition::validate_spec` → [`GateOutcome::Admit`] (room created lazily from the spec).
//! * `Resume` → route by the token's room (the room verifies the secret) → `Admit` with
//!   `identity: None`.
//! * `Dev` → only if `allow_dev_auth` (and loopback-bound, enforced by config validation).
//!
//! Anything else before the Hello, a second Hello, or a timeout → close + IP strike.
//! After admission every packet/close of the link is forwarded to its room.

use crate::game::RoomSpec;
use crate::security::bans::BanList;
use crate::security::netaddr::Cidr;
use crate::security::ratelimit::ConnectionLimiter;
use crate::security::token::{KeyRing, NonceCache};
use space3d_replication::protocol::handshake::RejectCode;
use space3d_replication::server::RoomInbound;
use space3d_replication::transport::{LinkEvent, LinkTx};
use space3d_replication::{LinkId, RoomId};
use std::sync::Arc;
use std::time::Duration;

/// Gatekeeper settings.
#[derive(Clone, Debug)]
pub struct GatekeeperConfig {
    /// This server's id (tickets must name it).
    pub server_id: String,
    /// Accepted `Hello::game_id`.
    pub game_id: String,
    /// Hello deadline.
    pub handshake_timeout: Duration,
    /// Accept `HelloAuth::Dev`.
    pub allow_dev_auth: bool,
    /// Per-IP concurrent connections.
    pub max_connections_per_ip: u32,
    /// Handshakes per IP per minute.
    pub handshakes_per_ip_per_min: u32,
    /// Trusted proxies for `X-Forwarded-For`.
    pub trusted_proxies: Vec<Cidr>,
    /// Browser origins (empty = any).
    pub allowed_origins: Vec<String>,
    /// Token clock leeway.
    pub leeway: Duration,
}

/// What the ingress task must do with an event.
pub enum GateOutcome {
    /// Nothing (buffered, or dropped).
    None,
    /// Route a join to `room`, creating it from `spec` if it does not exist.
    Admit {
        /// Target room.
        room: RoomId,
        /// Spec from the ticket (None for resume/dev → room must exist or a dev room is used).
        spec: Option<RoomSpec>,
        /// The `RoomInbound::Join`.
        inbound: RoomInbound,
    },
    /// Forward a packet/close of an admitted link.
    Forward {
        /// Room.
        room: RoomId,
        /// Message.
        inbound: RoomInbound,
    },
    /// Send `Reject` and close.
    Refuse {
        /// Link sender.
        tx: Arc<dyn LinkTx>,
        /// Code.
        code: RejectCode,
        /// Detail.
        message: String,
    },
}

/// The gatekeeper state machine.
pub struct Gatekeeper {
    config: GatekeeperConfig,
    keys: Arc<KeyRing>,
    nonces: NonceCache,
    bans: BanList,
    limiter: ConnectionLimiter,
    draining: bool,
    pending: space3d_replication::FastMap<LinkId, Pending>,
    routed: space3d_replication::FastMap<LinkId, RoomId>,
}

struct Pending {
    tx: Arc<dyn LinkTx>,
    peer: space3d_replication::transport::PeerInfo,
    opened_at: Duration,
}

impl Gatekeeper {
    /// New gatekeeper.
    pub fn new(config: GatekeeperConfig, keys: Arc<KeyRing>, bans: BanList) -> Self {
        todo!()
    }

    /// Handle one link event at monotonic `now` / wall `now_ms`.
    pub fn handle(&mut self, event: LinkEvent, now: Duration, now_ms: i64) -> GateOutcome {
        todo!()
    }

    /// Close links whose Hello is overdue; returns how many were closed.
    pub fn sweep(&mut self, now: Duration) -> usize {
        todo!()
    }

    /// Reject new tickets with `ServerDraining` (resumes still pass).
    pub fn set_draining(&mut self, draining: bool) {
        self.draining = draining;
    }

    /// Forget a room's links (room closed).
    pub fn forget_room(&mut self, room: RoomId) {
        todo!()
    }
}
