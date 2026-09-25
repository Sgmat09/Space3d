//! Router assembly. Public `/v1/*` (routes documented in `space3d_replication::api`), internal
//! `/internal/v1/*` (fleet token: `WS /internal/v1/fleet`, `PUT|GET|DELETE
//! /internal/v1/rooms/{id}/checkpoint` ≤ 16 MiB, `GET /internal/v1/checkpoints?since=`).
//! Body limit `player_service.max_body_bytes`, CORS allowlist, per-IP request rate
//! (`http_rps_per_ip`), JSON errors (`api::ApiError`), request metrics. Internal routes check
//! the per-IP rate and the fleet token in a middleware before any body is read, and bound
//! the upload time of a checkpoint body.

use super::PlayerService;
use super::auth::{profile, require, sanitize_name};
use super::fleet::Build;
use super::matchmaker::{MatchmakingError, QueueEntry, TicketState};
use super::store::StoreError;
use crate::gs::fleet::DataWrite;
use crate::security::netaddr::rate_key;
use space3d_net::server::axum::body::Bytes;
use space3d_net::server::axum::extract::{
    ConnectInfo, DefaultBodyLimit, FromRequestParts, Path, Query, Request, State,
};
use space3d_net::server::axum::http::request::Parts;
use space3d_net::server::axum::http::{HeaderValue, Method, StatusCode, header};
use space3d_net::server::axum::middleware::{self, Next};
use space3d_net::server::axum::response::{IntoResponse, Response};
use space3d_net::server::axum::routing::{get, post, put};
use space3d_net::server::axum::{Json, Router};
use space3d_net::{CorsLayer, HttpServer, WsServer};
use space3d_replication::api::{
    ApiError, Assignment, CreateLobbyRequest, JoinLobbyRequest, LeaderboardRow, LobbyState, MatchmakeRequest,
    MatchmakeResponse, PlayerDataEntry, Profile, PutPlayerDataRequest, QuickPlayRequest, RegionInfo, ServiceStatus,
    TicketId, TicketStatus, UpdateProfileRequest,
};
use space3d_replication::{PlayerId, RoomId};
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

/// An error response: status + [`ApiError`] body.
#[derive(Debug)]
pub struct ApiErr(pub StatusCode, pub ApiError);

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        let mut res = (self.0, Json(self.1.clone())).into_response();
        if let Some(ms) = self.1.retry_after_ms
            && let Ok(v) = HeaderValue::from_str(&ms.div_ceil(1000).to_string())
        {
            res.headers_mut().insert(header::RETRY_AFTER, v);
        }
        res
    }
}

/// Build an [`ApiErr`].
pub fn api_err(status: u16, code: &str, message: &str) -> ApiErr {
    ApiErr(
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        ApiError {
            code: code.into(),
            message: message.into(),
            retry_after_ms: None,
        },
    )
}

/// Map a store error to a response.
pub fn store_err(e: StoreError) -> ApiErr {
    match e {
        StoreError::Conflict(m) => api_err(409, "conflict", &m),
        StoreError::VersionMismatch => api_err(409, "version_mismatch", "stale version"),
        StoreError::NotFound => api_err(404, "not_found", "not found"),
        StoreError::Db(m) => {
            tracing::error!(error = %m, "store failure");
            api_err(500, "internal", "storage failure")
        }
    }
}

/// Parse a JSON body (400 with an `ApiError` instead of axum's plain-text rejection).
pub fn parse_json<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, ApiErr> {
    serde_json::from_slice(body).map_err(|e| api_err(400, "bad_request", &e.to_string()))
}

async fn wait<T: Send + 'static>(task: space3d_tasks::Task<Result<T, StoreError>>) -> Result<T, ApiErr> {
    task.wait()
        .await
        .ok_or_else(|| api_err(500, "internal", "worker gone"))?
        .map_err(store_err)
}

/// Per-request facts every handler needs: client IP (trusted-proxy aware) and the bearer header.
#[derive(Clone, Debug)]
pub struct RequestMeta {
    /// Client IP.
    pub ip: IpAddr,
    /// `Authorization` header.
    pub authorization: Option<String>,
}

impl FromRequestParts<Arc<PlayerService>> for RequestMeta {
    type Rejection = ApiErr;

