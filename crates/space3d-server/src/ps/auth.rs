//! Authentication: guest/device login, password accounts (feature `password-auth`), refresh
//! rotation with reuse detection, logout, and the [`AuthedPlayer`] extractor for
//! `Authorization: Bearer <access token>` (stateless HMAC verification + 30 s revocation cache).
//! Per-IP and per-username login rate limits; argon2 on the compute pool.

use crate::security::token::SessionClaims;
use space3d_replication::api::SessionTokens;
use std::sync::Arc;

/// A verified caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthedPlayer(pub SessionClaims);

/// Mint access + refresh tokens for `account` (new refresh family unless `family` is given).
pub fn issue_session(
    svc: &super::PlayerService,
    account: &super::store::Account,
    family: Option<[u8; 16]>,
) -> space3d_tasks::Task<Result<SessionTokens, super::store::StoreError>> {
    todo!()
}

/// Verify a bearer header value.
pub fn authenticate(
    svc: &super::PlayerService,
    authorization: Option<&str>,
) -> Result<AuthedPlayer, space3d_replication::api::ApiError> {
    todo!()
}

/// `/v1/auth/*` routes.
pub fn routes(svc: Arc<super::PlayerService>) -> space3d_net::server::axum::Router {
    todo!()
}
