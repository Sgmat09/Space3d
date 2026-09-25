//! Player-service REST DTOs (JSON), shared by the axum handlers in `space3d-server` and by
//! [`crate::services::PlayerServiceClient`] (native + web), so the two sides cannot drift.
//!
//! **Owner: the `server` builder** (see `docs/design/game-server.md` §3). Routes:
//!
//! ```text
//! POST /v1/auth/guest            GuestLoginRequest          → SessionTokens
//! POST /v1/auth/register         RegisterRequest            → SessionTokens   (feature password-auth)
//! POST /v1/auth/login            PasswordLoginRequest       → SessionTokens   (feature password-auth)
//! POST /v1/auth/refresh          RefreshRequest             → SessionTokens
//! POST /v1/auth/logout           RefreshRequest             → 204
//! GET  /v1/me                                               → Profile
//! PATCH /v1/me                   UpdateProfileRequest       → Profile
//! GET  /v1/me/data/{key}                                    → PlayerDataEntry
//! PUT  /v1/me/data/{key}         PutPlayerDataRequest       → PlayerDataEntry (client_writable_keys only)
//! POST /v1/matchmaking/tickets   MatchmakeRequest           → MatchmakeResponse
//! GET  /v1/matchmaking/tickets/{id}?wait=20                 → TicketStatus   (long poll)
//! DELETE /v1/matchmaking/tickets/{id}                       → 204
//! POST /v1/play/quick            QuickPlayRequest           → Assignment
//! POST /v1/sessions/rejoin                                  → Assignment | 404
//! POST /v1/lobbies               CreateLobbyRequest         → LobbyState
//! POST /v1/lobbies/join          JoinLobbyRequest           → LobbyState
//! GET  /v1/lobbies/{id}?wait=20                             → LobbyState     (long poll)
//! POST /v1/lobbies/{id}/ready | /start | /leave             → LobbyState
//! GET  /v1/leaderboards/{board}?top=50                      → Vec<LeaderboardRow>
//! GET  /v1/leaderboards/{board}/me                          → Option<LeaderboardRow>
//! GET  /v1/status                                           → ServiceStatus
//! ```
//!
//! Errors: non-2xx with an [`ApiError`] body. Auth: `Authorization: Bearer <access_token>`.

use crate::{PlayerId, RoomId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Error body of every non-2xx response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    /// Machine-readable code (`"unauthorized"`, `"rate_limited"`, `"banned"`, `"not_found"`, ...).
    pub code: String,
    /// Human-readable message.
    pub message: String,
    /// Suggested retry delay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u32>,
}

/// Account type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    /// Device-bound guest.
    Guest,
    /// Username + password.
    Password,
}

/// `POST /v1/auth/guest`: find or create the guest bound to this device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestLoginRequest {
    /// Random device id (uuid v4).
    pub device_id: String,
    /// 256-bit device secret, hex (the server stores only its SHA-256).
    pub device_secret: String,
    /// Initial display name.
    #[serde(default)]
    pub display_name: Option<String>,
}

/// `POST /v1/auth/register` (upgrades the current guest when authenticated).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterRequest {
    /// Username (3–32 chars).
    pub username: String,
    /// Password (≥ 8 chars).
    pub password: String,
    /// Display name.
    pub display_name: String,
}

/// `POST /v1/auth/login`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasswordLoginRequest {
    /// Username.
    pub username: String,
    /// Password.
    pub password: String,
}

/// `POST /v1/auth/refresh` and `/logout`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshRequest {
    /// Opaque refresh token (rotated on every use; reuse revokes the family).
    pub refresh_token: String,
}

/// A player's public profile.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    /// Account id.
    pub player_id: PlayerId,
    /// Display name.
    pub display_name: String,
    /// Account type.
    pub kind: AccountKind,
    /// Matchmaking rating (Elo).
    pub rating: f64,
    /// Creation time (unix ms).
    pub created_ms: i64,
}

/// Login result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionTokens {
    /// Stateless HMAC access token (1 h).
    pub access_token: String,
    /// Expiry (unix ms).
    pub access_expires_ms: i64,
    /// Opaque refresh token (30 d).
    pub refresh_token: String,
    /// Expiry (unix ms).
    pub refresh_expires_ms: i64,
    /// The player.
    pub profile: Profile,
}

/// `PATCH /v1/me`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateProfileRequest {
    /// New display name.
    #[serde(default)]
    pub display_name: Option<String>,
}

/// One persistent player-data key (progress, inventory, settings; JSON value).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlayerDataEntry {
    /// Key.
    pub key: String,
    /// Value.
    pub value: serde_json::Value,
    /// Optimistic-concurrency version.
    pub version: i64,
}