    async fn from_request_parts(parts: &mut Parts, svc: &Arc<PlayerService>) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map_or(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)), |c| c.0);
        // Every `X-Forwarded-For` line, joined in order: a proxy that adds its own line (rather
        // than appending to the client's) must not leave the client's line as the one read.
        let xff = space3d_net::joined_header(&parts.headers, "x-forwarded-for");
        Ok(Self {
            ip: svc.client_ip(peer, xff.as_deref()),
            authorization: parts
                .headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
        })
    }
}

/// Per-IP request limit, bounded body read + request metrics.
async fn guard(State(svc): State<Arc<PlayerService>>, meta: RequestMeta, req: Request, next: Next) -> Response {
    let route = req
        .extensions()
        .get::<space3d_net::server::axum::extract::MatchedPath>()
        .map_or_else(|| "unmatched".to_owned(), |p| p.as_str().to_owned());
    if !svc.limits.lock().http.check(&rate_key(meta.ip), svc.limiter_now()) {
        svc.metrics.http(&route, 429, 0.0);
        let mut e = api_err(429, "rate_limited", "too many requests");
        e.1.retry_after_ms = Some(1_000);
        return e.into_response();
    }
    // The whole body is read here, bounded in size and in time: a client trickling a body
    // must not hold a request (and its connection slot) open.
    let (parts, body) = req.into_parts();
    let limit = svc.config.player_service.max_body_bytes as usize;
    let body = match tokio::time::timeout(
        svc.config.limits.http_body_timeout,
        space3d_net::server::axum::body::to_bytes(body, limit),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => {
            svc.metrics.http(&route, 413, 0.0);
            return api_err(413, "too_large", "request body too large").into_response();
        }
        Err(_) => {
            svc.metrics.http(&route, 408, 0.0);
            return api_err(408, "timeout", "request body too slow").into_response();
        }
    };
    let req = Request::from_parts(parts, space3d_net::server::axum::body::Body::from(body));
    let started = std::time::Instant::now();
    let res = next.run(req).await;
    svc.metrics
        .http(&route, res.status().as_u16(), started.elapsed().as_secs_f64());
    res
}

fn cors(origins: &[String]) -> CorsLayer {
    let origins: Vec<HeaderValue> = origins.iter().filter_map(|o| HeaderValue::from_str(o).ok()).collect();
    CorsLayer::new()
        .allow_origin(origins)
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::PATCH, Method::DELETE])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE])
        .max_age(Duration::from_secs(600))
}

/// Public API router (with auth routes, CORS, body limit, rate limit).
pub fn public_router(svc: Arc<PlayerService>) -> Router {
    let limit = svc.config.player_service.max_body_bytes as usize;
    let origins = svc.config.player_service.cors_origins.clone();
    let api = Router::new()
        .route("/v1/me", get(me_get).patch(me_patch))
        .route("/v1/me/data/{key}", get(data_get).put(data_put))
        .route("/v1/matchmaking/tickets", post(mm_create))
        .route("/v1/matchmaking/tickets/{id}", get(mm_poll).delete(mm_cancel))
        .route("/v1/play/quick", post(quick_play))
        .route("/v1/sessions/rejoin", post(rejoin))
        .route("/v1/lobbies", post(lobby_create))
        .route("/v1/lobbies/join", post(lobby_join))
        .route("/v1/lobbies/{id}", get(lobby_get))
        .route("/v1/lobbies/{id}/{action}", post(lobby_action))
        .route("/v1/leaderboards/{board}", get(lb_top))
        .route("/v1/leaderboards/{board}/me", get(lb_me))
        .route("/v1/status", get(status))
        .with_state(svc.clone())
        .merge(super::auth::routes(svc.clone()));
    let mut router = api
        .layer(middleware::from_fn_with_state(svc.clone(), guard))
        .layer(DefaultBodyLimit::max(limit));
    if !origins.is_empty() {
        router = router.layer(cors(&origins));
    }
    router
}

/// Slowest accepted upload of an internal request body (checkpoints ≤ 16 MiB).
pub const INTERNAL_BODY_TIMEOUT: Duration = Duration::from_secs(30);

