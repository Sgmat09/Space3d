//! Authentication: guest/device login, password accounts (feature `password-auth`), refresh
//! rotation with reuse detection, logout, and [`authenticate`] for
//! `Authorization: Bearer <access token>` (stateless HMAC verification + in-process revocation
//! set, warmed from the store at startup). Per-IP and per-username login rate limits; argon2 on
//! the compute pool.

use super::PlayerService;
use super::routes::{ApiErr, RequestMeta, api_err, parse_json, store_err};
use super::store::{Account, Rotation};
use crate::security::netaddr::rate_key;
use crate::security::token::{SessionClaims, sign, verify};
use sha2::{Digest, Sha256};
use space3d_net::server::axum::body::Bytes;
use space3d_net::server::axum::extract::State;
use space3d_net::server::axum::http::StatusCode;
use space3d_net::server::axum::response::{IntoResponse, Response};
use space3d_net::server::axum::routing::post;
use space3d_net::server::axum::{Json, Router};
use space3d_replication::api::{
    ApiError, GuestLoginRequest, PasswordLoginRequest, Profile, RefreshRequest, RegisterRequest, SessionTokens,
};
use std::sync::Arc;

/// A verified caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthedPlayer(pub SessionClaims);

impl AuthedPlayer {
    /// The caller's account.
    pub fn player(&self) -> space3d_replication::PlayerId {
        self.0.player
    }
}

/// Profile DTO of an account.
pub fn profile(a: &Account) -> Profile {
    Profile {
        player_id: a.id,
        display_name: a.display_name.clone(),
        kind: a.kind,
        rating: a.rating,
        created_ms: a.created_ms,
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn b64url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// A fresh opaque refresh token and its stored hash.
pub fn new_refresh_token() -> (String, [u8; 32]) {
    let token = format!("rt.{}", b64url(&crate::security::random_bytes::<32>()));
    let hash = sha256(token.as_bytes());
    (token, hash)
}

fn mint_access(svc: &PlayerService, account: &Account, family: [u8; 16]) -> (String, i64) {
    let now = svc.now_ms();
    let exp = now + svc.config.player_service.session_ttl.as_millis() as i64;
    let token = sign(
        &svc.keys,
        &SessionClaims {
            player: account.id,
            sid: family,
            iat_ms: now,
            exp_ms: exp,
        },
    );
    (token, exp)
}

/// Mint access + refresh tokens for `account` (new refresh family unless `family` is given).
pub fn issue_session(
    svc: &PlayerService,
    account: &Account,
    family: Option<[u8; 16]>,
) -> space3d_tasks::Task<Result<SessionTokens, super::store::StoreError>> {
    let family = family.unwrap_or_else(crate::security::random_bytes::<16>);
    let (refresh, hash) = new_refresh_token();
    let refresh_exp = svc.now_ms() + svc.config.player_service.refresh_ttl.as_millis() as i64;
    let (access, access_exp) = mint_access(svc, account, family);
    let insert = svc.store.insert_refresh(account.id, family, hash, refresh_exp);
    let tokens = SessionTokens {
        access_token: access,
        access_expires_ms: access_exp,
        refresh_token: refresh,
        refresh_expires_ms: refresh_exp,
        profile: profile(account),
    };
    space3d_tasks::spawn(async move {
        insert
            .wait()
            .await
            .unwrap_or_else(|| Err(super::store::StoreError::Db("worker gone".into())))?;
        Ok(tokens)
    })
}

/// Verify a bearer header value.
pub fn authenticate(svc: &PlayerService, authorization: Option<&str>) -> Result<AuthedPlayer, ApiError> {
    let unauthorized = |m: &str| ApiError {
        code: "unauthorized".into(),
        message: m.into(),
        retry_after_ms: None,
    };
    let token = authorization
        .and_then(|h| h.strip_prefix("Bearer ").or_else(|| h.strip_prefix("bearer ")))
        .map(str::trim)
        .ok_or_else(|| unauthorized("missing bearer token"))?;
    let claims: SessionClaims =
        verify(&svc.keys, token, svc.now_ms(), 5_000).map_err(|e| unauthorized(&e.to_string()))?;
    if svc.is_revoked(&claims.sid) {
        return Err(unauthorized("session revoked"));
    }
    if let Some(ban) = svc.bans.player(claims.player, svc.now_ms()) {
        return Err(ApiError {
            code: "banned".into(),
            message: ban.reason,
            retry_after_ms: None,
        });
    }
    Ok(AuthedPlayer(claims))
}

/// Authenticate a request (`401` / `403` on failure).
pub fn require(svc: &PlayerService, meta: &RequestMeta) -> Result<AuthedPlayer, ApiErr> {
    authenticate(svc, meta.authorization.as_deref()).map_err(|e| {
        let status = if e.code == "banned" {
            StatusCode::FORBIDDEN
        } else {
            StatusCode::UNAUTHORIZED
        };
        ApiErr(status, e)
    })
}

/// Trimmed display name: 1–32 characters, no control characters.
pub fn sanitize_name(name: &str) -> Result<String, ApiErr> {
    let name = name.trim();
    let n = name.chars().count();
    if n == 0 || n > 32 || name.chars().any(char::is_control) {
        return Err(api_err(
            400,
            "invalid_name",
            "display names are 1-32 printable characters",
        ));
    }
    Ok(name.to_owned())
}

fn valid_username(u: &str) -> bool {
    (3..=32).contains(&u.len())
        && u.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b))
}

