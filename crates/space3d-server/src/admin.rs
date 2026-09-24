//! Admin HTTP API (separate listener, default `127.0.0.1:9090`; never public).
//! `Authorization: Bearer <admin token>` compared in constant time against the SHA-256 hashes
//! of the configured tokens; every mutating call is audit-logged (`space3d::audit` target +
//! player-service `audit_log` table). `/metrics`, `/healthz`, `/readyz` need no token.
//!
//! ```text
//! GET  /admin/v1/status                     process, role, phase, rooms, players, tick p99
//! GET  /admin/v1/rooms                      RoomStatus list
//! GET  /admin/v1/rooms/{id}                 RoomDetail (players, per-client rtt/bandwidth)
//! POST /admin/v1/rooms/{id}/close           {reason}
//! POST /admin/v1/rooms/{id}/checkpoint      force a checkpoint
//! GET  /admin/v1/players[?q=prefix]         connected players (game server) / search (player service)
//! POST /admin/v1/players/{id}/kick          {reason}
//! POST /admin/v1/bans                       {player?|cidr?, duration_s?, reason}  (player service: persisted + pushed)
//! DELETE /admin/v1/bans/{id}
//! POST /admin/v1/broadcast                  {text, room?, level}
//! POST /admin/v1/drain                      start graceful drain
//! GET  /admin/v1/config                     redacted effective config
//! GET  /admin/v1/schema                     ReplicationRegistry::manifest() of this game
//! POST /admin/v1/backup                     player service: VACUUM INTO data_dir/backup-<ts>.db
//! GET  /metrics
//! ```

use crate::clock::UnixClock;
use crate::gs::fleet::FleetDown;
use crate::gs::room::notice_level;
use crate::gs::rooms::{RoomCommand, RoomManager};
use crate::health::HealthState;
use crate::lifecycle::LifecycleHandle;
use crate::logging::AUDIT_TARGET;
use crate::metrics::Metrics;
use crate::ps::routes::{ApiErr, api_err, parse_json};
use crate::security::bans::{Ban, BanList, BanTarget};
use crate::security::netaddr::Cidr;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use space3d_net::server::axum::body::Bytes;
use space3d_net::server::axum::extract::{Path, Query, Request, State};
use space3d_net::server::axum::http::{StatusCode, header};
use space3d_net::server::axum::middleware::{self, Next};
use space3d_net::server::axum::response::{IntoResponse, Response};
use space3d_net::server::axum::routing::{delete, get, post};
use space3d_net::server::axum::{Json, Router};
use space3d_replication::{PlayerId, RoomId};
use std::sync::Arc;
use std::time::Duration;

/// Everything the admin routes need (clone = shared).
#[derive(Clone)]
pub struct AdminState {
    /// SHA-256 of each accepted admin token.
    pub token_hashes: Arc<Vec<[u8; 32]>>,
    /// Allow requests without a token (dev only, when no token is configured).
    pub open: bool,
    /// Rooms (game-server role).
    pub rooms: Option<RoomManager>,
    /// Player service (player-service role).
    pub players: Option<Arc<crate::ps::PlayerService>>,
    /// Bans.
    pub bans: BanList,
    /// Lifecycle control (drain).
    pub lifecycle: LifecycleHandle,
    /// Metrics registry.
    pub metrics: Metrics,
    /// Health.
    pub health: HealthState,
    /// Redacted config text.
    pub config_text: Arc<String>,
    /// Schema manifest lines.
    pub schema: Arc<Vec<String>>,
    /// Static facts (server id, role, region, version) for `/status`.
    pub info: Arc<serde_json::Value>,
    /// Wall clock.
    pub clock: Arc<dyn UnixClock>,
    /// Whether readiness requires fleet registration (game-server role).
    pub needs_registration: bool,
    /// Backup directory (player service).
    pub data_dir: std::path::PathBuf,
}

/// SHA-256 of a token (what [`AdminState::token_hashes`] holds).
pub fn hash_token(token: &str) -> [u8; 32] {
    Sha256::digest(token.trim().as_bytes()).into()
}