/// Guard of every internal route, run **before** any body is read. Requests with the fleet
/// token pass (game servers upload a checkpoint per room and interval: they must not share the
/// anonymous per-IP budget); anything else counts against the per-IP request rate (429) and is
/// refused (401), without the server buffering its body.
async fn internal_guard(
    State(svc): State<Arc<PlayerService>>,
    meta: RequestMeta,
    req: Request,
    next: Next,
) -> Response {
    if fleet_token_ok(&svc, &meta) {
        return next.run(req).await;
    }
    if !svc.limits.lock().http.check(&rate_key(meta.ip), svc.limiter_now()) {
        let mut e = api_err(429, "rate_limited", "too many requests");
        e.1.retry_after_ms = Some(1_000);
        return e.into_response();
    }
    match fleet_auth(&svc, &meta) {
        Ok(()) => next.run(req).await,
        Err(e) => e.into_response(),
    }
}

/// Internal (fleet-token) checkpoint routes. The fleet WebSocket is mounted by [`mount`] and
/// authenticated at the upgrade (`gs::fleet::fleet_upgrade_authorized`).
pub fn internal_router(svc: Arc<PlayerService>) -> Router {
    Router::new()
        .route(
            "/internal/v1/rooms/{id}/checkpoint",
            put(cp_put).get(cp_get).delete(cp_delete),
        )
        .route("/internal/v1/checkpoints", get(cp_recent))
        .with_state(svc.clone())
        .layer(DefaultBodyLimit::max(crate::gs::checkpoint::MAX_CHECKPOINT_BYTES))
        .layer(middleware::from_fn_with_state(svc, internal_guard))
}

/// Mount the whole player-service API on `server`: public `/v1/*`, internal checkpoints and
/// the fleet WebSocket (`/internal/v1/fleet`, drained by the supervisor).
pub fn mount(server: HttpServer, svc: Arc<PlayerService>, fleet_ws: &WsServer) -> HttpServer {
    server
        .merge(public_router(svc.clone()))
        .merge(internal_router(svc))
        .ws("/internal/v1/fleet", fleet_ws)
}

// ---- helpers ------------------------------------------------------------------------------

type Res<T> = Result<Json<T>, ApiErr>;

/// Whether the request carries the fleet token (constant-time compare, no logging).
fn fleet_token_ok(svc: &PlayerService, meta: &RequestMeta) -> bool {
    let token = meta
        .authorization
        .as_deref()
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or_default();
    !svc.fleet_token.is_empty() && crate::security::ct_eq(token.as_bytes(), svc.fleet_token.as_bytes())
}

fn fleet_auth(svc: &PlayerService, meta: &RequestMeta) -> Result<(), ApiErr> {
    if !fleet_token_ok(svc, meta) {
        tracing::warn!(event = "bad_fleet_token", ip = %meta.ip, "internal endpoint refused");
        return Err(api_err(401, "unauthorized", "fleet token required"));
    }
    Ok(())
}

fn best_region(pings: &BTreeMap<String, u32>) -> String {
    pings
        .iter()
        .min_by_key(|(_, p)| **p)
        .map(|(r, _)| r.clone())
        .unwrap_or_default()
}

fn clean_pings(pings: BTreeMap<String, u32>) -> BTreeMap<String, u32> {
    pings
        .into_iter()
        .filter(|(r, _)| !r.is_empty() && r.len() <= 32)
        .take(16)
        .collect()
}

async fn account_of(svc: &PlayerService, player: PlayerId) -> Result<super::store::Account, ApiErr> {
    wait(svc.store.account(player))
        .await?
        .ok_or_else(|| api_err(401, "unauthorized", "account gone"))
}

// ---- profile & data ------------------------------------------------------------------------

async fn me_get(State(svc): State<Arc<PlayerService>>, meta: RequestMeta) -> Res<Profile> {
    let who = require(&svc, &meta)?;
    Ok(Json(profile(&account_of(&svc, who.player()).await?)))
}

async fn me_patch(State(svc): State<Arc<PlayerService>>, meta: RequestMeta, body: Bytes) -> Res<Profile> {
    let who = require(&svc, &meta)?;
    let req: UpdateProfileRequest = parse_json(&body)?;
    let account = match req.display_name {
        Some(name) => wait(svc.store.set_display_name(who.player(), sanitize_name(&name)?)).await?,
        None => account_of(&svc, who.player()).await?,
    };
    Ok(Json(profile(&account)))
}

