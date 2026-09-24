//! Player-service HTTP client (native + web, on `space3d_net::Http`). Every call returns a
//! pollable [`Task`]; nothing blocks the frame.
//!
//! **Owner: the `server` builder** (it owns the routes in `crate::api`).
//!
//! ```ignore
//! let ps = PlayerServiceClient::new(&http, "https://api.arena.example");
//! let device = DeviceCredentials::load_or_create(&db.kv("space3d.device"));   // Task
//! let tokens = ps.login_guest(&device, Some("alice"));                          // Task
//! let ticket = ps.matchmake(&MatchmakeRequest { queue: "duel".into(), ..Default::default() });
//! // poll_ticket(...) until Found { assignment } → NetClient::connect(ConnectTarget::Ticket { .. })
//! ```

use crate::api::{ApiError, CreateLobbyRequest};
use crate::api::{
    Assignment, LeaderboardRow, LobbyState, MatchmakeRequest, MatchmakeResponse, Profile, QuickPlayRequest,
    SessionTokens, TicketId, TicketStatus,
};
use parking_lot::Mutex;
use space3d_net::{Http, NetError};
use space3d_tasks::Task;
use std::sync::Arc;

/// Player-service call failure.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum OnlineError {
    /// Transport / HTTP failure.
    #[error(transparent)]
    Net(#[from] NetError),
    /// The service answered with an error body.
    #[error("{status}: {} ({})", error.message, error.code)]
    Api {
        /// HTTP status.
        status: u16,
        /// Body.
        error: ApiError,
    },
    /// No session (call a login first).
    #[error("not logged in")]
    NotLoggedIn,
    /// Local storage failure (device credentials).
    #[error("storage: {0}")]
    Storage(String),
}

/// Device-bound guest credentials, persisted with `space3d_db::KvStore` (SQLite natively,
/// `localStorage` on the web).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeviceCredentials {
    /// uuid v4.
    pub device_id: String,
    /// 256-bit secret, hex (two uuid v4s).
    pub device_secret: String,
}

impl DeviceCredentials {
    /// KV key used by [`Self::load_or_create`].
    pub const KEY: &'static str = "device_credentials";

    /// Fresh random credentials.
    pub fn generate() -> Self {
        todo!()
    }

    /// Load from `kv`, or generate and store new ones.
    pub fn load_or_create(kv: &space3d_db::KvStore) -> Task<Result<Self, OnlineError>> {
        todo!()
    }
}

/// HTTP client for the player service (clone = shared session).
#[derive(Clone)]
pub struct PlayerServiceClient {
    http: Http,
    base_url: String,
    session: Arc<Mutex<Option<SessionTokens>>>,
}

impl PlayerServiceClient {
    /// Client for `base_url` (e.g. `https://api.example.com`).
    pub fn new(http: &Http, base_url: &str) -> Self {
        Self {
            http: http.clone(),
            base_url: base_url.trim_end_matches('/').to_owned(),
            session: Arc::new(Mutex::new(None)),
        }
    }

    /// Current session, if logged in.
    pub fn session(&self) -> Option<SessionTokens> {
        self.session.lock().clone()
    }

    /// Guest login (stores the session on success).
    pub fn login_guest(
        &self,
        device: &DeviceCredentials,
        display_name: Option<&str>,
    ) -> Task<Result<SessionTokens, OnlineError>> {
        todo!()
    }

    /// Password login.
    pub fn login_password(&self, username: &str, password: &str) -> Task<Result<SessionTokens, OnlineError>> {
        todo!()
    }

    /// Rotate tokens.
    pub fn refresh(&self) -> Task<Result<SessionTokens, OnlineError>> {
        todo!()
    }

    /// Revoke the refresh token and forget the session.
    pub fn logout(&self) -> Task<Result<(), OnlineError>> {
        todo!()
    }

    /// Own profile.
    pub fn me(&self) -> Task<Result<Profile, OnlineError>> {
        todo!()
    }

    /// Enter a matchmaking queue.
    pub fn matchmake(&self, req: &MatchmakeRequest) -> Task<Result<MatchmakeResponse, OnlineError>> {
        todo!()
    }

    /// Long-poll a ticket (`wait_s` ≤ 20).
    pub fn poll_ticket(&self, id: &TicketId, wait_s: u8) -> Task<Result<TicketStatus, OnlineError>> {
        todo!()
    }

    /// Cancel a ticket.
    pub fn cancel_ticket(&self, id: &TicketId) -> Task<Result<(), OnlineError>> {
        todo!()
    }

    /// Drop into the best open room.
    pub fn quick_play(&self, req: &QuickPlayRequest) -> Task<Result<Assignment, OnlineError>> {
        todo!()
    }

    /// After a server loss: a new ticket for the (restored) room, if any.
    pub fn rejoin(&self) -> Task<Result<Option<Assignment>, OnlineError>> {
        todo!()
    }

    /// Create a lobby.
    pub fn create_lobby(&self, req: &CreateLobbyRequest) -> Task<Result<LobbyState, OnlineError>> {
        todo!()
    }

    /// Join a lobby by code.
    pub fn join_lobby(&self, code: &str) -> Task<Result<LobbyState, OnlineError>> {
        todo!()
    }

    /// Leaderboard top N.
    pub fn leaderboard(&self, board: &str, top: u16) -> Task<Result<Vec<LeaderboardRow>, OnlineError>> {
        todo!()
    }
}
