//! Versioned handshake: `Hello` (C→S) → `Welcome` | `Reject` (S→C), and `Disconnect` (both).
//!
//! All four are postcard structs inside a handshake-family packet (see [`super::encode_handshake`]),
//! each at most [`super::MAX_HANDSHAKE_BYTES`]. An unauthenticated link may send exactly one
//! `Hello` within the handshake timeout; anything else closes it.

use super::WireLimits;
use crate::wire::{DecodeError, Reader, Writer};
use crate::{NetClientId, PlayerId, RoomId, Tick};
use serde::{Deserialize, Serialize};

/// Longest `Hello::game_id`.
pub const MAX_GAME_ID: usize = 32;
/// Longest `Hello::game_version`.
pub const MAX_GAME_VERSION: usize = 64;
/// Longest join ticket string.
pub const MAX_TICKET: usize = 1024;
/// Longest dev name.
pub const MAX_DEV_NAME: usize = 32;
/// Longest human-readable reason / message.
pub const MAX_REASON: usize = 128;

/// How the client authenticates.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HelloAuth {
    /// A join ticket from the player service (`v1.<kid>.<claims>.<mac>`), verified by the
    /// game server gateway with HMAC, offline (no call to the player service).
    Ticket(String),
    /// Resume a disconnected slot inside its resume window ([`ResumeToken::encode`] bytes from
    /// the last `Welcome`). Verified by the room.
    Resume(Vec<u8>),
    /// Development only: accepted only when the server enables dev auth *and* binds loopback.
    /// The `PlayerId` is derived from the name.
    Dev {
        /// Display name.
        name: String,
    },
}

/// Client platform (metrics / debugging only).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Platform {
    /// Unknown.
    #[default]
    Unknown,
    /// Native desktop.
    Desktop,
    /// Browser (wasm32).
    Web,
    /// Android.
    Android,
    /// iOS.
    Ios,
    /// Headless bot / load-test client.
    Bot,
}

/// Client capabilities and preferences (clamped by the server).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientCaps {
    /// Preferred downstream budget in bytes/s (0 = server default).
    pub max_down_bytes_per_sec: u32,
    /// Platform.
    pub platform: Platform,
}

/// First packet of every connection (C→S).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// Must be [`super::MAGIC`].
    pub magic: [u8; 4],
    /// Must be [`super::PROTOCOL_VERSION`].
    pub protocol: u16,
    /// Game identifier (≤ [`MAX_GAME_ID`]), e.g. `"arena"`.
    pub game_id: String,
    /// Client build (≤ [`MAX_GAME_VERSION`], informational; the schema hash is the gate).
    pub game_version: String,
    /// `ReplicationRegistry::schema_hash()` of the client build.
    pub schema_hash: u64,
    /// Credentials.
    pub auth: HelloAuth,
    /// Capabilities.
    pub caps: ClientCaps,
    /// Client clock (µs) when sent; echoed for a first RTT sample.
    pub client_time_us: u64,
}

impl Hello {
    /// Structural checks shared by gateway and harness: magic, protocol version, string and
    /// ticket lengths. Does **not** authenticate.
    pub fn validate(&self) -> Result<(), RejectCode> {
        if self.magic != super::MAGIC || self.protocol != super::PROTOCOL_VERSION {
            return Err(RejectCode::VersionMismatch);
        }
        if self.game_id.is_empty() || self.game_id.len() > MAX_GAME_ID || self.game_version.len() > MAX_GAME_VERSION {
            return Err(RejectCode::Malformed);
        }
        match &self.auth {
            HelloAuth::Ticket(t) if t.is_empty() || t.len() > MAX_TICKET => Err(RejectCode::Malformed),
            HelloAuth::Resume(b) if b.len() != ResumeToken::LEN => Err(RejectCode::Malformed),
            HelloAuth::Dev { name } if name.trim().is_empty() || name.len() > MAX_DEV_NAME => {
                Err(RejectCode::Malformed)
            }
            _ => Ok(()),
        }
    }

    /// A Hello for this build with the given credentials (`client_time_us` from the caller's clock).
    pub fn new(game_id: &str, game_version: &str, schema_hash: u64, auth: HelloAuth, caps: ClientCaps) -> Self {
        Self {
            magic: super::MAGIC,
            protocol: super::PROTOCOL_VERSION,
            game_id: game_id.to_owned(),
            game_version: game_version.to_owned(),
            schema_hash,
            auth,
            caps,
            client_time_us: 0,
        }
    }
}

/// Resume credentials issued by a room in `Welcome::resume` (opaque to the client).
/// The room keeps only a hash of `secret`; tokens are single use and rotate on every Welcome.
/// They are process-local by design: after a server crash clients rejoin via the player service.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeToken {
    /// Room to route to (the gateway reads this without verifying).
    pub room: RoomId,
    /// Slot to resume.
    pub client: NetClientId,
    /// Slot epoch (bumped on every resume; old tokens are rejected).
    pub epoch: u32,
    /// Random secret (verified by the room in constant time).
    pub secret: [u8; 16],
}