#[derive(Clone, Debug)]
struct Actor(String);

async fn auth(State(state): State<AdminState>, mut req: Request, next: Next) -> Response {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    let actor = match token {
        Some(t) => {
            let h = hash_token(t);
            // Compare against every configured hash (no early exit on match).
            let ok = state
                .token_hashes
                .iter()
                .fold(false, |acc, k| crate::security::ct_eq(k, &h) | acc);
            if !ok {
                tracing::warn!(event = "admin_auth_failed", "bad admin token");
                return api_err(401, "unauthorized", "bad admin token").into_response();
            }
            format!(
                "admin:{}",
                h[..4].iter().map(|b| format!("{b:02x}")).collect::<String>()
            )
        }
        None if state.open => "admin:dev".to_owned(),
        None => return api_err(401, "unauthorized", "admin token required").into_response(),
    };
    req.extensions_mut().insert(Actor(actor));
    next.run(req).await
}

fn audit(state: &AdminState, req: &Request, action: &str, detail: serde_json::Value) {
    let actor = req
        .extensions()
        .get::<Actor>()
        .map_or_else(|| "admin:?".to_owned(), |a| a.0.clone());
    audit_as(state, &actor, action, detail);
}

fn audit_as(state: &AdminState, actor: &str, action: &str, detail: serde_json::Value) {
    tracing::info!(target: AUDIT_TARGET, event = "admin_action", %actor, %action, detail = %detail, "admin action");
    if let Some(ps) = &state.players {
        drop(
            ps.store
                .audit(actor.to_owned(), action.to_owned(), detail, state.clock.now_ms()),
        );
    }
}

/// Build the admin router (admin API with auth + unauthenticated `/metrics`, `/healthz`,
/// `/readyz`).
pub fn router(state: AdminState) -> Router {
    let api = Router::new()
        .route("/admin/v1/status", get(status))
        .route("/admin/v1/rooms", get(rooms))
        .route("/admin/v1/rooms/{id}", get(room_detail))
        .route("/admin/v1/rooms/{id}/{action}", post(room_action))
        .route("/admin/v1/players", get(players))
        .route("/admin/v1/players/{id}/kick", post(kick))
        .route("/admin/v1/bans", post(ban).get(list_bans))
        .route("/admin/v1/bans/{id}", delete(unban))
        .route("/admin/v1/broadcast", post(broadcast))
        .route("/admin/v1/drain", post(drain))
        .route("/admin/v1/config", get(config))
        .route("/admin/v1/schema", get(schema))
        .route("/admin/v1/backup", post(backup))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state.clone());
    let metrics = state.metrics.clone();
    api.route(
        "/metrics",
        get(move || {
            let text = metrics.render();
            async move { ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], text) }
        }),
    )
    .merge(state.health.router(state.clock.clone(), state.needs_registration))
}

type Res<T> = Result<Json<T>, ApiErr>;

fn room_id(id: &str) -> Result<RoomId, ApiErr> {
    id.parse()
        .map_err(|_| api_err(400, "bad_request", "room ids are 16 hex digits"))
}

fn gs(state: &AdminState) -> Result<&RoomManager, ApiErr> {
    state
        .rooms
        .as_ref()
        .ok_or_else(|| api_err(404, "not_game_server", "this process runs no rooms"))
}

async fn status(State(state): State<AdminState>) -> Json<serde_json::Value> {
    let rooms = state.rooms.as_ref().map(RoomManager::list).unwrap_or_default();
    let players: u32 = rooms.iter().map(|r| u32::from(r.players)).sum();
    let tick_p99 = rooms.iter().map(|r| r.tick_p99_us).max().unwrap_or(0);
    let now = state.clock.now_ms();
    Json(serde_json::json!({
        "info": *state.info,
        "phase": format!("{:?}", state.lifecycle.phase()),
        "live": state.health.is_live(now),
        "ready": state.health.is_ready(now, state.needs_registration),
        "draining": state.health.is_draining(),
        "registered": state.health.is_registered(),
        "rooms": rooms.len(),
        "players": players,
        "tick_p99_us": tick_p99,
        "fleet_servers": state.players.as_ref().map(|p| p.fleet.lock().servers().len()),
    }))
}

