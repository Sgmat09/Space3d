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
//!
//! Session handling: a successful login / refresh stores the [`SessionTokens`] inside the
//! client (clones share them). Authenticated calls send `Authorization: Bearer <access>` and
//! rotate the tokens first when the access token expires within [`REFRESH_MARGIN_MS`]. Error
//! bodies ([`ApiError`]) surface as [`OnlineError::Api`].

use crate::api::{
    ApiError, Assignment, CreateLobbyRequest, GuestLoginRequest, JoinLobbyRequest, LeaderboardRow, LobbyState,
    MatchmakeRequest, MatchmakeResponse, PasswordLoginRequest, PlayerDataEntry, Profile, PutPlayerDataRequest,
    QuickPlayRequest, RefreshRequest, RegisterRequest, ServiceStatus, SessionTokens, TicketId, TicketStatus,
    UpdateProfileRequest,
};
use parking_lot::Mutex;
use serde::Serialize;
use serde::de::DeserializeOwned;
use space3d_net::{Http, HttpRequest, HttpResponse, Method, NetError};
use space3d_tasks::{MaybeSend, Task, spawn};
use std::sync::Arc;

/// Access tokens expiring within this many milliseconds are refreshed before a call.
pub const REFRESH_MARGIN_MS: i64 = 30_000;

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

impl OnlineError {
    /// HTTP status of a non-2xx answer.
    pub fn status(&self) -> Option<u16> {
        match self {
            OnlineError::Api { status, .. } | OnlineError::Net(NetError::Status { status, .. }) => Some(*status),
            _ => None,
        }
    }

    /// Machine-readable API error code (`"rate_limited"`, `"banned"`, ...).
    pub fn code(&self) -> Option<&str> {
        match self {
            OnlineError::Api { error, .. } => Some(&error.code),
            _ => None,
        }
    }

    /// Server-suggested retry delay.
    pub fn retry_after_ms(&self) -> Option<u32> {
        match self {
            OnlineError::Api { error, .. } => error.retry_after_ms,
            _ => None,
        }
    }

    /// Whether retrying later may succeed (transport failures, 429, 5xx).
    pub fn is_retryable(&self) -> bool {
        match self {
            OnlineError::Net(NetError::Status { status, .. }) | OnlineError::Api { status, .. } => {
                *status == 429 || *status >= 500
            }
            OnlineError::Net(e) => e.is_retryable(),
            _ => false,
        }
    }
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

    /// Fresh random credentials (OS CSPRNG through `uuid`'s v4 generator).
    pub fn generate() -> Self {
        let (a, b) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        Self {
            device_id: uuid::Uuid::new_v4().to_string(),
            device_secret: format!("{}{}", a.simple(), b.simple()),
        }
    }

    /// Whether the fields look like credentials [`Self::generate`] made.
    pub fn is_well_formed(&self) -> bool {
        uuid::Uuid::parse_str(&self.device_id).is_ok()
            && self.device_secret.len() == 64
            && self.device_secret.bytes().all(|b| b.is_ascii_hexdigit())
    }

    /// Load from `kv`, or generate and store new ones (an unreadable value is replaced).
    pub fn load_or_create(kv: &space3d_db::KvStore) -> Task<Result<Self, OnlineError>> {
        let kv = kv.clone();
        spawn(async move {
            let lost = || OnlineError::Storage("storage task lost".into());
            let stored = kv.get::<DeviceCredentials>(Self::KEY).wait().await.ok_or_else(lost)?;
            match stored {
                Ok(Some(creds)) if creds.is_well_formed() => return Ok(creds),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "unreadable device credentials; generating new ones"),
            }
            let creds = Self::generate();
            kv.set(Self::KEY, &creds)
                .wait()
                .await
                .ok_or_else(lost)?
                .map_err(|e| OnlineError::Storage(e.to_string()))?;
            Ok(creds)
        })
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

    /// Base URL (no trailing slash).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Current session, if logged in.
    pub fn session(&self) -> Option<SessionTokens> {
        self.session.lock().clone()
    }

    /// Adopt a session obtained elsewhere (e.g. restored from storage), or forget it.
    pub fn set_session(&self, tokens: Option<SessionTokens>) {
        *self.session.lock() = tokens;
    }

    /// Whether a session is held.
    pub fn is_logged_in(&self) -> bool {
        self.session.lock().is_some()
    }

    /// `GET /v1/status` (no auth).
    pub fn status(&self) -> Task<Result<ServiceStatus, OnlineError>> {
        self.public(HttpRequest::get(self.url("/v1/status")))
    }