fn check_banned(svc: &PlayerService, account: &Account) -> Result<(), ApiErr> {
    let now = svc.now_ms();
    if let Some(ban) = svc.bans.player(account.id, now) {
        return Err(ApiErr(
            StatusCode::FORBIDDEN,
            ApiError {
                code: "banned".into(),
                message: ban.reason,
                retry_after_ms: None,
            },
        ));
    }
    if account.banned_until_ms.is_some_and(|u| u > now) {
        return Err(api_err(403, "banned", "account banned"));
    }
    Ok(())
}

fn limited(retry_ms: u32) -> ApiErr {
    ApiErr(
        StatusCode::TOO_MANY_REQUESTS,
        ApiError {
            code: "rate_limited".into(),
            message: "too many attempts".into(),
            retry_after_ms: Some(retry_ms),
        },
    )
}

async fn finish(
    svc: &PlayerService,
    account: Account,
    family: Option<[u8; 16]>,
) -> Result<Json<SessionTokens>, ApiErr> {
    let tokens = issue_session(svc, &account, family)
        .wait()
        .await
        .ok_or_else(|| api_err(500, "internal", "worker gone"))?
        .map_err(store_err)?;
    Ok(Json(tokens))
}

/// `POST /v1/auth/guest`.
pub async fn guest_login(
    svc: &PlayerService,
    meta: &RequestMeta,
    req: GuestLoginRequest,
) -> Result<Json<SessionTokens>, ApiErr> {
    if !svc.config.player_service.guest_auth {
        return Err(api_err(403, "disabled", "guest accounts are disabled"));
    }
    if !svc.limits.lock().guest_ip.check(&rate_key(meta.ip), svc.limiter_now()) {
        svc.metrics.login("guest", "rate_limited");
        return Err(limited(15_000));
    }
    let id_ok = (8..=64).contains(&req.device_id.len())
        && req.device_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let secret_ok =
        (32..=128).contains(&req.device_secret.len()) && req.device_secret.bytes().all(|b| b.is_ascii_hexdigit());
    if !id_ok || !secret_ok {
        svc.metrics.login("guest", "invalid");
        return Err(api_err(400, "invalid_device", "device_id / device_secret malformed"));
    }
    let name = req.display_name.as_deref().map(sanitize_name).transpose()?;
    let hash = sha256(format!("{}:{}", req.device_id, req.device_secret.to_ascii_lowercase()).as_bytes());
    let account = svc
        .store
        .guest(hash, name, svc.now_ms())
        .wait()
        .await
        .ok_or_else(|| api_err(500, "internal", "worker gone"))?
        .map_err(store_err)?;
    check_banned(svc, &account)?;
    svc.metrics.login("guest", "ok");
    finish(svc, account, None).await
}