/// `PUT /v1/me/data/{key}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PutPlayerDataRequest {
    /// New value.
    pub value: serde_json::Value,
    /// Required current version (`None` = create).
    #[serde(default)]
    pub expected_version: Option<i64>,
}

/// `POST /v1/matchmaking/tickets`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchmakeRequest {
    /// Queue name (config `[[matchmaking.queues]]`).
    pub queue: String,
    /// Measured RTT (ms) per region (clients ping each region's `/ping`).
    #[serde(default)]
    pub region_pings: BTreeMap<String, u32>,
    /// Game build of the client: only game servers registered with this `game_id` host its
    /// room (empty = any).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub game_id: String,
    /// Replication schema hash of the client build (`ReplicationRegistry::schema_hash`): only
    /// game servers with the same hash host its room (0 = any).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub schema_hash: u64,
}

/// Matchmaking ticket id.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TicketId(pub String);

/// `POST /v1/matchmaking/tickets` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchmakeResponse {
    /// Ticket to poll.
    pub ticket_id: TicketId,
}

/// Where to play: returned by matchmaking, quick play, lobbies and rejoin.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    /// Game server WebSocket URL (`wss://gs-3.play.example.com/play`).
    pub server_url: String,
    /// Game server id.
    pub server_id: String,
    /// Room.
    pub room_id: RoomId,
    /// Signed single-use join ticket (60 s) for `HelloAuth::Ticket`.
    pub join_ticket: String,
    /// Ticket expiry (unix ms).
    pub expires_ms: i64,
}

/// `GET /v1/matchmaking/tickets/{id}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TicketStatus {
    /// Still queued.
    Searching {
        /// Seconds waited.
        waited_s: u32,
        /// Estimate, if known.
        #[serde(default)]
        eta_s: Option<u32>,
    },
    /// Matched.
    Found {
        /// Where to connect.
        assignment: Assignment,
    },
    /// Ticket timed out.
    Expired,
    /// Cancelled by the player.
    Cancelled,
}

/// `POST /v1/play/quick`: best open room with free seats (drop-in games).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickPlayRequest {
    /// Game mode.
    pub mode: String,
    /// Region RTTs.
    #[serde(default)]
    pub region_pings: BTreeMap<String, u32>,
    /// Game build of the client: only game servers registered with this `game_id` host its
    /// room (empty = any).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub game_id: String,
    /// Replication schema hash of the client build (`ReplicationRegistry::schema_hash`): only
    /// game servers with the same hash host its room (0 = any).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub schema_hash: u64,
}

/// `POST /v1/lobbies`. Lobbies are joined by code only (no lobby browser); a host has one
/// open lobby at a time (creating another closes the previous one).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateLobbyRequest {
    /// Game mode.
    pub mode: String,
    /// Capacity.
    pub max_players: u16,
    /// Game build of the client: only game servers registered with this `game_id` host its
    /// room (empty = any).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub game_id: String,
    /// Replication schema hash of the client build (`ReplicationRegistry::schema_hash`): only
    /// game servers with the same hash host its room (0 = any).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub schema_hash: u64,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

/// `POST /v1/lobbies/join`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinLobbyRequest {
    /// 6-character code.
    pub code: String,
}

/// Lobby phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LobbyPhase {
    /// Gathering players.
    Open,
    /// Allocating a room.
    Starting,
    /// Room assigned (see `LobbyState::assignment`).
    InGame,
    /// Closed.
    Closed,
}

/// Lobby member.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LobbyMember {
    /// Account.
    pub player_id: PlayerId,
    /// Name.
    pub display_name: String,
    /// Ready flag.
    pub ready: bool,
}

/// Lobby snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LobbyState {
    /// Id.
    pub lobby_id: String,
    /// Join code.
    pub code: String,
    /// Host.
    pub host: PlayerId,
    /// Mode.
    pub mode: String,
    /// Capacity.
    pub max_players: u16,
    /// Members.
    pub members: Vec<LobbyMember>,
    /// Phase.
    pub phase: LobbyPhase,
    /// Set for the requesting member once `InGame` (their own ticket).
    #[serde(default)]
    pub assignment: Option<Assignment>,
}

/// Leaderboard row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaderboardRow {
    /// 1-based rank.
    pub rank: u64,
    /// Account.
    pub player_id: PlayerId,
    /// Name.
    pub display_name: String,
    /// Score.
    pub score: i64,
}

/// A region clients can ping.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionInfo {
    /// Region name.
    pub region: String,
    /// URL answering `GET` quickly (a game server's `/ping`).
    pub ping_url: String,
}

/// `GET /v1/status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceStatus {
    /// Wire protocol version.
    pub protocol: u16,
    /// Server build.
    pub version: String,
    /// Message of the day.
    #[serde(default)]
    pub motd: Option<String>,
    /// Regions.
    pub regions: Vec<RegionInfo>,
}