    /// Guest login (stores the session on success).
    pub fn login_guest(
        &self,
        device: &DeviceCredentials,
        display_name: Option<&str>,
    ) -> Task<Result<SessionTokens, OnlineError>> {
        let body = GuestLoginRequest {
            device_id: device.device_id.clone(),
            device_secret: device.device_secret.clone(),
            display_name: display_name.map(str::to_owned),
        };
        self.login(self.url("/v1/auth/guest"), &body, None)
    }

    /// Password login.
    pub fn login_password(&self, username: &str, password: &str) -> Task<Result<SessionTokens, OnlineError>> {
        let body = PasswordLoginRequest {
            username: username.to_owned(),
            password: password.to_owned(),
        };
        self.login(self.url("/v1/auth/login"), &body, None)
    }

    /// Create a password account, or upgrade the current guest session to one.
    pub fn register(&self, req: &RegisterRequest) -> Task<Result<SessionTokens, OnlineError>> {
        let bearer = self.session.lock().as_ref().map(|s| s.access_token.clone());
        self.login(self.url("/v1/auth/register"), req, bearer)
    }

    /// Rotate tokens.
    pub fn refresh(&self) -> Task<Result<SessionTokens, OnlineError>> {
        let Some(refresh_token) = self.session.lock().as_ref().map(|s| s.refresh_token.clone()) else {
            return ready(Err(OnlineError::NotLoggedIn));
        };
        let this = self.clone();
        spawn(async move { this.refresh_with(refresh_token).await })
    }

    /// Revoke the refresh token and forget the session.
    pub fn logout(&self) -> Task<Result<(), OnlineError>> {
        let Some(refresh_token) = self.session.lock().take().map(|s| s.refresh_token) else {
            return ready(Ok(()));
        };
        let req = match json_request(
            Method::POST,
            self.url("/v1/auth/logout"),
            &RefreshRequest { refresh_token },
        ) {
            Ok(r) => r,
            Err(e) => return ready(Err(e)),
        };
        let fut = self.http.fetch(req);
        spawn(async move { check(fut.await?).map(drop) })
    }

    /// Own profile.
    pub fn me(&self) -> Task<Result<Profile, OnlineError>> {
        self.authed(Method::GET, "/v1/me".into(), None::<&()>)
    }

    /// Change the display name.
    pub fn update_profile(&self, req: &UpdateProfileRequest) -> Task<Result<Profile, OnlineError>> {
        self.authed(Method::PATCH, "/v1/me".into(), Some(req))
    }

    /// Read one of the player's data keys.
    pub fn get_data(&self, key: &str) -> Task<Result<PlayerDataEntry, OnlineError>> {
        self.authed(Method::GET, format!("/v1/me/data/{}", encode(key)), None::<&()>)
    }

    /// Write a client-writable data key (optimistic concurrency via `expected_version`).
    pub fn put_data(&self, key: &str, req: &PutPlayerDataRequest) -> Task<Result<PlayerDataEntry, OnlineError>> {
        self.authed(Method::PUT, format!("/v1/me/data/{}", encode(key)), Some(req))
    }

    /// Enter a matchmaking queue.
    pub fn matchmake(&self, req: &MatchmakeRequest) -> Task<Result<MatchmakeResponse, OnlineError>> {
        self.authed(Method::POST, "/v1/matchmaking/tickets".into(), Some(req))
    }

    /// Long-poll a ticket (`wait_s` ≤ 20).
    pub fn poll_ticket(&self, id: &TicketId, wait_s: u8) -> Task<Result<TicketStatus, OnlineError>> {
        let path = format!("/v1/matchmaking/tickets/{}?wait={}", encode(&id.0), wait_s.min(20));
        self.authed(Method::GET, path, None::<&()>)
    }

    /// Cancel a ticket.
    pub fn cancel_ticket(&self, id: &TicketId) -> Task<Result<(), OnlineError>> {
        self.authed_empty(Method::DELETE, format!("/v1/matchmaking/tickets/{}", encode(&id.0)))
    }

    /// Drop into the best open room.
    pub fn quick_play(&self, req: &QuickPlayRequest) -> Task<Result<Assignment, OnlineError>> {
        self.authed(Method::POST, "/v1/play/quick".into(), Some(req))
    }

    /// After a server loss: a new ticket for the (restored) room, if any.
    pub fn rejoin(&self) -> Task<Result<Option<Assignment>, OnlineError>> {
        let call: Task<Result<Assignment, OnlineError>> =
            self.authed(Method::POST, "/v1/sessions/rejoin".into(), None::<&()>);
        spawn(async move {
            match call.wait().await.unwrap_or(Err(OnlineError::Net(NetError::Closed))) {
                Ok(a) => Ok(Some(a)),
                Err(e) if e.status() == Some(404) => Ok(None),
                Err(e) => Err(e),
            }
        })
    }