/// `POST /v1/auth/register` (upgrades the caller's guest account when authenticated).
pub async fn register(
    svc: &PlayerService,
    meta: &RequestMeta,
    req: RegisterRequest,
) -> Result<Json<SessionTokens>, ApiErr> {
    password_enabled(svc)?;
    if !svc.limits.lock().login_ip.check(&rate_key(meta.ip), svc.limiter_now()) {
        return Err(limited(60_000));
    }
    let username = req.username.trim().to_ascii_lowercase();
    if !valid_username(&username) {
        return Err(api_err(400, "invalid_username", "usernames are 3-32 of [a-z0-9_.-]"));
    }
    if req.password.chars().count() < 8 || req.password.len() > MAX_PASSWORD_LEN {
        return Err(api_err(400, "weak_password", "passwords are 8-256 characters"));
    }
    let name = sanitize_name(&req.display_name)?;
    let upgrade = match meta.authorization.as_deref() {
        Some(_) => Some(require(svc, meta)?.player()),
        None => None,
    };
    let phc = pw::hash(req.password)
        .await
        .map_err(|e| api_err(400, "weak_password", &e))?;
    let account = svc
        .store
        .create_password(username, phc, name, upgrade, svc.now_ms())
        .wait()
        .await
        .ok_or_else(|| api_err(500, "internal", "worker gone"))?
        .map_err(store_err)?;
    svc.metrics.login("password", "registered");
    finish(svc, account, None).await
}

/// `POST /v1/auth/login`.
pub async fn login(
    svc: &PlayerService,
    meta: &RequestMeta,
    req: PasswordLoginRequest,
) -> Result<Json<SessionTokens>, ApiErr> {
    password_enabled(svc)?;
    let username = req.username.trim().to_ascii_lowercase();
    // Per-username budgets are charged only by *failed* logins, and per address prefix first:
    // other addresses' wrong guesses do not lock the owner out (only a sustained, distributed
    // attack that exhausts the 4× username-wide backstop does).
    let pair = (username.clone(), rate_key(meta.ip));
    {
        let mut limits = svc.limits.lock();
        let now = svc.limiter_now();
        if !limits.login_ip.check(&rate_key(meta.ip), now)
            || !limits.login_user_ip.available(&pair, now)
            || !limits.login_user.available(&username, now)
        {
            svc.metrics.login("password", "rate_limited");
            tracing::warn!(event = "login_rate_limited", ip = %meta.ip, "login attempts limited");
            return Err(limited(60_000));
        }
    }
    let found = svc
        .store
        .by_username(username)
        .wait()
        .await
        .ok_or_else(|| api_err(500, "internal", "worker gone"))?
        .map_err(store_err)?;
    // Unknown users cost the same argon2 verification as known ones.
    let phc = found.as_ref().map_or_else(pw::dummy, |(_, p)| p.clone());
    let ok = pw::verify(req.password, phc).await;
    match found {
        Some((account, _)) if ok => {
            check_banned(svc, &account)?;
            svc.metrics.login("password", "ok");
            finish(svc, account, None).await
        }
        _ => {
            {
                let mut limits = svc.limits.lock();
                let now = svc.limiter_now();
                limits.login_user_ip.check(&pair, now);
                limits.login_user.check(&pair.0, now);
            }
            svc.metrics.login("password", "bad_credentials");
            Err(api_err(401, "bad_credentials", "wrong username or password"))
        }
    }
}

/// Longest accepted password.
const MAX_PASSWORD_LEN: usize = 256;

#[cfg(feature = "password-auth")]
mod pw {
    use crate::security::password;

    pub async fn hash(p: String) -> Result<String, String> {
        password::hash_password(p)
            .wait()
            .await
            .unwrap_or_else(|| Err("hash task lost".into()))
    }

    pub async fn verify(p: String, phc: String) -> bool {
        password::verify_password(p, phc).wait().await.unwrap_or(false)
    }

    pub fn dummy() -> String {
        password::dummy_phc().to_owned()
    }
}

#[cfg(not(feature = "password-auth"))]
mod pw {
    pub async fn hash(_p: String) -> Result<String, String> {
        Err("password accounts are not compiled in".into())
    }

    pub async fn verify(_p: String, _phc: String) -> bool {
        false
    }

    pub fn dummy() -> String {
        String::new()
    }
}

#[cfg(feature = "password-auth")]
fn password_enabled(svc: &PlayerService) -> Result<(), ApiErr> {
    if svc.config.player_service.password_auth {
        Ok(())
    } else {
        Err(api_err(403, "disabled", "password accounts are disabled"))
    }
}

#[cfg(not(feature = "password-auth"))]
fn password_enabled(_svc: &PlayerService) -> Result<(), ApiErr> {
    Err(api_err(403, "disabled", "this build has no password accounts"))
}

