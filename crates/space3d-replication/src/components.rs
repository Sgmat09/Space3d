//! Marker and control components.
//!
//! | component | side | meaning |
//! |---|---|---|
//! | [`Replicated`] | server | replicate this entity (serde, so it can be persisted in checkpoints) |
//! | [`ControlledBy`] | server | this player's inputs drive it; predicted on that client |
//! | [`Owner`] | server | ownership for [`crate::Audience::Owner`] / priority without input control |
//! | [`NetTeam`] | server | team for [`crate::Audience::Team`] / `Target::Team` |
//! | [`NetPriority`] | server | bandwidth priority multiplier (default 1) |
//! | [`NetRelevance`] | server | interest policy override |
//! | [`InterestSource`] | server | a viewer position for spatial interest (usually the pawn) |
//! | [`Simulated`] | both | processed by `SimSchedule` (server: every simulated entity; client: predicted only) |
//! | [`NetEntity`] | both | the entity's `NetId` (inserted automatically) |
//! | [`Remote`], [`Predicted`], [`Interpolated`], [`Controlled`] | client | set at spawn |

use crate::{NetId, PlayerId};
use serde::{Deserialize, Serialize};

/// Replicate this entity to clients that can see it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replicated;

/// Inputs of this player drive the entity (`PlayerInput<I>` is written onto it every tick).
/// Keyed by `PlayerId`, so control survives reconnect/resume. Implies ownership.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ControlledBy(pub PlayerId);

/// Owning player without input control (e.g. a projectile).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Owner(pub PlayerId);

/// Team membership.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NetTeam(pub u16);

/// Bandwidth priority multiplier (default 1.0).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct NetPriority(pub f32);

/// Interest policy override for one entity.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum NetRelevance {
    /// Visible to every client in the room.
    Global,
    /// Visible to clients with an [`InterestSource`] within `radius` (room default if `None`).
    Spatial {
        /// Override radius.
        radius: Option<f32>,
    },
    /// Only the owner / controller.
    OwnerOnly,
    /// Only via `NetServer::set_visibility`.
    Manual,
}

/// A viewer for spatial interest (on the player's pawn or camera entity; needs `Transform`
/// and `ControlledBy`/`Owner` to know whose view it is).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct InterestSource {
    /// View radius in metres.
    pub radius: f32,
}

/// Processed by the shared `SimSchedule`. The server marks every entity its sim should touch;
/// the client marks only locally predicted entities, so interpolated remote entities are never
/// advanced by simulation code. Sim systems query `With<&Simulated>`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Simulated;

/// The entity's network id (both sides; inserted by the replication plugins).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NetEntity(pub NetId);

/// Client: spawned by replication.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Remote;

/// Client: locally predicted (controlled by this client and has `Predict*` components).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Predicted;

/// Client: has at least one interpolated component.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Interpolated;

/// Client: controlled by this client's inputs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Controlled;
