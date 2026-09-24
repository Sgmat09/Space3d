//! Lag compensation ("favour the shooter", Source-style). **Phase 2**: implement after the
//! core replication and prediction are green; the wire already carries what it needs
//! (`InputWindow::view_delay_q8`).
//!
//! Hitboxes of `LagCompensated` entities are recorded every tick (PostUpdate) into a ring of
//! `max_rewind_ticks`. A hit-scan from client C at server tick T rewinds to
//! `T − view_delay(C)` (clamped to `max_rewind`, default 200 ms) and interpolates between the
//! two bracketing ticks exactly as the client rendered them.

use crate::{NetClientId, NetId, Tick};
use space3d_core::Entity;
use space3d_core::glam::Vec3;

/// Hit shape in the entity's local space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HitShape {
    /// Capsule along local Y.
    Capsule {
        /// Radius.
        radius: f32,
        /// Half height of the cylinder part.
        half_height: f32,
    },
    /// Axis-aligned box.
    Aabb {
        /// Half extents.
        half: Vec3,
    },
}

/// Marks an entity whose hitbox history is recorded.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LagCompensated {
    /// Hitbox.
    pub shape: HitShape,
}

/// Result of a rewound raycast.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LagHit {
    /// Entity hit.
    pub entity: Entity,
    /// Its wire id.
    pub net_id: NetId,
    /// Distance along the ray.
    pub distance: f32,
    /// Hit point (rewound space).
    pub point: Vec3,
}

/// Hitbox history (resource; inserted by `ServerReplicationPlugin` when enabled).
#[derive(Debug, Default)]
pub struct LagCompensation {
    max_rewind_ticks: u32,
}

impl LagCompensation {
    /// Raycast against hitboxes as client `shooter` saw them at `tick` (excluding `exclude`).
    pub fn raycast(
        &self,
        shooter: NetClientId,
        tick: Tick,
        origin: Vec3,
        dir: Vec3,
        max_distance: f32,
        exclude: Option<Entity>,
    ) -> Option<LagHit> {
        todo!()
    }
}