/// `POST /v1/auth/refresh`: rotate the refresh token (reuse revokes the whole family).
pub async fn refresh(
    svc: &PlayerService,
    meta: &RequestMeta,
    req: RefreshRequest,
) -> Result<Json<SessionTokens>, ApiErr> {
    if !svc.limits.lock().guest_ip.check(&rate_key(meta.ip), svc.limiter_now()) {
        return Err(limited(15_000));
    }
    let old = sha256(req.refresh_token.as_bytes());
    let (token, new) = new_refresh_token();
    let now = svc.now_ms();
    let exp = now + svc.config.player_service.refresh_ttl.as_millis() as i64;
    let rotation = svc
        .store
        .rotate_refresh(old, new, exp, now)
        .wait()
        .await
        .ok_or_else(|| api_err(500, "internal", "worker gone"))?
        .map_err(store_err)?;
    match rotation {
        Rotation::Rotated { player, family } => {
            let account = svc
                .store
                .account(player)
                .wait()
                .await
                .ok_or_else(|| api_err(500, "internal", "worker gone"))?
                .map_err(store_err)?
                .ok_or_else(|| api_err(401, "unauthorized", "account gone"))?;
            check_banned(svc, &account)?;
            if svc.is_revoked(&family) {
                return Err(api_err(401, "unauthorized", "session revoked"));
            }
            let (access, access_exp) = mint_access(svc, &account, family);
            svc.metrics.login("refresh", "ok");
            Ok(Json(SessionTokens {
                access_token: access,
                access_expires_ms: access_exp,
                refresh_token: token,
                refresh_expires_ms: exp,
                profile: profile(&account),
            }))
        }
        Rotation::ReuseDetected { family } => {
            svc.revoke_sid(family);
            svc.metrics.login("refresh", "reuse");
            tracing::warn!(event = "refresh_reuse", ip = %meta.ip, "refresh token reused; session family revoked");
            Err(api_err(
                401,
                "refresh_reused",
                "refresh token already used; session revoked",
            ))
        }
        Rotation::Invalid => {
            svc.metrics.login("refresh", "invalid");
            Err(api_err(401, "unauthorized", "invalid refresh token"))
        }
    }
}

/// `POST /v1/auth/logout`: revoke the refresh token's family.
pub async fn logout(svc: &PlayerService, req: RefreshRequest) -> Result<StatusCode, ApiErr> {
    let hash = sha256(req.refresh_token.as_bytes());
    let family = svc
        .store
        .revoke_token(hash)
        .wait()
        .await
        .ok_or_else(|| api_err(500, "internal", "worker gone"))?
        .map_err(store_err)?;
    if let Some(f) = family {
        svc.revoke_sid(f);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `/v1/auth/*` routes.
pub fn routes(svc: Arc<PlayerService>) -> Router {
    async fn guest_h(State(svc): State<Arc<PlayerService>>, meta: RequestMeta, body: Bytes) -> Response {
        match parse_json(&body) {
            Ok(req) => guest_login(&svc, &meta, req).await.into_response(),
            Err(e) => e.into_response(),
        }
    }
    async fn register_h(State(svc): State<Arc<PlayerService>>, meta: RequestMeta, body: Bytes) -> Response {
        match parse_json(&body) {
            Ok(req) => register(&svc, &meta, req).await.into_response(),
            Err(e) => e.into_response(),
        }
    }
    async fn login_h(State(svc): State<Arc<PlayerService>>, meta: RequestMeta, body: Bytes) -> Response {
        match parse_json(&body) {
            Ok(req) => login(&svc, &meta, req).await.into_response(),
            Err(e) => e.into_response(),
        }
    }
    async fn refresh_h(State(svc): State<Arc<PlayerService>>, meta: RequestMeta, body: Bytes) -> Response {
        match parse_json(&body) {
            Ok(req) => refresh(&svc, &meta, req).await.into_response(),
            Err(e) => e.into_response(),
        }
    }
    async fn logout_h(State(svc): State<Arc<PlayerService>>, body: Bytes) -> Response {
        match parse_json(&body) {
            Ok(req) => logout(&svc, req).await.into_response(),
            Err(e) => e.into_response(),
        }
    }
    Router::new()
        .route("/v1/auth/guest", post(guest_h))
        .route("/v1/auth/register", post(register_h))
        .route("/v1/auth/login", post(login_h))
        .route("/v1/auth/refresh", post(refresh_h))
        .route("/v1/auth/logout", post(logout_h))
        .with_state(svc)
}