    /// Create a lobby.
    pub fn create_lobby(&self, req: &CreateLobbyRequest) -> Task<Result<LobbyState, OnlineError>> {
        self.authed(Method::POST, "/v1/lobbies".into(), Some(req))
    }

    /// Join a lobby by code.
    pub fn join_lobby(&self, code: &str) -> Task<Result<LobbyState, OnlineError>> {
        let req = JoinLobbyRequest { code: code.to_owned() };
        self.authed(Method::POST, "/v1/lobbies/join".into(), Some(&req))
    }

    /// Long-poll a lobby (`wait_s` ≤ 20; returns early on any change).
    pub fn poll_lobby(&self, lobby_id: &str, wait_s: u8) -> Task<Result<LobbyState, OnlineError>> {
        let path = format!("/v1/lobbies/{}?wait={}", encode(lobby_id), wait_s.min(20));
        self.authed(Method::GET, path, None::<&()>)
    }

    /// Lobby action: `"ready"`, `"start"` (host) or `"leave"`.
    pub fn lobby_action(&self, lobby_id: &str, action: &str) -> Task<Result<LobbyState, OnlineError>> {
        let path = format!("/v1/lobbies/{}/{}", encode(lobby_id), encode(action));
        self.authed(Method::POST, path, None::<&()>)
    }

    /// Leaderboard top N (no auth).
    pub fn leaderboard(&self, board: &str, top: u16) -> Task<Result<Vec<LeaderboardRow>, OnlineError>> {
        let path = format!("/v1/leaderboards/{}?top={}", encode(board), top.max(1));
        self.public(HttpRequest::get(self.url(&path)))
    }

    /// Own leaderboard row, if ranked.
    pub fn leaderboard_me(&self, board: &str) -> Task<Result<Option<LeaderboardRow>, OnlineError>> {
        self.authed(
            Method::GET,
            format!("/v1/leaderboards/{}/me", encode(board)),
            None::<&()>,
        )
    }

    // ---- plumbing ------------------------------------------------------------------------------

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    fn public<T: DeserializeOwned + MaybeSend + 'static>(&self, req: HttpRequest) -> Task<Result<T, OnlineError>> {
        let fut = self.http.fetch(req);
        spawn(async move { decode(check(fut.await?)?) })
    }

    fn login<B: Serialize + ?Sized>(
        &self,
        url: String,
        body: &B,
        bearer: Option<String>,
    ) -> Task<Result<SessionTokens, OnlineError>> {
        let mut req = match json_request(Method::POST, url, body) {
            Ok(r) => r,
            Err(e) => return ready(Err(e)),
        };
        if let Some(token) = bearer {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        let fut = self.http.fetch(req);
        let session = self.session.clone();
        spawn(async move {
            let tokens: SessionTokens = decode(check(fut.await?)?)?;
            *session.lock() = Some(tokens.clone());
            Ok(tokens)
        })
    }

    async fn refresh_with(&self, refresh_token: String) -> Result<SessionTokens, OnlineError> {
        let req = json_request(
            Method::POST,
            self.url("/v1/auth/refresh"),
            &RefreshRequest { refresh_token },
        )?;
        let res = self.http.fetch(req).await?;
        match check(res).and_then(decode::<SessionTokens>) {
            Ok(tokens) => {
                *self.session.lock() = Some(tokens.clone());
                Ok(tokens)
            }
            Err(e) => {
                // A rejected refresh token (expired, revoked, reused) ends the session.
                if matches!(e.status(), Some(400 | 401 | 403)) {
                    self.session.lock().take();
                }
                Err(e)
            }
        }
    }

    /// Bearer token for a call, refreshing first when it is about to expire.
    async fn bearer(&self) -> Result<String, OnlineError> {
        let current = self.session.lock().clone().ok_or(OnlineError::NotLoggedIn)?;
        if current.access_expires_ms - now_ms() > REFRESH_MARGIN_MS {
            return Ok(current.access_token);
        }
        Ok(self.refresh_with(current.refresh_token).await?.access_token)
    }

    fn authed<T, B>(&self, method: Method, path: String, body: Option<&B>) -> Task<Result<T, OnlineError>>
    where
        T: DeserializeOwned + MaybeSend + 'static,
        B: Serialize + ?Sized,
    {
        let url = self.url(&path);
        let req = match body {
            Some(b) => json_request(method, url, b),
            None => Ok(HttpRequest::new(method, url)),
        };
        let req = match req {
            Ok(r) => r,
            Err(e) => return ready(Err(e)),
        };
        let this = self.clone();
        spawn(async move { decode(this.send_authed(req).await?) })
    }

    fn authed_empty(&self, method: Method, path: String) -> Task<Result<(), OnlineError>> {
        let req = HttpRequest::new(method, self.url(&path));
        let this = self.clone();
        spawn(async move { this.send_authed(req).await.map(drop) })
    }

    async fn send_authed(&self, req: HttpRequest) -> Result<HttpResponse, OnlineError> {
        let token = self.bearer().await?;
        let res = self
            .http
            .fetch(req.header("authorization", format!("Bearer {token}")))
            .await?;
        check(res)
    }
}

