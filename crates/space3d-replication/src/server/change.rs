//! Encode-once change detection (`replication::server_collect`, send ticks only).
//!
//! hecs has no change ticks (and `hecs::ChangeTracker` allows one tracker per world per type),
//! so every replicated component of every `Replicated` entity is encoded with its codec once
//! per send tick and compared with the cached bytes; a difference sets `changed_tick`.
//! Comparing *quantized* bytes suppresses sub-quantum jitter for free, and the cached bytes are
//! copied into every client's packet (no per-client re-encoding). Presence differences become
//! insert/remove; entities that disappeared (or lost `Replicated`) become despawn tombstones.

use crate::registry::ComponentKind;
use crate::{NetId, Tick};
use smallvec::SmallVec;
use space3d_core::Entity;

/// Cached encoding of one component of one entity.
#[derive(Clone, Debug, Default)]
pub struct CompSlot {
    /// Kind.
    pub kind: u16,
    /// Last tick the encoded bytes changed.
    pub changed_tick: Tick,
    /// Encoded bytes (codec output).
    pub bytes: SmallVec<[u8; 24]>,
}

/// One replicated entity.
#[derive(Clone, Debug)]
pub struct EntityRecord {
    /// ECS entity.
    pub entity: Entity,
    /// Wire id.
    pub net_id: NetId,
    /// Tick the record was created.
    pub spawn_tick: Tick,
    /// Presence bitmask over `ComponentKind`s.
    pub present: u64,
    /// Last insert/remove tick.
    pub structure_tick: Tick,
    /// Kinds removed at `structure_tick` (for `HAS_REMOVED`).
    pub removed: u64,
    /// Encoded components (ascending kind).
    pub comps: SmallVec<[CompSlot; 6]>,
    /// Owner / controller (for audience + priority).
    pub owner: Option<crate::PlayerId>,
    /// Controller.
    pub controller: Option<crate::PlayerId>,
    /// Team.
    pub team: Option<u16>,
    /// World position for spatial interest (from `Transform`).
    pub pos: Option<space3d_core::glam::Vec3>,
    /// Priority multiplier.
    pub priority: f32,
}

/// Record table + tombstones for the room (internal to the replication builder).
#[derive(Default)]
pub struct ChangeCache {
    /// Records in `NetId` order.
    pub records: Vec<EntityRecord>,
    /// `(net_id, despawn tick)` kept until every client has acked it (or `resume_window`).
    pub tombstones: Vec<(NetId, Tick)>,
}

impl ChangeCache {
    /// Run collection for `tick` over the room world (see the module docs).
    pub fn collect(
        &mut self,
        tick: Tick,
        world: &mut space3d_core::World,
        registry: &crate::ReplicationRegistry,
        ids: &mut crate::mapping::NetIdMap,
    ) {
        todo!()
    }

    /// Record of a `NetId`.
    pub fn get(&self, id: NetId) -> Option<&EntityRecord> {
        todo!()
    }

    /// Kind helper.
    pub fn has(&self, id: NetId, kind: ComponentKind) -> bool {
        todo!()
    }
}