async fn rooms(State(state): State<AdminState>) -> Res<Vec<crate::gs::rooms::RoomStatus>> {
    Ok(Json(gs(&state)?.list()))
}

async fn room_detail(State(state): State<AdminState>, Path(id): Path<String>) -> Res<crate::gs::rooms::RoomDetail> {
    let id = room_id(&id)?;
    let handle = gs(&state)?
        .get(id)
        .ok_or_else(|| api_err(404, "not_found", "no such room"))?;
    let (tx, rx) = futures::channel::oneshot::channel();
    handle
        .command(RoomCommand::Inspect(tx))
        .map_err(|_| api_err(503, "busy", "room command queue full"))?;
    match tokio::time::timeout(Duration::from_secs(2), rx).await {
        Ok(Ok(detail)) => Ok(Json(detail)),
        _ => Err(api_err(504, "timeout", "room did not answer")),
    }
}

#[derive(Deserialize, Default)]
struct ReasonBody {
    #[serde(default)]
    reason: Option<String>,
}

async fn room_action(
    State(state): State<AdminState>,
    Path((id, action)): Path<(String, String)>,
    req: Request,
) -> Response {
    let result = async {
        let id = room_id(&id)?;
        let handle = gs(&state)?
            .get(id)
            .ok_or_else(|| api_err(404, "not_found", "no such room"))?;
        let body = body_bytes(req.into_body()).await?;
        let cmd = match action.as_str() {
            "close" => {
                let reason = if body.is_empty() {
                    None
                } else {
                    parse_json::<ReasonBody>(&body)?.reason
                };
                RoomCommand::Close {
                    reason: reason.unwrap_or_else(|| "closed by admin".into()),
                }
            }
            "checkpoint" => RoomCommand::Checkpoint,
            _ => return Err(api_err(404, "not_found", "unknown room action")),
        };
        handle
            .command(cmd)
            .map_err(|_| api_err(503, "busy", "room command queue full"))?;
        Ok::<_, ApiErr>(serde_json::json!({ "room": id, "action": action }))
    }
    .await;
    match result {
        Ok(detail) => {
            audit_as(&state, "admin", "room_action", detail);
            StatusCode::ACCEPTED.into_response()
        }
        Err(e) => e.into_response(),
    }
}

async fn body_bytes(body: space3d_net::server::axum::body::Body) -> Result<Bytes, ApiErr> {
    space3d_net::server::axum::body::to_bytes(body, 64 * 1024)
        .await
        .map_err(|e| api_err(400, "bad_request", &e.to_string()))
}

#[derive(Deserialize)]
struct PlayersQuery {
    #[serde(default)]
    q: Option<String>,
}

#[derive(Serialize)]
struct PlayerRow {
    player_id: PlayerId,
    room: Option<RoomId>,
    display_name: Option<String>,
}

async fn players(State(state): State<AdminState>, Query(q): Query<PlayersQuery>) -> Res<Vec<PlayerRow>> {
    if let Some(rooms) = &state.rooms {
        let mut out = Vec::new();
        for s in rooms.list() {
            out.extend(s.player_ids.iter().map(|p| PlayerRow {
                player_id: *p,
                room: Some(s.room_id),
                display_name: None,
            }));
        }
        return Ok(Json(out));
    }
    let ps = state
        .players
        .as_ref()
        .ok_or_else(|| api_err(404, "not_found", "no players here"))?;
    let accounts = ps
        .store
        .search_accounts(q.q.unwrap_or_default(), 100)
        .wait()
        .await
        .ok_or_else(|| api_err(500, "internal", "worker gone"))?
        .map_err(crate::ps::routes::store_err)?;
    let fleet = ps.fleet.lock();
    Ok(Json(
        accounts
            .into_iter()
            .map(|a| PlayerRow {
                player_id: a.id,
                room: fleet.room_of(a.id).map(|(r, _)| r),
                display_name: Some(a.display_name),
            })
            .collect(),
    ))
}