fn valid_key(key: &str) -> bool {
    (1..=64).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

async fn data_get(
    State(svc): State<Arc<PlayerService>>,
    meta: RequestMeta,
    Path(key): Path<String>,
) -> Res<PlayerDataEntry> {
    let who = require(&svc, &meta)?;
    if !valid_key(&key) {
        return Err(api_err(400, "invalid_key", "keys are 1-64 of [A-Za-z0-9_.-]"));
    }
    wait(svc.store.get_data(who.player(), vec![key]))
        .await?
        .into_iter()
        .next()
        .map(Json)
        .ok_or_else(|| api_err(404, "not_found", "no such key"))
}

async fn data_put(
    State(svc): State<Arc<PlayerService>>,
    meta: RequestMeta,
    Path(key): Path<String>,
    body: Bytes,
) -> Res<PlayerDataEntry> {
    let who = require(&svc, &meta)?;
    if !svc.config.player_service.client_writable_keys.iter().any(|k| k == &key) {
        return Err(api_err(403, "read_only", "this key is written by game servers only"));
    }
    let req: PutPlayerDataRequest = parse_json(&body)?;
    let op = format!(
        "client:{}:{:032x}",
        who.player(),
        u128::from_le_bytes(crate::security::random_bytes::<16>())
    );
    let writes = vec![DataWrite {
        key,
        value: req.value,
        expected_version: req.expected_version,
    }];
    wait(svc.store.apply_writes(op, who.player(), writes, svc.now_ms()))
        .await?
        .into_iter()
        .next()
        .map(Json)
        .ok_or_else(|| api_err(500, "internal", "write lost"))
}

// ---- matchmaking ---------------------------------------------------------------------------

async fn mm_create(State(svc): State<Arc<PlayerService>>, meta: RequestMeta, body: Bytes) -> Res<MatchmakeResponse> {
    let who = require(&svc, &meta)?;
    let req: MatchmakeRequest = parse_json(&body)?;
    let account = account_of(&svc, who.player()).await?;
    let ticket = super::new_ticket_id();
    let entry = QueueEntry {
        ticket: ticket.clone(),
        player: account.id,
        display_name: account.display_name,
        rating: account.rating,
        region_pings: clean_pings(req.region_pings),
        enqueued_ms: svc.now_ms(),
        build: Build::new(&req.game_id, req.schema_hash),
    };
    match svc.matchmaker.lock().enqueue(&req.queue, entry) {
        Ok(()) => Ok(Json(MatchmakeResponse { ticket_id: ticket })),
        Err(MatchmakingError::UnknownQueue(q)) => Err(api_err(404, "unknown_queue", &format!("no queue {q}"))),
        Err(MatchmakingError::AlreadyQueued) => Err(api_err(409, "already_queued", "already in a queue")),
    }
}

#[derive(serde::Deserialize)]
struct WaitQuery {
    #[serde(default)]
    wait: Option<u64>,
}

fn ticket_status(state: TicketState) -> TicketStatus {
    match state {
        TicketState::Searching { waited_ms } => TicketStatus::Searching {
            waited_s: (waited_ms / 1000) as u32,
            eta_s: None,
        },
        TicketState::Matched(None) => TicketStatus::Searching {
            waited_s: 0,
            eta_s: Some(1),
        },
        TicketState::Matched(Some(assignment)) => TicketStatus::Found { assignment },
        TicketState::Expired => TicketStatus::Expired,
        TicketState::Cancelled => TicketStatus::Cancelled,
    }
}

async fn mm_poll(
    State(svc): State<Arc<PlayerService>>,
    meta: RequestMeta,
    Path(id): Path<String>,
    Query(q): Query<WaitQuery>,
) -> Res<TicketStatus> {
    let who = require(&svc, &meta)?;
    let ticket = TicketId(id);
    let deadline = std::time::Instant::now() + Duration::from_secs(q.wait.unwrap_or(0).min(20));
    loop {
        let (owner, state) = {
            let mm = svc.matchmaker.lock();
            (mm.owner(&ticket), mm.state(&ticket, svc.now_ms()))
        };
        if owner != Some(who.player()) {
            return Err(api_err(404, "not_found", "no such ticket"));
        }
        let state = state.ok_or_else(|| api_err(404, "not_found", "no such ticket"))?;
        let pending = matches!(state, TicketState::Searching { .. } | TicketState::Matched(None));
        if !pending || std::time::Instant::now() >= deadline {
            return Ok(Json(ticket_status(state)));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn mm_cancel(
    State(svc): State<Arc<PlayerService>>,
    meta: RequestMeta,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiErr> {
    let who = require(&svc, &meta)?;
    let ticket = TicketId(id);
    let mut mm = svc.matchmaker.lock();
    if mm.owner(&ticket) != Some(who.player()) {
        return Err(api_err(404, "not_found", "no such ticket"));
    }
    if mm.cancel(&ticket) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(api_err(409, "not_cancellable", "ticket already matched or finished"))
    }
}

// ---- quick play / rejoin -------------------------------------------------------------------

/// Mode names: 1-32 of `[A-Za-z0-9_.-]`.
pub fn valid_mode(mode: &str) -> bool {
    (1..=32).contains(&mode.len()) && mode.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

fn limited(retry_after_ms: u32, what: &str) -> ApiErr {
    let mut e = api_err(429, "rate_limited", what);
    e.1.retry_after_ms = Some(retry_after_ms);
    e
}

async fn quick_play(State(svc): State<Arc<PlayerService>>, meta: RequestMeta, body: Bytes) -> Res<Assignment> {
    let who = require(&svc, &meta)?;
    let req: QuickPlayRequest = parse_json(&body)?;
    if !valid_mode(&req.mode) {
        return Err(api_err(400, "bad_request", "mode: 1-32 of [A-Za-z0-9_.-]"));
    }
    if !svc.limits.lock().quick_play.check(&who.player(), svc.limiter_now()) {
        return Err(limited(1_000, "too many quick plays"));
    }
    let account = account_of(&svc, who.player()).await?;
    let extra = serde_json::json!({ "rating": account.rating });
    let build = Build::new(&req.game_id, req.schema_hash);
    let (server, spec) = svc
        .quick_play_room(&best_region(&req.region_pings), &req.mode, &build, account.id)
        .map_err(|why| match why {
            super::QuickPlayRefusal::NoCapacity => {
                let mut e = api_err(503, "no_capacity", "no game server available");
                e.1.retry_after_ms = Some(5_000);
                e
            }
            super::QuickPlayRefusal::UnknownMode => api_err(404, "unknown_mode", "no server hosts this mode"),
            super::QuickPlayRefusal::RoomRate => limited(10_000, "too many new rooms"),
        })?;
    Ok(Json(svc.issue_ticket(
        account.id,
        &account.display_name,
        &server,
        &spec,
        None,
        extra,
    )))
}

/// Longest a rejoin waits for a persistent room that its draining server froze for a handoff
/// (`FleetUp::RoomMoving`) until the room's final checkpoint is stored.
const HANDOFF_REJOIN_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Wait (bounded, real time) while `player`'s room is being handed off: going back to the
/// frozen room would fail, and restoring it now would use an older checkpoint.
async fn wait_for_handoff(svc: &PlayerService, player: PlayerId) -> Result<(), ApiErr> {
    let started = std::time::Instant::now();
    loop {
        let moving = {
            let fleet = svc.fleet.lock();
            fleet.room_of(player).is_some_and(|(room, server)| {
                fleet.room(room).is_some_and(|e| e.moving && !e.orphaned)
                    && fleet
                        .server(&server)
                        .is_some_and(|s| s.state != super::fleet::ServerState::Dead)
            })
        };
        if !moving {
            return Ok(());
        }
        if started.elapsed() >= HANDOFF_REJOIN_WAIT {
            return Err(api_err(
                503,
                "room_moving",
                "the room is moving to another server; retry",
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

async fn rejoin(State(svc): State<Arc<PlayerService>>, meta: RequestMeta) -> Res<Assignment> {
    let who = require(&svc, &meta)?;
    let account = account_of(&svc, who.player()).await?;
    let now = svc.now_ms();
    let reserve = svc.config.player_service.join_ticket_ttl.as_millis() as i64;
    wait_for_handoff(&svc, account.id).await?;
    let mut abandoned: Option<(RoomId, Option<space3d_replication::MatchId>)> = None;
    let placed = {
        let mut fleet = svc.fleet.lock();
        let (room, server_id) = fleet
            .room_of(account.id)
            .ok_or_else(|| api_err(404, "not_found", "no room to rejoin"))?;
        let entry = fleet
            .room(room)
            .cloned()
            .ok_or_else(|| api_err(404, "not_found", "room gone"))?;
        let spec = entry
            .spec
            .clone()
            .ok_or_else(|| api_err(404, "not_found", "room cannot be restored"))?;
        let team = spec
            .teams
            .iter()
            .position(|t| t.contains(&account.id))
            .map(|t| t as u16);
        // The hosting server still runs the room (it is in its heartbeat, even while it
        // drains): the rejoin goes back to that same room — a match finishes where it runs.
        // Only a room whose server was lost (its lease expired), or that a draining server
        // handed off (checkpoint stored, `RoomClosed { restorable }`), is restored elsewhere.
        // A lease also expires when only the fleet link is cut while the server keeps running
        // its rooms (a player-service outage must not end matches): the restored copy is then
        // the authoritative one, and the stale copy is closed as soon as its server is heard
        // from again (`FleetDown::RoomSuperseded`).
        let host = fleet
            .server(&server_id)
            .filter(|s| s.state != super::fleet::ServerState::Dead)
            .cloned();
        let abandon = entry.status.as_ref().is_some_and(|s| s.abandon_on_loss);
        if let (false, Some(host)) = (entry.orphaned, host) {
            Some((host, spec, team))
        } else if entry.orphaned && abandon {
            // `RecoveryPolicy::Abandon`: void the room; clients go back to matchmaking.
            fleet.abandon_room(room);
            abandoned = Some((room, spec.match_id));
            None
        } else {
            // Server lost or room handed off: restore the room elsewhere from its checkpoint,
            // on a server of the same game and schema (it must be able to load the checkpoint).
            let region = fleet
                .server(&server_id)
                .map(|s| s.registration.region.clone())
                .unwrap_or_default();
            let seats = u32::from(spec.max_players.max(1));
            let target = fleet
                .allocate(
                    &region,
                    &entry.build.game_id,
                    entry.build.schema_hash,
                    seats,
                    now,
                    reserve,
                )
                .ok_or_else(|| api_err(503, "no_capacity", "no compatible game server available"))?;
            fleet.reassign(room, &target.registration.server_id, now);
            tracing::info!(room = %room, from = %server_id, to = %target.registration.server_id, "room moved for rejoin");
            svc.send_fleet(
                &target.registration.server_id,
                crate::gs::fleet::FleetDown::PrepareRoom { spec: spec.clone() },
            );
            Some((target, spec, team))
        }
    };
    if let Some((room, match_id)) = abandoned {
        tracing::info!(room = %room, ?match_id, "orphaned room abandoned (recovery policy); match void");
        // Its checkpoint is never restored (the room is void); do not keep it until GC.
        drop(svc.store.delete_checkpoint(room));
        let detail = serde_json::json!({ "room": room, "match_id": match_id, "reason": "server lost" });
        let _ = wait(svc.store.audit("system".into(), "match_void".into(), detail, now)).await;
        svc.metrics.match_void();
        return Err(api_err(404, "abandoned", "the match was voided; queue again"));
    }
    let (server, spec, team) = placed.ok_or_else(|| api_err(404, "not_found", "no room to rejoin"))?;
    let extra = serde_json::json!({ "rating": account.rating });
    Ok(Json(svc.issue_ticket(
        account.id,
        &account.display_name,
        &server,
        &spec,
        team,
        extra,
    )))
}

// ---- lobbies ---------------------------------------------------------------------------------

fn lobby_err(e: String) -> ApiErr {
    let status = if e.contains("not found") || e.contains("not a member") {
        404
    } else {
        409
    };
    api_err(status, "lobby", &e)
}

async fn lobby_create(State(svc): State<Arc<PlayerService>>, meta: RequestMeta, body: Bytes) -> Res<LobbyState> {
    let who = require(&svc, &meta)?;
    let req: CreateLobbyRequest = parse_json(&body)?;
    if !valid_mode(&req.mode) {
        return Err(api_err(400, "bad_request", "mode: 1-32 of [A-Za-z0-9_.-]"));
    }
    if req.game_id.len() > 64 {
        return Err(api_err(400, "bad_request", "game_id too long"));
    }
    let account = account_of(&svc, who.player()).await?;
    let code = super::lobbies::random_code();
    let mut lobbies = svc.lobbies.lock();
    // A host has one open lobby: this closes their previous one.
    let state = lobbies.create(
        account.id,
        &account.display_name,
        &req.mode,
        req.max_players,
        code,
        svc.now_ms(),
    );
    lobbies.set_build(&state.lobby_id, Build::new(&req.game_id, req.schema_hash));
    drop(lobbies);
    Ok(Json(state))
}

async fn lobby_join(State(svc): State<Arc<PlayerService>>, meta: RequestMeta, body: Bytes) -> Res<LobbyState> {
    let who = require(&svc, &meta)?;
    let req: JoinLobbyRequest = parse_json(&body)?;
    let account = account_of(&svc, who.player()).await?;
    svc.lobbies
        .lock()
        .join(&req.code, account.id, &account.display_name, svc.now_ms())
        .map(Json)
        .map_err(lobby_err)
}

async fn lobby_get(
    State(svc): State<Arc<PlayerService>>,
    meta: RequestMeta,
    Path(id): Path<String>,
    Query(q): Query<WaitQuery>,
) -> Res<LobbyState> {
    let who = require(&svc, &meta)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(q.wait.unwrap_or(0).min(20));
    let (start_version, _) = svc
        .lobbies
        .lock()
        .view(&id, who.player())
        .ok_or_else(|| api_err(404, "not_found", "no such lobby"))?;
    loop {
        let (version, state) = svc
            .lobbies
            .lock()
            .view(&id, who.player())
            .ok_or_else(|| api_err(404, "not_found", "no such lobby"))?;
        if version != start_version || std::time::Instant::now() >= deadline {
            return Ok(Json(state));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[derive(serde::Deserialize, Default)]
struct ReadyBody {
    #[serde(default = "yes")]
    ready: bool,
}

fn yes() -> bool {
    true
}

async fn lobby_action(
    State(svc): State<Arc<PlayerService>>,
    meta: RequestMeta,
    Path((id, action)): Path<(String, String)>,
    body: Bytes,
) -> Res<LobbyState> {
    let who = require(&svc, &meta)?;
    let now = svc.now_ms();
    let mut lobbies = svc.lobbies.lock();
    let result = match action.as_str() {
        "ready" => {
            let ready = if body.is_empty() {
                true
            } else {
                parse_json::<ReadyBody>(&body)?.ready
            };
            lobbies.ready(&id, who.player(), ready, now)
        }
        "start" => lobbies.start(&id, who.player(), now),
        "leave" => lobbies.leave(&id, who.player(), now).and_then(|()| {
            lobbies
                .view(&id, who.player())
                .map(|(_, s)| s)
                // Left: the caller is no longer a member (answered as 404 below).
                .ok_or_else(|| "left".to_owned())
        }),
        _ => return Err(api_err(404, "not_found", "unknown lobby action")),
    };
    match result {
        Ok(state) => Ok(Json(state)),
        Err(e) if e == "left" => Err(api_err(404, "not_found", "left the lobby")),
        Err(e) => Err(lobby_err(e)),
    }
}

// ---- leaderboards / status -----------------------------------------------------------------

#[derive(serde::Deserialize)]
struct TopQuery {
    #[serde(default)]
    top: Option<u32>,
}

fn valid_board(b: &str) -> bool {
    valid_key(b)
}

async fn lb_top(
    State(svc): State<Arc<PlayerService>>,
    Path(board): Path<String>,
    Query(q): Query<TopQuery>,
) -> Res<Vec<LeaderboardRow>> {
    if !valid_board(&board) {
        return Err(api_err(400, "invalid_board", "bad board name"));
    }
    let n = q.top.unwrap_or(50).clamp(1, 100);
    Ok(Json(wait(svc.store.leaderboard_top(board, n)).await?))
}

async fn lb_me(
    State(svc): State<Arc<PlayerService>>,
    meta: RequestMeta,
    Path(board): Path<String>,
) -> Res<Option<LeaderboardRow>> {
    let who = require(&svc, &meta)?;
    if !valid_board(&board) {
        return Err(api_err(400, "invalid_board", "bad board name"));
    }
    Ok(Json(wait(svc.store.leaderboard_rank(board, who.player())).await?))
}

/// `ws://host/play` → `http://host/ping`.
pub fn ping_url(public_url: &str) -> String {
    let http = public_url
        .replacen("wss://", "https://", 1)
        .replacen("ws://", "http://", 1);
    match http
        .find("://")
        .and_then(|i| http[i + 3..].find('/').map(|j| i + 3 + j))
    {
        Some(path) => format!("{}/ping", &http[..path]),
        None => format!("{http}/ping"),
    }
}

async fn status(State(svc): State<Arc<PlayerService>>) -> Json<ServiceStatus> {
    let mut regions: BTreeMap<String, String> = BTreeMap::new();
    for s in svc.fleet.lock().servers() {
        if s.state == super::fleet::ServerState::Ready {
            regions
                .entry(s.registration.region.clone())
                .or_insert_with(|| ping_url(&s.registration.public_url));
        }
    }
    Json(ServiceStatus {
        protocol: space3d_replication::PROTOCOL_VERSION,
        version: env!("CARGO_PKG_VERSION").into(),
        motd: svc.config.player_service.motd.clone(),
        regions: regions
            .into_iter()
            .map(|(region, ping_url)| RegionInfo { region, ping_url })
            .collect(),
    })
}

// ---- internal: checkpoints -------------------------------------------------------------------

async fn cp_put(
    State(svc): State<Arc<PlayerService>>,
    meta: RequestMeta,
    Path(id): Path<String>,
    body: space3d_net::server::axum::body::Body,
) -> Result<StatusCode, ApiErr> {
    fleet_auth(&svc, &meta)?;
    let room: RoomId = id.parse().map_err(|_| api_err(400, "bad_request", "room id"))?;
    let limit = crate::gs::checkpoint::MAX_CHECKPOINT_BYTES;
    let body = tokio::time::timeout(
        INTERNAL_BODY_TIMEOUT,
        space3d_net::server::axum::body::to_bytes(body, limit),
    )
    .await
    .map_err(|_| api_err(408, "timeout", "request body too slow"))?
    .map_err(|_| api_err(413, "too_large", "checkpoint too large"))?;
    // Ordered by the blob's own (epoch, capture time), never by arrival: a slow upload of an
    // older capture, or one from a room's previous incarnation, never replaces newer state.
    let (id, key) = crate::gs::checkpoint::Checkpoint::peek(&body)
        .map_err(|e| api_err(400, "bad_checkpoint", &format!("invalid checkpoint: {e}")))?;
    if id != room {
        return Err(api_err(400, "bad_checkpoint", "checkpoint is for another room"));
    }
    if !wait(svc.store.put_checkpoint(room, body.to_vec(), key)).await? {
        tracing::debug!(%room, epoch = key.epoch, saved_ms = key.saved_ms, "stale checkpoint upload ignored");
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn cp_get(State(svc): State<Arc<PlayerService>>, meta: RequestMeta, Path(id): Path<String>) -> Response {
    if let Err(e) = fleet_auth(&svc, &meta) {
        return e.into_response();
    }
    let Ok(room) = id.parse::<RoomId>() else {
        return api_err(400, "bad_request", "room id").into_response();
    };
    match wait(svc.store.get_checkpoint(room)).await {
        Ok(Some((_, bytes))) => ([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response(),
        Ok(None) => api_err(404, "not_found", "no checkpoint").into_response(),
        Err(e) => e.into_response(),
    }
}

async fn cp_delete(
    State(svc): State<Arc<PlayerService>>,
    meta: RequestMeta,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiErr> {
    fleet_auth(&svc, &meta)?;
    let room: RoomId = id.parse().map_err(|_| api_err(400, "bad_request", "room id"))?;
    wait(svc.store.delete_checkpoint(room)).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(serde::Deserialize)]
struct SinceQuery {
    #[serde(default)]
    since: i64,
}

async fn cp_recent(
    State(svc): State<Arc<PlayerService>>,
    meta: RequestMeta,
    Query(q): Query<SinceQuery>,
) -> Res<Vec<RoomId>> {
    fleet_auth(&svc, &meta)?;
    Ok(Json(wait(svc.store.recent_checkpoints(q.since)).await?))
}

#[cfg(test)]
mod tests {
    #[test]
    fn ping_urls() {
        assert_eq!(
            super::ping_url("wss://gs-1.play.example.com/play"),
            "https://gs-1.play.example.com/ping"
        );
        assert_eq!(
            super::ping_url("ws://127.0.0.1:7777/play"),
            "http://127.0.0.1:7777/ping"
        );
        assert_eq!(super::ping_url("ws://host"), "http://host/ping");
    }
}