impl ResumeToken {
    /// Encoded size.
    pub const LEN: usize = 8 + 2 + 4 + 16;

    /// Fixed-size little-endian encoding.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(Self::LEN);
        w.u64(self.room.0);
        w.u16(self.client.0);
        w.u32(self.epoch);
        w.raw(&self.secret);
        w.into_vec()
    }

    /// Inverse of [`Self::encode`].
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.len() != Self::LEN {
            return Err(DecodeError::Invalid("resume token length"));
        }
        let mut r = Reader::new(bytes);
        let room = RoomId(r.u64()?);
        let client = NetClientId(r.u16()?);
        let epoch = r.u32()?;
        let mut secret = [0u8; 16];
        secret.copy_from_slice(r.raw(16)?);
        Ok(Self {
            room,
            client,
            epoch,
            secret,
        })
    }
}

/// Successful join (S→C).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    /// Room-local slot.
    pub client: NetClientId,
    /// Authenticated account.
    pub player: PlayerId,
    /// Room.
    pub room: RoomId,
    /// Room epoch (changes after a crash restore; the client drops its entity map).
    pub room_epoch: u32,
    /// Simulation rate.
    pub tick_hz: u16,
    /// Snapshots every `send_interval` ticks.
    pub send_interval: u8,
    /// Server tick when the Welcome was built.
    pub server_tick: Tick,
    /// [`ResumeToken::encode`] bytes for the next resume.
    pub resume: Vec<u8>,
    /// Resume window in seconds.
    pub resume_window_s: u16,
    /// Server-side limits the client must respect.
    pub limits: WireLimits,
    /// Downstream budget granted (bytes/s).
    pub down_bytes_per_sec: u32,
    /// `true` when an existing slot was resumed (entities are kept and reconciled).
    pub resumed: bool,
    /// Echo of `Hello::client_time_us`.
    pub echo_client_time_us: u64,
}

/// Why a join was refused.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RejectCode {
    /// `protocol`/`magic` mismatch.
    VersionMismatch = 1,
    /// `schema_hash` mismatch (client and server registered different things).
    SchemaMismatch,
    /// `game_id` not served here.
    WrongGame,
    /// Ticket malformed or MAC invalid.
    BadTicket,
    /// Ticket expired.
    TicketExpired,
    /// Ticket already used (`jti` replay).
    TicketReused,
    /// Ticket issued for another game server.
    WrongServer,
    /// Account or IP banned.
    Banned,
    /// Room has no free slot.
    RoomFull,
    /// Room does not exist (and cannot be created from the ticket).
    RoomNotFound,
    /// Room is closing.
    RoomClosed,
    /// Resume token unknown, stale or outside the resume window.
    ResumeExpired,
    /// The server is draining (retry via the player service).
    ServerDraining,
    /// Too many attempts.
    RateLimited,
    /// Dev auth is disabled on this server.
    DevAuthDisabled,
    /// Malformed hello.
    Malformed,
    /// Server-side failure.
    Internal,
}

/// Refused join (S→C), followed by a close.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reject {
    /// Reason.
    pub code: RejectCode,
    /// Human-readable detail (≤ [`MAX_REASON`]).
    pub message: String,
    /// Suggested retry delay.
    pub retry_after_ms: Option<u32>,
}

/// Why a connection ended.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DisconnectCode {
    /// Client left on purpose (no resume grace).
    Leave = 1,
    /// Kicked by the game or an admin.
    Kicked,
    /// Banned.
    Banned,
    /// Too many protocol violations / strikes.
    ProtocolViolation,
    /// Rate limits exceeded persistently.
    RateLimited,
    /// Could not keep up with the downstream (backlog cap).
    SlowConsumer,
    /// No packets within the idle timeout.
    Timeout,
    /// The same player connected again elsewhere.
    Superseded,
    /// The room closed (match over, empty, admin).
    RoomClosed,
    /// The room crashed; rejoin via the player service.
    RoomCrashed,
    /// The server is shutting down.
    ServerShutdown,
}

impl DisconnectCode {
    /// Every code, in wire order.
    pub const ALL: [DisconnectCode; 11] = [
        DisconnectCode::Leave,
        DisconnectCode::Kicked,
        DisconnectCode::Banned,
        DisconnectCode::ProtocolViolation,
        DisconnectCode::RateLimited,
        DisconnectCode::SlowConsumer,
        DisconnectCode::Timeout,
        DisconnectCode::Superseded,
        DisconnectCode::RoomClosed,
        DisconnectCode::RoomCrashed,
        DisconnectCode::ServerShutdown,
    ];

    /// Stable snake_case label (metrics, logs).
    pub fn label(self) -> &'static str {
        match self {
            DisconnectCode::Leave => "leave",
            DisconnectCode::Kicked => "kicked",
            DisconnectCode::Banned => "banned",
            DisconnectCode::ProtocolViolation => "protocol_violation",
            DisconnectCode::RateLimited => "rate_limited",
            DisconnectCode::SlowConsumer => "slow_consumer",
            DisconnectCode::Timeout => "timeout",
            DisconnectCode::Superseded => "superseded",
            DisconnectCode::RoomClosed => "room_closed",
            DisconnectCode::RoomCrashed => "room_crashed",
            DisconnectCode::ServerShutdown => "server_shutdown",
        }
    }
}