fn ready<T: MaybeSend + 'static>(value: T) -> Task<T> {
    spawn(async move { value })
}

fn json_request<B: Serialize + ?Sized>(method: Method, url: String, body: &B) -> Result<HttpRequest, OnlineError> {
    Ok(HttpRequest::new(method, url).json(body)?)
}

/// 2xx → the response; otherwise [`OnlineError::Api`] when the body is an [`ApiError`], else
/// [`NetError::Status`].
fn check(res: HttpResponse) -> Result<HttpResponse, OnlineError> {
    if res.is_success() {
        return Ok(res);
    }
    match serde_json::from_slice::<ApiError>(&res.body) {
        Ok(error) => Err(OnlineError::Api {
            status: res.status,
            error,
        }),
        Err(_) => Err(OnlineError::Net(
            res.error_for_status().err().unwrap_or(NetError::Closed),
        )),
    }
}

fn decode<T: DeserializeOwned>(res: HttpResponse) -> Result<T, OnlineError> {
    Ok(res.json::<T>()?)
}

fn now_ms() -> i64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Percent-encode a path segment (RFC 3986 unreserved characters pass through).
fn encode(segment: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn generated_credentials_are_unique_and_well_formed() {
        let a = DeviceCredentials::generate();
        let b = DeviceCredentials::generate();
        assert!(a.is_well_formed() && b.is_well_formed());
        assert_ne!(a, b);
        assert!(
            !DeviceCredentials {
                device_id: "nope".into(),
                device_secret: a.device_secret.clone()
            }
            .is_well_formed()
        );
    }

    #[test]
    fn path_segments_are_encoded() {
        assert_eq!(encode("abc-DEF_1.2~"), "abc-DEF_1.2~");
        assert_eq!(encode("a/b c?"), "a%2Fb%20c%3F");
        assert_eq!(encode("é"), "%C3%A9");
    }

    #[test]
    fn error_bodies_become_api_errors() {
        let body = serde_json::to_vec(&ApiError {
            code: "rate_limited".into(),
            message: "slow down".into(),
            retry_after_ms: Some(1000),
        })
        .unwrap();
        let err = check(HttpResponse {
            status: 429,
            headers: Vec::new(),
            body,
        })
        .unwrap_err();
        assert_eq!(err.status(), Some(429));
        assert_eq!(err.code(), Some("rate_limited"));
        assert_eq!(err.retry_after_ms(), Some(1000));
        assert!(err.is_retryable());
        let plain = check(HttpResponse {
            status: 404,
            headers: Vec::new(),
            body: b"nope".to_vec(),
        })
        .unwrap_err();
        assert_eq!(plain.status(), Some(404));
        assert!(plain.code().is_none());
        assert!(!plain.is_retryable());
    }

    #[test]
    fn calls_without_a_session_fail_fast() {
        let ps = PlayerServiceClient::new(&Http::new(), "http://127.0.0.1:9/");
        assert_eq!(ps.base_url(), "http://127.0.0.1:9");
        assert!(!ps.is_logged_in());
        assert_eq!(ps.me().block(), Some(Err(OnlineError::NotLoggedIn)));
        assert_eq!(ps.refresh().block(), Some(Err(OnlineError::NotLoggedIn)));
        assert_eq!(ps.logout().block(), Some(Ok(())));
    }

    #[test]
    fn device_credentials_persist_in_kv() {
        let db = space3d_db::Db::open_in_memory();
        let kv = db.kv("space3d.device");
        let first = DeviceCredentials::load_or_create(&kv).block().unwrap().unwrap();
        let again = DeviceCredentials::load_or_create(&kv).block().unwrap().unwrap();
        assert_eq!(first, again);
        kv.set(DeviceCredentials::KEY, &"garbage").block().unwrap().unwrap();
        let fresh = DeviceCredentials::load_or_create(&kv).block().unwrap().unwrap();
        assert!(fresh.is_well_formed());
        assert_ne!(fresh, first);
    }
}
