//! Interest management: which entities are relevant to which client.
//!
//! Rooms are the hard partition (separate `World`s). Inside a room: `InterestPolicy::RoomWide`
//! or a spatial grid queried around each client's `InterestSource`s (enter at `radius`, leave
//! at `radius × hysteresis`). Precedence: own entities (always) > `Visibility` override >
//! `NetRelevance` > `VisibilityRule`s > spatial. Children inherit their root's relevance so
//! `Parent` never dangles. Component-level `Audience` is applied later, per component.

use crate::PlayerId;
use space3d_core::glam::Vec3;
use space3d_core::{Entity, World};

/// Game relevancy rule (fog of war, team vision). Evaluated only for spatial candidates.
pub trait VisibilityRule: Send + Sync + 'static {
    /// Whether `viewer` may see `entity`.
    fn visible(&self, world: &World, viewer: PlayerId, entity: Entity) -> bool;
}

impl<F: Fn(&World, PlayerId, Entity) -> bool + Send + Sync + 'static> VisibilityRule for F {
    fn visible(&self, world: &World, viewer: PlayerId, entity: Entity) -> bool {
        self(world, viewer, entity)
    }
}

/// Uniform 2D (XZ) hash grid of record indices, updated incrementally.
#[derive(Debug, Default)]
pub struct SpatialGrid {
    cell: f32,
    cells: crate::FastMap<(i32, i32), smallvec::SmallVec<[u32; 8]>>,
}

impl SpatialGrid {
    /// Grid with `cell` metres per cell.
    pub fn new(cell: f32) -> Self {
        Self {
            cell,
            cells: Default::default(),
        }
    }

    /// Cell of a position.
    pub fn cell_of(&self, p: Vec3) -> (i32, i32) {
        ((p.x / self.cell).floor() as i32, (p.z / self.cell).floor() as i32)
    }

    /// Move / insert a record.
    pub fn update(&mut self, record: u32, old: Option<Vec3>, new: Option<Vec3>) {
        todo!()
    }

    /// Records in cells overlapping the circle (caller filters by exact distance).
    pub fn query(&self, center: Vec3, radius: f32, out: &mut Vec<u32>) {
        todo!()
    }
}
