//! The ingress task (tokio): `WsServerTransport::next_event().await` → `Gatekeeper::handle`
//! → `RoomManager::get_or_create` → `RoomSender::try_send`. On `InboxFull`: unreliable-only
//! packets are dropped (metric), anything else closes the link (`1008`). Also runs
//! `Gatekeeper::sweep` every 250 ms and applies drain state from the lifecycle.

use super::gatekeeper::Gatekeeper;
use super::rooms::RoomManager;
use super::transport_ws::WsServerTransport;
use crate::lifecycle::LifecycleHandle;
use crate::metrics::GameServerMetrics;

/// Handles needed by the ingress task.
pub struct Ingress {
    /// Transport.
    pub transport: WsServerTransport,
    /// Gatekeeper (owned by the task).
    pub gate: Gatekeeper,
    /// Rooms.
    pub rooms: RoomManager,
    /// Drain state.
    pub lifecycle: LifecycleHandle,
    /// Metrics.
    pub metrics: GameServerMetrics,
}

/// Spawn the ingress loop on `space3d_tasks::runtime()`; it ends when the transport closes.
pub fn spawn(ingress: Ingress) -> space3d_tasks::Task<()> {
    todo!()
}