/// Orderly disconnect (both directions), followed by a close.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disconnect {
    /// Reason code.
    pub code: DisconnectCode,
    /// Detail (≤ [`MAX_REASON`]).
    pub message: String,
    /// Whether the client may try `HelloAuth::Resume`.
    pub resume_allowed: bool,
}

impl Reject {
    /// A reject with a (truncated) message.
    pub fn new(code: RejectCode, message: &str) -> Self {
        Self {
            code,
            message: truncate(message, MAX_REASON),
            retry_after_ms: None,
        }
    }

    /// A refusal worth retrying after `retry_after_ms`: the server is draining (the room is
    /// moving, or will be placed elsewhere), the room is full, or the client is rate limited.
    pub fn is_transient(&self) -> bool {
        self.retry_after_ms.is_some()
            && matches!(
                self.code,
                RejectCode::ServerDraining | RejectCode::RoomFull | RejectCode::RateLimited
            )
    }
}

impl Disconnect {
    /// A disconnect with a (truncated) message.
    pub fn new(code: DisconnectCode, message: &str, resume_allowed: bool) -> Self {
        Self {
            code,
            message: truncate(message, MAX_REASON),
            resume_allowed,
        }
    }
}

/// Truncate to at most `max` bytes on a char boundary.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Packet, PacketKind, decode_packet, encode_handshake};

    fn hello(auth: HelloAuth) -> Hello {
        Hello::new("arena", "1.0.0", 42, auth, ClientCaps::default())
    }

    #[test]
    fn hello_validation() {
        assert_eq!(hello(HelloAuth::Dev { name: "al".into() }).validate(), Ok(()));
        let mut h = hello(HelloAuth::Dev { name: "al".into() });
        h.magic = *b"XXXX";
        assert_eq!(h.validate(), Err(RejectCode::VersionMismatch));
        let mut h = hello(HelloAuth::Dev { name: "al".into() });
        h.protocol += 1;
        assert_eq!(h.validate(), Err(RejectCode::VersionMismatch));
        assert_eq!(
            hello(HelloAuth::Ticket("x".repeat(MAX_TICKET + 1))).validate(),
            Err(RejectCode::Malformed)
        );
        assert_eq!(
            hello(HelloAuth::Resume(vec![0; 3])).validate(),
            Err(RejectCode::Malformed)
        );
        assert_eq!(
            hello(HelloAuth::Dev { name: " ".into() }).validate(),
            Err(RejectCode::Malformed)
        );
        let mut h = hello(HelloAuth::Ticket("t".into()));
        h.game_id = "g".repeat(MAX_GAME_ID + 1);
        assert_eq!(h.validate(), Err(RejectCode::Malformed));
    }

    #[test]
    fn resume_token_is_30_bytes_and_roundtrips() {
        let t = ResumeToken {
            room: RoomId(7),
            client: NetClientId(3),
            epoch: 9,
            secret: [5; 16],
        };
        let b = t.encode();
        assert_eq!(b.len(), 30);
        assert_eq!(ResumeToken::decode(&b), Ok(t));
        assert!(ResumeToken::decode(&b[..29]).is_err());
    }

    #[test]
    fn handshake_packets_roundtrip() {
        let h = hello(HelloAuth::Ticket("s3.join.k1.abc.def".into()));
        let bytes = encode_handshake(PacketKind::Hello, &h);
        assert_eq!(bytes[0], PacketKind::Hello as u8);
        match decode_packet(&bytes, &Default::default()).unwrap() {
            Packet::Hello(back) => assert_eq!(back, h),
            other => panic!("{other:?}"),
        }
        let r = Reject::new(RejectCode::RoomFull, &"x".repeat(500));
        assert_eq!(r.message.len(), MAX_REASON);
        let bytes = encode_handshake(PacketKind::Reject, &r);
        assert_eq!(decode_packet(&bytes, &Default::default()).unwrap(), Packet::Reject(r));
        let d = Disconnect::new(DisconnectCode::SlowConsumer, "slow", true);
        let bytes = encode_handshake(PacketKind::Disconnect, &d);
        assert_eq!(
            decode_packet(&bytes, &Default::default()).unwrap(),
            Packet::Disconnect(d)
        );
        // Flags on a handshake packet and trailing garbage are rejected.
        let mut bad = encode_handshake(
            PacketKind::Disconnect,
            &Disconnect::new(DisconnectCode::Leave, "", false),
        );
        bad[0] |= 0x08;
        assert!(decode_packet(&bad, &Default::default()).is_err());
        let mut bad = encode_handshake(
            PacketKind::Disconnect,
            &Disconnect::new(DisconnectCode::Leave, "", false),
        );
        bad.push(0);
        assert!(decode_packet(&bad, &Default::default()).is_err());
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("héllo", 2), "h");
        assert_eq!(truncate("abc", 5), "abc");
    }
}
