//! Game-server role.
//!
//! ```text
//! tokio (space3d_tasks::runtime())                     shard threads × S (std::thread)
//!  axum /play (WsServerTransport) ─LinkEvent─▶ ingress ─RoomInbound (bounded, try_send)─▶ room App
//!  /ping, /healthz, /readyz            (Gatekeeper: Hello, ticket HMAC,     (built on the shard;
//!  admin :9090, /metrics                jti, bans, per-IP limits, routing)   App is !Send)
//!  fleet link task, checkpoint uploads, outbox flusher        ◀─ RoomStatus (1 Hz, Mutex) ─┘
//! supervisor App (main thread, 10 Hz): lifecycle, room directory, fleet heartbeats, drain
//! space3d-db worker: local outbox + local checkpoints      rayon: checkpoint serialization
//! ```
//!
//! Rule: a room tick never awaits, blocks or takes a contended lock; everything crossing
//! threads goes through bounded queues (`try_send` / `try_recv`).

pub mod checkpoint;
pub mod fleet;
pub mod gatekeeper;
pub mod ingress;
pub mod outbox;
pub mod room;
pub mod rooms;
pub mod shard;
pub mod transport_ws;

use crate::config::ServerConfig;
use crate::game::GameDefinition;
use std::net::SocketAddr;
use std::sync::Arc;

/// Supervisor plugin for the game-server role: binds `/play`, starts shards, the ingress task,
/// the fleet link (HTTP/WS or in-process), checkpoint/outbox services; publishes
/// [`GameServerHandle`].
pub struct GameServerPlugin {
    /// The game.
    pub game: Arc<dyn GameDefinition>,
    /// Config.
    pub config: ServerConfig,
    /// Fleet link (in-process for all-in-one, WS otherwise).
    pub fleet: Arc<dyn fleet::FleetLink>,
}

impl space3d_core::Plugin for GameServerPlugin {
    fn build(&self, app: &mut space3d_core::App) {
        todo!()
    }
}

/// Resource: addresses and handles of a running game server.
#[derive(Clone)]
pub struct GameServerHandle {
    /// Bound `/play` address (real port when configured with 0).
    pub game_addr: SocketAddr,
    /// Bound admin address.
    pub admin_addr: SocketAddr,
    /// Room directory.
    pub rooms: rooms::RoomManager,
    /// Schema hash of the game's protocol.
    pub schema_hash: u64,
}
