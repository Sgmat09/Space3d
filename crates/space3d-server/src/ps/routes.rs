//! Router assembly. Public `/v1/*` (routes documented in `space3d_replication::api`), internal
//! `/internal/v1/*` (fleet token: `WS /internal/v1/fleet`, `PUT|GET /internal/v1/rooms/{id}/checkpoint`
//! ≤ 16 MiB), plus `/healthz`, `/readyz`. Body limit `player_service.max_body_bytes`, CORS
//! allowlist, per-IP request rate (`http_rps_per_ip`), JSON errors (`api::ApiError`).

use std::sync::Arc;

/// Public API router.
pub fn public_router(svc: Arc<super::PlayerService>) -> space3d_net::server::axum::Router {
    todo!()
}

/// Internal (fleet) router; `fleet_ws` is the WebSocket server the supervisor drains.
pub fn internal_router(
    svc: Arc<super::PlayerService>,
    fleet_ws: &space3d_net::WsServer,
) -> space3d_net::server::axum::Router {
    todo!()
}
