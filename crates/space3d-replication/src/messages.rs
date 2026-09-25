//! Typed messages (server events / RPCs and client commands), postcard-encoded on the
//! `MSG_RELIABLE` / `MSG_UNRELIABLE` channels. Register with `add_message::<M>(name, config)`.
//!
//! * Server → client: `NetServer::send(target, &msg)` → `Events<FromServer<M>>` on the client
//!   (released immediately, or at the render clock for `EventTiming::Interpolated`).
//! * Client → server: `NetClient::send(&msg)` → size/rate checks → [`NetMessage::validate`] →
//!   `Events<FromClient<M>>` in the room (read them in `Update`, not inside the sim).
//!
//! Ordering: on stream transports (WebSocket) messages and snapshots share one ordered stream;
//! a message referencing an entity (`NetEntity`) is decoded after the snapshot that spawned
//! it. On datagram transports unresolved references wait up to 1 s (see `mapping`).

use crate::{NetClientId, PlayerId, Tick};
use serde::Serialize;
use serde::de::DeserializeOwned;
use space3d_core::Entity;

/// Why a client message was rejected (counts as a strike).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid message: {0}")]
pub struct InvalidMessage(pub &'static str);

/// A network message type.
pub trait NetMessage: Serialize + DeserializeOwned + Send + Sync + 'static {
    /// Server-side validation of client commands (lengths, ranges). Default: accept.
    fn validate(&self) -> Result<(), InvalidMessage> {
        Ok(())
    }
}

/// A validated client command, as seen by the room (`Events<FromClient<M>>`).
#[derive(Clone, Debug, PartialEq)]
pub struct FromClient<M> {
    /// Room-local slot of the sender.
    pub client: NetClientId,
    /// Sender's account.
    pub player: PlayerId,
    /// Server tick at which it was received.
    pub tick: Tick,
    /// Payload.
    pub message: M,
}

/// A server message, as seen by a client (`Events<FromServer<M>>`).
#[derive(Clone, Debug, PartialEq)]
pub struct FromServer<M> {
    /// Server tick at which it was sent.
    pub tick: Tick,
    /// Payload.
    pub message: M,
}

/// Recipients of a server message.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// Every connected client.
    All,
    /// One slot.
    Client(NetClientId),
    /// One player (if connected).
    Player(PlayerId),
    /// Everyone but one player.
    AllExcept(PlayerId),
    /// A set of players.
    Players(Vec<PlayerId>),
    /// Clients that currently have `entity` in scope.
    ObserversOf(Entity),
    /// Players on a team (`NetTeam`).
    Team(u16),
}