fn kick_everywhere(state: &AdminState, player: PlayerId, reason: &str) {
    if let Some(rooms) = &state.rooms
        && let Some(room) = rooms.find_player(player)
        && let Some(h) = rooms.get(room)
    {
        let _ = h.command(RoomCommand::Kick {
            player,
            reason: reason.to_owned(),
        });
    }
    if let Some(ps) = &state.players {
        ps.send_fleet(
            "*",
            FleetDown::Kick {
                player,
                reason: reason.to_owned(),
            },
        );
    }
}

async fn kick(State(state): State<AdminState>, Path(id): Path<String>, req: Request) -> Response {
    let actor = req.extensions().get::<Actor>().cloned();
    let result = async {
        let player: PlayerId = id.parse().map_err(|_| api_err(400, "bad_request", "player id"))?;
        let body = body_bytes(req.into_body()).await?;
        let reason = if body.is_empty() {
            None
        } else {
            parse_json::<ReasonBody>(&body)?.reason
        }
        .unwrap_or_else(|| "kicked by admin".into());
        kick_everywhere(&state, player, &reason);
        Ok::<_, ApiErr>(serde_json::json!({ "player": player, "reason": reason }))
    }
    .await;
    match result {
        Ok(detail) => {
            audit_as(&state, &actor.map_or_else(|| "admin".into(), |a| a.0), "kick", detail);
            StatusCode::ACCEPTED.into_response()
        }
        Err(e) => e.into_response(),
    }
}

/// `POST /admin/v1/bans` body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BanRequest {
    /// Player to ban.
    #[serde(default)]
    pub player: Option<PlayerId>,
    /// Address range to ban.
    #[serde(default)]
    pub cidr: Option<String>,
    /// Duration in seconds (`None` = permanent).
    #[serde(default)]
    pub duration_s: Option<u64>,
    /// Reason (shown to the player).
    pub reason: String,
}

async fn ban(State(state): State<AdminState>, req: Request) -> Response {
    let actor = req.extensions().get::<Actor>().cloned();
    let result = async {
        let body = body_bytes(req.into_body()).await?;
        let r: BanRequest = parse_json(&body)?;
        let target = match (r.player, r.cidr.as_deref()) {
            (Some(p), None) => BanTarget::Player(p),
            (None, Some(c)) => BanTarget::Ip(Cidr::parse(c).map_err(|e| api_err(400, "bad_request", &e))?),
            _ => return Err(api_err(400, "bad_request", "exactly one of player / cidr")),
        };
        let now = state.clock.now_ms();
        let mut ban = Ban {
            id: 0,
            target,
            until_ms: r.duration_s.map(|s| now + (s.min(100 * 365 * 86_400) * 1000) as i64),
            reason: r.reason.chars().take(256).collect(),
        };
        match &state.players {
            Some(ps) => {
                ban = ps
                    .store
                    .add_ban(ban, now)
                    .wait()
                    .await
                    .ok_or_else(|| api_err(500, "internal", "worker gone"))?
                    .map_err(crate::ps::routes::store_err)?;
                ps.send_fleet("*", FleetDown::Ban(ban.clone()));
            }
            None => ban.id = u64::from_le_bytes(crate::security::random_bytes::<8>()) >> 12,
        }
        state.bans.upsert(ban.clone());
        if let BanTarget::Player(p) = ban.target {
            kick_everywhere(&state, p, &ban.reason);
        }
        Ok::<_, ApiErr>(ban)
    }
    .await;
    match result {
        Ok(ban) => {
            audit_as(
                &state,
                &actor.map_or_else(|| "admin".into(), |a| a.0),
                "ban",
                serde_json::to_value(&ban).unwrap_or_default(),
            );
            (StatusCode::CREATED, Json(ban)).into_response()
        }
        Err(e) => e.into_response(),
    }
}

