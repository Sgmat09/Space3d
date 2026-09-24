//! SNAPSHOT block (S→C, unreliable lane, latest-wins). The tick is the Data packet's tick.
//!
//! ```text
//! Snapshot   := SnapshotHeader · varu32 entity_count · Entity* · [HAS_DESPAWNS] Despawns
//! SnapshotHeader := u8 flags · varu32 input_ack_delta · i8 input_slack
//!     flags: bit0 FULL_RESET     client drops every replicated entity not listed (resume / new epoch)
//!            bit1 SYNC_COMPLETE  every in-scope entity has been delivered at least once (loading gate)
//!            bit2 HAS_DESPAWNS
//!     input_ack_delta: tick − newest input tick consumed for this client (u32::MAX = none yet)
//!     input_slack: how many ticks early this client's input for `tick` arrived (i8::MIN = missing)
//! Entity     := varu32 net_id_delta (ascending NetIds; first is absolute)
//!               · u8 op   bit0 SPAWN (full audience-filtered state follows)
//!                         bit1 HAS_REMOVED
//!                         bit2 CONTROLLED (ControlledBy == receiving player)
//!                         bit3 OWNED (Owner/ControlledBy == receiving player)
//!               · varu64 present_mask (bit i = ComponentKind(i) payload follows)
//!               · [HAS_REMOVED] varu64 removed_mask
//!               · for each set bit of present_mask, ascending: varu32 len · codec bytes
//! Despawns   := varu32 n · n × varu32 (net_id_delta << 1 | out_of_scope)
//! ```
//!
//! **Invariant** (makes loss and reordering harmless): whenever an entity is written for a
//! client, *every* component whose `changed_tick` is newer than that client's acked tick for
//! the entity is included. Locally predicted entities are written to their controller in every
//! snapshot with all `Predict*` components (reconciliation needs a confirmation each time).

use crate::NetId;
use crate::wire::{DecodeError, Reader, Writer};

/// Snapshot flag bits.
pub mod flags {
    /// Drop all replicated entities not listed.
    pub const FULL_RESET: u8 = 0x01;
    /// Initial sync finished.
    pub const SYNC_COMPLETE: u8 = 0x02;
    /// Despawn list present.
    pub const HAS_DESPAWNS: u8 = 0x04;
}

/// Entity op bits.
pub mod op {
    /// Spawn: full state follows.
    pub const SPAWN: u8 = 0x01;
    /// Removed-components mask present.
    pub const HAS_REMOVED: u8 = 0x02;
    /// Controlled by the receiving player (predict it).
    pub const CONTROLLED: u8 = 0x04;
    /// Owned by the receiving player.
    pub const OWNED: u8 = 0x08;
}

/// Parsed snapshot header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SnapshotHeader {
    /// [`flags`] bits.
    pub flags: u8,
    /// `tick − newest consumed input tick` (`u32::MAX` = none).
    pub input_ack_delta: u32,
    /// Input slack feedback for the tick pacer.
    pub input_slack: i8,
}

impl SnapshotHeader {
    /// Encode.
    pub fn encode(&self, w: &mut Writer) {
        w.u8(self.flags);
        w.varu32(self.input_ack_delta);
        w.u8(self.input_slack as u8);
    }

    /// Decode (unknown flag bits are rejected).
    pub fn decode(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let flags = r.u8()?;
        if flags & !(flags::FULL_RESET | flags::SYNC_COMPLETE | flags::HAS_DESPAWNS) != 0 {
            return Err(DecodeError::Invalid("snapshot flags"));
        }
        Ok(Self {
            flags,
            input_ack_delta: r.varu32()?,
            input_slack: r.u8()? as i8,
        })
    }
}

/// One decoded entity record (component payloads borrowed from the packet).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityRecord<'a> {
    /// Entity.
    pub net_id: NetId,
    /// [`op`] bits.
    pub op: u8,
    /// Components present in this record.
    pub present: u64,
    /// Components removed since the client's baseline.
    pub removed: u64,
    /// `(kind index, codec bytes)` in ascending kind order.
    pub components: smallvec::SmallVec<[(u16, &'a [u8]); 8]>,
}

/// A despawned / out-of-scope entity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DespawnRecord {
    /// Entity.
    pub net_id: NetId,
    /// Left the client's interest set (vs. destroyed on the server).
    pub out_of_scope: bool,
}

/// Incremental snapshot encoder used by the server's per-client packet builder: it supports
/// rolling back a partially written entity when the byte budget is exceeded.
#[derive(Debug, Default)]
pub struct SnapshotWriter {
    last_id: Option<u32>,
    count: u32,
    body: Writer,
    despawns: Vec<DespawnRecord>,
}

impl SnapshotWriter {
    /// Empty snapshot.
    pub fn new() -> Self {
        Self::default()
    }

    /// Start an entity; returns a checkpoint for [`Self::rollback`]. Ids must ascend.
    pub fn begin_entity(&mut self, net_id: NetId, op: u8, present: u64, removed: u64) -> usize {
        todo!()
    }

    /// Append one component payload (kinds in ascending order, matching `present`).
    pub fn component(&mut self, bytes: &[u8]) {
        todo!()
    }

    /// Undo the entity started at `checkpoint`.
    pub fn rollback(&mut self, checkpoint: usize) {
        todo!()
    }

    /// Queue a despawn record.
    pub fn despawn(&mut self, rec: DespawnRecord) {
        self.despawns.push(rec);
    }

    /// Current encoded size estimate (for budgeting).
    pub fn len(&self) -> usize {
        self.body.len() + self.despawns.len() * 3 + 8
    }

    /// Whether no entity or despawn was written.
    pub fn is_empty(&self) -> bool {
        self.count == 0 && self.despawns.is_empty()
    }

    /// Write header + entities + despawns into `w`.
    pub fn finish(self, header: SnapshotHeader, w: &mut Writer) {
        todo!()
    }
}

/// Decoded snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotView<'a> {
    /// Header.
    pub header: SnapshotHeader,
    /// Entities in ascending `NetId` order.
    pub entities: Vec<EntityRecord<'a>>,
    /// Despawns.
    pub despawns: Vec<DespawnRecord>,
}

impl<'a> SnapshotView<'a> {
    /// Decode a SNAPSHOT block (bounded by the block length; component count ≤ 64).
    pub fn decode(bytes: &'a [u8]) -> Result<Self, DecodeError> {
        todo!()
    }
}