async fn list_bans(State(state): State<AdminState>) -> Json<Vec<Ban>> {
    Json(state.bans.snapshot())
}

async fn unban(State(state): State<AdminState>, Path(id): Path<u64>, req: Request) -> Response {
    let mut removed = state.bans.remove(id);
    if let Some(ps) = &state.players {
        removed |= ps
            .store
            .remove_ban(id)
            .wait()
            .await
            .and_then(Result::ok)
            .unwrap_or(false);
        ps.send_fleet("*", FleetDown::Unban { ban_id: id });
    }
    audit(
        &state,
        &req,
        "unban",
        serde_json::json!({ "id": id, "removed": removed }),
    );
    if removed {
        StatusCode::NO_CONTENT.into_response()
    } else {
        api_err(404, "not_found", "no such ban").into_response()
    }
}

#[derive(Deserialize)]
struct BroadcastBody {
    text: String,
    #[serde(default)]
    room: Option<RoomId>,
    #[serde(default)]
    level: Option<String>,
}

async fn broadcast(State(state): State<AdminState>, req: Request) -> Response {
    let actor = req.extensions().get::<Actor>().cloned();
    let result = async {
        let body = body_bytes(req.into_body()).await?;
        let b: BroadcastBody = parse_json(&body)?;
        let text: String = b.text.chars().take(256).collect();
        let level = notice_level(b.level.as_deref().unwrap_or("info"));
        if let Some(rooms) = &state.rooms {
            match b.room {
                Some(id) => {
                    let h = rooms.get(id).ok_or_else(|| api_err(404, "not_found", "no such room"))?;
                    let _ = h.command(RoomCommand::Broadcast {
                        text: text.clone(),
                        level,
                    });
                }
                None => rooms.broadcast(|| RoomCommand::Broadcast {
                    text: text.clone(),
                    level,
                }),
            }
        }
        if let Some(ps) = &state.players {
            ps.send_fleet(
                "*",
                FleetDown::Broadcast {
                    room_id: b.room,
                    text: text.clone(),
                },
            );
        }
        Ok::<_, ApiErr>(serde_json::json!({ "text": text, "room": b.room }))
    }
    .await;
    match result {
        Ok(detail) => {
            audit_as(
                &state,
                &actor.map_or_else(|| "admin".into(), |a| a.0),
                "broadcast",
                detail,
            );
            StatusCode::ACCEPTED.into_response()
        }
        Err(e) => e.into_response(),
    }
}

async fn drain(State(state): State<AdminState>, req: Request) -> StatusCode {
    // Idempotent: a second admin call must not escalate to a forced stop.
    if state.lifecycle.requests() == 0 {
        state.lifecycle.drain();
    }
    audit(&state, &req, "drain", serde_json::Value::Null);
    StatusCode::ACCEPTED
}

async fn config(State(state): State<AdminState>) -> Response {
    (
        [(header::CONTENT_TYPE, "application/toml")],
        state.config_text.as_str().to_owned(),
    )
        .into_response()
}

async fn schema(State(state): State<AdminState>) -> Json<Vec<String>> {
    Json(state.schema.as_ref().clone())
}

async fn backup(State(state): State<AdminState>, req: Request) -> Response {
    let Some(ps) = &state.players else {
        return api_err(404, "not_player_service", "backups are taken on the player service").into_response();
    };
    let path = state
        .data_dir
        .join(format!("backup-{}.db", state.clock.now_ms()))
        .display()
        .to_string();
    let result = ps.store.backup(path.clone()).wait().await;
    audit(&state, &req, "backup", serde_json::json!({ "path": path }));
    match result {
        Some(Ok(())) => (StatusCode::CREATED, Json(serde_json::json!({ "path": path }))).into_response(),
        Some(Err(e)) => crate::ps::routes::store_err(e).into_response(),
        None => api_err(500, "internal", "worker gone").into_response(),
    }
}
